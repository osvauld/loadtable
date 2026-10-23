//! Things a command needs from the process, behind traits so tests control them.

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Waits between polls.
pub trait Pacer {
    /// Pauses for about `d`.
    fn pause(&self, d: Duration);
}

/// Sleeps.
#[derive(Debug, Default, Clone, Copy)]
pub struct SleepPacer;

impl Pacer for SleepPacer {
    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Reports a termination signal received since start.
pub trait Signals {
    /// The number of the first pending signal (2 INT, 1 HUP, 15 TERM).
    fn pending(&self) -> Option<i32>;
}

/// No signals ever (for commands that do not follow a job, and for tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoSignals;

impl Signals for NoSignals {
    fn pending(&self) -> Option<i32> {
        None
    }
}

/// SIGINT, SIGTERM and SIGHUP recorded in flags; the handlers only set the flags.
pub struct RealSignals {
    flags: Vec<(i32, Arc<AtomicBool>)>,
}

impl RealSignals {
    /// Registers the handlers.
    pub fn install() -> std::io::Result<RealSignals> {
        let mut flags = Vec::new();
        for sig in [SIGINT, SIGTERM, SIGHUP] {
            let f = Arc::new(AtomicBool::new(false));
            signal_hook::flag::register(sig, Arc::clone(&f))?;
            flags.push((sig, f));
        }
        Ok(RealSignals { flags })
    }
}

impl Signals for RealSignals {
    fn pending(&self) -> Option<i32> {
        self.flags
            .iter()
            .find(|(_, f)| f.load(Ordering::SeqCst))
            .map(|(s, _)| *s)
    }
}
