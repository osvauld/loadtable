//! Forwarded jobs in the scheduler: offload-client jobs.
//!
//! A job placed on a peer stays in this ledger as a normal row. It waits for a local offload
//! slot, but only while the forwarder says its checkout lock is held or would be granted
//! (R-230); a thread of the scheduler's own then polls the forwarder until the job ends and
//! mirrors the peer's state into the row. Rules, each with a test:
//!
//! - the row moves to `Running` only once the peer has accepted the job; until then nothing was
//!   submitted and the job may fall back to local placement, with a note in its summary;
//! - once the peer may have the job it is never run here and never resubmitted: a lost peer
//!   ends it `Interrupted` (R-150, R-173);
//! - after a daemon restart a forwarded job that was running is followed read-only by its
//!   remote id; a job with a stored key but no remote id is re-registered with that same key,
//!   so the peer deduplicates (R-152).

use crate::forwarder::{ForwardEnd, ForwardPoll};
use crate::scheduler::{RunningJob, Scheduler, lock};
use crate::stuck::StuckDetector;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tasks_admission::{Hold, Waiter};
use tasks_core::{JobId, MachineName, Phase, Placement, SlotKind, WaitReason as LedgerWait};
use tasks_ledger::{Finish, JobRecord, LedgerError, RetryPolicy, retry_busy};
use tasks_platform::SlotGuard;
use tasks_runner::{CancelState, CancelToken};
use tasks_timing::{Estimates, Timeline};

fn sleep(d: std::time::Duration) {
    std::thread::sleep(d);
}

impl Scheduler {
    /// The admission waiter of a forwarded job, or `None` when it is handled another way (a
    /// resume) or cannot be registered now. A job not ready to start carries a hold, so it
    /// takes no slot and blocks nobody (R-230).
    pub(crate) fn forward_waiter(
        self: &Arc<Self>,
        rec: &JobRecord,
        blocked: Option<&Hold>,
    ) -> Option<Waiter> {
        let fw = rec.forward.as_ref()?;
        let id = rec.id;
        if fw.remote_job.is_some() && !self.forwarder.registered(id) {
            // the peer already has it (a crash between the submit and the row going Running):
            // follow it by its id, never submit again
            self.resume_forwarded(rec.clone());
            return None;
        }
        if !self.forwarder.registered(id) {
            // after a restart: register again, with the key the ledger holds
            if let Err(e) = self.forwarder.forward(rec, &fw.peer) {
                let text = format!("cannot forward to {}: {e}", fw.peer.0);
                self.set_wait(id, LedgerWait::Placement(text.clone()), &text);
                return None;
            }
        }
        let mut w = Waiter::new(
            id,
            SlotKind::Offload,
            u64::try_from(rec.submitted_at_ms).unwrap_or(0),
        );
        w.owner = rec.spec.owner.as_str().to_string();
        w.priority = rec.spec.priority;
        if let Some(h) = blocked {
            w.holds.push(h.clone());
        } else if !self.forwarder.ready_to_start(id) {
            w.holds.push(Hold::Other(
                self.forwarder
                    .wait_reason(id)
                    .unwrap_or_else(|| "waiting for the checkout".to_string()),
            ));
        }
        Some(w)
    }

    fn forwarded_running(&self, rec: &JobRecord, slot: Option<(SlotKind, u32)>) -> Arc<RunningJob> {
        let policy = self.policy();
        Arc::new(RunningJob {
            id: rec.id,
            owner: rec.spec.owner.as_str().to_string(),
            command: crate::scheduler::shorten(&rec.spec.argv.join(" ")),
            slot_kind: slot.map(|s| s.0),
            slot,
            checkout: None,
            token: CancelToken::new(),
            started: Instant::now(),
            log_path: self.cfg.log_path(rec.id),
            group: Mutex::new(None),
            timeline: Mutex::new(Timeline::new()),
            estimates: Estimates {
                compile: None,
                targets: BTreeMap::new(),
            },
            kill_reason: Mutex::new(None),
            stuck: Mutex::new(StuckDetector::new(policy.stuck_window_secs())),
            est_median: None,
            figures: Mutex::new(None),
        })
    }

    /// Starts driving a forwarded job that was admitted to an offload slot.
    pub(crate) fn launch_forwarded(
        self: &Arc<Self>,
        rec: JobRecord,
        slot: Option<(SlotKind, u32)>,
        guard: Option<SlotGuard>,
    ) -> bool {
        let id = rec.id;
        match self.ledger.cancel_requested(id) {
            Ok(false) => {}
            Ok(true) | Err(LedgerError::NotFound(_)) => return false,
            Err(e) => {
                self.note_error(&format!("cancel_requested: {e}"));
                return false;
            }
        }
        let Some(fw) = rec.forward.clone() else {
            return false;
        };
        let rj = self.forwarded_running(&rec, slot);
        if let Some(dir) = rj.log_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        self.forwarder
            .set_class(id, rec.spec.priority, rec.spec.priority_reason.clone());
        if let Err(e) = self.forwarder.start(id, &rj.log_path) {
            let text = format!("cannot start forwarding to {}: {e}", fw.peer.0);
            self.set_wait(id, LedgerWait::Placement(text.clone()), &text);
            return false;
        }
        lock(&self.state).last_wait.remove(&id);
        self.spawn_driver(rj, fw.peer, guard, false)
    }

    /// Follows a forwarded job by its remote id after a restart. Read-only: nothing is
    /// submitted. The job holds no local slot while followed.
    pub(crate) fn resume_forwarded(self: &Arc<Self>, rec: JobRecord) {
        let id = rec.id;
        let Some(fw) = rec.forward.clone() else {
            return;
        };
        if lock(&self.running).contains_key(&id) {
            return;
        }
        let rj = self.forwarded_running(&rec, None);
        let log = rec.log_path.clone().unwrap_or_else(|| rj.log_path.clone());
        let running = rec.phase == Phase::Running;
        match self.forwarder.resume(&rec, &log) {
            Ok(()) => {
                self.spawn_driver(rj, fw.peer, None, running);
            }
            Err(e) => {
                let mut fin = Finish::from_phase(Phase::Interrupted);
                fin.summary = Some(format!(
                    "daemon restarted while the job ran on {}: {e}; result unknown, not rerun",
                    fw.peer.0
                ));
                let _ = self.guard("finish", self.ledger.finish(id, &fin));
            }
        }
    }

    /// After a restart: every job the ledger says is running on a peer is followed by its
    /// remote id (R-173). Called once, on the first tick.
    pub(crate) fn reconcile_forwards(self: &Arc<Self>) {
        let Some(running) = self.guard("find_running", self.ledger.find_running()) else {
            return;
        };
        for rec in running {
            if rec.forward.is_some() {
                self.resume_forwarded(rec);
            }
        }
    }

    fn spawn_driver(
        self: &Arc<Self>,
        rj: Arc<RunningJob>,
        peer: MachineName,
        guard: Option<SlotGuard>,
        started: bool,
    ) -> bool {
        let id = rj.id;
        lock(&self.running).insert(id, rj.clone());
        let sch = self.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("forwarded-{}", id.0))
            .spawn(move || drive(&sch, &rj, &peer, guard, started));
        if let Err(e) = spawned {
            lock(&self.running).remove(&id);
            self.forwarder.cancel(id, "no thread to follow it");
            self.note_error(&format!("cannot start a forwarding thread: {e}"));
            return false;
        }
        true
    }

    /// Moves a forwarded job that has not started back to the local queue (a forced reroute,
    /// R-166). The peer's copy is cancelled first, or confirmed gone; if that cannot be done
    /// the job stays where it is, because moving it would run it twice. Started jobs never
    /// move. Only a job with a remote id needs this: one without was never accepted.
    pub fn reroute_local(&self, id: JobId, why: &str) -> Result<(), String> {
        let rec = self
            .ledger
            .get(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no such job {}", id.0))?;
        let Some(fw) = rec.forward.clone() else {
            return Err("the job was not forwarded".to_string());
        };
        if !matches!(rec.phase, Phase::Queued | Phase::Waiting(_)) {
            return Err(format!("job {} has started or ended; it never moves", id.0));
        }
        let Some(remote) = fw.remote_job else {
            return Err(format!(
                "{} may already hold the job (no remote id is recorded to cancel); not moved",
                fw.peer.0
            ));
        };
        self.forwarder
            .cancel_remote(&fw.peer, remote)
            .map_err(|e| format!("not moved: {e}"))?;
        self.ledger.clear_forward(id).map_err(|e| e.to_string())?;
        self.forwarder.forget(id);
        {
            let mut st = lock(&self.state);
            st.no_forward
                .entry(id)
                .or_default()
                .insert(fw.peer.0.clone());
            st.fallbacks.insert(
                id,
                format!(
                    "moved off {} ({why}); its copy there was cancelled",
                    fw.peer.0
                ),
            );
        }
        self.wake.notify();
        Ok(())
    }

    /// Ends a forward: records the peer's result, or lets a job nothing was submitted for fall
    /// back to local placement.
    fn finish_forwarded(
        &self,
        rj: &RunningJob,
        peer: &MachineName,
        end: ForwardEnd,
        started: bool,
    ) {
        let id = rj.id;
        let cancelled = rj.token.state() != CancelState::None;
        let kill = lock(&rj.kill_reason).clone();
        if end.nothing_submitted && !cancelled && !started && self.may_fall_back(id) {
            let note = format!(
                "forwarding to {} failed ({}); ran here instead",
                peer.0, end.reason
            );
            if self.ledger.clear_forward(id).is_ok() {
                self.forwarder.forget(id);
                {
                    let mut st = lock(&self.state);
                    st.no_forward.entry(id).or_default().insert(peer.0.clone());
                    st.fallbacks.insert(id, note.clone());
                    st.last_wait.remove(&id);
                }
                self.set_wait(id, LedgerWait::Placement(note.clone()), &note);
                return;
            }
        }
        let mut phase = end.phase.clone();
        let mut summary = end.reason.clone();
        if let Some(k) = &kill
            && phase == Phase::Cancelled
        {
            phase = Phase::Failed {
                exit: end.exit_code,
            };
            summary = format!("{summary} ({k})");
        }
        if !started && phase != Phase::Cancelled {
            // The row never ran, and the ledger lets an unstarted row end only as NotStarted or
            // Cancelled. Nothing was submitted: NotStarted. The peer may have the job: the row
            // goes through Running so that it can end Interrupted, never as "not started".
            if end.nothing_submitted {
                phase = Phase::NotStarted {
                    reason: end.reason.clone(),
                };
            } else {
                let _ = self.ledger.set_phase(id, Phase::Running);
            }
        }
        let fin = Finish {
            phase,
            exit_code: end.exit_code,
            signal: end.signal,
            timeout: None,
            summary: Some(summary),
        };
        // The peer's last word replaces the last phase mirrored while it ran (a job that passed
        // must not keep showing `remote queued`). Only when the peer held the job at all.
        if end.remote_job.is_some() && !end.nothing_submitted {
            let _ = self.guard(
                "set_remote_phase",
                self.ledger.set_remote_phase(id, &end.phase),
            );
        }
        let retry = RetryPolicy::default();
        match retry_busy(&retry, &sleep, || self.ledger.finish(id, &fin)) {
            Ok(()) | Err(LedgerError::IllegalTransition { .. } | LedgerError::NotFound(_)) => {}
            Err(e) => self.note_error(&format!("finish forwarded job {}: {e}", id.0)),
        }
        for (k, s) in &end.samples {
            let _ = retry_busy(&retry, &sleep, || self.ledger.record_sample(k, s));
        }
        self.audit_fetched(id, peer, &end.fetch);
    }

    /// One `data.fetched` row per output file brought back: path, hash, bytes, peer and job.
    fn audit_fetched(&self, id: JobId, peer: &MachineName, results: &[tasks_core::FetchResult]) {
        let Some(sink) = self.audit_sink() else {
            return;
        };
        let Ok(Some(rec)) = self.ledger.get(id) else {
            return;
        };
        for r in results {
            if let tasks_core::FetchStatus::Fetched {
                bytes,
                sha256,
                unchanged,
            } = &r.status
            {
                sink.record(
                    sink.job_event(tasks_ledger::action::DATA_FETCHED, &rec)
                        .target(r.path.clone())
                        .detail("sha256", sha256.clone())
                        .detail("bytes", *bytes)
                        .detail("unchanged", *unchanged)
                        .detail("peer_name", peer.0.as_str()),
                );
            }
        }
    }

    /// Only a job that asked for "anywhere" may fall back; one pinned to a peer fails instead.
    fn may_fall_back(&self, id: JobId) -> bool {
        let Ok(Some(rec)) = self.ledger.get(id) else {
            return false;
        };
        let hint = match (&rec.spec.placement, &rec.hints.placement) {
            (Placement::Anywhere, Some(h)) => h.as_str(),
            (Placement::Anywhere, None) => "anywhere",
            _ => return false,
        };
        hint == "anywhere"
    }
}

/// The body of a forwarded job's thread: poll the forwarder, mirror the peer into the row.
fn drive(
    sch: &Arc<Scheduler>,
    rj: &Arc<RunningJob>,
    peer: &MachineName,
    guard: Option<SlotGuard>,
    mut started: bool,
) {
    let id = rj.id;
    let mut cancel_sent = false;
    let mut last_mirror = String::new();
    let mut last_remote_phase: Option<Phase> = None;
    let mut last_remote_job: Option<i64> = None;
    let mut ended: Option<ForwardEnd> = None;
    while ended.is_none() {
        if rj.token.state() != CancelState::None && !cancel_sent {
            let why = lock(&rj.kill_reason)
                .clone()
                .unwrap_or_else(|| "cancelled".to_string());
            sch.forwarder.cancel(id, &why);
            cancel_sent = true;
        }
        match sch.forwarder.poll(id) {
            ForwardPoll::Active(p) => {
                if p.accepted && !started {
                    // the remote id is recorded before the row goes Running, so a crash between
                    // the two leaves a row that knows where its job is
                    let _ = sch.guard(
                        "set_forward",
                        sch.ledger.set_forward(id, peer, p.remote_job, None),
                    );
                    match sch.ledger.set_phase(id, Phase::Running) {
                        Ok(()) => {
                            started = true;
                            let _ = sch.ledger.set_log_path(id, &rj.log_path);
                        }
                        // cancelled while the submit was in flight: the peer's copy is cancelled
                        // through the token on the next pass
                        Err(LedgerError::IllegalTransition { .. }) => rj.token.cancel(),
                        Err(e) => sch.note_error(&format!("start forwarded job {}: {e}", id.0)),
                    }
                    last_remote_job = p.remote_job;
                } else if p.remote_job != last_remote_job && p.remote_job.is_some() {
                    last_remote_job = p.remote_job;
                    let _ = sch.guard(
                        "set_forward",
                        sch.ledger.set_forward(id, peer, p.remote_job, None),
                    );
                }
                if p.remote_phase != last_remote_phase
                    && let Some(ph) = &p.remote_phase
                {
                    last_remote_phase = Some(ph.clone());
                    let _ = sch.guard("set_remote_phase", sch.ledger.set_remote_phase(id, ph));
                }
                if !started && p.mirror != last_mirror {
                    last_mirror = p.mirror.clone();
                    sch.set_wait(id, LedgerWait::Placement(p.mirror.clone()), &p.mirror);
                }
            }
            ForwardPoll::Done(end) => ended = Some(end),
        }
        if ended.is_none() {
            std::thread::sleep(sch.runner_poll);
        }
    }
    if let Some(end) = ended {
        sch.finish_forwarded(rj, peer, end, started);
    }
    drop(guard);
    lock(&sch.running).remove(&id);
    sch.wake.notify();
}
