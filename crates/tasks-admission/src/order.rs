//! The queue order (design §10): `(class, effective wait, submit time)`, taken in turns by
//! owner. Pure: the clock and the owners' last start times are passed in.

use crate::Waiter;
use std::collections::BTreeMap;
use tasks_core::{JobId, Priority};

/// A waiter's class after aging, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective {
    /// The class it is ordered by now.
    pub class: Priority,
    /// When it entered that class: its submit time, plus one aging period per step it rose.
    /// "Effective wait" is `now - since_ms`; the longer it is, the earlier the job is served.
    pub since_ms: u64,
    /// How many classes aging has lifted it (0 for a job still in its own class).
    pub lifted: u32,
}

impl Effective {
    /// True when aging, not the submitter, put the job in this class.
    pub fn is_aged(&self) -> bool {
        self.lifted > 0
    }
}

/// The class `w` is ordered by at `now_ms`. After every `aging_ms` of waiting a job rises one
/// class, where [`Priority::aged`] allows it (background to normal; never to blocking).
/// `aging_ms == 0` turns aging off.
pub fn effective(w: &Waiter, now_ms: u64, aging_ms: u64) -> Effective {
    let mut e = Effective {
        class: w.priority,
        since_ms: w.queued_at_ms,
        lifted: 0,
    };
    if aging_ms == 0 {
        return e;
    }
    let periods = now_ms.saturating_sub(w.queued_at_ms) / aging_ms;
    for _ in 0..periods {
        let Some(next) = e.class.aged() else {
            break;
        };
        e.class = next;
        e.lifted += 1;
        e.since_ms = w
            .queued_at_ms
            .saturating_add(aging_ms * u64::from(e.lifted));
    }
    e
}

/// Where an owner stands in a class's rotation. Lowest goes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Turn {
    /// Has not started a job since the daemon began watching.
    Never,
    /// Started its latest job at this time, ms.
    Started(u64),
    /// Has been given the nth place in this pass already.
    Given(u32),
}

/// The order in which `decide` walks the waiters, as indices into `waiters`.
///
/// Classes go in order (blocking, normal, background; each waiter's class is its
/// [`effective`] one). Inside a class the owners take turns: the owner that started a job least
/// recently goes first (an owner that never started goes before all), and gets its oldest
/// waiter (by `since_ms`, then submit time, then job id); then the owner that started least
/// recently among the rest, and so on, round after round. Ties between owners go to the one
/// whose next waiter is older, then to the owner name. So one owner's thirty jobs take every
/// other turn at most, never the whole queue. A waiter that is held (checkout busy, drain)
/// keeps its place and its owner's turn, because every other rule works on this order.
pub fn sequence(
    waiters: &[Waiter],
    now_ms: u64,
    aging_ms: u64,
    last_started: &BTreeMap<String, u64>,
) -> Vec<usize> {
    let mut classes: BTreeMap<Priority, BTreeMap<&str, Vec<(Effective, usize)>>> = BTreeMap::new();
    for (i, w) in waiters.iter().enumerate() {
        let e = effective(w, now_ms, aging_ms);
        classes
            .entry(e.class)
            .or_default()
            .entry(w.owner.as_str())
            .or_default()
            .push((e, i));
    }
    let key = |(e, i): &(Effective, usize)| {
        let w = &waiters[*i];
        (e.since_ms, w.queued_at_ms, w.job)
    };
    let mut out = Vec::with_capacity(waiters.len());
    for (_, owners) in classes {
        let mut queues: Vec<(&str, Turn, std::collections::VecDeque<(Effective, usize)>)> = owners
            .into_iter()
            .map(|(owner, mut v)| {
                v.sort_by_key(key);
                let turn = last_started
                    .get(owner)
                    .map_or(Turn::Never, |t| Turn::Started(*t));
                (owner, turn, v.into())
            })
            .collect();
        let mut given = 0u32;
        loop {
            let head = |q: &std::collections::VecDeque<(Effective, usize)>| {
                q.front()
                    .map(&key)
                    .unwrap_or((u64::MAX, u64::MAX, JobId(i64::MAX)))
            };
            let Some(pick) = queues
                .iter_mut()
                .filter(|(_, _, q)| !q.is_empty())
                .min_by(|a, b| {
                    a.1.cmp(&b.1)
                        .then_with(|| head(&a.2).cmp(&head(&b.2)))
                        .then_with(|| a.0.cmp(b.0))
                })
            else {
                break;
            };
            if let Some((_, i)) = pick.2.pop_front() {
                out.push(i);
            }
            given += 1;
            pick.1 = Turn::Given(given);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_core::SlotKind;

    const MIN: u64 = 60_000;

    fn w(job: i64, owner: &str, class: Priority, queued: u64) -> Waiter {
        let mut w = Waiter::new(JobId(job), SlotKind::Compile, queued);
        w.owner = owner.to_string();
        w.priority = class;
        w
    }

    fn jobs(ws: &[Waiter], order: &[usize]) -> Vec<i64> {
        order.iter().map(|i| ws[*i].job.0).collect()
    }

    fn seq(ws: &[Waiter], now: u64, last: &[(&str, u64)]) -> Vec<i64> {
        let last = last.iter().map(|(o, t)| (o.to_string(), *t)).collect();
        jobs(ws, &sequence(ws, now, 20 * MIN, &last))
    }

    #[test]
    fn r338_blocking_goes_before_normal_before_background() {
        let ws = [
            w(1, "a", Priority::Background, 0),
            w(2, "a", Priority::Normal, 10),
            w(3, "a", Priority::Blocking, 20),
            w(4, "a", Priority::Normal, 5),
        ];
        assert_eq!(seq(&ws, 30, &[]), [3, 4, 2, 1]);
    }

    #[test]
    fn r338_inside_a_class_the_older_submission_goes_first_then_the_lower_id() {
        let ws = [
            w(7, "a", Priority::Normal, 100),
            w(5, "a", Priority::Normal, 100),
            w(6, "a", Priority::Normal, 50),
        ];
        assert_eq!(seq(&ws, 200, &[]), [6, 5, 7]);
    }

    #[test]
    fn r340_background_rises_to_normal_after_the_aging_wait_and_takes_its_place_by_age() {
        let bg = w(1, "a", Priority::Background, 0);
        // not yet
        let e = effective(&bg, 20 * MIN - 1, 20 * MIN);
        assert_eq!(
            (e.class, e.lifted, e.is_aged()),
            (Priority::Background, 0, false)
        );
        // exactly the wait: normal, entered at 20 min
        let e = effective(&bg, 20 * MIN, 20 * MIN);
        assert_eq!(
            (e.class, e.lifted, e.since_ms),
            (Priority::Normal, 1, 20 * MIN)
        );
        // however long it waits it stops at normal, entered when it rose
        let e = effective(&bg, 500 * MIN, 20 * MIN);
        assert_eq!(
            (e.class, e.lifted, e.since_ms),
            (Priority::Normal, 1, 20 * MIN)
        );
        // aging off
        assert_eq!(effective(&bg, 500 * MIN, 0).class, Priority::Background);
        // normal and blocking do not rise
        let n = w(2, "a", Priority::Normal, 0);
        assert_eq!(effective(&n, 500 * MIN, 20 * MIN).class, Priority::Normal);
        let b = w(3, "a", Priority::Blocking, 0);
        assert_eq!(effective(&b, 500 * MIN, 20 * MIN).class, Priority::Blocking);
    }

    #[test]
    fn r340_an_aged_job_queues_among_normal_jobs_by_when_it_rose() {
        let ws = [
            w(1, "a", Priority::Background, 0),    // rises at 20 min
            w(2, "b", Priority::Normal, 10 * MIN), // before it rose
            w(3, "c", Priority::Normal, 25 * MIN), // after it rose
        ];
        // at 30 min: 2 (submitted at 10), then 1 (entered normal at 20), then 3 (25)
        assert_eq!(seq(&ws, 30 * MIN, &[]), [2, 1, 3]);
        // at 19 min it is still behind everything normal
        assert_eq!(seq(&ws[..2], 19 * MIN, &[]), [2, 1]);
    }

    #[test]
    fn r341_owners_take_turns_so_thirty_jobs_do_not_starve_one() {
        let mut ws: Vec<Waiter> = (1..=30)
            .map(|i| w(i, "busy", Priority::Normal, i as u64))
            .collect();
        ws.push(w(100, "other", Priority::Normal, 1000));
        let order = seq(&ws, 2000, &[]);
        // "busy" has the older head, so it opens; "other" is second, not thirty-first
        assert_eq!(&order[..4], [1, 100, 2, 3]);
        assert_eq!(order.len(), 31);
    }

    #[test]
    fn r341_the_owner_that_started_least_recently_goes_first() {
        let ws = [
            w(1, "a", Priority::Normal, 10),
            w(2, "b", Priority::Normal, 20),
            w(3, "c", Priority::Normal, 30),
        ];
        // a started a second ago, b a minute ago, c never
        let order = seq(&ws, 100_000, &[("a", 99_000), ("b", 40_000)]);
        assert_eq!(order, [3, 2, 1]);
        // equal start times fall back to the older waiting job
        assert_eq!(
            seq(&ws, 100_000, &[("a", 5), ("b", 5), ("c", 5)]),
            [1, 2, 3]
        );
    }

    #[test]
    fn r341_rounds_alternate_between_owners_with_several_jobs_each() {
        let ws = [
            w(1, "a", Priority::Normal, 1),
            w(2, "a", Priority::Normal, 2),
            w(3, "b", Priority::Normal, 3),
            w(4, "b", Priority::Normal, 4),
            w(5, "c", Priority::Normal, 5),
        ];
        assert_eq!(seq(&ws, 100, &[]), [1, 3, 5, 2, 4]);
    }

    #[test]
    fn r341_turns_do_not_cross_classes() {
        // a blocking job of "a" does not make a's normal job wait behind b's: classes are strict
        let ws = [
            w(1, "a", Priority::Blocking, 1),
            w(2, "a", Priority::Normal, 2),
            w(3, "b", Priority::Normal, 3),
        ];
        assert_eq!(seq(&ws, 100, &[]), [1, 2, 3]);
    }

    #[test]
    fn the_order_is_the_same_whatever_order_the_waiters_arrive_in() {
        let ws = vec![
            w(1, "a", Priority::Normal, 1),
            w(2, "a", Priority::Background, 2),
            w(3, "b", Priority::Normal, 3),
            w(4, "b", Priority::Blocking, 4),
            w(5, "c", Priority::Normal, 5),
            w(6, "c", Priority::Background, 6),
        ];
        let want = seq(&ws, 100, &[("a", 50)]);
        let mut rev = ws.clone();
        rev.reverse();
        assert_eq!(seq(&rev, 100, &[("a", 50)]), want);
        assert_eq!(want.len(), 6);
    }
}
