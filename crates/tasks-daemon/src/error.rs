//! Daemon errors.

use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors that stop the daemon from starting or a daemon operation from completing.
#[derive(Debug, Error)]
pub enum DaemonError {
    /// A file or directory operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// Another daemon holds this state directory.
    #[error("another daemon already runs on this state directory (pid {}; lock {path})", pid.map_or_else(|| "unknown".to_string(), |p| p.to_string()))]
    AlreadyRunning {
        /// The holder's pid, when readable.
        pid: Option<u32>,
        /// The locked pid file.
        path: PathBuf,
    },
    /// No directory could be chosen because the environment has no `HOME`.
    #[error("cannot choose the {0}: set HOME or pass it explicitly")]
    NoHome(&'static str),
    /// A configured directory is not absolute.
    #[error("the {0} must be an absolute path, got {1}")]
    RelativePath(&'static str, PathBuf),
    /// The ledger failed.
    #[error("ledger: {0}")]
    Ledger(#[from] tasks_ledger::LedgerError),
    /// The platform layer failed.
    #[error("platform: {0}")]
    Platform(#[from] tasks_platform::PlatformError),
    /// The runner failed.
    #[error("runner: {0}")]
    Runner(#[from] tasks_runner::RunnerError),
    /// Admission failed to measure the machine.
    #[error("admission: {0}")]
    Admission(#[from] tasks_admission::AdmissionError),
    /// The machine identity or trust state could not be opened.
    #[error("identity: {0}")]
    Identity(String),
    /// The peer link could not start.
    #[error("peers: {0}")]
    Peer(String),
    /// A command-line argument is wrong.
    #[error("usage: {0}")]
    Usage(String),
}

impl DaemonError {
    /// Wraps an I/O error with its path.
    pub fn io(path: &Path, source: std::io::Error) -> DaemonError {
        DaemonError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}
