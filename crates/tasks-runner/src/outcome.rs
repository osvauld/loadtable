//! The final outcome of a run.

use crate::Lease;
use std::time::Duration;
use tasks_core::{JobFigures, Phase};
use tasks_platform::Usage;

/// How the run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The command exited on its own.
    Exited,
    /// Cancelled by a user or the daemon.
    Cancelled,
    /// Cancelled because the client left and the job asked for that.
    ClientLost,
    /// Stopped by the wall-clock timeout.
    WallTimeout,
    /// Stopped by the output-idle timeout.
    IdleTimeout,
    /// The leader could not be reaped; the result is unknown.
    Lost,
}

/// Everything known about a finished run.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// How it ended.
    pub end: End,
    /// The command's exit code (128 + n when killed by signal n); `None` when unknown.
    pub exit_code: Option<i32>,
    /// The signal we sent that ended it, when the code is 128 plus that signal.
    pub signal: Option<i32>,
    /// Last usage sample taken while the group was alive.
    pub usage: Option<Usage>,
    /// Wall time from spawn to end.
    pub wall: Duration,
    /// The lease the group ran under.
    pub lease: Lease,
    /// What the job cost, read after the last process was gone; `None` when nothing could be
    /// measured (no attribution configured).
    pub figures: Option<JobFigures>,
}

impl Outcome {
    /// The terminal phase, always reachable from `Running` per `Phase::can_move_to`.
    /// Timeouts are `TimedOut`; an unknown result is `Interrupted`, never a pass (R-6.9).
    pub fn phase(&self) -> Phase {
        match self.end {
            End::Cancelled | End::ClientLost => Phase::Cancelled,
            End::Lost => Phase::Interrupted,
            End::WallTimeout | End::IdleTimeout => Phase::TimedOut {
                exit: self.exit_code,
            },
            End::Exited => match self.exit_code {
                Some(0) => Phase::Passed,
                code => Phase::Failed { exit: code },
            },
        }
    }

    /// One line naming the command's own status, never a wrapper's (R-086).
    pub fn describe(&self) -> String {
        let status = match (self.exit_code, self.signal) {
            (_, Some(sig)) => format!("killed by signal {sig}"),
            (Some(c), None) => format!("exit {c}"),
            (None, None) => "exit -".to_string(),
        };
        let why = match self.end {
            End::Exited => "command",
            End::Cancelled => "cancelled command",
            End::ClientLost => "command (client lost)",
            End::WallTimeout => "command (wall timeout)",
            End::IdleTimeout => "command (idle timeout)",
            End::Lost => "command (result lost)",
        };
        format!("{why} {status}")
    }
}
