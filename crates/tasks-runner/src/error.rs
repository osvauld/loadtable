//! Runner errors.

use std::path::PathBuf;
use tasks_platform::PlatformError;
use thiserror::Error;

/// Errors from running a job. Command failure is an [`crate::Outcome`], not an error.
#[derive(Debug, Error)]
pub enum RunnerError {
    /// The platform could not start the process group.
    #[error("cannot start job: {0}")]
    Spawn(#[source] PlatformError),
    /// A log file operation failed.
    #[error("log io error on {path}: {source}")]
    Log {
        /// The log file involved.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// The job has no command.
    #[error("job has an empty argv")]
    EmptyArgv,
    /// A policy value is unusable.
    #[error("invalid policy: {0}")]
    Policy(String),
}

impl RunnerError {
    /// Builds a [`RunnerError::Log`].
    pub fn log(path: impl Into<PathBuf>, source: std::io::Error) -> RunnerError {
        RunnerError::Log {
            path: path.into(),
            source,
        }
    }
}
