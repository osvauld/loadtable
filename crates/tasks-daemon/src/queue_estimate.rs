//! Start estimates for queued jobs (R-129): when would each one get a slot?
//!
//! The model is `slots` identical servers. Each running job frees its server after its
//! remaining time; each queued job, in queue order, takes the server that frees first and
//! holds it for its own estimated runtime.

use std::collections::BTreeMap;
use tasks_admission::{Waiter, sequence};
use tasks_core::JobId;

/// A queued job as the estimate sees it: the waiter admission would order, and how long it runs.
#[derive(Debug, Clone)]
pub struct Queued {
    /// Class, owner and submit time; the same values admission orders by.
    pub waiter: Waiter,
    /// Its estimated runtime, seconds.
    pub runtime_secs: f64,
}

/// Seconds until each queued job starts, listed in the order admission will take them: by class,
/// effective wait and submit time, owners in turn ([`sequence`]), with the same clock and the
/// same last-start times. The list position is the job's queue position.
pub fn starts_in_queue_order(
    slots: usize,
    running_remaining: &[f64],
    queued: &[Queued],
    now_ms: u64,
    aging_ms: u64,
    last_started: &BTreeMap<String, u64>,
) -> Vec<(JobId, Option<f64>)> {
    let waiters: Vec<Waiter> = queued.iter().map(|q| q.waiter.clone()).collect();
    let order = sequence(&waiters, now_ms, aging_ms, last_started);
    let runtimes: Vec<f64> = order
        .iter()
        .filter_map(|i| queued.get(*i))
        .map(|q| q.runtime_secs)
        .collect();
    let starts = start_offsets(slots, running_remaining, &runtimes);
    order
        .iter()
        .filter_map(|i| queued.get(*i))
        .zip(starts)
        .map(|(q, start)| (q.waiter.job, start))
        .collect()
}

/// Seconds until each queued job starts, in queue order. `None` for every job when the machine
/// has no slot of that kind. `running_remaining` has one entry per running job of the kind
/// (the short lane may make that more than `slots`; the extra jobs still count).
pub fn start_offsets(
    slots: usize,
    running_remaining: &[f64],
    queued_runtimes: &[f64],
) -> Vec<Option<f64>> {
    if slots == 0 {
        return vec![None; queued_runtimes.len()];
    }
    let clean = |v: f64| if v.is_finite() { v.max(0.0) } else { 0.0 };
    let mut free_at: Vec<f64> = running_remaining.iter().map(|r| clean(*r)).collect();
    free_at.sort_by(f64::total_cmp);
    // With more runners than slots only the latest `slots` finish times matter.
    if free_at.len() > slots {
        let drop = free_at.len() - slots;
        free_at.drain(..drop);
    }
    while free_at.len() < slots {
        free_at.insert(0, 0.0);
    }
    queued_runtimes
        .iter()
        .map(|runtime| {
            free_at.sort_by(f64::total_cmp);
            let start = free_at.first().copied().unwrap_or(0.0);
            if let Some(first) = free_at.first_mut() {
                *first = start + clean(*runtime);
            }
            Some(start)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_core::{Priority, SlotKind};

    const MIN: u64 = 60_000;

    fn q(job: i64, owner: &str, class: Priority, submitted: u64, runtime: f64) -> Queued {
        let mut waiter = Waiter::new(JobId(job), SlotKind::Compile, submitted);
        waiter.owner = owner.into();
        waiter.priority = class;
        Queued {
            waiter,
            runtime_secs: runtime,
        }
    }

    fn starts(
        slots: usize,
        running: &[f64],
        queued: &[Queued],
        now: u64,
    ) -> Vec<(i64, Option<f64>)> {
        starts_in_queue_order(slots, running, queued, now, 20 * MIN, &BTreeMap::new())
            .into_iter()
            .map(|(j, s)| (j.0, s))
            .collect()
    }

    // R-347
    #[test]
    fn r347_estimates_follow_the_class_order_not_the_submit_order() {
        // one slot, 30 s left. Submitted in the order 1, 2, 3; served 3, 2, 1.
        let queued = [
            q(1, "a", Priority::Background, 1, 10.0),
            q(2, "a", Priority::Normal, 2, 20.0),
            q(3, "a", Priority::Blocking, 3, 5.0),
        ];
        assert_eq!(
            starts(1, &[30.0], &queued, 100),
            vec![(3, Some(30.0)), (2, Some(35.0)), (1, Some(55.0))]
        );
    }

    #[test]
    fn r347_estimates_follow_the_owners_turns() {
        // a's three jobs were submitted first; b's one takes the second turn, not the fourth
        let queued = [
            q(1, "a", Priority::Normal, 1, 10.0),
            q(2, "a", Priority::Normal, 2, 10.0),
            q(3, "a", Priority::Normal, 3, 10.0),
            q(4, "b", Priority::Normal, 4, 10.0),
        ];
        let got = starts(1, &[0.0], &queued, 100);
        assert_eq!(
            got,
            vec![
                (1, Some(0.0)),
                (4, Some(10.0)),
                (2, Some(20.0)),
                (3, Some(30.0))
            ]
        );
    }

    #[test]
    fn r347_an_aged_background_job_is_estimated_where_aging_puts_it() {
        let queued = [
            q(1, "a", Priority::Background, 0, 10.0),
            q(2, "b", Priority::Normal, 25 * MIN, 10.0),
        ];
        // before aging: normal first
        let before: Vec<i64> = starts(1, &[], &queued, 10 * MIN)
            .iter()
            .map(|x| x.0)
            .collect();
        assert_eq!(before, [2, 1]);
        // after the wait it rose to normal at minute 20, ahead of the job submitted at 25
        let after: Vec<i64> = starts(1, &[], &queued, 26 * MIN)
            .iter()
            .map(|x| x.0)
            .collect();
        assert_eq!(after, [1, 2]);
    }

    // R-129
    #[test]
    fn r129_queue_start_math_single_and_multi_slot() {
        // one slot, 30 s left on the runner
        assert_eq!(
            start_offsets(1, &[30.0], &[10.0, 20.0, 5.0]),
            vec![Some(30.0), Some(40.0), Some(60.0)]
        );
        // two slots: one frees at 5 s, one at 30 s
        assert_eq!(
            start_offsets(2, &[30.0, 5.0], &[10.0, 10.0, 10.0]),
            vec![Some(5.0), Some(15.0), Some(25.0)]
        );
        // free slots start at once
        assert_eq!(
            start_offsets(3, &[], &[10.0, 10.0, 10.0, 10.0]),
            vec![Some(0.0), Some(0.0), Some(0.0), Some(10.0)]
        );
        // no slot of that kind on this machine
        assert_eq!(start_offsets(0, &[], &[1.0]), vec![None]);
        // junk estimates cannot poison the math
        assert_eq!(
            start_offsets(1, &[f64::NAN], &[f64::INFINITY, 1.0]),
            vec![Some(0.0), Some(0.0)]
        );
    }

    #[test]
    fn more_runners_than_slots_wait_for_the_latest_ones() {
        // a short-lane job runs beside two normal ones on a 2-slot kind
        assert_eq!(
            start_offsets(2, &[10.0, 20.0, 3.0], &[1.0]),
            vec![Some(10.0)]
        );
    }
}
