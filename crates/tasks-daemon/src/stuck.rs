//! The stuck-job detector (R-095, R-096).
//!
//! It is deliberately conservative. A job is ended as stuck only when all three hold:
//! the log tail has a terminal failure line, no log bytes arrived during the window, and the
//! process group's CPU stayed flat for the window. Quiet alone, test failures (`panicked`) or
//! moving CPU never fire it. An unreadable or empty group is "no evidence", not zero CPU
//! (R-096): it restarts the window.

/// CPU seconds a group may use during the window and still count as flat.
pub const FLAT_CPU_SECS: f64 = 0.5;

/// Smallest and largest configurable window, in seconds.
pub const WINDOW_RANGE_SECS: std::ops::RangeInclusive<u64> = 30..=3600;

/// Lines that say the build tool already gave up (a wedged daemon often leaves one behind).
const TERMINAL: &[&str] = &[
    "BUILD FAILED",
    "FAILURE: Build failed",
    "OutOfMemoryError",
    "GC overhead limit exceeded",
    "Gradle build daemon disappeared unexpectedly",
    "error: could not compile",
];

/// Phrases that mark an ordinary test failure; such a line never counts as terminal.
const ORDINARY: &[&str] = &["panicked", "test result:", "assertion"];

/// True when the line is a terminal failure line.
pub fn is_terminal_failure(line: &str) -> bool {
    !ORDINARY.iter().any(|o| line.contains(o)) && TERMINAL.iter().any(|t| line.contains(t))
}

/// True when any of the tail lines is a terminal failure line.
pub fn tail_is_terminal<S: AsRef<str>>(tail: &[S]) -> bool {
    tail.iter().any(|l| is_terminal_failure(l.as_ref()))
}

/// Tracks one job's quiet window.
#[derive(Debug, Clone)]
pub struct StuckDetector {
    window_ms: i64,
    baseline: Option<Baseline>,
}

#[derive(Debug, Clone, Copy)]
struct Baseline {
    at_ms: i64,
    log_len: u64,
    cpu_secs: f64,
}

impl StuckDetector {
    /// A detector with a window of `window_secs`.
    pub fn new(window_secs: u64) -> StuckDetector {
        StuckDetector {
            window_ms: i64::try_from(window_secs.saturating_mul(1000)).unwrap_or(i64::MAX),
            baseline: None,
        }
    }

    /// Records one observation and says whether the log and CPU have been flat for the whole
    /// window. `cpu_secs` is `None` when the group could not be read (no evidence).
    pub fn quiet_for_window(&mut self, now_ms: i64, log_len: u64, cpu_secs: Option<f64>) -> bool {
        let Some(cpu) = cpu_secs else {
            self.baseline = None;
            return false;
        };
        match self.baseline {
            Some(b) if log_len == b.log_len && cpu - b.cpu_secs <= FLAT_CPU_SECS => {
                now_ms.saturating_sub(b.at_ms) >= self.window_ms
            }
            _ => {
                self.baseline = Some(Baseline {
                    at_ms: now_ms,
                    log_len,
                    cpu_secs: cpu,
                });
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRADLE_OOM: &[&str] = &[
        "> Task :app:compileDebugKotlin",
        "java.lang.OutOfMemoryError: Java heap space",
        "FAILURE: Build failed with an exception.",
    ];

    fn drive(d: &mut StuckDetector, from_s: i64, to_s: i64, len: u64, cpu: f64) -> bool {
        let mut fired = false;
        for t in (from_s..=to_s).step_by(10) {
            fired = d.quiet_for_window(t * 1000, len, Some(cpu));
        }
        fired
    }

    // R-095
    #[test]
    fn r095_stuck_job_fires_on_a_gradle_oom_log_that_went_quiet_with_flat_cpu() {
        let mut d = StuckDetector::new(300);
        assert!(!drive(&mut d, 0, 290, 4096, 55.0));
        assert!(drive(&mut d, 300, 310, 4096, 55.2));
        assert!(tail_is_terminal(GRADLE_OOM));
    }

    // R-095
    #[test]
    fn r095_stuck_job_ignores_test_failures_quiet_alone_and_moving_cpu() {
        // a terminal line but the log keeps growing: restarts the window
        let mut d = StuckDetector::new(300);
        for t in (0..=600).step_by(10) {
            assert!(!d.quiet_for_window(t * 1000, 100 + t as u64, Some(10.0)));
        }
        // a terminal line, quiet log, but the CPU keeps moving
        let mut d = StuckDetector::new(300);
        for t in (0..=600).step_by(10) {
            assert!(!d.quiet_for_window(t * 1000, 5, Some(10.0 + t as f64)));
        }
        // quiet and flat for the window fires the quiet test, but the tail decides: cargo test
        // failures are not terminal lines
        let cargo_failures = [
            "thread 'x' panicked at src/lib.rs:3:5:",
            "test result: FAILED. 1 passed; 1 failed",
            "error: test failed, to rerun pass `-p x --lib`",
        ];
        assert!(!tail_is_terminal(&cargo_failures));
        assert!(!tail_is_terminal(&["Running tests", "still compiling"]));
        // "error: could not compile" next to a panic line is an ordinary test failure line
        assert!(!is_terminal_failure(
            "thread panicked: error: could not compile"
        ));
    }

    // R-096
    #[test]
    fn r096_an_unreadable_group_is_no_evidence_not_zero_cpu() {
        let mut d = StuckDetector::new(30);
        assert!(!drive(&mut d, 0, 20, 1, 1.0));
        // the group vanished from view: the window restarts
        assert!(!d.quiet_for_window(25_000, 1, None));
        assert!(!d.quiet_for_window(40_000, 1, Some(1.0)));
        assert!(!d.quiet_for_window(60_000, 1, Some(1.0)));
        assert!(d.quiet_for_window(70_000, 1, Some(1.0)));
    }
}
