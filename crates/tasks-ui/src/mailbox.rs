//! A latest-only hand-over between a feed thread and the window (R-200).
//!
//! The feed [`Mailbox::put`]s snapshots as fast as it likes; the window [`Mailbox::take`]s
//! when it draws. A snapshot nobody took is replaced by the next one, so a hidden (undrained)
//! window holds exactly one snapshot, never a growing queue.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

struct State<T> {
    value: Option<T>,
    /// Snapshots put so far.
    put: u64,
    /// Snapshots replaced before anyone took them.
    dropped: u64,
}

/// One slot holding the newest value.
pub struct Mailbox<T> {
    state: Mutex<State<T>>,
    taken: Condvar,
}

impl<T> Default for Mailbox<T> {
    fn default() -> Self {
        Mailbox::new()
    }
}

impl<T> Mailbox<T> {
    /// An empty mailbox.
    pub fn new() -> Mailbox<T> {
        Mailbox {
            state: Mutex::new(State {
                value: None,
                put: 0,
                dropped: 0,
            }),
            taken: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Stores `value`, replacing a value nobody took.
    pub fn put(&self, value: T) {
        let mut s = self.lock();
        if s.value.replace(value).is_some() {
            s.dropped += 1;
        }
        s.put += 1;
    }

    /// Takes the newest value, if there is one, and wakes [`Mailbox::wait_taken`].
    pub fn take(&self) -> Option<T> {
        let mut s = self.lock();
        let v = s.value.take();
        if v.is_some() {
            self.taken.notify_all();
        }
        v
    }

    /// True when a value waits to be taken.
    pub fn is_full(&self) -> bool {
        self.lock().value.is_some()
    }

    /// How many values were replaced before they were taken.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// How many values were put in all.
    pub fn puts(&self) -> u64 {
        self.lock().put
    }

    /// Blocks until the waiting value has been taken, at most `timeout`. True when the
    /// mailbox is empty on return.
    pub fn wait_taken(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut s = self.lock();
        while s.value.is_some() {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            s = match self.taken.wait_timeout(s, deadline - now) {
                Ok((g, _)) => g,
                Err(p) => p.into_inner().0,
            };
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn r200_an_undrained_mailbox_keeps_only_the_newest_snapshot() {
        let m = Mailbox::new();
        for i in 0..10_000 {
            m.put(i);
        }
        assert_eq!(m.puts(), 10_000);
        assert_eq!(m.dropped(), 9_999);
        assert_eq!(m.take(), Some(9_999));
        assert_eq!(m.take(), None);
    }

    #[test]
    fn r200_coalesced_updates_end_in_the_same_state_as_the_full_sequence() {
        // A snapshot replaces the whole state, so a window that skipped some ends equal.
        let sequence = [3, 1, 4, 1, 5, 9, 2, 6];
        let full = sequence.iter().fold(0, |_, v| *v);
        let m = Mailbox::new();
        let mut shown = 0;
        for (i, v) in sequence.iter().enumerate() {
            m.put(*v);
            if i % 3 == 0 {
                shown = m.take().unwrap_or(shown);
            }
        }
        shown = m.take().unwrap_or(shown);
        assert_eq!(shown, full);
    }

    #[test]
    fn r200_wait_taken_blocks_until_the_window_takes_the_snapshot() {
        let m = Arc::new(Mailbox::new());
        m.put("snapshot");
        assert!(!m.wait_taken(Duration::from_millis(20)), "nobody took it");
        let reader = {
            let m = Arc::clone(&m);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                m.take()
            })
        };
        assert!(m.wait_taken(Duration::from_secs(5)));
        assert_eq!(reader.join().ok().flatten(), Some("snapshot"));
        assert!(m.wait_taken(Duration::ZERO), "empty returns at once");
    }
}
