//! Job lifecycle phases.

use crate::{DeviceId, JobId};
use serde::{Deserialize, Serialize};

/// Kind of slot a job can wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SlotKind {
    /// Compile slot.
    Compile,
    /// Run slot.
    Run,
    /// Offload slot.
    Offload,
    /// Device slot.
    Device,
}

/// Why a job is waiting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitReason {
    /// No free slot.
    Slot(SlotKind),
    /// System pressure (description).
    Pressure(String),
    /// Waiting on another job.
    Dependency(JobId),
    /// Low disk space.
    Disk,
    /// Device is busy.
    Device(DeviceId),
}

/// Lifecycle phase of a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Submitted, not yet considered.
    Queued,
    /// Considered but not admitted.
    Waiting(WaitReason),
    /// Executing.
    Running,
    /// Finished with success.
    Passed,
    /// Finished with failure.
    Failed {
        /// Exit code, if any.
        exit: Option<i32>,
    },
    /// Cancelled by a user.
    Cancelled,
    /// Lost because the runner went away.
    Interrupted,
}

impl Phase {
    /// True when no further transition is possible.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Phase::Passed | Phase::Failed { .. } | Phase::Cancelled | Phase::Interrupted
        )
    }

    /// True when moving from `self` to `next` is allowed.
    pub fn can_move_to(&self, next: &Phase) -> bool {
        match self {
            Phase::Queued | Phase::Waiting(_) => {
                matches!(next, Phase::Waiting(_) | Phase::Running | Phase::Cancelled)
            }
            Phase::Running => matches!(
                next,
                Phase::Passed | Phase::Failed { .. } | Phase::Cancelled | Phase::Interrupted
            ),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Phase> {
        vec![
            Phase::Queued,
            Phase::Waiting(WaitReason::Disk),
            Phase::Running,
            Phase::Passed,
            Phase::Failed { exit: Some(1) },
            Phase::Cancelled,
            Phase::Interrupted,
        ]
    }

    #[test]
    fn transitions_exhaustive() {
        for from in all() {
            for to in all() {
                let expected = matches!(
                    (&from, &to),
                    (
                        Phase::Queued | Phase::Waiting(_),
                        Phase::Waiting(_) | Phase::Running | Phase::Cancelled
                    ) | (
                        Phase::Running,
                        Phase::Passed
                            | Phase::Failed { .. }
                            | Phase::Cancelled
                            | Phase::Interrupted
                    )
                );
                assert_eq!(from.can_move_to(&to), expected, "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn terminal() {
        let t: Vec<bool> = all().iter().map(Phase::is_terminal).collect();
        assert_eq!(t, [false, false, false, true, true, true, true]);
    }
}
