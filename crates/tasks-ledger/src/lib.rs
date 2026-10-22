//! The job store: a `Store` trait with in-memory and SQLite implementations, durable plans and
//! dependencies, and the timing-sample store behind `tasks_timing::History`.

mod clock;
mod error;
mod logic;
mod memory;
mod migrate;
mod record;
mod request;
mod retry;
mod sqlite;
#[cfg(test)]
mod suite;

pub use clock::{Clock, ManualClock, SystemClock};
pub use error::LedgerError;
pub use memory::MemoryStore;
pub use migrate::SCHEMA_VERSION;
pub use record::{CancelRequest, Forward, JobRecord, LeaseRecord, ListFilter, TimeoutReason};
pub use request::{
    CancelOutcome, ChangeToken, Finish, NewJob, PlanContext, PlanSubmitted, PruneReport,
    RetentionPolicy, StartInfo,
};
pub use retry::{RetryPolicy, retry_busy};
pub use sqlite::{OpenOptions, SqliteStore};

use std::path::Path;
use std::time::Duration;
use tasks_core::plan::Plan;
use tasks_core::{JobId, JobSpec, MachineName, Phase, WaitReason};
use tasks_timing::{Sample, SampleKey};

/// Result type for ledger operations.
pub type Result<T> = std::result::Result<T, LedgerError>;

/// Durable storage of jobs, plans, dependencies and timing samples. Every write is one short
/// transaction; a store never holds a lock across calls.
pub trait Store {
    /// Validates and stores a new queued job, returning its id. Dependencies must exist and not
    /// have ended without passing.
    fn submit_job(&self, job: &NewJob) -> Result<JobId>;
    /// Submits a plain job (no hints, no plan).
    fn submit(&self, spec: &JobSpec) -> Result<JobId> {
        self.submit_job(&NewJob::new(spec.clone()))
    }
    /// Validates a plan and stores all its steps with durable dependencies, or none of them.
    fn submit_plan(&self, ctx: &PlanContext, plan: &Plan) -> Result<PlanSubmitted>;
    /// Fetches one job.
    fn get(&self, id: JobId) -> Result<Option<JobRecord>>;
    /// Moves a job to `next`, rejecting transitions `Phase::can_move_to` forbids. A terminal
    /// `next` finishes the job with only that phase's data; prefer `finish`.
    fn set_phase(&self, id: JobId, next: Phase) -> Result<()>;
    /// Lists jobs matching the filter, oldest first.
    fn list(&self, filter: &ListFilter) -> Result<Vec<JobRecord>>;
    /// Queued or waiting jobs whose dependencies have all passed (a missing dependency is not
    /// passed).
    fn ready(&self) -> Result<Vec<JobId>>;
    /// Queued or waiting jobs with a dependency that ended failed, timed out, cancelled or
    /// interrupted: any terminal dependency other than `Passed`.
    fn blocked_by_failure(&self) -> Result<Vec<JobId>>;

    /// Records why a job is waiting: the coarse reason in the phase and the full text beside it.
    fn set_wait(&self, id: JobId, reason: WaitReason, text: &str) -> Result<()>;
    /// Atomically records machine, lease and log path and moves `Queued|Waiting` to `Running`.
    fn start(&self, id: JobId, info: &StartInfo) -> Result<()>;
    /// Ends a job with an outcome, and records `sample` in the same transaction when given. A
    /// cancel committed earlier wins over a passed/failed outcome (R-006); a terminal job is
    /// immutable. An invalid sample fails the whole call and changes nothing (R-013).
    fn finish_recording(
        &self,
        id: JobId,
        outcome: &Finish,
        sample: Option<(&SampleKey, &Sample)>,
    ) -> Result<()>;
    /// Ends a job with an outcome.
    fn finish(&self, id: JobId, outcome: &Finish) -> Result<()> {
        self.finish_recording(id, outcome, None)
    }
    /// Ends a job and records its timing sample atomically.
    fn finish_with_sample(
        &self,
        id: JobId,
        outcome: &Finish,
        key: &SampleKey,
        sample: &Sample,
    ) -> Result<()> {
        self.finish_recording(id, outcome, Some((key, sample)))
    }
    /// Records a cancel request and who made it (R-6.2). An unstarted job is cancelled at once.
    fn request_cancel(&self, id: JobId, by: &str) -> Result<CancelOutcome>;
    /// True when a cancel was requested. A ledger error is an `Err`, never `Ok(true)` (R-001).
    fn cancel_requested(&self, id: JobId) -> Result<bool> {
        self.get(id)?
            .map(|r| r.cancel_requested())
            .ok_or(LedgerError::NotFound(id))
    }
    /// Records where the job was forwarded.
    fn set_forward(&self, id: JobId, peer: &MachineName, remote_job: Option<i64>) -> Result<()>;
    /// Records the last known peer phase without touching the local phase (R-015).
    fn set_remote_phase(&self, id: JobId, phase: &Phase) -> Result<()>;
    /// Records the job's log path.
    fn set_log_path(&self, id: JobId, path: &Path) -> Result<()>;
    /// Jobs recorded as running, for restart reconciliation.
    fn find_running(&self) -> Result<Vec<JobRecord>> {
        Ok(self
            .list(&ListFilter {
                active_only: true,
                ..ListFilter::default()
            })?
            .into_iter()
            .filter(|r| r.phase == Phase::Running)
            .collect())
    }
    /// Marks every running job interrupted, never re-queued or re-run (R-008). Returns their ids.
    fn interrupt_running(&self, reason: &str) -> Result<Vec<JobId>> {
        let mut ids = vec![];
        for r in self.find_running()? {
            let mut f = Finish::from_phase(Phase::Interrupted);
            f.summary = Some(reason.to_string());
            match self.finish(r.id, &f) {
                Ok(()) => ids.push(r.id),
                Err(LedgerError::IllegalTransition { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(ids)
    }
    /// Cancels, transitively, every unstarted job whose dependency can no longer pass (R-178).
    /// Returns the cancelled ids.
    fn cancel_blocked(&self) -> Result<Vec<JobId>> {
        let mut out = vec![];
        loop {
            let blocked = self.blocked_by_failure()?;
            if blocked.is_empty() {
                return Ok(out);
            }
            for id in blocked {
                if self.request_cancel(id, "dependency failed")? == CancelOutcome::Cancelled {
                    out.push(id);
                }
            }
        }
    }
    /// All jobs with exactly this owner, cwd and argv submitted within `within` of now, oldest
    /// first. Several matches are ambiguous and the caller decides what to do (R-099).
    fn find_recent_spec(
        &self,
        owner: &str,
        cwd: &Path,
        argv: &[String],
        within: Duration,
    ) -> Result<Vec<JobRecord>>;

    /// Stores one timing sample; invalid samples are refused (R-013).
    fn record_sample(&self, key: &SampleKey, sample: &Sample) -> Result<()>;
    /// Removes old finished jobs and samples. Running and queued jobs, finished jobs that an
    /// unfinished job depends on, and the newest job id are always kept.
    fn prune(&self, policy: &RetentionPolicy) -> Result<PruneReport>;
    /// Changes whenever any connection commits (R-025).
    fn change_token(&self) -> Result<ChangeToken>;
}
