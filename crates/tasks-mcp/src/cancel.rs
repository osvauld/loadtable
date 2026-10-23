//! A cancellation flag that also wakes sleepers.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

/// Set by `notifications/cancelled`; a waiting tool sleeps on it so it stops at once.
#[derive(Debug, Default)]
pub struct CancelToken {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl CancelToken {
    /// A fresh, unset token.
    pub fn new() -> Arc<CancelToken> {
        Arc::new(CancelToken::default())
    }

    /// Marks the request cancelled and wakes its sleeper.
    pub fn cancel(&self) {
        *self.flag.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.cv.notify_all();
    }

    /// Whether the request was cancelled.
    pub fn is_cancelled(&self) -> bool {
        *self.flag.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sleeps up to `dur`, or until cancelled. Returns true when cancelled.
    pub fn sleep(&self, dur: Duration) -> bool {
        let guard = self.flag.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = self
            .cv
            .wait_timeout_while(guard, dur, |cancelled| !*cancelled)
            .unwrap_or_else(PoisonError::into_inner);
        *guard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn sleep_ends_early_when_cancelled() {
        let t = CancelToken::new();
        let t2 = Arc::clone(&t);
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            t2.cancel();
        });
        let start = Instant::now();
        assert!(t.sleep(Duration::from_secs(30)));
        assert!(start.elapsed() < Duration::from_secs(10));
        let _ = h.join();
        assert!(t.is_cancelled());
    }

    #[test]
    fn sleep_runs_out_when_not_cancelled() {
        let t = CancelToken::new();
        assert!(!t.sleep(Duration::from_millis(5)));
        assert!(!t.is_cancelled());
    }
}
