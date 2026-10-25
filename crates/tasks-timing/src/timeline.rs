//! Folds parser events into per-phase durations.

use crate::event::{Event, TargetKind, TaskOutcome};

/// What a phase measures.
#[derive(Debug, Clone, PartialEq)]
pub enum PhaseKind {
    /// Compilation, up to cargo's `Finished`.
    Compile,
    /// One cargo test target.
    Target(TargetKind),
    /// One gradle task.
    Task(TaskOutcome),
}

/// One timed phase of a job.
#[derive(Debug, Clone, PartialEq)]
pub struct Phase {
    /// `compile`, a target label such as `test:lifecycle`, or a gradle task path.
    pub name: String,
    /// What it measures.
    pub kind: PhaseKind,
    /// Timestamp of the first line, ms.
    pub start_ms: u64,
    /// Timestamp that ended it, if it has ended.
    pub end_ms: Option<u64>,
    /// Duration the tool itself printed.
    pub reported_secs: Option<f64>,
    /// Tests passed (targets only).
    pub passed: u32,
    /// Tests failed (targets only).
    pub failed: u32,
    /// Tests ignored (targets only).
    pub ignored: u32,
}

impl Phase {
    /// Measured duration in ms; open phases are measured up to `now_ms`.
    pub fn duration_ms(&self, now_ms: u64) -> u64 {
        self.end_ms.unwrap_or(now_ms).saturating_sub(self.start_ms)
    }

    /// Whether this phase is a cargo test target.
    pub fn is_target(&self) -> bool {
        matches!(self.kind, PhaseKind::Target(_))
    }
}

/// Per-phase breakdown of one job, built from timestamped events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Timeline {
    phases: Vec<Phase>,
    last_ms: u64,
    compile_failed: bool,
    /// Failed tests as `(target, test)`.
    pub failed_tests: Vec<(String, String)>,
    /// Panics as `(target, message)`.
    pub panics: Vec<(String, String)>,
    /// gradle `BUILD ...` result as `(success, reported_secs)`.
    pub build: Option<(bool, Option<f64>)>,
    /// Crates cargo compiled or checked, and how many of them were dependencies (R-800).
    pub crates_built: u32,
    /// The dependencies among [`Timeline::crates_built`].
    pub deps_built: u32,
}

impl Timeline {
    /// An empty timeline.
    pub fn new() -> Timeline {
        Timeline::default()
    }

    /// All phases in start order.
    pub fn phases(&self) -> &[Phase] {
        &self.phases
    }

    /// The latest timestamp seen.
    pub fn last_ms(&self) -> u64 {
        self.last_ms
    }

    /// Whether the job failed while compiling (so it has no valid run-time data).
    pub fn failed_in_compile(&self) -> bool {
        self.compile_failed
    }

    /// Total compile time in ms.
    pub fn compile_ms(&self) -> u64 {
        self.phases
            .iter()
            .filter(|p| p.kind == PhaseKind::Compile)
            .map(|p| p.duration_ms(self.last_ms))
            .sum()
    }

    /// The `n` slowest cargo test targets by measured duration.
    pub fn slowest_targets(&self, n: usize) -> Vec<&Phase> {
        let mut v: Vec<&Phase> = self.phases.iter().filter(|p| p.is_target()).collect();
        v.sort_by_key(|p| std::cmp::Reverse(p.duration_ms(self.last_ms)));
        v.truncate(n);
        v
    }

    fn close_open(&mut self, ts: u64, pred: impl Fn(&Phase) -> bool) {
        for p in self
            .phases
            .iter_mut()
            .filter(|p| p.end_ms.is_none() && pred(p))
        {
            p.end_ms = Some(ts.max(p.start_ms));
        }
    }

    fn push(&mut self, name: String, kind: PhaseKind, ts: u64) {
        self.phases.push(Phase {
            name,
            kind,
            start_ms: ts,
            end_ms: None,
            reported_secs: None,
            passed: 0,
            failed: 0,
            ignored: 0,
        });
    }

    /// Records one event seen at `ts_ms`.
    pub fn record(&mut self, ts_ms: u64, event: &Event) {
        let ts = ts_ms.max(self.last_ms);
        self.last_ms = ts;
        match event {
            Event::CompileStarted => self.push("compile".into(), PhaseKind::Compile, ts),
            Event::CrateBuilt { dependency } => {
                self.crates_built += 1;
                self.deps_built += u32::from(*dependency);
            }
            Event::CompileFinished { .. } => self.close_open(ts, |p| p.kind == PhaseKind::Compile),
            Event::CompileFailed => {
                self.compile_failed = true;
                self.close_open(ts, |p| p.kind == PhaseKind::Compile);
            }
            Event::TargetStarted { name, kind } => {
                self.close_open(ts, Phase::is_target);
                self.push(kind.label(name), PhaseKind::Target(kind.clone()), ts);
            }
            Event::TargetFinished {
                name,
                passed,
                failed,
                ignored,
                reported_secs,
            } => {
                let idx = self.phases.iter().rposition(|p| {
                    p.end_ms.is_none()
                        && matches!(&p.kind, PhaseKind::Target(k) if k.label(name) == p.name)
                });
                if idx.is_none() {
                    self.push(
                        TargetKind::Bin.label(name),
                        PhaseKind::Target(TargetKind::Bin),
                        ts,
                    );
                }
                let idx = idx.unwrap_or(self.phases.len().saturating_sub(1));
                if let Some(p) = self.phases.get_mut(idx) {
                    p.end_ms = Some(ts.max(p.start_ms));
                    p.passed = *passed;
                    p.failed = *failed;
                    p.ignored = *ignored;
                    p.reported_secs = *reported_secs;
                }
            }
            Event::TestFailed { target, test } => {
                self.failed_tests.push((target.clone(), test.clone()))
            }
            Event::Panic { target, message } => self.panics.push((target.clone(), message.clone())),
            Event::GradleTask { path, outcome } => {
                self.close_open(ts, |p| matches!(p.kind, PhaseKind::Task(_)));
                self.push(path.clone(), PhaseKind::Task(*outcome), ts);
            }
            Event::BuildFinished {
                success,
                reported_secs,
            } => {
                self.close_open(ts, |p| matches!(p.kind, PhaseKind::Task(_)));
                self.build = Some((*success, *reported_secs));
            }
        }
    }

    /// Closes every open phase at `end_ms` (job exit).
    pub fn finish(&mut self, end_ms: u64) {
        let ts = end_ms.max(self.last_ms);
        self.last_ms = ts;
        self.close_open(ts, |_| true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tstart(name: &str) -> Event {
        Event::TargetStarted {
            name: name.into(),
            kind: TargetKind::Test(name.into()),
        }
    }
    fn tend(name: &str, secs: f64) -> Event {
        Event::TargetFinished {
            name: name.into(),
            passed: 3,
            failed: 0,
            ignored: 1,
            reported_secs: Some(secs),
        }
    }

    #[test]
    fn compile_and_targets_get_durations() {
        let mut t = Timeline::new();
        t.record(1_000, &Event::CompileStarted);
        t.record(
            63_000,
            &Event::CompileFinished {
                profile: "test".into(),
            },
        );
        t.record(63_000, &tstart("a"));
        t.record(70_500, &tend("a", 7.2));
        t.record(70_500, &tstart("b"));
        t.record(80_000, &tend("b", 9.4));
        assert_eq!(t.compile_ms(), 62_000);
        let ph = t.phases();
        assert_eq!(ph.len(), 3);
        assert_eq!(ph[1].name, "test:a");
        assert_eq!(ph[1].duration_ms(0), 7_500);
        assert_eq!(ph[1].reported_secs, Some(7.2));
        assert_eq!((ph[1].passed, ph[1].ignored), (3, 1));
        assert_eq!(t.slowest_targets(1)[0].name, "test:b");
    }

    #[test]
    fn gradle_tasks_end_at_next_task_or_build() {
        let mut t = Timeline::new();
        t.record(
            0,
            &Event::GradleTask {
                path: ":a".into(),
                outcome: TaskOutcome::Executed,
            },
        );
        t.record(
            4_000,
            &Event::GradleTask {
                path: ":b".into(),
                outcome: TaskOutcome::UpToDate,
            },
        );
        t.record(
            5_000,
            &Event::BuildFinished {
                success: true,
                reported_secs: Some(5.0),
            },
        );
        assert_eq!(t.phases()[0].duration_ms(0), 4_000);
        assert_eq!(t.phases()[1].duration_ms(0), 1_000);
        assert_eq!(t.build, Some((true, Some(5.0))));
    }

    #[test]
    fn compile_failure_and_finish_close_phases() {
        let mut t = Timeline::new();
        t.record(0, &Event::CompileStarted);
        t.record(2_000, &Event::CompileFailed);
        assert!(t.failed_in_compile());
        t.record(3_000, &tstart("x"));
        t.finish(9_000);
        assert_eq!(t.phases()[1].duration_ms(0), 6_000);
    }

    #[test]
    fn timestamps_never_go_backwards() {
        let mut t = Timeline::new();
        t.record(5_000, &tstart("x"));
        t.record(1_000, &tend("x", 1.0));
        assert_eq!(t.phases()[0].duration_ms(0), 0);
        assert_eq!(t.last_ms(), 5_000);
    }

    #[test]
    fn finished_without_start_still_recorded() {
        let mut t = Timeline::new();
        t.record(1, &tend("orphan", 1.0));
        assert_eq!(t.phases().len(), 1);
        assert_eq!(t.phases()[0].passed, 3);
    }

    #[test]
    fn records_failures_and_panics() {
        let mut t = Timeline::new();
        t.record(
            0,
            &Event::TestFailed {
                target: "t".into(),
                test: "a::b".into(),
            },
        );
        t.record(
            0,
            &Event::Panic {
                target: "t".into(),
                message: "boom".into(),
            },
        );
        assert_eq!(t.failed_tests, vec![("t".to_string(), "a::b".to_string())]);
        assert_eq!(t.panics.len(), 1);
    }
}
