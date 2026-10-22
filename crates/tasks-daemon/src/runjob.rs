//! One job's runner thread and the adapter between the runner's events and the ledger.
//!
//! The thread owns the slot guard for the job's whole life. The runner reports `started`
//! (the lease is persisted with the move to Running), `lines` (parsed into a timeline for
//! progress and timing samples) and `finished` (the outcome and the timing sample are written
//! in one ledger transaction).

use crate::scheduler::{RunningJob, Scheduler, lock};
use std::sync::Arc;
use tasks_core::{Kind, MachineName, Phase};
use tasks_ledger::{
    Finish, LeaseRecord, LedgerError, RetryPolicy, StartInfo, TimeoutReason, retry_busy,
};
use tasks_platform::{Child, Signal, SlotGuard};
use tasks_runner::{
    ElisionCaps, End, EnvPolicy, JobStart, Lease, Outcome, Policy as RunPolicy, RunEvents, Runner,
    RunnerError, SystemClock, TimedLine, cap_log_file,
};
use tasks_timing::record::{JobResult, samples_from_timeline};
use tasks_timing::{Cache, CargoParser, Event, GradleParser, Sample, SampleKey};

/// Facts about the job the events adapter needs.
pub(crate) struct JobMeta {
    pub kind: Kind,
    pub recipe: String,
}

enum Parser {
    Cargo(CargoParser),
    Gradle(GradleParser),
    Plain,
}

impl Parser {
    fn for_recipe(recipe: &str) -> Parser {
        match recipe.split_whitespace().next() {
            Some("cargo") => Parser::Cargo(CargoParser::new()),
            Some("gradle") => Parser::Gradle(GradleParser::new()),
            _ => Parser::Plain,
        }
    }
    fn feed(&mut self, ts_ms: u64, line: &str) -> Vec<Event> {
        match self {
            Parser::Cargo(p) => p.feed(ts_ms, line),
            Parser::Gradle(p) => p.feed(ts_ms, line),
            Parser::Plain => Vec::new(),
        }
    }
    fn finish(&mut self) -> Vec<Event> {
        match self {
            Parser::Cargo(p) => p.finish(),
            _ => Vec::new(),
        }
    }
}

fn sleep(d: std::time::Duration) {
    std::thread::sleep(d);
}

/// The body of a job's thread.
pub(crate) fn run_job(
    sch: Arc<Scheduler>,
    rj: Arc<RunningJob>,
    start: JobStart,
    guard: Option<SlotGuard>,
    policy: RunPolicy,
    env_policy: EnvPolicy,
    meta: JobMeta,
) {
    let clock = SystemClock::new();
    let runner = Runner::new(
        &*sch.platform.procs,
        &clock,
        policy,
        env_policy,
        sch.parent_env.clone(),
    );
    let mut ev = Events {
        sch: sch.clone(),
        rj: rj.clone(),
        parser: Parser::for_recipe(&meta.recipe),
        meta,
        started_ok: false,
        finished: false,
    };
    let result = runner.run(&start, &rj.token, &mut ev);
    if let Err(e) = result {
        ev.runner_failed(&e);
    }
    drop(guard);
    lock(&sch.running).remove(&rj.id);
    sch.wake.notify();
}

struct Events {
    sch: Arc<Scheduler>,
    rj: Arc<RunningJob>,
    meta: JobMeta,
    parser: Parser,
    started_ok: bool,
    finished: bool,
}

impl Events {
    fn write_finish(&self, fin: &Finish, sample: Option<&(SampleKey, Sample)>) -> bool {
        let id = self.rj.id;
        let ledger = &self.sch.ledger;
        let retry = RetryPolicy::default();
        let first = retry_busy(&retry, &sleep, || {
            ledger.finish_recording(id, fin, sample.map(|p| (&p.0, &p.1)))
        });
        let result = match first {
            // an invalid sample fails the whole call; the job's result must still be recorded
            Err(LedgerError::InvalidSample(_)) => {
                retry_busy(&retry, &sleep, || ledger.finish_recording(id, fin, None))
            }
            other => other,
        };
        match result {
            Ok(()) => true,
            // a cancel committed earlier already ended the job
            Err(LedgerError::IllegalTransition { .. } | LedgerError::NotFound(_)) => true,
            Err(e) => {
                self.sch.note_error(&format!("finish job {}: {e}", id.0));
                false
            }
        }
    }

    fn runner_failed(&mut self, e: &RunnerError) {
        if self.finished {
            return;
        }
        self.finished = true;
        let group = *lock(&self.rj.group);
        let fin = match group {
            Some(g) => {
                // the process may still be running: stop it, then record that we lost track
                let _ = self.sch.platform.procs.signal_group(g, Signal::Kill);
                let mut f = Finish::from_phase(Phase::Interrupted);
                f.summary = Some(format!("runner error: {e}"));
                f
            }
            None => {
                let mut f = Finish::from_phase(Phase::NotStarted {
                    reason: e.to_string(),
                });
                f.summary = Some(e.to_string());
                f
            }
        };
        self.write_finish(&fin, None);
    }

    fn cap_log(&self) {
        let policy = self.sch.policy();
        let soft = policy.log_soft_cap_bytes();
        let len = std::fs::metadata(&self.rj.log_path)
            .map(|m| m.len())
            .unwrap_or(0);
        if len > soft {
            let full = self.rj.log_path.with_extension("full.log");
            let caps = ElisionCaps {
                max_bytes: usize::try_from(soft).unwrap_or(usize::MAX),
                max_line_bytes: 4096,
            };
            let _ = cap_log_file(&self.rj.log_path, &full, caps);
        }
    }
}

impl RunEvents for Events {
    fn started(&mut self, child: &Child, _lease: &Lease) {
        *lock(&self.rj.group) = Some(child.group);
        let info = StartInfo {
            machine: MachineName(self.sch.cfg.machine.clone()),
            lease: LeaseRecord {
                pgid: child.group.pgid,
                leader_start: child.group.start_time,
            },
            log_path: Some(self.rj.log_path.clone()),
        };
        let id = self.rj.id;
        let ledger = &self.sch.ledger;
        match retry_busy(&RetryPolicy::default(), &sleep, || ledger.start(id, &info)) {
            Ok(()) => self.started_ok = true,
            // cancelled between admission and spawn: the runner stops it right away
            Err(LedgerError::IllegalTransition { .. }) => self.rj.token.cancel(),
            Err(e) => {
                *lock(&self.rj.kill_reason) =
                    Some(format!("the ledger could not record the start: {e}"));
                self.sch.note_error(&format!("start job {}: {e}", id.0));
                self.rj.token.cancel();
            }
        }
    }

    fn lines(&mut self, lines: &[TimedLine]) {
        let mut tl = lock(&self.rj.timeline);
        for l in lines {
            let ts = u64::try_from(l.offset.as_millis()).unwrap_or(u64::MAX);
            for e in self.parser.feed(ts, &l.text) {
                tl.record(ts, &e);
            }
        }
    }

    fn finished(&mut self, o: &Outcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        let wall_ms = u64::try_from(o.wall.as_millis()).unwrap_or(u64::MAX);
        {
            let mut tl = lock(&self.rj.timeline);
            for e in self.parser.finish() {
                tl.record(wall_ms, &e);
            }
            tl.finish(wall_ms);
        }
        let reason = lock(&self.rj.kill_reason).clone();
        let mut phase = o.phase();
        if reason.is_some() && o.end == End::Cancelled {
            phase = Phase::Failed { exit: o.exit_code };
        }
        if !self.started_ok {
            phase = Phase::NotStarted {
                reason: reason
                    .clone()
                    .unwrap_or_else(|| "the start was not recorded".into()),
            };
        }
        let mut summary = match &reason {
            Some(r) => format!("{} ({r})", o.describe()),
            None => o.describe(),
        };
        // a job that fell back from a peer says so (nothing had been submitted there)
        if let Some(note) = lock(&self.sch.state).fallbacks.remove(&self.rj.id) {
            summary = format!("{summary} [{note}]");
        }
        let fin = Finish {
            phase: phase.clone(),
            exit_code: o.exit_code,
            signal: o.signal,
            timeout: match o.end {
                End::WallTimeout => Some(TimeoutReason::Wall),
                End::IdleTimeout => Some(TimeoutReason::Idle),
                _ => None,
            },
            summary: Some(summary),
        };
        // Timing samples: only for runs that say something about the work's duration.
        let mut samples = Vec::new();
        if reason.is_none() && self.started_ok && phase != Phase::Interrupted {
            let tl = lock(&self.rj.timeline);
            let job = JobResult {
                recipe: self.meta.recipe.clone(),
                kind: self.meta.kind,
                machine: self.sch.cfg.machine.clone(),
                cache: Cache::Unknown,
                cancelled: phase == Phase::Cancelled,
                passed: phase == Phase::Passed,
                secs: o.wall.as_secs_f64(),
                at_ms: u64::try_from(self.sch.clock.now_ms()).unwrap_or(0),
            };
            samples = samples_from_timeline(&job, &tl);
        }
        let mut rest = samples.split_off(samples.len().min(1));
        let first = samples.pop();
        self.write_finish(&fin, first.as_ref());
        // the per-target samples follow; they are not part of the job's own transaction
        for (k, s) in rest.drain(..) {
            let ledger = &self.sch.ledger;
            let _ = retry_busy(&RetryPolicy::default(), &sleep, || {
                ledger.record_sample(&k, &s)
            });
        }
        self.cap_log();
    }
}
