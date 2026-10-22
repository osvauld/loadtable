//! Policy values and the cancel token. Nothing here is hard-coded by the runner.

use crate::RunnerError;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// Why the runner was asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelState {
    /// Nothing requested.
    None,
    /// A user or the daemon cancelled the job.
    Cancel,
    /// The submitting client went away.
    ClientLost,
}

/// Shared, idempotent cancel flag. Clone it to the thread that may cancel.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicU8>);

impl CancelToken {
    /// A fresh token with nothing requested.
    pub fn new() -> CancelToken {
        CancelToken::default()
    }
    /// Requests cancellation; repeated calls change nothing. Wins over client loss.
    pub fn cancel(&self) {
        self.0.store(1, Ordering::SeqCst);
    }
    /// Records that the client is gone; never downgrades an explicit cancel.
    pub fn client_lost(&self) {
        let _ = self
            .0
            .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst);
    }
    /// Current request.
    pub fn state(&self) -> CancelState {
        match self.0.load(Ordering::SeqCst) {
            1 => CancelState::Cancel,
            2 => CancelState::ClientLost,
            _ => CancelState::None,
        }
    }
}

/// How long to wait and what to enforce. The caller supplies every value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Wait between `Term` and `Kill` (R-078).
    pub cancel_grace: Duration,
    /// Wall-clock limit for the whole run; `None` disables it.
    pub wall_timeout: Option<Duration>,
    /// Maximum time without new log bytes; `None` disables it.
    pub idle_timeout: Option<Duration>,
    /// Whether losing the client cancels this job. Default false (R-104, R-106).
    pub cancel_on_client_loss: bool,
    /// Poll interval of the supervision loop.
    pub poll: Duration,
}

impl Policy {
    /// Rejects a zero poll or grace, which would busy-loop or skip the grace.
    pub fn validate(&self) -> Result<(), RunnerError> {
        if self.poll.is_zero() {
            return Err(RunnerError::Policy("poll must be positive".into()));
        }
        if self.cancel_grace.is_zero() {
            return Err(RunnerError::Policy("cancel_grace must be positive".into()));
        }
        Ok(())
    }
}

/// Default cancel grace in milliseconds (R-078).
const GRACE_DEFAULT_MS: u64 = 300;
/// Accepted range of the cancel grace in milliseconds (R-078).
const GRACE_RANGE_MS: std::ops::RangeInclusive<u64> = 100..=30_000;

/// Strictly parses the `TASKS_CANCEL_GRACE_MS` value: unset means 300 ms; the
/// value must be a plain integer in 100..=30000 (R-078).
pub fn parse_grace_ms(value: Option<&str>) -> Result<Duration, RunnerError> {
    let Some(v) = value else {
        return Ok(Duration::from_millis(GRACE_DEFAULT_MS));
    };
    let ms: u64 = v
        .parse()
        .map_err(|_| RunnerError::Policy(format!("cancel grace {v:?} is not an integer")))?;
    if !GRACE_RANGE_MS.contains(&ms) {
        return Err(RunnerError::Policy(format!(
            "cancel grace {ms} ms is outside {}..={}",
            GRACE_RANGE_MS.start(),
            GRACE_RANGE_MS.end()
        )));
    }
    Ok(Duration::from_millis(ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r078_cancel_grace_defaults_to_300ms_and_parses_strictly() {
        assert_eq!(parse_grace_ms(None).unwrap(), Duration::from_millis(300));
        assert_eq!(
            parse_grace_ms(Some("100")).unwrap(),
            Duration::from_millis(100)
        );
        assert_eq!(
            parse_grace_ms(Some("30000")).unwrap(),
            Duration::from_millis(30_000)
        );
        for bad in ["99", "30001", "", " 300", "3e2", "-1", "300ms", "0"] {
            assert!(parse_grace_ms(Some(bad)).is_err(), "{bad:?}");
        }
    }
}
