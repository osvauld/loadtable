//! Errors for loading, validating and expanding a policy.

use std::fmt;
use std::path::PathBuf;
use tasks_core::plan::PlanError;
use tasks_timing::TimingError;
use thiserror::Error;

/// What is wrong with one place in the policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The schema version is not supported.
    Version(u32),
    /// A suite or step name has characters outside `[A-Za-z0-9._-]` (suites: no `.`), or is empty.
    BadName(String),
    /// Two suites cannot share a name (never produced by TOML) or two steps share one.
    DuplicateStep(String),
    /// `after` names a step that is not in the suite.
    UnknownAfter(String),
    /// `after` lists the step itself.
    SelfAfter,
    /// `after` lists a step twice.
    DuplicateAfter(String),
    /// The dependencies form a cycle.
    Cycle,
    /// The command is empty or its first word is empty.
    EmptyCommand,
    /// The command has a NUL byte.
    NulInCommand,
    /// The timeout is zero or above the maximum.
    BadTimeout(u64),
    /// The placement hint is empty.
    EmptyPlacement,
    /// `split = "cargo-test"` on a command that is not `cargo test`.
    SplitNotCargoTest,
    /// A target key is not of the form `kind:name`.
    BadTargetKey(String),
    /// A suite has no steps.
    NoSteps,
    /// A `[sync] extra` pattern is empty, absolute, climbs out of the checkout or names `.git`;
    /// or there are too many.
    BadSyncExtra(String),
    /// A step asks for `priority = "blocking"`, which only a submit with a reason can claim.
    BlockingInPolicy,
}

impl Problem {
    /// How to fix it.
    pub fn hint(&self) -> String {
        match self {
            Problem::Version(_) => "set `version = 1` at the top of the file".into(),
            Problem::BadName(_) => "use letters, digits, `_` and `-` (steps may also use `.`)".into(),
            Problem::DuplicateStep(_) => "rename one of the steps; names must be unique per suite".into(),
            Problem::UnknownAfter(d) => format!("add a step named {d:?} or remove it from `after`"),
            Problem::SelfAfter => "remove the step's own name from `after`".into(),
            Problem::DuplicateAfter(_) => "list each dependency once".into(),
            Problem::Cycle => "break the cycle: a step cannot depend, directly or not, on itself".into(),
            Problem::EmptyCommand => "write `command = [\"cargo\", \"test\"]`: argv words, not a shell line".into(),
            Problem::NulInCommand => "remove the NUL byte from the command".into(),
            Problem::BadTimeout(_) => format!(
                "use `timeout_secs` between 1 and {}",
                crate::validate::MAX_TIMEOUT_SECS
            ),
            Problem::EmptyPlacement => "remove `placement` or name a machine".into(),
            Problem::SplitNotCargoTest => {
                "`split = \"cargo-test\"` needs `command = [\"cargo\", \"test\", ...]`; drop `split`".into()
            }
            Problem::BadTargetKey(_) => "name targets like \"test:lifecycle\" or \"lib:mycrate\"".into(),
            Problem::NoSteps => "add a `[[suites.NAME.steps]]` entry or delete the suite".into(),
            Problem::BlockingInPolicy => {
                "use `priority = \"background\"` (or leave it out); claim blocking with `--priority blocking --reason TEXT` when submitting".into()
            }
            Problem::BadSyncExtra(_) => {
                "list `[sync] extra` entries as relative paths like \"local.properties\" or \"config/*.json\"".into()
            }
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::Version(v) => write!(f, "unsupported policy version {v}"),
            Problem::BadName(n) => write!(f, "bad name {n:?}"),
            Problem::DuplicateStep(n) => write!(f, "duplicate step name {n:?}"),
            Problem::UnknownAfter(d) => write!(f, "`after` names unknown step {d:?}"),
            Problem::SelfAfter => write!(f, "step depends on itself"),
            Problem::DuplicateAfter(d) => write!(f, "`after` lists {d:?} twice"),
            Problem::Cycle => write!(f, "dependency cycle"),
            Problem::EmptyCommand => write!(f, "empty command"),
            Problem::NulInCommand => write!(f, "command contains a NUL byte"),
            Problem::BadTimeout(t) => write!(f, "absurd timeout of {t} s"),
            Problem::EmptyPlacement => write!(f, "empty placement hint"),
            Problem::SplitNotCargoTest => {
                write!(f, "split rule on a command that is not `cargo test`")
            }
            Problem::BadTargetKey(k) => write!(f, "bad target key {k:?}"),
            Problem::NoSteps => write!(f, "suite has no steps"),
            Problem::BadSyncExtra(why) => write!(f, "bad [sync] extra entry: {why}"),
            Problem::BlockingInPolicy => write!(f, "priority \"blocking\" in the run policy"),
        }
    }
}

/// One validation finding with its location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Suite name; empty for file-level problems.
    pub suite: String,
    /// Step name, when the problem is about a step.
    pub step: Option<String>,
    /// The problem.
    pub problem: Problem,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.suite.is_empty() {
            write!(f, "suite {:?}", self.suite)?;
        }
        if let Some(s) = &self.step {
            write!(f, " step {s:?}")?;
        }
        write!(f, ": {} (fix: {})", self.problem, self.problem.hint())
    }
}

/// All findings of one validation run, tagged with the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violations {
    /// The policy file.
    pub file: PathBuf,
    /// The findings; never empty.
    pub list: Vec<Violation>,
}

impl fmt::Display for Violations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: invalid run policy", self.file.display())?;
        for v in &self.list {
            write!(f, "\n  {v}")?;
        }
        Ok(())
    }
}

/// Errors from this crate.
#[derive(Debug, Error, PartialEq)]
pub enum PolicyError {
    /// No policy file between the cwd and the repo root.
    #[error(
        "no .tasks/run-policy.toml found from {0} up to the repo root (fix: create one; see docs/examples/run-policy.toml)"
    )]
    NotFound(PathBuf),
    /// The file could not be read.
    #[error("{file}: cannot read policy: {message}")]
    Read {
        /// The file.
        file: PathBuf,
        /// The I/O message.
        message: String,
    },
    /// TOML syntax error or unknown/misspelled key.
    #[error(
        "{file}: {message} (fix: remove or correct the named key; allowed keys are listed in docs/examples/run-policy.toml)"
    )]
    Parse {
        /// The file.
        file: PathBuf,
        /// The parser message, with line and column.
        message: String,
    },
    /// Validation findings.
    #[error("{0}")]
    Invalid(Violations),
    /// `suite run NAME` for a suite that does not exist.
    #[error("unknown suite {name:?} (fix: choose one of: {known})")]
    UnknownSuite {
        /// The requested name.
        name: String,
        /// Comma-separated known suites.
        known: String,
    },
    /// Timing history or splitting failed.
    #[error("timing: {0}")]
    Timing(#[from] TimingError),
    /// The expanded plan is invalid.
    #[error("expanded plan invalid: {0}")]
    Plan(#[from] PlanError),
}
