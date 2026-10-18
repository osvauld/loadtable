//! Worker-side ledger access that tolerates a busy SQLite ledger.
//!
//! A failed ledger read is "unknown", never a cancellation. Only a committed
//! `cancel_requested` record cancels a job. If the ledger stays unreadable past
//! [`LEDGER_UNAVAILABLE_DEADLINE`], the job is stopped and reported as "ledger
//! unavailable", which is a failure, not a cancellation.
use crate::{Result, Store};
use std::time::{Duration, Instant};

/// How long the ledger may stay continuously unreadable (or unwritable) before a
/// worker stops waiting for it. Each failed attempt already waits up to the
/// store's busy timeout, so this spans many attempts.
pub const LEDGER_UNAVAILABLE_DEADLINE: Duration = Duration::from_secs(300);
const BACKOFF_START: Duration = Duration::from_millis(100);
/// Kept short so a cancel committed right after recovery is still seen promptly.
const BACKOFF_MAX: Duration = Duration::from_secs(1);
const LOG_EVERY: Duration = Duration::from_secs(30);

/// The ledger stayed unavailable past the deadline. Not a cancellation.
#[derive(Debug)]
pub struct LedgerUnavailable(pub String);
impl std::fmt::Display for LedgerUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ledger unavailable (not a cancellation): {}", self.0)
    }
}
impl std::error::Error for LedgerUnavailable {}

/// SQLITE_BUSY / SQLITE_LOCKED: another connection holds the database.
pub fn is_transient(error: &(dyn std::error::Error + 'static)) -> bool {
    use rusqlite::ffi::ErrorCode;
    matches!(
        error.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(e, _))
            if matches!(e.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// Reads the committed cancel flag; `Err` means unknown, not cancelled.
pub fn cancel_requested(store: &Store, id: i64) -> Result<bool> {
    Ok(store.execution(id)?.is_some_and(|e| e.cancel_requested))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelState {
    /// No cancel recorded, or the ledger is temporarily unreadable.
    Continue,
    /// A cancel record was read from the ledger.
    Cancelled,
    /// The ledger was unreadable for longer than the deadline.
    LedgerUnavailable(String),
}

/// Non-blocking cancellation poll for supervisor/admission loops, which must keep
/// reacting to worker loss and signals while the ledger is busy.
pub struct CancelProbe {
    deadline: Duration,
    failing_since: Option<Instant>,
    next_try: Option<Instant>,
    backoff: Duration,
    last_log: Option<Instant>,
    last_error: String,
}

impl Default for CancelProbe {
    fn default() -> Self {
        Self::with_deadline(LEDGER_UNAVAILABLE_DEADLINE)
    }
}

impl CancelProbe {
    pub fn with_deadline(deadline: Duration) -> Self {
        Self {
            deadline,
            failing_since: None,
            next_try: None,
            backoff: BACKOFF_START,
            last_log: None,
            last_error: String::new(),
        }
    }

    fn unavailable(&self, now: Instant) -> CancelState {
        match self.failing_since {
            Some(since) if now.duration_since(since) >= self.deadline => {
                CancelState::LedgerUnavailable(format!(
                    "cancellation state unreadable for {}s (limit {}s): {}",
                    now.duration_since(since).as_secs(),
                    self.deadline.as_secs(),
                    self.last_error
                ))
            }
            _ => CancelState::Continue,
        }
    }

    /// `read` is attempted unless a backoff is pending; `log` receives notes on the
    /// first failure, every 30 s while failing, and on recovery.
    pub fn poll(
        &mut self,
        read: impl FnOnce() -> Result<bool>,
        log: &mut dyn FnMut(&str),
    ) -> CancelState {
        let now = Instant::now();
        if self.next_try.is_some_and(|next| now < next) {
            return self.unavailable(now);
        }
        match read() {
            Ok(cancelled) => {
                if let Some(since) = self.failing_since.take() {
                    log(&format!(
                        "ledger readable again after {:.1}s; job continues\n",
                        since.elapsed().as_secs_f64()
                    ));
                }
                self.next_try = None;
                self.backoff = BACKOFF_START;
                self.last_log = None;
                if cancelled {
                    CancelState::Cancelled
                } else {
                    CancelState::Continue
                }
            }
            Err(error) => {
                let now = Instant::now();
                self.last_error = error.to_string();
                let since = *self.failing_since.get_or_insert(now);
                if self
                    .last_log
                    .is_none_or(|at| now.duration_since(at) >= LOG_EVERY)
                {
                    log(&format!(
                        "ledger read failed ({}; {} for {:.1}s); cancel state unknown, job keeps running, retrying\n",
                        self.last_error,
                        if is_transient(error.as_ref()) {
                            "busy/locked"
                        } else {
                            "error"
                        },
                        now.duration_since(since).as_secs_f64()
                    ));
                    self.last_log = Some(now);
                }
                self.next_try = Some(now + self.backoff);
                self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
                self.unavailable(now)
            }
        }
    }
}

/// Blocking retry for worker-thread ledger calls. Only busy/locked errors are
/// retried, with backoff, until `deadline`; any other error returns at once.
pub fn retry<T>(
    what: &str,
    deadline: Duration,
    log: &mut dyn FnMut(&str),
    mut f: impl FnMut() -> Result<T>,
) -> Result<T> {
    let start = Instant::now();
    let mut backoff = BACKOFF_START;
    let mut logged: Option<Instant> = None;
    loop {
        match f() {
            Ok(value) => {
                if logged.is_some() {
                    log(&format!(
                        "ledger {what} succeeded after {:.1}s of busy retries\n",
                        start.elapsed().as_secs_f64()
                    ));
                }
                return Ok(value);
            }
            Err(error) if is_transient(error.as_ref()) => {
                if start.elapsed() >= deadline {
                    return Err(LedgerUnavailable(format!(
                        "{what} still failing after {}s: {error}",
                        start.elapsed().as_secs()
                    ))
                    .into());
                }
                if logged.is_none_or(|at| at.elapsed() >= LOG_EVERY) {
                    log(&format!(
                        "ledger {what} busy ({error}); retrying for up to {}s\n",
                        deadline.as_secs()
                    ));
                    logged = Some(Instant::now());
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_command::{Recipe, Resource};
    use std::path::PathBuf;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "task-manager-ledger-{label}-{}",
                std::process::id()
            ));
            assert!(!path.exists(), "refusing to reuse another fixture");
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn db(&self) -> PathBuf {
            self.0.join("tasks.sqlite")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn approved_job(store: &mut Store, cwd: &std::path::Path) -> i64 {
        let recipe = Recipe {
            broad_test: false,
            argv: vec!["/bin/true".into()],
            resource: Resource::Run,
        };
        let spec = crate::JobSpec {
            suite: crate::local_command::SUITE.into(),
            owner: "ledger-test".into(),
            source: cwd.to_str().unwrap().into(),
            revision: "live-workspace".into(),
            configuration: serde_json::to_string(&recipe).unwrap(),
            cost: crate::Cost::CpuSimulation,
        };
        let id = store.submit(&spec).unwrap().id;
        store.approve_command(id).unwrap();
        id
    }

    fn lock_exclusively(path: &std::path::Path) -> rusqlite::Connection {
        let blocker = rusqlite::Connection::open(path).unwrap();
        blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
        blocker
    }

    #[test]
    fn busy_ledger_during_polling_does_not_cancel() {
        let fixture = Fixture::new("busy-poll");
        let mut store =
            Store::open_with_busy_timeout(&fixture.db(), Duration::from_millis(20)).unwrap();
        let id = approved_job(&mut store, &fixture.0);
        let blocker = lock_exclusively(&fixture.db());
        let mut probe = CancelProbe::with_deadline(Duration::from_secs(60));
        let mut notes = String::new();
        let mut errors = 0;
        let until = Instant::now() + Duration::from_millis(600);
        while Instant::now() < until {
            let state = probe.poll(
                || {
                    let r = cancel_requested(&store, id);
                    errors += usize::from(r.is_err());
                    r
                },
                &mut |m| notes.push_str(m),
            );
            assert_eq!(state, CancelState::Continue, "{notes}");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(errors >= 2, "the ledger was never actually busy");
        assert!(notes.contains("busy/locked"), "{notes}");
        drop(blocker);
        // A real cancel after recovery still cancels promptly (within the backoff cap).
        store.cancel(id).unwrap();
        let start = Instant::now();
        let state = loop {
            let state = probe.poll(|| cancel_requested(&store, id), &mut |m| notes.push_str(m));
            if state != CancelState::Continue || start.elapsed() > Duration::from_secs(3) {
                break state;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(state, CancelState::Cancelled);
        assert!(start.elapsed() <= BACKOFF_MAX + Duration::from_millis(500));
        assert!(notes.contains("readable again"), "{notes}");
    }

    #[test]
    fn real_cancel_is_seen_on_the_first_poll() {
        let fixture = Fixture::new("cancel");
        let mut store = Store::open(&fixture.db()).unwrap();
        let id = approved_job(&mut store, &fixture.0);
        let mut probe = CancelProbe::default();
        let mut log = |_: &str| {};
        assert_eq!(
            probe.poll(|| cancel_requested(&store, id), &mut log),
            CancelState::Continue
        );
        store.cancel(id).unwrap();
        assert_eq!(
            probe.poll(|| cancel_requested(&store, id), &mut log),
            CancelState::Cancelled
        );
    }

    #[test]
    fn persistent_ledger_failure_is_unavailable_not_cancelled() {
        let fixture = Fixture::new("deadline");
        let mut store =
            Store::open_with_busy_timeout(&fixture.db(), Duration::from_millis(10)).unwrap();
        let id = approved_job(&mut store, &fixture.0);
        let _blocker = lock_exclusively(&fixture.db());
        let mut probe = CancelProbe::with_deadline(Duration::from_millis(150));
        let mut log = |_: &str| {};
        let start = Instant::now();
        let state = loop {
            let state = probe.poll(|| cancel_requested(&store, id), &mut log);
            if state != CancelState::Continue || start.elapsed() > Duration::from_secs(5) {
                break state;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            matches!(&state, CancelState::LedgerUnavailable(m) if m.contains("locked")),
            "{state:?}"
        );
        assert!(start.elapsed() >= Duration::from_millis(150));
    }

    #[test]
    fn retry_waits_out_a_busy_write_but_not_other_errors() {
        let fixture = Fixture::new("retry");
        let mut store =
            Store::open_with_busy_timeout(&fixture.db(), Duration::from_millis(10)).unwrap();
        let id = approved_job(&mut store, &fixture.0);
        let blocker = lock_exclusively(&fixture.db());
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(blocker);
        });
        let mut notes = String::new();
        let cancelled = retry(
            "cancel",
            Duration::from_secs(30),
            &mut |m| notes.push_str(m),
            || store.cancel(id),
        );
        release.join().unwrap();
        cancelled.unwrap();
        assert!(notes.contains("busy"), "{notes}");
        assert!(cancel_requested(&store, id).unwrap());
        // A non-transient error is returned immediately, never retried.
        let start = Instant::now();
        let error = retry("cancel", Duration::from_secs(30), &mut |_| {}, || {
            store.cancel(id + 1000)
        })
        .unwrap_err();
        assert!(!is_transient(error.as_ref()));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn retry_gives_up_with_ledger_unavailable_after_deadline() {
        let fixture = Fixture::new("retry-deadline");
        let mut store =
            Store::open_with_busy_timeout(&fixture.db(), Duration::from_millis(10)).unwrap();
        let id = approved_job(&mut store, &fixture.0);
        let _blocker = lock_exclusively(&fixture.db());
        let error = retry("cancel", Duration::from_millis(200), &mut |_| {}, || {
            store.cancel(id)
        })
        .unwrap_err();
        assert!(error.is::<LedgerUnavailable>(), "{error}");
        assert!(error.to_string().contains("not a cancellation"));
    }
}
