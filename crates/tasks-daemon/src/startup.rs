//! Startup reconciliation: what to do with jobs the ledger says were running (R-008).
//!
//! A job is never re-run. If its process group is still ours (same leader start time) the
//! group survived a daemon restart: it is terminated with the grace, then the job is marked
//! `Interrupted`. If the group is gone the job is marked `Interrupted` as well.

use std::time::Duration;
use tasks_core::{JobId, Phase};
use tasks_ledger::{Finish, LeaseRecord, LedgerError, RetryPolicy, Store, retry_busy};
use tasks_platform::{GroupId, PlatformError, Processes, Signal};
use tasks_runner::Lease;

/// What startup did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StartupReport {
    /// Jobs whose surviving group was terminated.
    pub terminated: Vec<JobId>,
    /// Every job marked interrupted.
    pub interrupted: Vec<JobId>,
    /// Problems, in words.
    pub notes: Vec<String>,
}

/// The group a lease record names.
pub fn group_of(l: &LeaseRecord) -> GroupId {
    GroupId {
        pgid: l.pgid,
        start_time: l.leader_start,
    }
}

/// Stops a surviving group: Term, wait up to `grace`, then Kill. `sleep` is injected so tests
/// do not wait. Returns true when a group was ours and was signalled.
pub fn terminate_group(
    procs: &dyn Processes,
    group: GroupId,
    grace: Duration,
    sleep: &dyn Fn(Duration),
) -> Result<bool, PlatformError> {
    let lease = Lease::new(group);
    if !lease.still_ours(procs)? {
        return Ok(false);
    }
    match procs.signal_group(group, Signal::Term) {
        Ok(()) | Err(PlatformError::Gone { .. }) => {}
        Err(e) => return Err(e),
    }
    let step = Duration::from_millis(20);
    let mut waited = Duration::ZERO;
    while waited < grace {
        if !lease.still_ours(procs)? {
            return Ok(true);
        }
        sleep(step);
        waited += step;
    }
    if lease.still_ours(procs)? {
        match procs.signal_group(group, Signal::Kill) {
            Ok(()) | Err(PlatformError::Gone { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Reconciles the ledger with the machine at daemon start.
pub fn reconcile(
    store: &dyn Store,
    procs: &dyn Processes,
    grace: Duration,
    sleep: &dyn Fn(Duration),
) -> Result<StartupReport, LedgerError> {
    let mut report = StartupReport::default();
    let retry = RetryPolicy::default();
    let running = retry_busy(&retry, sleep, || store.find_running())?;
    for rec in running {
        let summary = match rec.lease.as_ref() {
            Some(l) => match terminate_group(procs, group_of(l), grace, sleep) {
                Ok(true) => {
                    report.terminated.push(rec.id);
                    "daemon restarted: the job's process group survived and was terminated"
                        .to_string()
                }
                Ok(false) => {
                    "daemon restarted: the job's process group was already gone".to_string()
                }
                Err(e) => {
                    report
                        .notes
                        .push(format!("job {}: could not check its group: {e}", rec.id.0));
                    format!(
                        "daemon restarted: could not verify the job's group ({e}); not signalled"
                    )
                }
            },
            None => "daemon restarted: the job had no recorded process group".to_string(),
        };
        let mut fin = Finish::from_phase(Phase::Interrupted);
        fin.summary = Some(summary);
        match retry_busy(&retry, sleep, || store.finish(rec.id, &fin)) {
            Ok(()) => report.interrupted.push(rec.id),
            // finished by someone else in the meantime
            Err(LedgerError::IllegalTransition { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(report)
}
