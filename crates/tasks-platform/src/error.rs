//! Platform error type.

use thiserror::Error;

/// Errors from platform operations.
#[derive(Debug, Error)]
pub enum PlatformError {
    /// The operation is not implemented yet.
    #[error("not yet implemented: {0}")]
    NotYetImplemented(&'static str),
    /// An I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A system call failed.
    #[error("system error: {0}")]
    Errno(#[from] rustix::io::Errno),
    /// Data could not be parsed.
    #[error("parse error: {0}")]
    Parse(String),
}
