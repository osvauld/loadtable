//! The CLI's error type and its exit codes.

use crate::client::ClientError;
use std::io;
use tasks_daemon::api::ErrorCode;

/// Exit code: success.
pub const EXIT_OK: i32 = 0;
/// Exit code: a failure that is not the caller's fault (the job failed without a code, the
/// daemon failed internally, the ledger was busy).
pub const EXIT_FAILURE: i32 = 1;
/// Exit code: usage error, or the submission was refused (policy, daemon validation, owner).
pub const EXIT_USAGE: i32 = 2;
/// Exit code: the daemon could not be reached or started, or spoke an unreadable protocol.
pub const EXIT_UNREACHABLE: i32 = 3;
/// Exit code: no such job.
pub const EXIT_NOT_FOUND: i32 = 4;
/// Exit code: the job timed out and reported no code (as GNU `timeout`).
pub const EXIT_TIMED_OUT: i32 = 124;
/// Exit code: the job was cancelled, interrupted or never started.
pub const EXIT_NOT_COMPLETED: i32 = 125;
/// Exit code: standard output was closed (128 + SIGPIPE), R-106.
pub const EXIT_PIPE: i32 = 141;

/// The exit codes as `--help` documents them.
pub const EXIT_CODES_HELP: &str = "\
EXIT CODES
  run              the job's own exit code when it has one; otherwise
                   124 timed out, 125 cancelled / interrupted / never started, 1 failed
  128+N            `run` detached on signal N (130 INT, 129 HUP, 143 TERM); the job keeps running
  0                success
  1                failure: the daemon failed internally, the ledger was busy, a job failed
  2                usage error, or the submission was refused (policy, validation, not the owner)
  3                the daemon could not be reached or started, or its reply was unreadable
  4                no such job
  141              standard output was closed (`| head`); the job is unaffected
Because `run` passes the job's code through, codes 2 to 4 can also come from the job itself.";

/// Everything that can end a command.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// Wrong arguments; the text says what and how to fix it.
    #[error("{0}")]
    Usage(String),
    /// Refused before or by the daemon.
    #[error("{0}")]
    Refused(String),
    /// A failure with its message.
    #[error("{0}")]
    Failed(String),
    /// No such job or file.
    #[error("{0}")]
    NotFound(String),
    /// A call to the daemon failed.
    #[error(transparent)]
    Client(#[from] ClientError),
    /// Standard output is closed.
    #[error("standard output was closed")]
    Pipe,
}

impl From<io::Error> for CliError {
    fn from(e: io::Error) -> CliError {
        if e.kind() == io::ErrorKind::BrokenPipe {
            CliError::Pipe
        } else {
            CliError::Failed(format!("write failed: {e}"))
        }
    }
}

/// The exit code for a daemon refusal.
pub fn code_for_api(code: ErrorCode) -> i32 {
    match code {
        ErrorCode::NotFound => EXIT_NOT_FOUND,
        ErrorCode::BadRequest | ErrorCode::TooLarge | ErrorCode::Refused | ErrorCode::NotOwner => {
            EXIT_USAGE
        }
        ErrorCode::UnsupportedVersion | ErrorCode::ResponseTooLarge => EXIT_UNREACHABLE,
        ErrorCode::Busy | ErrorCode::LedgerUnavailable | ErrorCode::Internal => EXIT_FAILURE,
    }
}

impl CliError {
    /// The process exit code.
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Usage(_) | CliError::Refused(_) => EXIT_USAGE,
            CliError::Failed(_) => EXIT_FAILURE,
            CliError::NotFound(_) => EXIT_NOT_FOUND,
            CliError::Pipe => EXIT_PIPE,
            CliError::Client(ClientError::Daemon(e)) => code_for_api(e.code),
            CliError::Client(ClientError::RequestTooLarge { .. }) => EXIT_USAGE,
            CliError::Client(_) => EXIT_UNREACHABLE,
        }
    }
}
