//! Ledger errors.

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
    /// The spec failed validation.
    #[error("invalid job: {0}")]
    Invalid(#[from] CoreError),
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
