//! Turns a finished job's timeline into history samples, applying the pollution rules.

use crate::history::{Cache, Outcome, Sample, SampleKey};
use crate::timeline::{PhaseKind, Timeline};
use tasks_core::Kind;

/// Facts about a finished job.
#[derive(Debug, Clone)]
pub struct JobResult {
    /// The `RecipeKey` display string.
    pub recipe: String,
    /// The job's kind.
    pub kind: Kind,
    /// Machine it ran on.
    pub machine: String,
    /// Build warmth at start.
    pub cache: Cache,
    /// Whether it was cancelled.
    pub cancelled: bool,
    /// Whether it passed.
    pub passed: bool,
    /// Whole-job seconds.
    pub secs: f64,
    /// Finish time, ms.
    pub at_ms: u64,
}

/// Samples to store for a finished job: one per cargo test target plus one whole-job sample.
///
/// Probes and runs that failed while compiling produce no samples (they would pollute run-time
/// history). A cancelled job produces samples marked [`Outcome::Cancelled`], which estimates
/// ignore.
pub fn samples_from_timeline(job: &JobResult, timeline: &Timeline) -> Vec<(SampleKey, Sample)> {
    let JobResult {
        recipe,
        kind,
        machine,
        cache,
        cancelled,
        passed: job_passed,
        secs: job_secs,
        at_ms,
    } = job.clone();
    let recipe = recipe.as_str();
    let machine = machine.as_str();
    if !kind.counts_for_eta() || timeline.failed_in_compile() {
        return Vec::new();
    }
    let outcome_of = |passed: bool| {
        if cancelled {
            Outcome::Cancelled
        } else if passed {
            Outcome::Passed
        } else {
            Outcome::Failed
        }
    };
    let key = |target: Option<String>| SampleKey {
        recipe: recipe.to_string(),
        target,
        machine: machine.to_string(),
    };
    let mut out = vec![(
        key(None),
        Sample {
            secs: job_secs,
            cache,
            outcome: outcome_of(job_passed),
            at_ms,
        },
    )];
    for p in timeline
        .phases()
        .iter()
        .filter(|p| matches!(p.kind, PhaseKind::Target(_)))
    {
        if p.end_ms.is_none() {
            continue;
        }
        let secs = p.duration_ms(timeline.last_ms()) as f64 / 1000.0;
        out.push((
            key(Some(p.name.clone())),
            Sample {
                secs,
                cache,
                outcome: outcome_of(p.failed == 0),
                at_ms,
            },
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, TargetKind};

    fn job(kind: Kind, cache: Cache, cancelled: bool, passed: bool) -> JobResult {
        JobResult {
            recipe: "r".into(),
            kind,
            machine: "m".into(),
            cache,
            cancelled,
            passed,
            secs: 6.0,
            at_ms: 9,
        }
    }

    fn tl(fail_compile: bool) -> Timeline {
        let mut t = Timeline::new();
        t.record(0, &Event::CompileStarted);
        if fail_compile {
            t.record(1_000, &Event::CompileFailed);
        } else {
            t.record(
                1_000,
                &Event::CompileFinished {
                    profile: "test".into(),
                },
            );
            t.record(
                1_000,
                &Event::TargetStarted {
                    name: "a".into(),
                    kind: TargetKind::Test("a".into()),
                },
            );
            t.record(
                6_000,
                &Event::TargetFinished {
                    name: "a".into(),
                    passed: 1,
                    failed: 0,
                    ignored: 0,
                    reported_secs: Some(4.9),
                },
            );
        }
        t
    }

    #[test]
    fn r114_normal_run_gives_job_and_target_samples() {
        let v = samples_from_timeline(&job(Kind::Test, Cache::Warm, false, true), &tl(false));
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].0.target.as_deref(), Some("test:a"));
        assert_eq!(v[1].1.secs, 5.0);
    }

    #[test]
    fn probes_and_compile_failures_leave_no_samples() {
        assert!(
            samples_from_timeline(&job(Kind::Probe, Cache::Warm, false, true), &tl(false))
                .is_empty()
        );
        assert!(
            samples_from_timeline(&job(Kind::Test, Cache::Warm, false, false), &tl(true))
                .is_empty()
        );
    }

    #[test]
    fn cancelled_runs_are_kept_but_marked() {
        let v = samples_from_timeline(&job(Kind::Test, Cache::Cold, true, false), &tl(false));
        assert!(v.iter().all(|(_, s)| s.outcome == Outcome::Cancelled));
    }
}
