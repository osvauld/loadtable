//! Waiters and decisions.

use crate::{Thresholds, WaitReason};
use tasks_core::{DeviceId, JobId, Priority, SlotKind};

/// An extra reason a waiter cannot start. A held waiter is skipped, never blocking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// A running job already uses this checkout.
    SameCheckoutBusy(JobId),
    /// Waiting for another job to finish.
    Dependency(JobId),
    /// The operator is draining the queue.
    Drain,
    /// Any other reason, in words.
    Other(String),
}

/// A job waiting for admission.
#[derive(Debug, Clone, PartialEq)]
pub struct Waiter {
    /// The job.
    pub job: JobId,
    /// Slot kind it needs.
    pub kind: SlotKind,
    /// Who submitted it; owners take turns inside a class ([`crate::sequence`]).
    pub owner: String,
    /// The class the job was submitted in; aging may lift it (see [`crate::effective`]).
    pub priority: Priority,
    /// First enqueue time; kept across re-attempts (R-051).
    pub queued_at_ms: u64,
    /// Predicted runtime from history; `None` means never short.
    pub predicted_secs: Option<f64>,
    /// Predicted peak memory from history, bytes; `None` means never short.
    pub predicted_peak_memory: Option<u64>,
    /// Checkout it builds; at most one job per checkout runs, in queue order.
    pub checkout: Option<String>,
    /// Device it needs (device slots only).
    pub device: Option<DeviceId>,
    /// True when it creates new output (or fetches) on the cache filesystem (R-061, R-065).
    pub new_output: bool,
    /// Extra holds; non-empty means skipped.
    pub holds: Vec<Hold>,
    /// Runs only when nothing else runs on the machine, and nothing starts beside it (R-712).
    pub isolate: bool,
}

/// A job running on this machine now, as far as isolation is concerned (R-712).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Running {
    /// The job.
    pub job: JobId,
    /// It was started with `isolate`.
    pub isolate: bool,
}

impl Waiter {
    /// A normal-priority waiter with no prediction, checkout, device or holds.
    pub fn new(job: JobId, kind: SlotKind, queued_at_ms: u64) -> Waiter {
        Waiter {
            job,
            kind,
            owner: String::new(),
            priority: Priority::Normal,
            queued_at_ms,
            predicted_secs: None,
            predicted_peak_memory: None,
            checkout: None,
            device: None,
            new_output: false,
            holds: Vec::new(),
            isolate: false,
        }
    }

    /// True when history says the job is short and small: predicted duration within the limit
    /// and predicted peak memory within the cap. Either one unknown means not short; nothing
    /// the submitter says reaches this (design §10).
    pub fn is_short(&self, t: &Thresholds) -> bool {
        t.short_job_lane_slots > 0
            && matches!(self.kind, SlotKind::Compile | SlotKind::Run)
            && self
                .predicted_secs
                .is_some_and(|p| p <= t.short_job_max_secs)
            && self
                .predicted_peak_memory
                .is_some_and(|m| m <= t.short_job_max_memory_bytes)
    }
}

/// Which lane an admitted job uses; recorded so it is auditable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The ordinary slots.
    Normal,
    /// The small short-job lane.
    Short,
}

/// Outcome for one waiter.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Start now in this slot.
    Admit {
        /// Slot kind and index.
        slot: (SlotKind, u32),
        /// Lane used.
        lane: Lane,
    },
    /// Keep waiting, for this reason.
    Wait(WaitReason),
}

/// A verdict for a job.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// The job decided.
    pub job: JobId,
    /// What happens to it.
    pub verdict: Verdict,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::thresholds;

    #[test]
    fn r342_short_needs_a_prediction_within_limit() {
        let t = thresholds();
        let mut w = Waiter::new(JobId(1), SlotKind::Compile, 0);
        assert!(!w.is_short(&t));
        w.predicted_peak_memory = Some(t.short_job_max_memory_bytes);
        assert!(!w.is_short(&t), "duration unknown");
        w.predicted_secs = Some(t.short_job_max_secs);
        assert!(w.is_short(&t));
        w.predicted_secs = Some(f64::NAN);
        assert!(!w.is_short(&t));
        w.predicted_secs = Some(t.short_job_max_secs + 1.0);
        assert!(!w.is_short(&t));
        w.predicted_secs = Some(1.0);
        w.kind = SlotKind::Offload;
        assert!(!w.is_short(&t));
    }

    #[test]
    fn r342_short_needs_a_known_peak_memory_within_the_cap() {
        let t = thresholds();
        let mut w = Waiter::new(JobId(1), SlotKind::Run, 0);
        w.predicted_secs = Some(5.0);
        assert!(!w.is_short(&t), "memory unknown is not short");
        w.predicted_peak_memory = Some(t.short_job_max_memory_bytes);
        assert!(w.is_short(&t));
        w.predicted_peak_memory = Some(t.short_job_max_memory_bytes + 1);
        assert!(!w.is_short(&t), "over the memory cap");
    }
}
