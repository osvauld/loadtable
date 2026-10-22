//! The job store: a `Store` trait with in-memory and SQLite implementations.

mod error;
mod memory;
mod record;
mod sqlite;
#[cfg(test)]
mod suite;

pub use error::LedgerError;
pub use memory::MemoryStore;
pub use record::{JobRecord, ListFilter};
pub use sqlite::SqliteStore;

use tasks_core::{JobId, JobSpec, Phase};

/// Result type for ledger operations.
pub type Result<T> = std::result::Result<T, LedgerError>;

/// Durable storage of jobs and their phases.
pub trait Store {
    /// Validates and stores a new queued job, returning its id.
    fn submit(&self, spec: &JobSpec) -> Result<JobId>;
    /// Fetches one job.
    fn get(&self, id: JobId) -> Result<Option<JobRecord>>;
    /// Moves a job to `next`, rejecting transitions `Phase::can_move_to` forbids.
    fn set_phase(&self, id: JobId, next: Phase) -> Result<()>;
    /// Lists jobs matching the filter, oldest first.
    fn list(&self, filter: &ListFilter) -> Result<Vec<JobRecord>>;
    /// Queued jobs whose dependencies have all passed.
    fn ready(&self) -> Result<Vec<JobId>>;
    /// Queued jobs with a dependency that ended without passing.
    fn blocked_by_failure(&self) -> Result<Vec<JobId>>;
}
