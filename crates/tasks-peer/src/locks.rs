//! The per-checkout lock (R-161, R-230).
//!
//! Two jobs of the same checkout share one remote copy, so their syncs and
//! runs must not overlap. This lock is the only ordering between them, and it
//! obeys three rules that the old checkout gate broke:
//!
//! 1. One order: waiters of a checkout are served in the order they first
//!    asked. Nothing else (job ids, note mtimes) decides.
//! 2. Held only while the job runs on the copy: acquired immediately before
//!    the sync, released when the job ends (or the forward is dropped).
//! 3. It is never a queue for anything else. A waiter holds no slot (callers
//!    take a slot only when [`CheckoutLocks::would_grant`] is true), and
//!    waiting on this lock never blocks another checkout or another kind of job.

use crate::ids::CheckoutId;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

/// Who holds or wants a checkout (the local job id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LockOwner(pub i64);

/// The answer to an acquire attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquire {
    /// The caller now holds the lock.
    Granted,
    /// Not yet. `blocker` is the holder, or the waiter ahead, to name in the wait reason.
    Busy {
        /// Who the caller is waiting for.
        blocker: LockOwner,
    },
}

#[derive(Default)]
struct Entry {
    holder: Option<LockOwner>,
    waiters: VecDeque<LockOwner>,
}

/// Locks for all checkouts of this daemon.
#[derive(Default)]
pub struct CheckoutLocks {
    inner: Mutex<HashMap<CheckoutId, Entry>>,
}

impl CheckoutLocks {
    /// No locks held.
    pub fn new() -> CheckoutLocks {
        CheckoutLocks::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<CheckoutId, Entry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes a place in the queue without trying to acquire. Idempotent; the
    /// first call fixes the position.
    pub fn enqueue(&self, checkout: &CheckoutId, owner: LockOwner) {
        let mut m = self.lock();
        let e = m.entry(checkout.clone()).or_default();
        if e.holder != Some(owner) && !e.waiters.contains(&owner) {
            e.waiters.push_back(owner);
        }
    }

    /// Takes the lock if it is free and the caller is first in line.
    pub fn try_acquire(&self, checkout: &CheckoutId, owner: LockOwner) -> Acquire {
        let mut m = self.lock();
        let e = m.entry(checkout.clone()).or_default();
        if e.holder == Some(owner) {
            return Acquire::Granted;
        }
        if !e.waiters.contains(&owner) {
            e.waiters.push_back(owner);
        }
        if let Some(h) = e.holder {
            return Acquire::Busy { blocker: h };
        }
        match e.waiters.front().copied() {
            Some(first) if first == owner => {
                e.waiters.pop_front();
                e.holder = Some(owner);
                Acquire::Granted
            }
            Some(first) => Acquire::Busy { blocker: first },
            None => Acquire::Granted,
        }
    }

    /// True when [`try_acquire`](Self::try_acquire) would grant now. Does not
    /// take a place in the queue.
    pub fn would_grant(&self, checkout: &CheckoutId, owner: LockOwner) -> bool {
        let m = self.lock();
        match m.get(checkout) {
            None => true,
            Some(e) => {
                e.holder == Some(owner)
                    || (e.holder.is_none() && e.waiters.front().is_none_or(|first| *first == owner))
            }
        }
    }

    /// Gives up the lock or the place in the queue. Safe to call twice.
    pub fn release(&self, checkout: &CheckoutId, owner: LockOwner) {
        let mut m = self.lock();
        if let Some(e) = m.get_mut(checkout) {
            if e.holder == Some(owner) {
                e.holder = None;
            }
            e.waiters.retain(|w| *w != owner);
            if e.holder.is_none() && e.waiters.is_empty() {
                m.remove(checkout);
            }
        }
    }

    /// The current holder of a checkout.
    pub fn holder(&self, checkout: &CheckoutId) -> Option<LockOwner> {
        self.lock().get(checkout).and_then(|e| e.holder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn c(n: &str) -> CheckoutId {
        CheckoutId::derive("h", Path::new(n))
    }

    #[test]
    fn r161_second_job_waits_and_names_the_holder() {
        let l = CheckoutLocks::new();
        assert_eq!(l.try_acquire(&c("/a"), LockOwner(1)), Acquire::Granted);
        assert_eq!(
            l.try_acquire(&c("/a"), LockOwner(2)),
            Acquire::Busy {
                blocker: LockOwner(1)
            }
        );
        l.release(&c("/a"), LockOwner(1));
        assert_eq!(l.try_acquire(&c("/a"), LockOwner(2)), Acquire::Granted);
    }

    #[test]
    fn r161_earlier_waiter_goes_first_whatever_the_job_ids() {
        let l = CheckoutLocks::new();
        assert_eq!(l.try_acquire(&c("/a"), LockOwner(9)), Acquire::Granted);
        // 7 asks before 3; id order must not matter.
        l.enqueue(&c("/a"), LockOwner(7));
        l.enqueue(&c("/a"), LockOwner(3));
        l.release(&c("/a"), LockOwner(9));
        assert!(!l.would_grant(&c("/a"), LockOwner(3)));
        assert!(l.would_grant(&c("/a"), LockOwner(7)));
        assert_eq!(
            l.try_acquire(&c("/a"), LockOwner(3)),
            Acquire::Busy {
                blocker: LockOwner(7)
            }
        );
        assert_eq!(l.try_acquire(&c("/a"), LockOwner(7)), Acquire::Granted);
    }

    #[test]
    fn r230_other_checkouts_are_independent() {
        let l = CheckoutLocks::new();
        assert_eq!(l.try_acquire(&c("/a"), LockOwner(1)), Acquire::Granted);
        l.enqueue(&c("/a"), LockOwner(2));
        assert!(l.would_grant(&c("/b"), LockOwner(3)));
        assert_eq!(l.try_acquire(&c("/b"), LockOwner(3)), Acquire::Granted);
    }

    #[test]
    fn a_cancelled_waiter_leaves_the_line() {
        let l = CheckoutLocks::new();
        assert_eq!(l.try_acquire(&c("/a"), LockOwner(1)), Acquire::Granted);
        l.enqueue(&c("/a"), LockOwner(2));
        l.enqueue(&c("/a"), LockOwner(3));
        l.release(&c("/a"), LockOwner(2));
        l.release(&c("/a"), LockOwner(2));
        l.release(&c("/a"), LockOwner(1));
        assert!(l.would_grant(&c("/a"), LockOwner(3)));
        assert_eq!(l.holder(&c("/a")), None);
    }
}
