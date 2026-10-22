//! Waiters and decisions.

use crate::{Thresholds, WaitReason};
use tasks_core::{DeviceId, JobId, SlotKind};

/// Queue priority. `Urgent` is declared first so it sorts first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Claimed before every normal job (R-040).
    Urgent,
    /// Ordinary submission order.
    Normal,
}

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
    /// Queue priority.
    pub priority: Priority,
    /// First enqueue time; kept across re-attempts (R-051).
    pub queued_at_ms: u64,
    /// Predicted runtime from history; `None` means never short.
    pub predicted_secs: Option<f64>,
    /// Checkout it builds; at most one job per checkout runs, in queue order.
    pub checkout: Option<String>,
    /// Device it needs (device slots only).
    pub device: Option<DeviceId>,
    /// True when it creates new output (or fetches) on the cache filesystem (R-061, R-065).
    pub new_output: bool,
    /// Extra holds; non-empty means skipped.
    pub holds: Vec<Hold>,
}

impl Waiter {
    /// A normal-priority waiter with no prediction, checkout, device or holds.
    pub fn new(job: JobId, kind: SlotKind, queued_at_ms: u64) -> Waiter {
        Waiter {
            job,
            kind,
            priority: Priority::Normal,
            queued_at_ms,
            predicted_secs: None,
            checkout: None,
            device: None,
            new_output: false,
            holds: Vec::new(),
        }
    }

    /// The one queue order, used by every rule.
    pub fn sort_key(&self) -> (Priority, u64, JobId) {
        (self.priority, self.queued_at_ms, self.job)
    }

    /// True when the prediction fits the short lane. No prediction means not short.
    pub fn is_short(&self, t: &Thresholds) -> bool {
        t.short_job_lane_slots > 0
            && matches!(self.kind, SlotKind::Compile | SlotKind::Run)
            && self
                .predicted_secs
                .is_some_and(|p| p <= t.short_job_max_secs)
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
    fn order_is_priority_then_queue_time_then_id() {
        let mut a = Waiter::new(JobId(5), SlotKind::Run, 10);
        let b = Waiter::new(JobId(4), SlotKind::Run, 20);
        let c = Waiter::new(JobId(3), SlotKind::Run, 10);
        a.priority = Priority::Urgent;
        let mut v = [b.clone(), c.clone(), a.clone()];
        v.sort_by_key(Waiter::sort_key);
        assert_eq!(v.iter().map(|w| w.job.0).collect::<Vec<_>>(), [5, 3, 4]);
    }

    #[test]
    fn short_needs_a_prediction_within_limit() {
        let t = thresholds();
        let mut w = Waiter::new(JobId(1), SlotKind::Compile, 0);
        assert!(!w.is_short(&t));
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
}
