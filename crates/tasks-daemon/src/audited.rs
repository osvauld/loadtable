//! Audit hooks for the job lifecycle (design §8).
//!
//! [`AuditedLedger`] wraps the job store: every submission, start, end, cancel and forward goes
//! through the ledger whichever path it takes (the local API, the scheduler, the runner threads,
//! the peer receiver, restart reconciliation), so recording there cannot miss a path. A write is
//! recorded after the ledger accepted it, and a failed audit write never fails the job
//! operation. [`AuditSink`] is what the wrapper and the scheduler share: the log, plus the
//! trust layer to turn a peer's node id or name into the DID the audit rows carry.

use crate::trust_control::TrustControl;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tasks_core::plan::Plan;
use tasks_core::{JobId, JobSpec, MachineName, Phase, Placement, Priority, WaitReason};
use tasks_ledger::{
    AuditEvent, AuditLog, CancelOutcome, ChangeToken, Finish, JobRecord, ListFilter, NewJob,
    PlanContext, PlanSubmitted, PruneReport, Result, RetentionPolicy, StartInfo, Store,
    TimeoutReason, action,
};
use tasks_timing::{History, Sample, SampleKey, TimingError};

/// The audit log and the peer directory the daemon records with.
pub struct AuditSink {
    log: Arc<AuditLog>,
    trust: Mutex<Option<Arc<dyn TrustControl>>>,
}

impl AuditSink {
    /// A sink over `log`, with no peer directory yet.
    pub fn new(log: Arc<AuditLog>) -> Arc<AuditSink> {
        Arc::new(AuditSink {
            log,
            trust: Mutex::new(None),
        })
    }

    /// The log.
    pub fn log(&self) -> &Arc<AuditLog> {
        &self.log
    }

    /// Plugs in the trust layer that knows the paired peers.
    pub fn set_trust(&self, trust: Arc<dyn TrustControl>) {
        *crate::scheduler::lock(&self.trust) = Some(trust);
    }

    /// Records an event.
    pub fn record(&self, ev: AuditEvent) {
        self.log.record(ev);
    }

    fn peers(&self) -> Vec<crate::trust_control::PeerTrustView> {
        let t = crate::scheduler::lock(&self.trust).clone();
        t.map(|t| t.peers()).unwrap_or_default()
    }

    /// The DID of the paired peer with this node id.
    pub fn did_of_node(&self, node_id: &str) -> Option<String> {
        self.peers()
            .into_iter()
            .find(|p| p.node_id == node_id)
            .map(|p| p.did)
    }

    /// The DID of the paired peer with this name.
    pub fn did_of_name(&self, name: &str) -> Option<String> {
        self.peers()
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| p.did)
    }

    /// The name of the paired peer with this DID.
    pub fn name_of_did(&self, did: &str) -> Option<String> {
        self.peers()
            .into_iter()
            .find(|p| p.did == did)
            .map(|p| p.name)
    }

    /// Every paired peer's name by DID, for showing rows.
    pub fn names(&self) -> std::collections::BTreeMap<String, String> {
        self.peers().into_iter().map(|p| (p.did, p.name)).collect()
    }

    /// A peer given by name or DID, as the DID the rows carry. An unknown name is returned as
    /// given, so a filter on it matches nothing rather than everything.
    pub fn resolve_peer(&self, name_or_did: &str) -> String {
        self.did_of_name(name_or_did)
            .unwrap_or_else(|| name_or_did.to_string())
    }

    /// Records a queue class other than normal on a job: who asked (the owner), the class and
    /// the reason a blocking claim carries. `from` is the class before a change; a submit has
    /// none.
    pub fn record_priority(&self, id: JobId, spec: &JobSpec, via: &str, from: Option<Priority>) {
        if spec.priority == Priority::Normal && from.is_none() {
            return;
        }
        self.record(
            AuditEvent::new(action::JOB_PRIORITY)
                .job(id.0)
                .owner(spec.owner.as_str())
                .target(spec.cwd.display().to_string())
                .detail("priority", spec.priority.as_str())
                .detail_opt("from", from.map(Priority::as_str))
                .detail_opt("reason", spec.priority_reason.clone())
                .detail("via", via),
        );
    }

    /// An event for `rec`: its job, owner and the peer it came from or went to.
    pub fn job_event(&self, act: &str, rec: &JobRecord) -> AuditEvent {
        let mut ev = AuditEvent::new(act)
            .job(rec.id.0)
            .owner(rec.spec.owner.as_str());
        let peer = match (&rec.origin, &rec.forward) {
            (Some(o), _) => self.did_of_node(&o.peer),
            (None, Some(f)) => self.did_of_name(&f.peer.0),
            _ => None,
        };
        if let Some(p) = peer {
            ev = ev.peer(p);
        }
        ev
    }
}

/// Link changes per peer in one window; a flapping link is capped, the next row counting the rest.
const LINK_BURST: u32 = 6;
const LINK_WINDOW_MS: i64 = 60_000;

/// Writes peer link changes (up, down, direct or relay) to the audit log. The link probes every
/// peer every few seconds; only a change of state reaches the log, and a link that flaps faster
/// than [`LINK_BURST`] changes a minute is throttled.
pub struct AuditLinks {
    sink: Arc<AuditSink>,
    clock: Arc<dyn tasks_ledger::Clock>,
    throttle: tasks_ledger::Throttle,
    /// Paces the daemon log lines that name a probe's failure; apart from `throttle` so log
    /// lines and audit rows do not use each other's allowance.
    say_throttle: tasks_ledger::Throttle,
    say: Box<dyn Fn(&str) + Send + Sync>,
}

impl AuditLinks {
    /// Records into `sink`; `clock` paces the throttle.
    pub fn new(sink: Arc<AuditSink>, clock: Arc<dyn tasks_ledger::Clock>) -> AuditLinks {
        AuditLinks {
            sink,
            clock,
            throttle: tasks_ledger::Throttle::new(LINK_WINDOW_MS, LINK_BURST),
            say_throttle: tasks_ledger::Throttle::new(LINK_WINDOW_MS, LINK_BURST),
            say: Box::new(|line| crate::daemon_log::say(line)),
        }
    }

    /// Sends the failure lines to `say` instead of the daemon log. For tests.
    #[cfg(test)]
    pub fn with_say(mut self, say: Box<dyn Fn(&str) + Send + Sync>) -> AuditLinks {
        self.say = say;
        self
    }
}

impl crate::peer_link::LinkObserver for AuditLinks {
    /// One daemon log line per new reason, so a peer that cannot be reached is explained where
    /// the operator looks. A peer that flaps between reasons is capped like the audit rows, the
    /// next line counting what was left out (R-696).
    fn failed(&self, peer: &crate::peers_config::PeerSpec, reason: &str) {
        let Some(dropped) = self.say_throttle.admit(&peer.name, self.clock.now_ms()) else {
            return;
        };
        let more = if dropped > 0 {
            format!(" ({dropped} similar line(s) left out)")
        } else {
            String::new()
        };
        (self.say)(&format!("peer {} unreachable: {reason}{more}", peer.name));
    }

    fn changed(&self, peer: &crate::peers_config::PeerSpec, change: crate::peer_link::LinkChange) {
        use crate::peer_link::LinkChange as C;
        let Some(dropped) = self.throttle.admit(&peer.name, self.clock.now_ms()) else {
            return;
        };
        let ev = match change {
            C::Up { path } => AuditEvent::new(action::CONN_UP).detail("path", path),
            C::Down { reason } => AuditEvent::new(action::CONN_DOWN).detail("reason", reason),
            C::Path { from, to } => AuditEvent::new(action::CONN_PATH)
                .detail("from", from)
                .detail("to", to),
        };
        self.sink.record(
            ev.peer(peer.did.clone())
                .target(peer.node_id.to_string())
                .detail("peer_name", peer.name.as_str())
                .detail_opt("suppressed", (dropped > 0).then_some(dropped)),
        );
    }
}

/// Writes sync traffic and retention to the audit log (`data.*`, design §8): what was sent or
/// received (tree id, bytes), files the snapshot refused by name, and every deletion with the
/// bytes it freed.
pub struct AuditSync {
    sink: Arc<AuditSink>,
}

impl AuditSync {
    /// Records into `sink`.
    pub fn new(sink: Arc<AuditSink>) -> AuditSync {
        AuditSync { sink }
    }

    fn peer(&self, ev: AuditEvent, node: &tasks_peer::PeerId) -> AuditEvent {
        // a node id the trust layer does not know stays as it is
        let did = self
            .sink
            .did_of_node(node.as_str())
            .unwrap_or_else(|| node.to_string());
        ev.peer(did)
    }
}

fn job_of(ev: AuditEvent, job: Option<i64>) -> AuditEvent {
    match job {
        Some(j) => ev.job(j),
        None => ev,
    }
}

impl tasks_peer::sync::SyncObserver for AuditSync {
    fn event(&self, event: &tasks_peer::sync::SyncEvent) {
        use tasks_peer::sync::SyncEvent as E;
        let ev = match event {
            E::Sent {
                peer,
                checkout,
                job,
                tree,
                base,
                dirty,
                files,
                bytes,
            } => self
                .peer(job_of(AuditEvent::new(action::DATA_SYNC_SENT), *job), peer)
                .target(checkout.to_string())
                .detail("tree", tree.to_string())
                .detail_opt("base", base.as_ref().map(ToString::to_string))
                .detail("dirty_files", *dirty)
                .detail("files", *files)
                .detail("bytes", *bytes),
            E::Received {
                peer,
                checkout,
                tree,
                bytes,
                written,
                removed,
            } => self
                .peer(AuditEvent::new(action::DATA_SYNC_RECEIVED), peer)
                .target(checkout.to_string())
                .detail("tree", tree.to_string())
                .detail("bytes", *bytes)
                .detail("written", *written)
                .detail("removed", *removed),
            E::Refused {
                peer,
                checkout,
                job,
                files,
            } => self
                .peer(
                    job_of(AuditEvent::new(action::DATA_SYNC_REFUSED), *job),
                    peer,
                )
                .target(checkout.to_string())
                .detail(
                    "files",
                    files
                        .iter()
                        .map(|(p, why)| format!("{p}: {why}"))
                        .collect::<Vec<_>>(),
                ),
            E::Deleted {
                kind,
                peer,
                target,
                bytes,
            } => {
                let ev = AuditEvent::new(action::DATA_RETENTION)
                    .target(target.clone())
                    .detail("what", kind.name())
                    .detail("bytes_freed", *bytes);
                match peer {
                    Some(p) => self.peer(ev, p),
                    None => ev,
                }
            }
        };
        self.sink.record(ev);
    }
}

fn placement_text(p: &Placement) -> String {
    match p {
        Placement::Here => "here".to_string(),
        Placement::Pin => "pin".to_string(),
        Placement::Peer(m) => format!("peer:{}", m.0),
        Placement::Anywhere => "anywhere".to_string(),
    }
}

/// The program only: arguments can carry secrets, so they never reach the audit log.
fn program_of(spec: &JobSpec) -> String {
    spec.argv
        .first()
        .and_then(|a| Path::new(a).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn kind_text(spec: &JobSpec) -> String {
    format!("{:?}", spec.kind).to_lowercase()
}

fn phase_tag(p: &Phase) -> &'static str {
    crate::logs::phase_tag(p)
}

/// A [`Store`] that records the job lifecycle in the audit log. Reads pass straight through.
pub struct AuditedLedger {
    inner: Arc<dyn crate::platform::Ledger>,
    sink: Arc<AuditSink>,
}

impl AuditedLedger {
    /// Wraps `inner`.
    pub fn new(inner: Arc<dyn crate::platform::Ledger>, sink: Arc<AuditSink>) -> AuditedLedger {
        AuditedLedger { inner, sink }
    }

    fn submitted(&self, id: JobId, spec: &JobSpec, origin: Option<&tasks_ledger::Origin>) {
        let mut ev = if let Some(o) = origin {
            // the peer did this: it is the actor, and the owner stays as the peer gave it
            let did = self.sink.did_of_node(&o.peer);
            let mut ev =
                AuditEvent::new(action::JOB_RECEIVED).detail("origin_node", o.peer.as_str());
            if let Some(d) = did {
                ev = ev.by_actor(d.clone()).peer(d);
            }
            ev
        } else {
            AuditEvent::new(action::JOB_SUBMITTED)
        };
        ev = ev
            .job(id.0)
            .owner(spec.owner.as_str())
            .target(spec.cwd.display().to_string())
            .detail("program", program_of(spec))
            .detail("kind", kind_text(spec))
            .detail("placement", placement_text(&spec.placement));
        if spec.broad_test_escape {
            ev = ev.detail("broad_test_escape", true);
        }
        self.sink.record(ev);
        self.sink.record_priority(id, spec, "submit", None);
    }

    /// Records a start or an end after the ledger accepted the change, from the stored row.
    fn after_phase(&self, id: JobId) {
        let Ok(Some(rec)) = self.inner.get(id) else {
            return;
        };
        match &rec.phase {
            Phase::Running => {
                let ev = self
                    .sink
                    .job_event(action::JOB_STARTED, &rec)
                    .detail_opt("machine", rec.machine.as_ref().map(|m| m.0.clone()));
                self.sink.record(ev);
            }
            p if p.is_terminal() => {
                if matches!(p, Phase::TimedOut { .. }) {
                    let kind = match rec.timeout {
                        Some(TimeoutReason::Idle) => "idle",
                        _ => "wall",
                    };
                    let ev = self
                        .sink
                        .job_event(action::JOB_TIMEOUT, &rec)
                        .detail("limit", kind);
                    self.sink.record(ev);
                }
                let wall_ms = match (rec.started_at_ms, rec.finished_at_ms) {
                    (Some(s), Some(f)) => Some(f.saturating_sub(s)),
                    _ => None,
                };
                let ev = self
                    .sink
                    .job_event(action::JOB_ENDED, &rec)
                    .detail("phase", phase_tag(p))
                    .detail_opt("exit", rec.shown_exit())
                    .detail_opt("signal", rec.signal)
                    .detail_opt("wall_ms", wall_ms)
                    .detail_opt("summary", rec.summary.clone());
                self.sink.record(ev);
            }
            _ => {}
        }
    }
}

impl Store for AuditedLedger {
    fn submit_job(&self, job: &NewJob) -> Result<JobId> {
        let id = self.inner.submit_job(job)?;
        self.submitted(id, &job.spec, job.origin.as_ref());
        Ok(id)
    }

    fn submit_plan(&self, ctx: &PlanContext, plan: &Plan) -> Result<PlanSubmitted> {
        let done = self.inner.submit_plan(ctx, plan)?;
        for (step, id) in &done.jobs {
            let mut ev = AuditEvent::new(action::JOB_SUBMITTED)
                .job(id.0)
                .owner(ctx.owner.as_str())
                .target(ctx.cwd.display().to_string())
                .detail("plan", done.plan_id)
                .detail("step", step.as_str())
                .detail("placement", placement_text(&ctx.placement));
            if ctx.broad_test_escape {
                ev = ev.detail("broad_test_escape", true);
            }
            self.sink.record(ev);
            if let Ok(Some(rec)) = self.inner.get(*id) {
                self.sink.record_priority(*id, &rec.spec, "submit", None);
            }
        }
        Ok(done)
    }

    fn get(&self, id: JobId) -> Result<Option<JobRecord>> {
        self.inner.get(id)
    }

    fn set_phase(&self, id: JobId, next: Phase) -> Result<()> {
        self.inner.set_phase(id, next.clone())?;
        if next == Phase::Running || next.is_terminal() {
            self.after_phase(id);
        }
        Ok(())
    }

    fn list(&self, filter: &ListFilter) -> Result<Vec<JobRecord>> {
        self.inner.list(filter)
    }

    fn ready(&self) -> Result<Vec<JobId>> {
        self.inner.ready()
    }

    fn blocked_by_failure(&self) -> Result<Vec<JobId>> {
        self.inner.blocked_by_failure()
    }

    fn set_wait(&self, id: JobId, reason: WaitReason, text: &str) -> Result<()> {
        self.inner.set_wait(id, reason, text)
    }

    fn start(&self, id: JobId, info: &StartInfo) -> Result<()> {
        self.inner.start(id, info)?;
        self.after_phase(id);
        Ok(())
    }

    fn finish_recording(
        &self,
        id: JobId,
        outcome: &Finish,
        sample: Option<(&SampleKey, &Sample)>,
    ) -> Result<()> {
        self.inner.finish_recording(id, outcome, sample)?;
        self.after_phase(id);
        Ok(())
    }

    fn request_cancel_with(
        &self,
        id: JobId,
        by: &str,
        reason: Option<&str>,
    ) -> Result<CancelOutcome> {
        let before = self.inner.get(id).ok().flatten();
        let out = self.inner.request_cancel_with(id, by, reason)?;
        let first = before.as_ref().is_some_and(|r| !r.cancel_requested());
        let result = match &out {
            CancelOutcome::Cancelled => Some("cancelled"),
            CancelOutcome::Requested => Some("requested"),
            CancelOutcome::AlreadyFinished(_) => None,
        };
        if let (Some(rec), Some(result), true) = (before, result, first) {
            let mut ev = self
                .sink
                .job_event(action::JOB_CANCELLED, &rec)
                .detail("by", by)
                .detail("result", result)
                .detail_opt("reason", reason);
            // a peer cancelling its own job: the peer is the actor
            if let Some(node) = by.strip_prefix("peer ")
                && let Some(did) = self.sink.did_of_node(node)
            {
                ev = ev.by_actor(did);
            }
            self.sink.record(ev);
        }
        Ok(out)
    }

    fn set_forward(
        &self,
        id: JobId,
        peer: &MachineName,
        remote_job: Option<i64>,
        remote_key: Option<&str>,
    ) -> Result<()> {
        let before = self.inner.get(id).ok().flatten();
        self.inner.set_forward(id, peer, remote_job, remote_key)?;
        // the first record of where it went; later calls only learn the remote id
        let new = before
            .as_ref()
            .is_some_and(|r| r.forward.as_ref().is_none_or(|f| f.peer != *peer));
        if let (Some(rec), true) = (before, new) {
            let mut ev = AuditEvent::new(action::JOB_FORWARDED)
                .job(id.0)
                .owner(rec.spec.owner.as_str())
                .target(rec.spec.cwd.display().to_string())
                .detail("peer_name", peer.0.as_str());
            if let Some(did) = self.sink.did_of_name(&peer.0) {
                ev = ev.peer(did);
            }
            self.sink.record(ev);
        }
        Ok(())
    }

    fn clear_forward(&self, id: JobId) -> Result<()> {
        self.inner.clear_forward(id)
    }

    fn set_priority(
        &self,
        id: JobId,
        priority: Priority,
        reason: Option<&str>,
    ) -> Result<Priority> {
        let before = self.inner.set_priority(id, priority, reason)?;
        if let Ok(Some(rec)) = self.inner.get(id) {
            self.sink
                .record_priority(id, &rec.spec, "prioritize", Some(before));
        }
        Ok(before)
    }

    fn find_origin(&self, peer: &str, key: &str) -> Result<Option<JobId>> {
        self.inner.find_origin(peer, key)
    }

    fn set_remote_phase(&self, id: JobId, phase: &Phase) -> Result<()> {
        self.inner.set_remote_phase(id, phase)
    }

    fn set_log_path(&self, id: JobId, path: &Path) -> Result<()> {
        self.inner.set_log_path(id, path)
    }

    fn set_figures(&self, id: JobId, figures: &tasks_core::JobFigures) -> Result<()> {
        self.inner.set_figures(id, figures)
    }

    fn set_summary(&self, id: JobId, summary: &str) -> Result<()> {
        self.inner.set_summary(id, summary)
    }

    fn find_running(&self) -> Result<Vec<JobRecord>> {
        self.inner.find_running()
    }

    fn find_recent_spec(
        &self,
        owner: &str,
        cwd: &Path,
        argv: &[String],
        within: std::time::Duration,
    ) -> Result<Vec<JobRecord>> {
        self.inner.find_recent_spec(owner, cwd, argv, within)
    }

    fn record_sample(&self, key: &SampleKey, sample: &Sample) -> Result<()> {
        self.inner.record_sample(key, sample)
    }

    fn sample_keys(&self, recipes: &[String], limit: usize) -> Result<Vec<SampleKey>> {
        self.inner.sample_keys(recipes, limit)
    }

    fn prune(&self, policy: &RetentionPolicy) -> Result<PruneReport> {
        self.inner.prune(policy)
    }

    fn change_token(&self) -> Result<ChangeToken> {
        self.inner.change_token()
    }
}

impl History for AuditedLedger {
    fn samples(&self, key: &SampleKey) -> std::result::Result<Vec<Sample>, TimingError> {
        self.inner.samples(key)
    }

    fn targets(&self, recipe: &str) -> std::result::Result<Vec<String>, TimingError> {
        self.inner.targets(recipe)
    }

    fn recent_peaks(&self, key: &SampleKey) -> std::result::Result<Vec<Option<u64>>, TimingError> {
        self.inner.recent_peaks(key)
    }
}
