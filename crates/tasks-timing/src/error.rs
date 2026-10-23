//! Error type for this crate.

use tasks_core::plan::PlanError;
use thiserror::Error;

/// Errors from timing, splitting and history lookups.
#[derive(Debug, Error, PartialEq)]
pub enum TimingError {
    /// The argv to split is empty.
    #[error("argv is empty")]
    EmptyArgv,
    /// The command is not `cargo test`.
    #[error("not a `cargo test` command: {0}")]
    NotCargoTest(String),
    /// There are no test executables to split into steps.
    #[error("no test executables to split")]
    NoTestExecutables,
    /// A line of cargo JSON output could not be parsed.
    #[error("bad cargo JSON on line {line}: {message}")]
    BadJson {
        /// 1-based line number.
        line: usize,
        /// Parser message.
        message: String,
    },
    /// The history store failed.
    #[error("history store error: {0}")]
    History(String),
    /// The resulting plan is invalid.
    #[error("invalid plan: {0}")]
    Plan(#[from] PlanError),
}
