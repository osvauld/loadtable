//! Injectable time, so tests never sleep for real.

use std::time::{Duration, Instant};

/// A monotonic clock that can also wait.
pub trait Clock {
    /// Time since an arbitrary fixed origin; never goes backwards.
    fn now(&self) -> Duration;
    /// Waits for `d` (a fake clock just advances).
    fn sleep(&self, d: Duration);
}

/// The real monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// Starts a clock whose origin is now.
    pub fn new() -> SystemClock {
        SystemClock {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}
