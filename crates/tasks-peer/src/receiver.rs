//! The receiving daemon's side: admit allowlisted peers only, map a forwarded
//! job to a local spec in the synced copy, and serve events, sync and fetches.
//!
//! The daemon wires the traits below to its ledger, admission and runner; this
//! module owns the protocol rules and nothing else.

use crate::ids::{PeerId, RemoteJobId, SubmitKey};
use crate::outputs::{self, FetchPolicy};
use crate::proto::{
    Body, CancelState, Hello, JobEvent, MAX_CHUNK_BYTES, PeerStatus, Refusal, SubmitJob, Submitted,
    SyncApply, WireEnd, WireOutcome, WireSample,
};
use crate::sync::{SyncStore, check_rel_path};
use crate::transport::{EventStream, Handler, PeerError, StreamPoll};
use std::collections::BTreeSet;
use std::sync::Arc;
use tasks_core::{JobSpec, Phase, Placement};

/// Peers allowed to talk to this daemon, by transport-proven identity.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    peers: BTreeSet<PeerId>,
    open: bool,
}

impl Allowlist {
    /// An allowlist of exactly these peers; empty admits nobody.
    pub fn new(peers: impl IntoIterator<Item = PeerId>) -> Allowlist {
        Allowlist {
            peers: peers.into_iter().collect(),
            open: false,
        }
    }
    /// Admits every peer. For a receiver behind [`crate::trust::TrustedHandler`], which has
    /// already admitted the caller on a grant: the node-id allowlist is gone, grants decide.
    pub fn open() -> Allowlist {
        Allowlist {
            peers: BTreeSet::new(),
            open: true,
        }
    }
    /// True when `peer` is listed.
    pub fn admits(&self, peer: &PeerId) -> bool {
        self.open || self.peers.contains(peer)
    }
}

/// How a finished job ended, as the runner records it.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedJob {
    /// The outcome.
    pub outcome: WireOutcome,
    /// Timing samples measured here.
    pub samples: Vec<WireSample>,
}

/// What the runner knows of a job.
#[derive(Debug, Clone, PartialEq)]
pub struct JobView {
    /// Phase now.
    pub phase: Phase,
    /// Human detail (blocker, slot holder, queue position).
    pub detail: String,
    /// Bytes of log so far (final once finished).
    pub log_len: u64,
    /// Set once the job is over.
    pub finished: Option<FinishedJob>,
}

/// The daemon's runner, as the receiver needs it. All calls are scoped to the
/// submitting peer: a peer can see and cancel only its own jobs.
pub trait JobRunner: Send + Sync {
    /// Admits a job. A repeated `(peer, key)` returns the first job's id and
    /// `true`, starting nothing (R-152). Persist the key with the job so this
    /// holds across a restart.
    fn submit(
        &self,
        peer: &PeerId,
        key: &SubmitKey,
        spec: JobSpec,
    ) -> Result<(RemoteJobId, bool), String>;
    /// The job's state, if it exists and belongs to `peer`.
    fn view(&self, peer: &PeerId, id: RemoteJobId) -> Option<JobView>;
    /// Log bytes from `offset`, at most `max`.
    fn read_log(&self, peer: &PeerId, id: RemoteJobId, offset: u64, max: usize) -> Vec<u8>;
    /// The job's live metrics, when it exists, belongs to `peer` and the runner keeps them.
    fn metrics(
        &self,
        peer: &PeerId,
        id: RemoteJobId,
    ) -> Option<tasks_core::metrics::JobMetricsView> {
        let _ = (peer, id);
        None
    }
    /// Cancels; `None` when unknown.
    fn cancel(&self, peer: &PeerId, id: RemoteJobId) -> Option<CancelState>;
    /// True while a job of `peer` that has not ended runs in (or waits to run in) the synced
    /// copy under `tree`. The receiver then refuses a re-sync of that copy with
    /// [`Refusal::Busy`], so a live job never sees its files replaced.
    fn checkout_busy(&self, peer: &PeerId, tree: &std::path::Path) -> bool {
        let _ = (peer, tree);
        false
    }
}

/// The daemon's status, for the status request.
pub trait StatusSource: Send + Sync {
    /// Current status.
    fn status(&self) -> PeerStatus;
}

/// The receiving peer's own policy (device denylist, Cargo cap, broad-test
/// refusal: R-153). The generic spec rules of `tasks-core` always apply first.
pub trait SubmitPolicy: Send + Sync {
    /// `Err(reason)` refuses the job.
    fn check(&self, spec: &JobSpec) -> Result<(), String>;
    /// Like [`SubmitPolicy::check`], knowing which peer sent the job (for the audit log).
    fn check_from(&self, _from: &PeerId, spec: &JobSpec) -> Result<(), String> {
        self.check(spec)
    }
}

/// A policy that accepts everything `JobSpec::validate` accepts.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl SubmitPolicy for AllowAll {
    fn check(&self, _spec: &JobSpec) -> Result<(), String> {
        Ok(())
    }
}

/// The serving side of a daemon.
pub struct Receiver {
    name: String,
    allow: Allowlist,
    store: Arc<SyncStore>,
    runner: Arc<dyn JobRunner>,
    status: Arc<dyn StatusSource>,
    policy: Arc<dyn SubmitPolicy>,
    fetch: FetchPolicy,
}

impl Receiver {
    /// Assembles a receiver.
    pub fn new(
        name: &str,
        allow: Allowlist,
        store: Arc<SyncStore>,
        runner: Arc<dyn JobRunner>,
        status: Arc<dyn StatusSource>,
        policy: Arc<dyn SubmitPolicy>,
        fetch: FetchPolicy,
    ) -> Receiver {
        Receiver {
            name: name.to_string(),
            allow,
            store,
            runner,
            status,
            policy,
            fetch,
        }
    }

    /// Maps a submission onto a local spec whose cwd is inside the synced copy.
    pub fn local_spec(&self, from: &PeerId, job: SubmitJob) -> Result<JobSpec, Refusal> {
        if !self.store.is_synced(from, &job.checkout) {
            return Err(Refusal::NotSynced {
                checkout: job.checkout,
            });
        }
        job.spec.validate().map_err(|e| Refusal::PolicyDenied {
            reason: e.to_string(),
        })?;
        self.policy
            .check_from(from, &job.spec)
            .map_err(|reason| Refusal::PolicyDenied { reason })?;
        self.store.touch(from, &job.checkout);
        let tree = self.store.tree_root(from, &job.checkout);
        let cwd = if job.rel_cwd.is_empty() {
            tree.clone()
        } else {
            check_rel_path(&job.rel_cwd).map_err(|reason| Refusal::BadPath {
                path: job.rel_cwd.clone(),
                reason,
            })?;
            tree.join(&job.rel_cwd)
        };
        // Never run through a symlinked directory out of the copy.
        let mut walk = tree.clone();
        for c in job.rel_cwd.split('/').filter(|c| !c.is_empty()) {
            walk.push(c);
            if std::fs::symlink_metadata(&walk).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(Refusal::BadPath {
                    path: job.rel_cwd.clone(),
                    reason: format!("{c} is a symlink"),
                });
            }
        }
        if !cwd.is_dir() {
            return Err(Refusal::BadPath {
                path: job.rel_cwd,
                reason: "not a directory in the synced copy".to_string(),
            });
        }
        let mut spec = job.spec;
        spec.cwd = cwd;
        // Placement was the origin's decision; the job runs here and must not
        // be forwarded again. Dependencies are the origin's ordering, already met.
        spec.placement = Placement::Here;
        spec.after = Vec::new();
        Ok(spec)
    }

    fn submit(&self, from: &PeerId, job: SubmitJob) -> Body {
        let key = job.key.clone();
        let spec = match self.local_spec(from, job) {
            Ok(s) => s,
            Err(r) => return Body::Refuse(r),
        };
        match self.runner.submit(from, &key, spec) {
            Ok((remote, duplicate)) => Body::Submitted(Submitted { remote, duplicate }),
            Err(detail) => Body::Refuse(Refusal::PolicyDenied { reason: detail }),
        }
    }

    fn dispatch(&self, from: &PeerId, msg: Body) -> Body {
        match msg {
            Body::Hello(theirs) => {
                let ours = Hello::local(&self.name);
                match ours.negotiate(&theirs) {
                    Ok(_) => Body::HelloAck(ours),
                    Err(r) => Body::Refuse(r),
                }
            }
            Body::StatusRequest => Body::Status(self.status.status()),
            Body::Submit(job) => self.submit(from, job),
            Body::Cancel { remote } => match self.runner.cancel(from, remote) {
                Some(state) => Body::CancelAck { remote, state },
                None => Body::Refuse(Refusal::UnknownJob { remote }),
            },
            Body::JobMetricsRequest { remote } => match self.runner.metrics(from, remote) {
                Some(v) => Body::JobMetrics(crate::proto::fit_metrics(v)),
                None => Body::Refuse(Refusal::UnknownJob { remote }),
            },
            Body::SyncOffer { checkout, .. } | Body::SyncApply(SyncApply { checkout, .. })
                if self
                    .runner
                    .checkout_busy(from, &self.store.tree_root(from, &checkout)) =>
            {
                Body::Refuse(Refusal::Busy { checkout })
            }
            Body::SyncOffer { repo, .. } => match self.store.tips(from, &repo) {
                Ok(tips) => Body::SyncHave { tips },
                Err(r) => Body::Refuse(r),
            },
            Body::SyncPackChunk {
                checkout,
                session,
                offset,
                data,
            } => match self
                .store
                .put_chunk(from, &checkout, session, offset, &data)
            {
                Ok(received) => Body::SyncPackAck { received },
                Err(r) => Body::Refuse(r),
            },
            Body::SyncApply(apply) => match self.store.apply(from, &apply) {
                Ok(summary) => Body::SyncApplied(summary),
                Err(r) => Body::Refuse(r),
            },
            Body::FetchList { checkout, paths } => {
                if !self.store.is_synced(from, &checkout) {
                    return Body::Refuse(Refusal::NotSynced { checkout });
                }
                self.store.touch(from, &checkout);
                let tree = self.store.tree_root(from, &checkout);
                match outputs::list(&tree, &paths, &self.fetch) {
                    Ok(entries) => Body::FetchListing { entries },
                    Err(r) => Body::Refuse(r),
                }
            }
            Body::FetchChunk {
                checkout,
                path,
                hash,
                offset,
                len,
            } => {
                if !self.store.is_synced(from, &checkout) {
                    return Body::Refuse(Refusal::NotSynced { checkout });
                }
                let tree = self.store.tree_root(from, &checkout);
                match outputs::read_chunk(&tree, &path, &hash, offset, len, &self.fetch) {
                    Ok((data, eof)) => Body::FetchData { offset, data, eof },
                    Err(r) => Body::Refuse(r),
                }
            }
            other => Body::Refuse(Refusal::Malformed {
                detail: format!("{} is not a request", crate::fake::kind(&other)),
            }),
        }
    }
}

impl Handler for Receiver {
    fn handle(&self, from: &PeerId, msg: Body) -> Body {
        // Admission comes first and covers every message, hello included (R-154).
        if !self.allow.admits(from) {
            return Body::Refuse(Refusal::NotAllowed {
                peer: from.to_string(),
            });
        }
        self.dispatch(from, msg)
    }

    fn accept_upload(&self, from: &PeerId, head: &Body) -> Result<(), Refusal> {
        if !self.allow.admits(from) {
            return Err(Refusal::NotAllowed {
                peer: from.to_string(),
            });
        }
        match head {
            Body::SyncPackStream { checkout, .. } => {
                if self
                    .runner
                    .checkout_busy(from, &self.store.tree_root(from, checkout))
                {
                    return Err(Refusal::Busy {
                        checkout: checkout.clone(),
                    });
                }
                Ok(())
            }
            _ => Err(Refusal::Malformed {
                detail: "not an upload".to_string(),
            }),
        }
    }

    fn upload(&self, from: &PeerId, head: Body, data: &mut dyn std::io::Read) -> Body {
        if let Err(r) = self.accept_upload(from, &head) {
            return Body::Refuse(r);
        }
        match head {
            Body::SyncPackStream { checkout, session } => {
                match self.store.put_stream(from, &checkout, session, data) {
                    Ok(received) => Body::SyncPackAck { received },
                    Err(r) => Body::Refuse(r),
                }
            }
            _ => Body::Refuse(Refusal::Malformed {
                detail: "not an upload".to_string(),
            }),
        }
    }

    fn events(
        &self,
        from: &PeerId,
        remote: RemoteJobId,
        from_offset: u64,
    ) -> Result<Box<dyn EventStream>, Refusal> {
        if !self.allow.admits(from) {
            return Err(Refusal::NotAllowed {
                peer: from.to_string(),
            });
        }
        let view = self
            .runner
            .view(from, remote)
            .ok_or(Refusal::UnknownJob { remote })?;
        if from_offset > view.log_len {
            return Err(Refusal::OutOfOrder {
                expected: view.log_len,
            });
        }
        Ok(Box::new(JobEvents {
            runner: Arc::clone(&self.runner),
            peer: from.clone(),
            id: remote,
            offset: from_offset,
            last: None,
            finished: false,
        }))
    }
}

/// Events of one job: current phase first, then the log from the offset, then
/// `Finished` once every log byte has been sent, then closed.
struct JobEvents {
    runner: Arc<dyn JobRunner>,
    peer: PeerId,
    id: RemoteJobId,
    offset: u64,
    last: Option<(Phase, String)>,
    finished: bool,
}

impl EventStream for JobEvents {
    fn poll(&mut self) -> Result<StreamPoll, PeerError> {
        if self.finished {
            return Ok(StreamPoll::Closed);
        }
        let Some(view) = self.runner.view(&self.peer, self.id) else {
            return Err(PeerError::Refused(Refusal::UnknownJob { remote: self.id }));
        };
        let now = (view.phase.clone(), view.detail.clone());
        if !view.phase.is_terminal() && self.last.as_ref() != Some(&now) {
            self.last = Some(now);
            return Ok(StreamPoll::Event(JobEvent::Phase {
                phase: view.phase,
                detail: view.detail,
            }));
        }
        if self.offset < view.log_len {
            let data = self
                .runner
                .read_log(&self.peer, self.id, self.offset, MAX_CHUNK_BYTES);
            if data.is_empty() {
                return Ok(StreamPoll::Idle);
            }
            let ev = JobEvent::Log {
                offset: self.offset,
                data: data.clone(),
            };
            self.offset += data.len() as u64;
            return Ok(StreamPoll::Event(ev));
        }
        if view.phase.is_terminal() {
            self.finished = true;
            let fin = view.finished.unwrap_or_else(|| FinishedJob {
                outcome: WireOutcome {
                    end: WireEnd::Lost,
                    exit_code: None,
                    signal: None,
                    wall_ms: 0,
                    rss_bytes: None,
                    cpu_s: None,
                    description: "command (result lost)".to_string(),
                },
                samples: Vec::new(),
            });
            return Ok(StreamPoll::Event(JobEvent::Finished {
                outcome: fin.outcome,
                samples: fin.samples,
                log_len: view.log_len,
            }));
        }
        Ok(StreamPoll::Idle)
    }
}
