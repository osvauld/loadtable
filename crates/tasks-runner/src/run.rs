//! The supervision loop: spawn, follow the log, enforce policy, cancel, report.

use crate::{
    CancelState, CancelToken, Clock, End, EnvPolicy, Lease, LineSplitter, Outcome, Policy,
    RunEvents, RunnerError, SplitLimits, TimedLine, build_env,
};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Duration;
use tasks_core::JobId;
use tasks_platform::{Child, PlatformError, Processes, Signal, SpawnSpec, Usage};

/// Largest read from the log per poll; bounds memory per iteration.
const READ_CHUNK: usize = 64 * 1024;

/// What to run, as the daemon hands it over after admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobStart {
    /// The job id (exported as `TASKS_JOB_ID`).
    pub job: JobId,
    /// Command and arguments.
    pub argv: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Job-provided environment, applied over the allowlisted parent variables.
    pub env: Vec<(String, String)>,
    /// Combined stdout and stderr log (appended by the child).
    pub log_path: PathBuf,
    /// Sidecar receiving the timestamped line stream; `None` disables it.
    pub timed_path: Option<PathBuf>,
}

/// Builds the spawn request: allowlisted environment only (R-090), never the
/// daemon's whole environment.
pub fn build_spawn_spec(
    job: &JobStart,
    parent_env: impl IntoIterator<Item = (String, String)>,
    env_policy: &EnvPolicy,
) -> Result<SpawnSpec, RunnerError> {
    if job.argv.is_empty() {
        return Err(RunnerError::EmptyArgv);
    }
    Ok(SpawnSpec {
        argv: job.argv.clone(),
        cwd: job.cwd.clone(),
        env: build_env(job.job, parent_env, &job.env, env_policy),
        log_path: job.log_path.clone(),
    })
}

/// Runs jobs against a `Processes` implementation and a `Clock`.
pub struct Runner<'a> {
    procs: &'a dyn Processes,
    clock: &'a dyn Clock,
    policy: Policy,
    env_policy: EnvPolicy,
    parent_env: Vec<(String, String)>,
    max_line_bytes: usize,
}

/// Follows an append-only log file and splits it into timed lines.
struct Follower {
    file: Option<File>,
    path: PathBuf,
    pos: u64,
    splitter: LineSplitter,
    timed: Option<File>,
    timed_path: Option<PathBuf>,
}

impl Follower {
    fn new(job: &JobStart, max_line_bytes: usize) -> Result<Follower, RunnerError> {
        let timed = match &job.timed_path {
            Some(p) => Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .map_err(|e| RunnerError::log(p, e))?,
            ),
            None => None,
        };
        Ok(Follower {
            file: None,
            path: job.log_path.clone(),
            pos: 0,
            splitter: LineSplitter::new(SplitLimits { max_line_bytes }),
            timed,
            timed_path: job.timed_path.clone(),
        })
    }

    /// Reads newly appended bytes (at most one chunk) and returns whether any
    /// arrived. Complete lines go to `events`; a line is never emitted twice.
    fn poll(&mut self, now: Duration, events: &mut dyn RunEvents) -> Result<bool, RunnerError> {
        if self.file.is_none() {
            self.file = File::open(&self.path).ok();
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(false);
        };
        file.seek(SeekFrom::Start(self.pos))
            .map_err(|e| RunnerError::log(&self.path, e))?;
        let mut buf = vec![0u8; READ_CHUNK];
        let n = file
            .read(&mut buf)
            .map_err(|e| RunnerError::log(&self.path, e))?;
        if n == 0 {
            return Ok(false);
        }
        self.pos += n as u64;
        let mut lines = Vec::new();
        self.splitter
            .feed(buf.get(..n).unwrap_or(&[]), now, &mut lines);
        self.deliver(&lines, events)?;
        Ok(true)
    }

    /// Reads until the file is exhausted, then flushes the partial last line.
    fn drain(&mut self, now: Duration, events: &mut dyn RunEvents) -> Result<(), RunnerError> {
        while self.poll(now, events)? {}
        let mut lines = Vec::new();
        self.splitter.finish(now, &mut lines);
        self.deliver(&lines, events)
    }

    fn deliver(
        &mut self,
        lines: &[TimedLine],
        events: &mut dyn RunEvents,
    ) -> Result<(), RunnerError> {
        if lines.is_empty() {
            return Ok(());
        }
        if let (Some(f), Some(p)) = (self.timed.as_mut(), self.timed_path.as_ref()) {
            let text: String = lines.iter().map(TimedLine::to_record).collect();
            f.write_all(text.as_bytes())
                .map_err(|e| RunnerError::log(p, e))?;
        }
        events.lines(lines);
        Ok(())
    }
}

/// Mutable state of one run.
struct Live {
    child: Child,
    exit: Option<i32>,
    sent: Option<Signal>,
    usage: Option<Usage>,
}

impl<'a> Runner<'a> {
    /// A runner. `parent_env` is the daemon environment to filter; pass
    /// `std::env::vars().collect()` in production.
    pub fn new(
        procs: &'a dyn Processes,
        clock: &'a dyn Clock,
        policy: Policy,
        env_policy: EnvPolicy,
        parent_env: Vec<(String, String)>,
    ) -> Runner<'a> {
        Runner {
            procs,
            clock,
            policy,
            env_policy,
            parent_env,
            max_line_bytes: 64 * 1024,
        }
    }

    /// Spawns the job, supervises it to the end and reports to `events`.
    /// `events.started` is called before any output is read and `finished`
    /// exactly once. Command failure is an `Ok(Outcome)`.
    pub fn run(
        &self,
        job: &JobStart,
        cancel: &CancelToken,
        events: &mut dyn RunEvents,
    ) -> Result<Outcome, RunnerError> {
        self.policy.validate()?;
        let spec = build_spawn_spec(job, self.parent_env.clone(), &self.env_policy)?;
        let mut follower = Follower::new(job, self.max_line_bytes)?;
        let t0 = self.clock.now();
        let child = self.procs.spawn_group(&spec).map_err(RunnerError::Spawn)?;
        let lease = Lease::new(child.group);
        events.started(&child, &lease);
        let mut live = Live {
            child,
            exit: None,
            sent: None,
            usage: None,
        };
        let mut last_activity = t0;
        let end = loop {
            let now = self.clock.now();
            if follower.poll(now.saturating_sub(t0), events)? {
                last_activity = now;
            }
            match self.procs.try_wait(&live.child) {
                Ok(Some(code)) => {
                    live.exit = Some(code);
                    break End::Exited;
                }
                Ok(None) => {}
                // Lost reaping ownership is never authority to signal (R-080).
                Err(_) => break End::Lost,
            }
            if let Ok(u) = self.procs.usage(live.child.group) {
                live.usage = Some(u);
            }
            if let Some(why) = self.stop_reason(cancel, now, t0, last_activity) {
                self.terminate(&mut live);
                break why;
            }
            self.clock.sleep(self.policy.poll);
        };
        let wall = self.clock.now().saturating_sub(t0);
        follower.drain(wall, events)?;
        let outcome = Outcome {
            end: if live.exit.is_none() && end != End::Lost {
                End::Lost
            } else {
                end
            },
            exit_code: live.exit,
            signal: live.exit.and_then(|c| signal_of(c, live.sent)),
            usage: live.usage,
            wall,
            lease,
        };
        events.finished(&outcome);
        Ok(outcome)
    }

    /// Decides whether the run must be stopped now (R-099 to R-108).
    fn stop_reason(
        &self,
        cancel: &CancelToken,
        now: Duration,
        t0: Duration,
        last_activity: Duration,
    ) -> Option<End> {
        match cancel.state() {
            CancelState::Cancel => return Some(End::Cancelled),
            // A disconnect alone never kills the job (R-104, R-105, R-11).
            CancelState::ClientLost if self.policy.cancel_on_client_loss => {
                return Some(End::ClientLost);
            }
            _ => {}
        }
        if let Some(limit) = self.policy.wall_timeout
            && now.saturating_sub(t0) >= limit
        {
            return Some(End::WallTimeout);
        }
        if let Some(limit) = self.policy.idle_timeout
            && now.saturating_sub(last_activity) >= limit
        {
            return Some(End::IdleTimeout);
        }
        None
    }

    /// Term the whole group, wait the grace, then Kill and wait once more.
    /// Idempotent: nothing is signalled once the exit status is known, and a
    /// `Gone` group counts as already stopped (R-074, R-078).
    fn terminate(&self, live: &mut Live) {
        for sig in [Signal::Term, Signal::Kill] {
            if live.exit.is_some() {
                return;
            }
            match self.procs.signal_group(live.child.group, sig) {
                Ok(()) => live.sent = Some(sig),
                Err(PlatformError::Gone { .. }) => {}
                Err(_) => return,
            }
            let deadline = self.clock.now() + self.policy.cancel_grace;
            loop {
                match self.procs.try_wait(&live.child) {
                    Ok(Some(code)) => {
                        live.exit = Some(code);
                        return;
                    }
                    Ok(None) => {}
                    Err(_) => return,
                }
                if self.clock.now() >= deadline {
                    break;
                }
                self.clock.sleep(self.policy.poll);
            }
        }
    }
}

/// The signal number when the code is 128 plus the signal we sent.
fn signal_of(code: i32, sent: Option<Signal>) -> Option<i32> {
    let n = match sent? {
        Signal::Term => 15,
        Signal::Kill => 9,
        Signal::Stop | Signal::Cont => return None,
    };
    (code == 128 + n).then_some(n)
}

#[cfg(test)]
mod tests;
