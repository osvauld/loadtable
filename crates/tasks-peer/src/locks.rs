//! The per-checkout lock (R-161, R-230, R-700).
//!
//! Two jobs of the same checkout share one remote copy, so a sync must not overlap a run in
//! the copy. The lock has two modes: a job that must sync takes it EXCLUSIVE; a job that only
//! reads the copy (a test step after its plan's build, with the tree the copy already holds)
//! takes it SHARED, together with other shared holders. It obeys three rules that the old
//! checkout gate broke:
//!
//! 1. One order: waiters of a checkout are served in the order they first
//!    asked. Nothing else (job ids, note mtimes) decides. A shared request is granted with the
//!    shared requests directly ahead of it, never past an exclusive one, so a job that needs a
//!    new tree is not overtaken and cannot starve.
//! 2. Held only while the job runs on the copy: acquired immediately before
//!    the sync (or the submit, when shared), released when the job ends (or the forward is
//!    dropped).
//! 3. It is never a queue for anything else. A waiter holds no slot (callers
//!    take a slot only when [`CheckoutLocks::would_grant`] is true), and
//!    waiting on this lock never blocks another checkout or another kind of job. How many
//!    shared holders run at once is the machine's run slots' business, not this lock's.
//!
//! The lock also remembers which tree the remote copy holds ([`CopyTree`]), set after a
//! successful sync and forgotten when an exclusive holder starts (its sync may stop half-way).
//! A shared holder is only valid for a copy whose tree is the one it was snapshotted at.

use crate::ids::CheckoutId;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

/// Who holds or wants a checkout (the local job id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LockOwner(pub i64);

/// How a job holds a checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// Alone: the job syncs the copy, or may change it.
    Exclusive,
    /// With other shared holders: the job only reads the copy as it is.
    Shared,
}

/// The tree (and extras tree) a remote copy holds, as one comparable key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CopyTree(String);

impl CopyTree {
    /// The key of a working tree id and the id of its extras tree, if any.
    pub fn new(tree: &str, extras: Option<&str>) -> CopyTree {
        CopyTree(match extras {
            Some(x) => format!("{tree}+{x}"),
            None => tree.to_string(),
        })
    }

    /// The key as text, for notes.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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
    /// Current holders: one when exclusive, any number when shared.
    holders: Vec<LockOwner>,
    /// True while the holders hold it exclusively.
    exclusive: bool,
    waiters: VecDeque<(LockOwner, LockMode)>,
}

impl Entry {
    fn waiter_index(&self, owner: LockOwner) -> Option<usize> {
        self.waiters.iter().position(|(o, _)| *o == owner)
    }

    /// True when `owner` may hold the lock now: it does, or it is next in line for its mode.
    fn grantable(&self, owner: LockOwner) -> bool {
        if self.holders.contains(&owner) {
            return true;
        }
        let Some(i) = self.waiter_index(owner) else {
            return false;
        };
        let mode = self.waiters.get(i).map_or(LockMode::Exclusive, |w| w.1);
        match mode {
            LockMode::Exclusive => self.holders.is_empty() && i == 0,
            LockMode::Shared => {
                !self.exclusive
                    && self
                        .waiters
                        .iter()
                        .take(i)
                        .all(|(_, m)| *m == LockMode::Shared)
            }
        }
    }

    fn blocker(&self, owner: LockOwner) -> Option<LockOwner> {
        self.holders
            .first()
            .copied()
            .or_else(|| self.waiters.iter().map(|(o, _)| *o).find(|o| *o != owner))
    }
}

#[derive(Default)]
struct State {
    entries: HashMap<CheckoutId, Entry>,
    copies: HashMap<CheckoutId, CopyTree>,
}

/// Locks for all checkouts of this daemon.
#[derive(Default)]
pub struct CheckoutLocks {
    inner: Mutex<State>,
}

impl CheckoutLocks {
    /// No locks held.
    pub fn new() -> CheckoutLocks {
        CheckoutLocks::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes a place in the queue as an exclusive requester, without trying to acquire.
    /// Idempotent; the first call fixes the position.
    pub fn enqueue(&self, checkout: &CheckoutId, owner: LockOwner) {
        self.enqueue_as(checkout, owner, LockMode::Exclusive);
    }

    /// Like [`enqueue`](Self::enqueue), in the given mode. A second call keeps the place and
    /// the mode of the first.
    pub fn enqueue_as(&self, checkout: &CheckoutId, owner: LockOwner, mode: LockMode) {
        let mut m = self.lock();
        let e = m.entries.entry(checkout.clone()).or_default();
        if !e.holders.contains(&owner) && e.waiter_index(owner).is_none() {
            e.waiters.push_back((owner, mode));
        }
    }

    /// Makes a waiting shared requester an exclusive one, keeping its place in the line (its
    /// tree turned out not to be the copy's, so it must sync). No effect on a holder.
    pub fn make_exclusive(&self, checkout: &CheckoutId, owner: LockOwner) {
        let mut m = self.lock();
        if let Some(e) = m.entries.get_mut(checkout)
            && let Some(w) = e.waiters.iter_mut().find(|(o, _)| *o == owner)
        {
            w.1 = LockMode::Exclusive;
        }
    }

    /// Takes the lock if the caller may hold it: free, or shared with a shared requester, and
    /// first in line for its mode. A caller that never queued joins as an exclusive requester.
    pub fn try_acquire(&self, checkout: &CheckoutId, owner: LockOwner) -> Acquire {
        let mut m = self.lock();
        let State { entries, copies } = &mut *m;
        let e = entries.entry(checkout.clone()).or_default();
        if e.holders.contains(&owner) {
            return Acquire::Granted;
        }
        if e.waiter_index(owner).is_none() {
            e.waiters.push_back((owner, LockMode::Exclusive));
        }
        if !e.grantable(owner) {
            // a blocker always exists here: something holds it or waits ahead
            return Acquire::Busy {
                blocker: e.blocker(owner).unwrap_or(owner),
            };
        }
        let mode = e
            .waiter_index(owner)
            .and_then(|i| e.waiters.remove(i))
            .map_or(LockMode::Exclusive, |w| w.1);
        e.holders.push(owner);
        e.exclusive = mode == LockMode::Exclusive;
        if e.exclusive {
            copies.remove(checkout);
        }
        Acquire::Granted
    }

    /// True when [`try_acquire`](Self::try_acquire) would grant now. Does not
    /// take a place in the queue; a caller that never queued is judged as an exclusive
    /// requester at the end of the line.
    pub fn would_grant(&self, checkout: &CheckoutId, owner: LockOwner) -> bool {
        let m = self.lock();
        match m.entries.get(checkout) {
            None => true,
            Some(e) if e.holders.contains(&owner) || e.waiter_index(owner).is_some() => {
                e.grantable(owner)
            }
            Some(e) => e.holders.is_empty() && e.waiters.is_empty(),
        }
    }

    /// Gives up the lock or the place in the queue. Safe to call twice.
    pub fn release(&self, checkout: &CheckoutId, owner: LockOwner) {
        let mut m = self.lock();
        if let Some(e) = m.entries.get_mut(checkout) {
            e.holders.retain(|h| *h != owner);
            if e.holders.is_empty() {
                e.exclusive = false;
            }
            e.waiters.retain(|(w, _)| *w != owner);
            if e.holders.is_empty() && e.waiters.is_empty() {
                m.entries.remove(checkout);
            }
        }
    }

    /// A current holder of a checkout (the first, when several share it).
    pub fn holder(&self, checkout: &CheckoutId) -> Option<LockOwner> {
        self.lock()
            .entries
            .get(checkout)
            .and_then(|e| e.holders.first().copied())
    }

    /// All current holders of a checkout.
    pub fn holders(&self, checkout: &CheckoutId) -> Vec<LockOwner> {
        self.lock()
            .entries
            .get(checkout)
            .map(|e| e.holders.clone())
            .unwrap_or_default()
    }

    /// Records the tree the remote copy holds. Only the exclusive holder, which has just synced
    /// it, may say so.
    pub fn set_copy_tree(&self, checkout: &CheckoutId, owner: LockOwner, tree: CopyTree) {
        let mut m = self.lock();
        let holds = m
            .entries
            .get(checkout)
            .is_some_and(|e| e.exclusive && e.holders.contains(&owner));
        if holds {
            m.copies.insert(checkout.clone(), tree);
        }
    }

    /// The tree the remote copy is known to hold; `None` when it was never synced by this
    /// daemon, or an exclusive holder is syncing it now.
    pub fn copy_tree(&self, checkout: &CheckoutId) -> Option<CopyTree> {
        self.lock().copies.get(checkout).cloned()
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

    fn shared(l: &CheckoutLocks, co: &CheckoutId, n: i64) {
        l.enqueue_as(co, LockOwner(n), LockMode::Shared);
    }

    #[test]
    fn r700_shared_requesters_hold_the_checkout_together() {
        let l = CheckoutLocks::new();
        let co = c("/a");
        // the build holds it exclusively, then two test steps ask
        assert_eq!(l.try_acquire(&co, LockOwner(1)), Acquire::Granted);
        shared(&l, &co, 2);
        shared(&l, &co, 3);
        assert!(!l.would_grant(&co, LockOwner(2)));
        assert_eq!(
            l.try_acquire(&co, LockOwner(2)),
            Acquire::Busy {
                blocker: LockOwner(1)
            }
        );
        l.release(&co, LockOwner(1));
        assert!(l.would_grant(&co, LockOwner(2)) && l.would_grant(&co, LockOwner(3)));
        assert_eq!(l.try_acquire(&co, LockOwner(2)), Acquire::Granted);
        assert_eq!(l.try_acquire(&co, LockOwner(3)), Acquire::Granted);
        assert_eq!(l.holders(&co), vec![LockOwner(2), LockOwner(3)]);
    }

    #[test]
    fn r701_an_exclusive_requester_waits_for_every_shared_holder() {
        let l = CheckoutLocks::new();
        let co = c("/a");
        shared(&l, &co, 2);
        shared(&l, &co, 3);
        assert_eq!(l.try_acquire(&co, LockOwner(2)), Acquire::Granted);
        assert_eq!(l.try_acquire(&co, LockOwner(3)), Acquire::Granted);
        l.enqueue(&co, LockOwner(4));
        assert!(!l.would_grant(&co, LockOwner(4)));
        l.release(&co, LockOwner(2));
        assert!(
            !l.would_grant(&co, LockOwner(4)),
            "one shared holder is left"
        );
        l.release(&co, LockOwner(3));
        assert_eq!(l.try_acquire(&co, LockOwner(4)), Acquire::Granted);
        assert_eq!(l.holders(&co), vec![LockOwner(4)]);
    }

    #[test]
    fn r702_a_shared_request_behind_an_exclusive_one_is_not_granted_past_it() {
        let l = CheckoutLocks::new();
        let co = c("/a");
        shared(&l, &co, 2);
        assert_eq!(l.try_acquire(&co, LockOwner(2)), Acquire::Granted);
        l.enqueue(&co, LockOwner(4));
        shared(&l, &co, 5);
        // 5 asked after 4: a free checkout (shared) is no reason to overtake
        assert!(!l.would_grant(&co, LockOwner(5)));
        assert_eq!(
            l.try_acquire(&co, LockOwner(5)),
            Acquire::Busy {
                blocker: LockOwner(2)
            }
        );
        l.release(&co, LockOwner(2));
        assert_eq!(l.try_acquire(&co, LockOwner(4)), Acquire::Granted);
        assert!(!l.would_grant(&co, LockOwner(5)));
        l.release(&co, LockOwner(4));
        assert!(l.would_grant(&co, LockOwner(5)));
    }

    #[test]
    fn r702_a_waiter_that_leaves_the_line_lets_the_shared_ones_behind_it_go() {
        let l = CheckoutLocks::new();
        let co = c("/a");
        shared(&l, &co, 2);
        assert_eq!(l.try_acquire(&co, LockOwner(2)), Acquire::Granted);
        l.enqueue(&co, LockOwner(4));
        shared(&l, &co, 5);
        l.release(&co, LockOwner(4));
        assert!(l.would_grant(&co, LockOwner(5)));
    }

    #[test]
    fn r703_the_copy_tree_is_set_by_the_exclusive_holder_and_forgotten_when_one_starts() {
        let l = CheckoutLocks::new();
        let co = c("/a");
        let t = CopyTree::new("aaa", Some("xxx"));
        assert_eq!(l.copy_tree(&co), None);
        assert_eq!(l.try_acquire(&co, LockOwner(1)), Acquire::Granted);
        l.set_copy_tree(&co, LockOwner(1), t.clone());
        l.release(&co, LockOwner(1));
        assert_eq!(l.copy_tree(&co), Some(t.clone()), "kept between jobs");
        // a shared holder cannot change what the copy holds
        shared(&l, &co, 2);
        assert_eq!(l.try_acquire(&co, LockOwner(2)), Acquire::Granted);
        l.set_copy_tree(&co, LockOwner(2), CopyTree::new("bbb", None));
        assert_eq!(l.copy_tree(&co), Some(t));
        l.release(&co, LockOwner(2));
        // an exclusive holder may sync half-way: the copy is unknown until it says
        assert_eq!(l.try_acquire(&co, LockOwner(3)), Acquire::Granted);
        assert_eq!(l.copy_tree(&co), None);
    }

    #[test]
    fn r703_a_shared_request_that_must_sync_keeps_its_place_as_exclusive() {
        let l = CheckoutLocks::new();
        let co = c("/a");
        shared(&l, &co, 2);
        shared(&l, &co, 3);
        l.make_exclusive(&co, LockOwner(2));
        assert_eq!(l.try_acquire(&co, LockOwner(2)), Acquire::Granted);
        assert_eq!(
            l.try_acquire(&co, LockOwner(3)),
            Acquire::Busy {
                blocker: LockOwner(2)
            }
        );
    }
}
