//! The origin side of a forwarded job: a poll-driven state machine.
//!
//! `submit -> synced -> remote queued -> running -> finished | failed |
//! cancelled | lost`. Time comes in as a `Duration` since an arbitrary start
//! (a manual clock in tests); durations come from [`ForwardPolicy`], the
//! policy knobs. Nothing here sleeps.
//!
//! Safety rules, each with a test:
//! - A lost link is uncertain, never a reason to resubmit (R-150). The same
//!   idempotency key is the only thing ever resent, and only until the
//!   peer has answered once.
//! - The log resumes from a byte offset: no duplicated and no lost bytes.
//! - Cancel is sent to the peer and the job is only `Cancelled` once the peer
//!   says so; an unconfirmed cancel on a lost peer is `Interrupted`.
//! - The per-checkout lock is held only from the start of the sync to the end
//!   of the job, and is released when the forward is dropped.

use crate::ids::{CheckoutId, IdError, PeerId, RemoteJobId, SubmitKey};
use crate::locks::{Acquire, CheckoutLocks, LockOwner};
use crate::proto::{Body, JobEvent, Refusal, SubmitJob, Submitted, WireOutcome};
use crate::sync::{SyncDriver, SyncError, SyncJob, SyncReport};
use crate::transport::{Connection, EventStream, PeerError, StreamPoll, Transport};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tasks_core::{JobSpec, Phase, WaitReason};
use tasks_timing::history::{Sample, SampleKey};

/// Policy knobs for a forward. The defaults are the REQUIREMENTS numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardPolicy {
    /// An offline peer must fail within this (R-7.6: 15 s).
    pub connect_timeout: Duration,
    /// Sync attempts before giving up; 2 means "retry once" (R-157).
    pub sync_attempts: u32,
    /// No event for this long on an open stream counts as a lost link.
    pub stream_idle: Duration,
    /// Pause between reconnect attempts.
    pub reattach_interval: Duration,
    /// How long a lost peer is waited for before the job is `Interrupted`.
    pub lost_grace: Duration,
    /// How long an unconfirmed cancel is waited for.
    pub cancel_grace: Duration,
    /// Cancel the remote job when it is still queued after this long.
    pub remote_queue_timeout: Option<Duration>,
    /// Most events handled per tick.
    pub max_events_per_tick: usize,
}

impl Default for ForwardPolicy {
    fn default() -> Self {
        ForwardPolicy {
            connect_timeout: Duration::from_secs(15),
            sync_attempts: 2,
            stream_idle: Duration::from_secs(60),
            reattach_interval: Duration::from_secs(2),
            lost_grace: Duration::from_secs(120),
            cancel_grace: Duration::from_secs(30),
            remote_queue_timeout: None,
            max_events_per_tick: 256,
        }
    }
}

/// Where the forwarded job's output goes (the local job log).
pub trait LogSink {
    /// Bytes already in the log. The stream resumes from here.
    fn len(&self) -> u64;
    /// True when the log is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Appends bytes.
    fn append(&mut self, data: &[u8]) -> std::io::Result<()>;
}

impl LogSink for Vec<u8> {
    fn len(&self) -> u64 {
        Vec::len(self) as u64
    }
    fn append(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.extend_from_slice(data);
        Ok(())
    }
}

/// Everything a forward needs to know about the job and where it comes from.
#[derive(Debug, Clone)]
pub struct ForwardJob {
    /// The local job id; also the lock owner.
    pub local_id: i64,
    /// The job. Its `cwd` must be inside `root`.
    pub spec: JobSpec,
    /// The checkout root on this machine.
    pub root: PathBuf,
    /// This machine's name, part of the checkout id.
    pub origin_host: String,
    /// The peer to run on.
    pub peer: PeerId,
    /// Idempotency key of the submission.
    pub key: SubmitKey,
}

impl ForwardJob {
    /// A key from the origin host and local id, with anything unsafe replaced.
    pub fn default_key(origin_host: &str, local_id: i64) -> Result<SubmitKey, IdError> {
        let host: String = origin_host
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .take(64)
            .collect();
        let host = if host.is_empty() {
            "origin".to_string()
        } else {
            host
        };
        SubmitKey::new(&format!("{host}:{local_id}"))
    }
}

/// The I/O a forward uses on each tick.
pub struct ForwardIo<'a> {
    /// Opens links.
    pub transport: &'a dyn Transport,
    /// Snapshots the checkout and syncs it to the peer.
    pub sync: &'a dyn SyncDriver,
    /// The local log.
    pub log: &'a mut dyn LogSink,
}

/// Where a forward is.
#[derive(Debug, Clone, PartialEq)]
pub enum ForwardState {
    /// Waiting for the checkout's lock; `blocker` is who it waits for.
    WaitingCheckout {
        /// The holder, or the waiter ahead.
        blocker: Option<LockOwner>,
    },
    /// Syncing the checkout (attempt number from 1).
    Syncing {
        /// Attempt number.
        attempt: u32,
    },
    /// Synced; submitting to the peer.
    Submitting,
    /// Accepted by the peer and not running yet.
    RemoteQueued {
        /// The peer's id.
        remote: RemoteJobId,
        /// The peer's phase.
        phase: Phase,
        /// Blocker detail from the peer.
        detail: String,
    },
    /// Running on the peer.
    Running {
        /// The peer's id.
        remote: RemoteJobId,
    },
    /// The link is down; reattaching by remote id (never resubmitting).
    Reattaching {
        /// The peer's id, when it had answered.
        remote: Option<RemoteJobId>,
        /// Since when.
        since: Duration,
        /// Why.
        reason: String,
    },
    /// Over.
    Done(Box<ForwardResult>),
}

/// The end of a forward.
#[derive(Debug, Clone, PartialEq)]
pub struct ForwardResult {
    /// The terminal phase for the local job row.
    pub phase: Phase,
    /// Named reason when the phase is not the command's own result.
    pub reason: String,
    /// The peer's outcome, when it reported one.
    pub outcome: Option<WireOutcome>,
    /// Timing samples measured on the peer, ready for the origin's history.
    pub samples: Vec<(SampleKey, Sample)>,
    /// True when nothing reached the peer: the caller may fall back to the
    /// local queue, keeping its position (R-7.5).
    pub nothing_submitted: bool,
    /// The peer's id, when the job was accepted.
    pub remote: Option<RemoteJobId>,
    /// The sync that preceded the run.
    pub sync: Option<SyncReport>,
}

/// One forwarded job.
pub struct Forward {
    job: ForwardJob,
    policy: ForwardPolicy,
    locks: Arc<CheckoutLocks>,
    checkout: CheckoutId,
    owner: LockOwner,
    state: ForwardState,
    conn: Option<Box<dyn Connection>>,
    stream: Option<Box<dyn EventStream>>,
    remote: Option<RemoteJobId>,
    last_event: Duration,
    queued_since: Option<Duration>,
    next_try: Duration,
    ambiguous_since: Option<Duration>,
    cancel_requested: Option<Duration>,
    cancel_sent: bool,
    cancel_reason: Option<String>,
    remote_phase: Option<(Phase, String)>,
    sync_report: Option<SyncReport>,
    last_error: String,
    lost_since: Option<Duration>,
    busy_since: Option<Duration>,
    notes: Vec<String>,
}

impl Forward {
    /// Starts a forward. Nothing happens until the first `tick`.
    pub fn new(
        job: ForwardJob,
        policy: ForwardPolicy,
        locks: Arc<CheckoutLocks>,
        now: Duration,
    ) -> Forward {
        let checkout = CheckoutId::derive(&job.origin_host, &job.root);
        let owner = LockOwner(job.local_id);
        locks.enqueue(&checkout, owner);
        Forward {
            job,
            policy,
            locks,
            checkout,
            owner,
            state: ForwardState::WaitingCheckout { blocker: None },
            conn: None,
            stream: None,
            remote: None,
            last_event: now,
            queued_since: None,
            next_try: now,
            ambiguous_since: None,
            cancel_requested: None,
            cancel_sent: false,
            cancel_reason: None,
            remote_phase: None,
            sync_report: None,
            last_error: String::new(),
            lost_since: None,
            busy_since: None,
            notes: Vec::new(),
        }
    }

    /// The checkout this forward belongs to.
    pub fn checkout(&self) -> &CheckoutId {
        &self.checkout
    }

    /// The current state.
    pub fn state(&self) -> &ForwardState {
        &self.state
    }

    /// The result, once over.
    pub fn result(&self) -> Option<&ForwardResult> {
        match &self.state {
            ForwardState::Done(r) => Some(r.as_ref()),
            _ => None,
        }
    }

    /// True once over.
    pub fn is_done(&self) -> bool {
        matches!(self.state, ForwardState::Done(_))
    }

    /// True when a slot may be taken for this forward now: it holds, or would
    /// be granted, the checkout lock. A caller that takes a slot for a forward
    /// that is still waiting for its checkout recreates R-230.
    pub fn ready_to_start(&self) -> bool {
        !self.is_done() && self.locks.would_grant(&self.checkout, self.owner)
    }

    /// Sets the queue class the peer will see. Honoured only until the job is submitted: after
    /// that the peer's own queue holds the class. Returns whether it was applied.
    pub fn set_class(&mut self, priority: tasks_core::Priority, reason: Option<String>) -> bool {
        if self.remote.is_some() {
            return false;
        }
        self.job.spec.priority = priority;
        self.job.spec.priority_reason = reason;
        true
    }

    /// The remote id, once the peer has accepted the job.
    pub fn remote(&self) -> Option<RemoteJobId> {
        self.remote
    }

    /// Audit notes since the last call (retries, reattaches, fallbacks).
    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    /// One line mirroring the peer's state for the local row and log (R-3),
    /// e.g. `remote #156 waiting: compile slot held by #154`.
    pub fn mirror(&self) -> String {
        match &self.state {
            ForwardState::WaitingCheckout { blocker } => match blocker {
                Some(b) => format!(
                    "waiting for checkout: job #{} is using the remote copy",
                    b.0
                ),
                None => "waiting for checkout".to_string(),
            },
            ForwardState::Syncing { attempt } => format!("syncing checkout (attempt {attempt})"),
            ForwardState::Submitting => "submitting to peer".to_string(),
            ForwardState::RemoteQueued {
                remote,
                phase,
                detail,
            } => {
                let label = match phase {
                    Phase::Waiting(WaitReason::Slot(_)) => "waiting_slot",
                    Phase::Waiting(WaitReason::Pressure(_)) => "waiting_pressure",
                    Phase::Waiting(_) => "waiting",
                    _ => "queued",
                };
                if detail.is_empty() {
                    format!("{remote} {label}")
                } else {
                    format!("{remote} {label}: {detail}")
                }
            }
            ForwardState::Running { remote } => format!("{remote} running"),
            ForwardState::Reattaching { remote, reason, .. } => match remote {
                Some(r) => format!("{r} link lost, reattaching: {reason}"),
                None => format!("link lost before the peer answered: {reason}"),
            },
            ForwardState::Done(r) => format!("{}: {}", r.phase.word(), r.reason),
        }
    }

    /// Asks for cancellation. Propagated to the peer on the next tick (R-151).
    pub fn cancel(&mut self, now: Duration, reason: &str) {
        if self.cancel_requested.is_none() && !self.is_done() {
            self.cancel_requested = Some(now);
            self.cancel_reason = Some(reason.to_string());
        }
    }

    /// The peer cancelled it (or a cancel was applied there) is detected from
    /// the event stream; this is the entry point for everything else.
    pub fn tick(&mut self, now: Duration, io: &mut ForwardIo<'_>) {
        for _ in 0..8 {
            let before = std::mem::discriminant(&self.state);
            match self.state.clone() {
                ForwardState::Done(_) => return,
                ForwardState::WaitingCheckout { .. } => self.step_wait(now),
                ForwardState::Syncing { attempt } => self.step_sync(now, attempt, io),
                ForwardState::Submitting => self.step_submit(now, io),
                ForwardState::Reattaching { .. }
                | ForwardState::RemoteQueued { .. }
                | ForwardState::Running { .. } => self.step_attached(now, io),
            }
            // Keep stepping through instant transitions (wait -> sync -> submit)
            // but stop as soon as a step leaves the state unchanged.
            if std::mem::discriminant(&self.state) == before {
                return;
            }
            if matches!(
                self.state,
                ForwardState::RemoteQueued { .. } | ForwardState::Running { .. }
            ) {
                return;
            }
        }
    }

    fn finish(&mut self, r: ForwardResult) {
        self.locks.release(&self.checkout, self.owner);
        self.conn = None;
        self.stream = None;
        self.state = ForwardState::Done(Box::new(r));
    }

    fn finish_unsubmitted(&mut self, phase: Phase, reason: String) {
        let sync = self.sync_report.take();
        self.finish(ForwardResult {
            phase,
            reason,
            outcome: None,
            samples: Vec::new(),
            nothing_submitted: true,
            remote: None,
            sync,
        });
    }

    fn step_wait(&mut self, now: Duration) {
        if self.cancel_requested.is_some() {
            let why = self.cancel_reason.clone().unwrap_or_default();
            self.finish_unsubmitted(
                Phase::Cancelled,
                format!("cancelled before sync: {why}; nothing was submitted"),
            );
            return;
        }
        match self.locks.try_acquire(&self.checkout, self.owner) {
            Acquire::Granted => {
                self.notes.push("checkout lock acquired".to_string());
                self.state = ForwardState::Syncing { attempt: 1 };
            }
            Acquire::Busy { blocker } => {
                self.state = ForwardState::WaitingCheckout {
                    blocker: Some(blocker),
                };
            }
        }
        let _ = now;
    }

    fn connect(&mut self, io: &ForwardIo<'_>) -> Result<(), PeerError> {
        if self.conn.is_none() {
            self.conn = Some(
                io.transport
                    .connect(&self.job.peer, self.policy.connect_timeout)?,
            );
        }
        Ok(())
    }

    fn step_sync(&mut self, now: Duration, attempt: u32, io: &mut ForwardIo<'_>) {
        if self.cancel_requested.is_some() {
            let why = self.cancel_reason.clone().unwrap_or_default();
            self.finish_unsubmitted(
                Phase::Cancelled,
                format!("cancelled during sync: {why}; nothing was submitted"),
            );
            return;
        }
        if now < self.next_try {
            return;
        }
        let result = (|| -> Result<SyncReport, (String, bool, bool)> {
            self.connect(io)
                .map_err(|e| (e.to_string(), false, false))?;
            let conn = self
                .conn
                .as_mut()
                .ok_or(("no connection".to_string(), false, false))?;
            io.sync
                .push(
                    conn.as_mut(),
                    &SyncJob {
                        root: &self.job.root,
                        checkout: &self.checkout,
                        origin_host: &self.job.origin_host,
                        peer: &self.job.peer,
                        local_job: self.job.local_id,
                        session: u64::try_from(self.job.local_id)
                            .unwrap_or(0)
                            .wrapping_mul(1000)
                            .wrapping_add(u64::from(attempt)),
                    },
                )
                .map_err(|e: SyncError| (e.to_string(), e.is_busy(), e.is_fatal()))
        })();
        match result {
            // The peer still runs an earlier job from this checkout: wait for it without
            // spending a sync attempt, but not for ever, and nothing has been submitted.
            Err((e, true, _)) => {
                self.conn = None;
                let since = *self.busy_since.get_or_insert(now);
                if now.saturating_sub(since) >= self.policy.lost_grace {
                    // Not `nothing_submitted`: the live job on the peer may be this very job,
                    // submitted before a daemon restart whose reply was never recorded.
                    // Running it here as well could run it twice, so it is interrupted.
                    let sync = self.sync_report.take();
                    self.finish(ForwardResult {
                        phase: Phase::Interrupted,
                        reason: format!(
                            "{} stayed busy with a live job of this checkout for {:?} ({e}); the job was not submitted again, and it may be that job's earlier submission, so it is not run elsewhere",
                            self.job.peer, self.policy.lost_grace
                        ),
                        outcome: None,
                        samples: Vec::new(),
                        nothing_submitted: false,
                        remote: None,
                        sync,
                    });
                } else {
                    if since == now {
                        self.notes.push(format!("{e}; waiting"));
                    }
                    self.next_try = now + self.policy.reattach_interval;
                }
            }
            Ok(rep) => {
                self.busy_since = None;
                self.notes.push(format!("synced: {}", rep.line()));
                self.sync_report = Some(rep);
                self.state = ForwardState::Submitting;
            }
            // refused by name (LFS pointer, secret, size): trying again cannot change that
            Err((e, _, true)) => self.finish_unsubmitted(
                Phase::Failed { exit: None },
                format!(
                    "sync to {} refused: {e}; nothing was submitted",
                    self.job.peer
                ),
            ),
            Err((e, _, false)) if attempt < self.policy.sync_attempts => {
                self.notes
                    .push(format!("sync attempt {attempt} failed ({e}); retrying"));
                self.conn = None;
                self.next_try = now + self.policy.reattach_interval.min(Duration::from_secs(1));
                self.state = ForwardState::Syncing {
                    attempt: attempt + 1,
                };
            }
            Err((e, _, _)) => self.finish_unsubmitted(
                Phase::Failed { exit: None },
                format!(
                    "sync to {} failed after {attempt} attempts: {e}; nothing was submitted",
                    self.job.peer
                ),
            ),
        }
    }

    fn step_submit(&mut self, now: Duration, io: &mut ForwardIo<'_>) {
        if self.cancel_requested.is_some()
            && self.remote.is_none()
            && self.ambiguous_since.is_none()
        {
            let why = self.cancel_reason.clone().unwrap_or_default();
            self.finish_unsubmitted(
                Phase::Cancelled,
                format!("cancelled before submit: {why}; nothing was submitted"),
            );
            return;
        }
        if now < self.next_try {
            return;
        }
        let rel_cwd = match self.job.spec.cwd.strip_prefix(&self.job.root) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => {
                self.finish_unsubmitted(
                    Phase::Failed { exit: None },
                    format!(
                        "cwd {} is outside the checkout {}; nothing was submitted",
                        self.job.spec.cwd.display(),
                        self.job.root.display()
                    ),
                );
                return;
            }
        };
        let req = Body::Submit(SubmitJob {
            key: self.job.key.clone(),
            checkout: self.checkout.clone(),
            rel_cwd,
            spec: self.job.spec.clone(),
        });
        let reply = match self.connect(io) {
            Ok(()) => match self.conn.as_mut() {
                Some(c) => c.request(req),
                None => Err(PeerError::LinkLost("no connection".to_string())),
            },
            Err(e) => Err(e),
        };
        match reply {
            Ok(Body::Submitted(Submitted { remote, .. })) => {
                self.remote = Some(remote);
                self.last_event = now;
                self.queued_since = Some(now);
                self.state = ForwardState::RemoteQueued {
                    remote,
                    phase: Phase::Queued,
                    detail: String::new(),
                };
            }
            Ok(_) => self.finish_unsubmitted(
                Phase::Failed { exit: None },
                "peer answered a submission with the wrong message; nothing was submitted"
                    .to_string(),
            ),
            Err(PeerError::Refused(r)) => self.finish_unsubmitted(
                Phase::Failed { exit: None },
                format!("peer refused the job: {r}; nothing was submitted"),
            ),
            Err(e) if e.not_delivered() => self.finish_unsubmitted(
                Phase::Failed { exit: None },
                format!("{e}; nothing was submitted"),
            ),
            Err(e) => {
                // The request may have been delivered. Resending the same key
                // is safe (the peer deduplicates); anything else is not.
                let since = *self.ambiguous_since.get_or_insert(now);
                self.conn = None;
                self.last_error = e.to_string();
                if now.saturating_sub(since) >= self.policy.lost_grace {
                    let sync = self.sync_report.take();
                    self.finish(ForwardResult {
                        phase: Phase::Interrupted,
                        reason: format!(
                            "peer {} lost while submitting ({e}); it may have started the job; not resubmitted blindly",
                            self.job.peer
                        ),
                        outcome: None,
                        samples: Vec::new(),
                        nothing_submitted: false,
                        remote: None,
                        sync,
                    });
                } else {
                    self.notes
                        .push(format!("submit reply lost ({e}); retrying the same key"));
                    self.next_try = now + self.policy.reattach_interval;
                }
            }
        }
    }

    fn drop_link(&mut self, now: Duration, reason: String) {
        self.conn = None;
        self.stream = None;
        let since = *self.lost_since.get_or_insert(now);
        if !matches!(self.state, ForwardState::Reattaching { .. }) {
            self.notes.push(format!("link lost: {reason}"));
            self.state = ForwardState::Reattaching {
                remote: self.remote,
                since,
                reason,
            };
        }
        self.next_try = now + self.policy.reattach_interval;
    }

    fn finish_lost(&mut self, reason: String) {
        let remote = self.remote;
        let sync = self.sync_report.take();
        self.finish(ForwardResult {
            phase: Phase::Interrupted,
            reason,
            outcome: None,
            samples: Vec::new(),
            nothing_submitted: false,
            remote,
            sync,
        });
    }

    fn step_attached(&mut self, now: Duration, io: &mut ForwardIo<'_>) {
        let Some(remote) = self.remote else {
            return;
        };
        // Queue timeout: ask the peer to drop a job that has waited too long.
        if let (Some(limit), Some(since), None) = (
            self.policy.remote_queue_timeout,
            self.queued_since,
            self.cancel_requested,
        ) && matches!(self.state, ForwardState::RemoteQueued { .. })
            && now.saturating_sub(since) >= limit
        {
            self.cancel(
                now,
                &format!("still queued on {} after {limit:?}", self.job.peer),
            );
        }
        if let Some(at) = self.cancel_requested
            && now.saturating_sub(at) >= self.policy.cancel_grace
            && !matches!(self.state, ForwardState::Done(_))
        {
            self.finish_lost(format!(
                "cancel of {remote} on {} was not confirmed within {:?}; the job may still be running there",
                self.job.peer, self.policy.cancel_grace
            ));
            return;
        }
        // Lost for too long? The clock starts at the first drop and stops only
        // when the peer actually delivers an event, so a peer that accepts
        // connections but says nothing cannot keep a job alive forever.
        if let Some(since) = self.lost_since
            && now.saturating_sub(since) >= self.policy.lost_grace
        {
            let why = match &self.state {
                ForwardState::Reattaching { reason, .. } => reason.clone(),
                _ => "no events".to_string(),
            };
            let reason = format!(
                "peer {} unreachable for {:?} ({why}); result unknown, {remote} may still be running there; not resubmitted",
                self.job.peer, self.policy.lost_grace
            );
            self.finish_lost(reason);
            return;
        }
        if now < self.next_try && self.stream.is_none() {
            return;
        }
        // (Re)connect and (re)attach from the log offset we already hold.
        if self.stream.is_none() {
            let from = io.log.len();
            let attached = (|| -> Result<Box<dyn EventStream>, PeerError> {
                self.connect(io)?;
                let conn = self
                    .conn
                    .as_mut()
                    .ok_or_else(|| PeerError::LinkLost("no connection".to_string()))?;
                conn.subscribe(remote, from)
            })();
            match attached {
                Ok(s) => {
                    self.stream = Some(s);
                    self.last_event = now;
                    if matches!(self.state, ForwardState::Reattaching { .. }) {
                        self.notes
                            .push(format!("reattached to {remote} at log offset {from}"));
                    }
                }
                Err(PeerError::Refused(Refusal::UnknownJob { .. })) => {
                    self.finish_lost(format!(
                        "peer {} no longer knows {remote}; result unknown; not resubmitted",
                        self.job.peer
                    ));
                    return;
                }
                Err(e) => {
                    self.drop_link(now, e.to_string());
                    return;
                }
            }
        }
        // Propagate a cancel (retried on every tick until acknowledged).
        if self.cancel_requested.is_some() && !self.cancel_sent {
            match self.send_cancel(remote) {
                Ok(()) => self.cancel_sent = true,
                Err(PeerError::Refused(Refusal::UnknownJob { .. })) => {
                    self.finish_lost(format!("peer {} no longer knows {remote}", self.job.peer));
                    return;
                }
                Err(e) => {
                    self.drop_link(now, e.to_string());
                    return;
                }
            }
        }
        // Drain events.
        for _ in 0..self.policy.max_events_per_tick {
            let polled = match self.stream.as_mut() {
                Some(s) => s.poll(),
                None => return,
            };
            match polled {
                Ok(StreamPoll::Event(ev)) => {
                    self.last_event = now;
                    self.lost_since = None;
                    if matches!(self.state, ForwardState::Reattaching { .. }) {
                        self.state = ForwardState::RemoteQueued {
                            remote,
                            phase: Phase::Queued,
                            detail: String::new(),
                        };
                    }
                    if self.on_event(now, remote, ev, io) {
                        return;
                    }
                }
                Ok(StreamPoll::Idle) => {
                    if now.saturating_sub(self.last_event) >= self.policy.stream_idle {
                        self.drop_link(now, format!("no events for {:?}", self.policy.stream_idle));
                    }
                    return;
                }
                Ok(StreamPoll::Closed) => {
                    // A stream that closes without `Finished` is a broken one.
                    self.drop_link(now, "stream closed before the job finished".to_string());
                    return;
                }
                Err(PeerError::Refused(Refusal::UnknownJob { .. })) => {
                    self.finish_lost(format!("peer {} no longer knows {remote}", self.job.peer));
                    return;
                }
                Err(e) => {
                    self.drop_link(now, e.to_string());
                    return;
                }
            }
        }
    }

    fn send_cancel(&mut self, remote: RemoteJobId) -> Result<(), PeerError> {
        let conn = self
            .conn
            .as_mut()
            .ok_or_else(|| PeerError::LinkLost("no connection".to_string()))?;
        match conn.request(Body::Cancel { remote })? {
            Body::CancelAck { .. } => {
                self.notes.push(format!("cancel sent to {remote}"));
                Ok(())
            }
            _ => Err(PeerError::Protocol("wrong reply to cancel".to_string())),
        }
    }

    /// Returns true when the forward is over or the stream was dropped.
    fn on_event(
        &mut self,
        now: Duration,
        remote: RemoteJobId,
        ev: JobEvent,
        io: &mut ForwardIo<'_>,
    ) -> bool {
        match ev {
            JobEvent::Heartbeat => false,
            JobEvent::Phase { phase, detail } => {
                self.remote_phase = Some((phase.clone(), detail.clone()));
                match phase {
                    Phase::Running => {
                        self.queued_since = None;
                        self.state = ForwardState::Running { remote };
                    }
                    Phase::Queued | Phase::Waiting(_) => {
                        self.state = ForwardState::RemoteQueued {
                            remote,
                            phase,
                            detail,
                        };
                    }
                    // Terminal phases are reported by `Finished`, after the log.
                    _ => {}
                }
                false
            }
            JobEvent::Log { offset, data } => {
                let have = io.log.len();
                let end = offset + data.len() as u64;
                if offset > have {
                    // A gap would lose bytes: reattach from what we hold.
                    self.notes
                        .push(format!("log gap at {offset} (have {have}); resuming"));
                    self.stream = None;
                    return true;
                }
                if end <= have {
                    return false; // duplicate, already stored
                }
                let skip = usize::try_from(have - offset).unwrap_or(0);
                let fresh = data.get(skip..).unwrap_or(&[]);
                if let Err(e) = io.log.append(fresh) {
                    self.finish_lost(format!("cannot write the local log: {e}"));
                    return true;
                }
                false
            }
            JobEvent::Finished {
                outcome,
                samples,
                log_len,
            } => {
                if io.log.len() < log_len {
                    self.notes.push(format!(
                        "finished at log length {log_len} but have {}; resuming",
                        io.log.len()
                    ));
                    self.stream = None;
                    return true;
                }
                let phase = outcome.phase();
                let reason = match (&self.cancel_reason, &phase) {
                    (Some(r), Phase::Cancelled) => format!("cancelled: {r}"),
                    _ => outcome.description.clone(),
                };
                let samples = samples.iter().filter_map(|s| s.to_sample()).collect();
                let sync = self.sync_report.take();
                let _ = now;
                self.finish(ForwardResult {
                    phase,
                    reason,
                    outcome: Some(outcome),
                    samples,
                    nothing_submitted: false,
                    remote: Some(remote),
                    sync,
                });
                true
            }
        }
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        // A forward that goes away must not wedge its checkout (R-230).
        self.locks.release(&self.checkout, self.owner);
    }
}
