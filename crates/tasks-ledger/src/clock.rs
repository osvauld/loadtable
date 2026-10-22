//! Wall-clock time as an injectable dependency, so tests are deterministic.

use std::sync::atomic::{AtomicI64, Ordering};

/// A source of wall-clock time in ms since the Unix epoch.
pub trait Clock: Send + Sync {
    /// Current time in ms.
    fn now_ms(&self) -> i64;
}

/// The system clock (0 if the clock is before the epoch).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
}

/// A clock that only moves when told to; for tests.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    /// A clock showing `ms`.
    pub fn new(ms: i64) -> ManualClock {
        ManualClock(AtomicI64::new(ms))
    }

    /// Moves the clock forward by `ms`.
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    /// Sets the clock to `ms`.
    pub fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_when_told() {
        let c = ManualClock::new(10);
        assert_eq!(c.now_ms(), 10);
        c.advance(5);
        assert_eq!(c.now_ms(), 15);
        c.set(3);
        assert_eq!(c.now_ms(), 3);
    }

    #[test]
    fn system_clock_is_after_2020() {
        assert!(SystemClock.now_ms() > 1_577_836_800_000);
    }
}
