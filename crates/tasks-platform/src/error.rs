//! Platform error type.

use std::path::PathBuf;
use thiserror::Error;

/// Errors from platform operations. Each variant names what failed.
#[derive(Debug, Error)]
pub enum PlatformError {
    /// A file operation failed.
    #[error("io error on {path}: {source}")]
    Io {
        /// File or directory involved.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// A system call failed.
    #[error("{syscall} failed{}: {source}", path_suffix(.path))]
    Syscall {
        /// Name of the syscall.
        syscall: &'static str,
        /// Path involved, when there is one.
        path: Option<PathBuf>,
        /// Errno returned.
        source: rustix::io::Errno,
    },
    /// Text from the OS could not be parsed.
    #[error("cannot parse {what}: {detail}")]
    Parse {
        /// What was being parsed (file or command name).
        what: String,
        /// Why parsing failed.
        detail: String,
    },
    /// Linux PSI is not available (fail closed).
    #[error(
        "{path} is unavailable; pressure gating needs a kernel with CONFIG_PSI (and psi=1 if disabled by default): {detail}"
    )]
    PsiUnavailable {
        /// The missing or unreadable PSI file.
        path: PathBuf,
        /// Underlying detail.
        detail: String,
    },
    /// Pressure thresholds are unusable.
    #[error("invalid pressure thresholds: {0}")]
    InvalidThresholds(String),
    /// The process group no longer exists or its leader was replaced (PID reuse).
    #[error("process group {pgid} is gone (leader exited or pid was reused)")]
    Gone {
        /// The group id that was targeted.
        pgid: i32,
    },
    /// The group id is not a valid signalling target (0, 1 or negative).
    #[error("refusing to signal invalid process group {0}")]
    InvalidGroup(i32),
    /// The group is the caller's own group.
    #[error("refusing to signal our own process group {0}")]
    OwnGroup(i32),
    /// A path argument is unusable (relative or contains NUL).
    #[error("invalid path {0:?}")]
    InvalidPath(PathBuf),
    /// The spawn request was unusable.
    #[error("invalid spawn request: {0}")]
    InvalidSpawn(String),
    /// Starting a process failed.
    #[error("cannot start {program}: {source}")]
    Spawn {
        /// Program that failed to start.
        program: String,
        /// Underlying error.
        source: std::io::Error,
    },
    /// Running a helper command failed.
    #[error("command `{command}` failed: {detail}")]
    Command {
        /// The command line.
        command: String,
        /// What went wrong.
        detail: String,
    },
    /// A different service file already exists; it is never overwritten.
    #[error("{0} already exists with different contents; not overwriting")]
    Exists(PathBuf),
    /// The service name is unusable as a file name.
    #[error("invalid service name {0:?}")]
    InvalidServiceName(String),
    /// `HOME` is not set.
    #[error("HOME is not set; cannot locate the user service directory")]
    NoHome,
    /// Per-job cgroups are not available; the reason says what to change.
    #[error("cgroups unavailable: {0}")]
    NoCgroups(String),
    /// A cgroup still has processes or child cgroups, so it cannot be removed.
    #[error("cgroup {0} is not empty")]
    CgroupBusy(PathBuf),
    /// An internal lock was poisoned (fakes).
    #[error("internal lock poisoned: {0}")]
    Poisoned(&'static str),
}

fn path_suffix(path: &Option<PathBuf>) -> String {
    match path {
        Some(p) => format!(" on {}", p.display()),
        None => String::new(),
    }
}

impl PlatformError {
    /// Builds an [`PlatformError::Io`].
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> PlatformError {
        PlatformError::Io {
            path: path.into(),
            source,
        }
    }
    /// Builds a [`PlatformError::Parse`].
    pub fn parse(what: impl Into<String>, detail: impl Into<String>) -> PlatformError {
        PlatformError::Parse {
            what: what.into(),
            detail: detail.into(),
        }
    }
}
