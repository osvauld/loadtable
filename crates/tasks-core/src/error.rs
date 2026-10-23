//! Error type for core validation.

use thiserror::Error;

/// Errors produced by core validation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    /// The owner name is empty.
    #[error("owner is empty")]
    OwnerEmpty,
    /// The owner name exceeds 128 bytes.
    #[error("owner is longer than 128 bytes")]
    OwnerTooLong,
    /// The owner name has a character outside `[A-Za-z0-9._-]`.
    #[error("owner contains invalid character {0:?}")]
    OwnerBadChar(char),
    /// The argv is empty.
    #[error("argv is empty")]
    EmptyArgv,
    /// A NUL byte appears in argv or env.
    #[error("NUL byte in argv or env")]
    Nul,
    /// An environment variable name is not `[A-Z_][A-Z0-9_]*`.
    #[error("invalid environment variable name {0:?}")]
    BadEnvName(String),
    /// More than 64 `after` entries.
    #[error("too many dependencies: {0} (max 64)")]
    TooManyDeps(usize),
    /// The wall-clock limit is zero or above [`crate::MAX_TIMEOUT_SECS`].
    #[error("timeout of {0} s is outside 1..={max} s", max = crate::MAX_TIMEOUT_SECS)]
    BadTimeout(u64),
    /// argv names a denied device.
    #[error("argv mentions denied device {0}")]
    DeniedDevice(String),
    /// A `blocking` job names no reason.
    #[error("priority blocking needs a reason (--reason TEXT)")]
    BlockingNeedsReason,
    /// The priority reason is longer than [`crate::MAX_PRIORITY_REASON`] characters.
    #[error("the priority reason is longer than {max} characters", max = crate::MAX_PRIORITY_REASON)]
    PriorityReasonTooLong,
}
