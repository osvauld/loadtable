//! Ledger errors.

use std::time::Duration;
use tasks_core::plan::PlanError;
use tasks_core::{CoreError, JobId, Phase};
use thiserror::Error;

/// Errors from the job store.
#[derive(Debug, Error)]
pub enum LedgerError {
    /// The job does not exist.
    #[error("no such job {0:?}")]
    NotFound(JobId),
    /// The phase change is not allowed.
    #[error("illegal transition {from:?} -> {to:?}")]
    IllegalTransition {
        /// Current phase.
        from: Phase,
        /// Requested phase.
        to: Phase,
    },
    /// A dependency names a job that does not exist.
    #[error("unknown dependency {0:?}")]
    UnknownDependency(JobId),
    /// A dependency already ended without passing, so the new job could never run.
    #[error("dependency {0:?} already ended without passing")]
    DependencyNotPassable(JobId),
    /// The spec failed validation.
    #[error("invalid job: {0}")]
    Invalid(#[from] CoreError),
    /// The plan failed validation.
    #[error("invalid plan: {0}")]
    Plan(#[from] PlanError),
    /// A timing sample was rejected (not finite, negative, or out of range).
    #[error("invalid sample: {0}")]
    InvalidSample(&'static str),
    /// A value does not fit the column it is stored in.
    #[error("value out of range: {0}")]
    OutOfRange(&'static str),
    /// The database was written by a newer version of the program.
    #[error("database schema version {found} is newer than supported version {supported}")]
    SchemaTooNew {
        /// Version found in the database.
        found: i64,
        /// Newest version this build understands.
        supported: i64,
    },
    /// The ledger stayed busy past the retry deadline. This is not a cancellation (R-003).
    #[error("ledger unavailable after waiting {waited:?} (not a cancellation)")]
    Unavailable {
        /// Total time spent backing off.
        waited: Duration,
    },
    /// SQLite failed.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// JSON (de)serialization failed.
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    /// An internal lock was poisoned.
    #[error("store lock poisoned")]
    Poisoned,
}

impl LedgerError {
    /// True for SQLITE_BUSY / SQLITE_LOCKED: the operation can be retried later (R-004).
    pub fn is_busy(&self) -> bool {
        use rusqlite::ErrorCode::{DatabaseBusy, DatabaseLocked};
        matches!(
            self,
            LedgerError::Sqlite(rusqlite::Error::SqliteFailure(e, _))
                if matches!(e.code, DatabaseBusy | DatabaseLocked)
        )
    }
}
