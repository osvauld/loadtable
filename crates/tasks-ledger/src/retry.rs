//! Retry of busy ledger operations with capped exponential backoff (R-004).

use crate::{LedgerError, Result};
use std::time::Duration;

/// How long and how often to retry a busy operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total backoff after which the ledger is reported unavailable (300 s in the daemon, R-003).
    pub deadline: Duration,
    /// First pause.
    pub initial: Duration,
    /// Longest single pause.
    pub max_pause: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            deadline: Duration::from_secs(300),
            initial: Duration::from_millis(50),
            max_pause: Duration::from_secs(2),
        }
    }
}

/// Runs `op`, retrying while it fails busy or locked, pausing with `sleep` between attempts.
/// Any other error returns at once. When the summed pauses reach the deadline the result is
/// `LedgerError::Unavailable`, never anything that looks like a cancellation (R-003).
pub fn retry_busy<T>(
    policy: &RetryPolicy,
    sleep: &dyn Fn(Duration),
    mut op: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut waited = Duration::ZERO;
    let mut pause = policy.initial;
    loop {
        match op() {
            Err(e) if e.is_busy() => {
                if waited >= policy.deadline {
                    return Err(LedgerError::Unavailable { waited });
                }
                let p = pause.min(policy.deadline.saturating_sub(waited));
                sleep(p);
                waited += p;
                pause = (pause * 2).min(policy.max_pause);
            }
            other => return other,
        }
    }
}

#[cfg(test)]
pub(crate) fn busy_error() -> LedgerError {
    LedgerError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use tasks_core::JobId;

    // R-004: busy writes retry with backoff, other errors return at once.
    #[test]
    fn r004_retry_waits_out_a_busy_write_but_not_other_errors() {
        let pauses = RefCell::new(vec![]);
        let sleep = |d: Duration| pauses.borrow_mut().push(d);
        let n = Cell::new(0);
        let r = retry_busy(&RetryPolicy::default(), &sleep, || {
            n.set(n.get() + 1);
            if n.get() < 4 {
                Err(busy_error())
            } else {
                Ok(n.get())
            }
        });
        assert_eq!(r.unwrap(), 4);
        let p = pauses.borrow();
        assert_eq!(p.len(), 3);
        assert!(p.windows(2).all(|w| w[1] >= w[0]));

        let calls = Cell::new(0);
        let r: Result<()> = retry_busy(&RetryPolicy::default(), &sleep, || {
            calls.set(calls.get() + 1);
            Err(LedgerError::NotFound(JobId(1)))
        });
        assert!(matches!(r, Err(LedgerError::NotFound(_))));
        assert_eq!(calls.get(), 1);
    }

    // R-003: past the deadline the result is "unavailable", not a cancellation.
    #[test]
    fn r003_retry_gives_up_with_ledger_unavailable_after_deadline() {
        let total = Cell::new(Duration::ZERO);
        let sleep = |d: Duration| total.set(total.get() + d);
        let policy = RetryPolicy {
            deadline: Duration::from_secs(10),
            initial: Duration::from_secs(1),
            max_pause: Duration::from_secs(4),
        };
        let r: Result<()> = retry_busy(&policy, &sleep, || Err(busy_error()));
        match r {
            Err(LedgerError::Unavailable { waited }) => {
                assert_eq!(waited, Duration::from_secs(10));
                assert_eq!(total.get(), Duration::from_secs(10));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
        let msg = LedgerError::Unavailable {
            waited: Duration::ZERO,
        }
        .to_string();
        assert!(msg.contains("not a cancellation"));
    }

    #[test]
    fn locked_counts_as_busy_and_others_do_not() {
        let locked = LedgerError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            None,
        ));
        assert!(locked.is_busy() && busy_error().is_busy());
        assert!(!LedgerError::Poisoned.is_busy());
        let full = LedgerError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            None,
        ));
        assert!(!full.is_busy());
    }
}
