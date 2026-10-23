//! The reporting trait the daemon adapts to the ledger.

use crate::{Lease, Outcome};
use std::time::Duration;
use tasks_core::JobFigures;
use tasks_platform::Child;

/// One output line with its monotonic offset from the job start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedLine {
    /// Time since the job started when the line was read (read-time resolution).
    pub offset: Duration,
    /// Line text without the newline; invalid UTF-8 is replaced.
    pub text: String,
    /// True when this is the continuation of an over-long line that was split.
    pub continued: bool,
}

impl TimedLine {
    /// Renders the sidecar form `offset_ms<TAB>flag<TAB>text`, where flag is
    /// `+` for a continuation and `-` otherwise. `tasks-timing` consumes this.
    pub fn to_record(&self) -> String {
        let flag = if self.continued { '+' } else { '-' };
        format!("{}\t{}\t{}\n", self.offset.as_millis(), flag, self.text)
    }
}

/// Receives what happens to a running job. Methods cannot fail: the daemon's
/// adapter owns retry and error handling for its ledger.
pub trait RunEvents {
    /// The process group started; persist the lease before anything else.
    fn started(&mut self, child: &Child, lease: &Lease);
    /// New complete lines were read from the log.
    fn lines(&mut self, lines: &[TimedLine]);
    /// A fresh sample of what the job costs the machine (every couple of seconds, and once more
    /// before `finished`). The default ignores it.
    fn figures(&mut self, _figures: &JobFigures) {}
    /// The job ended; called exactly once.
    fn finished(&mut self, outcome: &Outcome);
}
