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

/// Makes SIGINT, SIGTERM and SIGHUP end this process as they would without any handler, for a
/// front end whose own loop never reads [`Signals`] (the UI's window loop, R-686). The flag-only
/// handlers `real_main` installs would otherwise swallow them.
pub fn die_on_termination() {
    let flags: Vec<(i32, Arc<AtomicBool>)> = [SIGTERM, SIGINT, SIGHUP]
        .into_iter()
        .filter_map(|sig| {
            let f = Arc::new(AtomicBool::new(false));
            signal_hook::flag::register(sig, Arc::clone(&f)).ok()?;
            Some((sig, f))
        })
        .collect();
    let _ = std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            loop {
                if let Some((sig, _)) = flags.iter().find(|(_, f)| f.load(Ordering::SeqCst)) {
                    let _ = signal_hook::low_level::emulate_default_handler(*sig);
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::Instant;

    /// The child half of the R-686 test: handlers like `real_main`'s, then the UI's guard.
    #[test]
    fn r686_child_waits_for_a_signal() {
        if std::env::var_os("R686_CHILD").is_none() {
            return;
        }
        let _flags = RealSignals::install().unwrap();
        die_on_termination();
        println!("r686-ready");
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn r686_a_front_end_with_flag_only_handlers_still_dies_on_sigterm() {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::tests::r686_child_waits_for_a_signal",
                "--nocapture",
            ])
            .env("R686_CHILD", "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        while !lines.next().unwrap().unwrap().contains("r686-ready") {}
        let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("SIGTERM was swallowed");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
