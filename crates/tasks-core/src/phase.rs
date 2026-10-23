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
    /// Transfer slot: checkout sync work (snapshot, pack, apply). Unlike offload it obeys the
    /// I/O gate, because a sync is local disk work.
    Transfer,
}

/// Why a job is waiting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitReason {
    /// No free slot.
    Slot(SlotKind),
    /// System pressure (description).
    Pressure(String),
    /// Placement has not chosen a machine yet: waiting for a peer, a checkout sync or a
    /// forward (description). Before this variant these waits were stored as `Pressure`.
    Placement(String),
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
    /// Stopped by a wall-clock or output-idle timeout.
    TimedOut {
        /// Exit code after the stop, if any.
        exit: Option<i32>,
    },
    /// Ended before the command started, e.g. its checkout could not be synced.
    NotStarted {
        /// Why it never started.
        reason: String,
    },
    /// Cancelled by a user.
    Cancelled,
    /// Lost because the runner went away.
    Interrupted,
}

impl Phase {
    /// The phase as one lowercase word, for messages people read.
    pub fn word(&self) -> &'static str {
        match self {
            Phase::Queued => "queued",
            Phase::Waiting(_) => "waiting",
            Phase::Running => "running",
            Phase::Passed => "passed",
            Phase::Failed { .. } => "failed",
            Phase::TimedOut { .. } => "timed_out",
            Phase::NotStarted { .. } => "not_started",
            Phase::Cancelled => "cancelled",
            Phase::Interrupted => "interrupted",
        }
    }

    /// True when no further transition is possible.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Phase::Passed
                | Phase::Failed { .. }
                | Phase::TimedOut { .. }
                | Phase::NotStarted { .. }
                | Phase::Cancelled
                | Phase::Interrupted
        )
    }

    /// True when moving from `self` to `next` is allowed.
    pub fn can_move_to(&self, next: &Phase) -> bool {
        match self {
            Phase::Queued | Phase::Waiting(_) => matches!(
                next,
                Phase::Waiting(_) | Phase::Running | Phase::NotStarted { .. } | Phase::Cancelled
            ),
            Phase::Running => matches!(
                next,
                Phase::Passed
                    | Phase::Failed { .. }
                    | Phase::TimedOut { .. }
                    | Phase::Cancelled
                    | Phase::Interrupted
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
            Phase::TimedOut { exit: Some(143) },
            Phase::NotStarted {
                reason: "sync failed".into(),
            },
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
                        Phase::Waiting(_)
                            | Phase::Running
                            | Phase::NotStarted { .. }
                            | Phase::Cancelled
                    ) | (
                        Phase::Running,
                        Phase::Passed
                            | Phase::Failed { .. }
                            | Phase::TimedOut { .. }
                            | Phase::Cancelled
                            | Phase::Interrupted
                    )
                );
                assert_eq!(from.can_move_to(&to), expected, "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn placement_wait_round_trips_and_is_distinct_from_pressure() {
        let p = Phase::Waiting(WaitReason::Placement("waiting for nixos".into()));
        let back: Phase = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        assert_ne!(
            p,
            Phase::Waiting(WaitReason::Pressure("waiting for nixos".into()))
        );
        assert!(!p.is_terminal() && p.can_move_to(&Phase::Running));
    }

    #[test]
    fn terminal() {
        let t: Vec<bool> = all().iter().map(Phase::is_terminal).collect();
        assert_eq!(t, [false, false, false, true, true, true, true, true, true]);
    }
}
