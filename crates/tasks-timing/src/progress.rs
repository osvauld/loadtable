//! Live progress of a running job against per-target history.

use crate::dur::{fmt_eta, fmt_precise};
use crate::estimate::Estimate;
use crate::timeline::{PhaseKind, Timeline};
use std::collections::BTreeMap;
use std::fmt;

/// What history says about the parts of a job.
#[derive(Debug, Clone, Default)]
pub struct Estimates {
    /// Expected compile time, if known.
    pub compile: Option<Estimate>,
    /// Estimates keyed by target label (e.g. `test:lifecycle`). Every expected target should be
    /// present, with `None` when it has no history, so the total is right before they run.
    pub targets: BTreeMap<String, Option<Estimate>>,
}

/// A snapshot of how far a job has got.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// Targets finished.
    pub done_targets: usize,
    /// Targets expected in total (expected plus any extra seen).
    pub total_targets: usize,
    /// Label of the running target, or `compile` while compiling.
    pub current: Option<String>,
    /// Median seconds the running target usually takes.
    pub current_usual_secs: Option<f64>,
    /// Estimated seconds left, from the estimates that exist.
    pub eta_secs: Option<f64>,
    /// Remaining targets with no estimate (the ETA is then a lower bound).
    pub unknown_targets: usize,
}

fn left(est: &Option<Estimate>, elapsed_s: f64) -> Option<f64> {
    est.as_ref().map(|e| {
        if elapsed_s <= e.median_secs {
            e.median_secs - elapsed_s
        } else {
            (e.p90_secs - elapsed_s).max(0.0)
        }
    })
}

/// Computes progress from the timeline so far and the per-target estimates.
pub fn progress(timeline: &Timeline, estimates: &Estimates) -> Progress {
    let now = timeline.last_ms();
    let mut done = 0;
    let mut seen: Vec<&str> = Vec::new();
    let mut current: Option<(String, f64)> = None;
    let mut compiling: Option<f64> = None;
    for p in timeline.phases() {
        let elapsed = p.duration_ms(now) as f64 / 1000.0;
        match (&p.kind, p.end_ms) {
            (PhaseKind::Target(_), Some(_)) => {
                done += 1;
                seen.push(&p.name);
            }
            (PhaseKind::Target(_), None) => {
                current = Some((p.name.clone(), elapsed));
                seen.push(&p.name);
            }
            (PhaseKind::Compile, None) => compiling = Some(elapsed),
            _ => {}
        }
    }
    let mut total = estimates.targets.len();
    total += seen
        .iter()
        .filter(|n| !estimates.targets.contains_key(**n))
        .count();
    let mut eta = 0.0;
    let mut any = false;
    let mut unknown = 0;
    if let Some(el) = compiling {
        if let Some(l) = left(&estimates.compile, el) {
            eta += l;
            any = true;
        }
    } else if timeline
        .phases()
        .iter()
        .all(|p| p.kind != PhaseKind::Compile)
        && done == 0
        && current.is_none()
        && let Some(c) = &estimates.compile
    {
        eta += c.median_secs;
        any = true;
    }
    let mut usual = None;
    if let Some((name, el)) = &current {
        let e = estimates.targets.get(name).and_then(Option::as_ref);
        usual = e.map(|e| e.median_secs);
        match left(&e.cloned(), *el) {
            Some(l) => {
                eta += l;
                any = true;
            }
            None => unknown += 1,
        }
    }
    for est in estimates
        .targets
        .iter()
        .filter(|(l, _)| !seen.contains(&l.as_str()))
        .map(|(_, e)| e)
    {
        match est {
            Some(e) => {
                eta += e.median_secs;
                any = true;
            }
            None => unknown += 1,
        }
    }
    let label = if compiling.is_some() {
        Some("compile".to_string())
    } else {
        current.map(|c| c.0)
    };
    Progress {
        done_targets: done,
        total_targets: total,
        current: label,
        current_usual_secs: usual,
        eta_secs: any.then_some(eta),
        unknown_targets: unknown,
    }
}

impl fmt::Display for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let short = |l: &str| {
            l.split_once(':')
                .map_or(l.to_string(), |(_, n)| n.to_string())
        };
        match &self.current {
            Some(c) if c == "compile" => write!(f, "compiling")?,
            Some(c) => {
                write!(
                    f,
                    "target {}/{} ({}",
                    self.done_targets + 1,
                    self.total_targets,
                    short(c)
                )?;
                if let Some(u) = self.current_usual_secs {
                    write!(f, ", usually {}", fmt_precise(u))?;
                }
                write!(f, ")")?;
            }
            None => write!(f, "target {}/{}", self.done_targets, self.total_targets)?,
        }
        match self.eta_secs {
            Some(e) if self.unknown_targets > 0 => {
                write!(
                    f,
                    "; ETA {}+ ({} unmeasured)",
                    fmt_eta(e),
                    self.unknown_targets
                )
            }
            Some(e) => write!(f, "; ETA {}", fmt_eta(e)),
            None => write!(f, "; ETA unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::Source;
    use crate::event::{Event, TargetKind};

    fn est(median: f64) -> Option<Estimate> {
        Some(Estimate {
            median_secs: median,
            p90_secs: median * 2.0,
            samples: 5,
            source: Source::SameMachine,
            machine: "m".into(),
            cache_fallback: false,
        })
    }
    fn start(t: &mut Timeline, ts: u64, n: &str) {
        t.record(
            ts,
            &Event::TargetStarted {
                name: n.into(),
                kind: TargetKind::Test(n.into()),
            },
        );
    }
    fn end(t: &mut Timeline, ts: u64, n: &str) {
        t.record(
            ts,
            &Event::TargetFinished {
                name: n.into(),
                passed: 1,
                failed: 0,
                ignored: 0,
                reported_secs: None,
            },
        );
    }

    #[test]
    fn r115_matches_the_spec_example() {
        let mut ests = Estimates::default();
        for i in 0..22 {
            let name = if i == 6 {
                "test:lifecycle".to_string()
            } else {
                format!("test:t{i:02}")
            };
            ests.targets
                .insert(name, est(if i == 6 { 190.0 } else { 40.0 }));
        }
        let mut t = Timeline::new();
        for i in 0..6 {
            start(&mut t, i * 1000, &format!("t{i:02}"));
            end(&mut t, i * 1000 + 500, &format!("t{i:02}"));
        }
        start(&mut t, 6_000, "lifecycle");
        let p = progress(&t, &ests);
        assert_eq!((p.done_targets, p.total_targets), (6, 22));
        assert_eq!(p.current.as_deref(), Some("test:lifecycle"));
        assert_eq!(p.eta_secs, Some(190.0 + 15.0 * 40.0));
        assert_eq!(
            p.to_string(),
            "target 7/22 (lifecycle, usually 3m10s); ETA 13m"
        );
    }

    #[test]
    fn running_target_counts_only_what_is_left() {
        let mut ests = Estimates::default();
        ests.targets.insert("test:a".into(), est(100.0));
        ests.targets.insert("test:b".into(), est(50.0));
        let mut t = Timeline::new();
        start(&mut t, 0, "a");
        t.record(
            60_000,
            &Event::Panic {
                target: "a".into(),
                message: "x".into(),
            },
        );
        assert_eq!(progress(&t, &ests).eta_secs, Some(40.0 + 50.0));
    }

    #[test]
    fn overrunning_target_uses_p90_then_zero() {
        let mut ests = Estimates::default();
        ests.targets.insert("test:a".into(), est(100.0));
        let mut t = Timeline::new();
        start(&mut t, 0, "a");
        t.record(
            150_000,
            &Event::Panic {
                target: "a".into(),
                message: String::new(),
            },
        );
        assert_eq!(progress(&t, &ests).eta_secs, Some(50.0));
        t.record(
            400_000,
            &Event::Panic {
                target: "a".into(),
                message: String::new(),
            },
        );
        assert_eq!(progress(&t, &ests).eta_secs, Some(0.0));
    }

    #[test]
    fn unknown_targets_make_eta_a_lower_bound() {
        let mut ests = Estimates::default();
        ests.targets.insert("test:a".into(), est(100.0));
        ests.targets.insert("test:new".into(), None);
        let p = progress(&Timeline::new(), &ests);
        assert_eq!((p.eta_secs, p.unknown_targets), (Some(100.0), 1));
        assert_eq!(p.to_string(), "target 0/2; ETA 2m+ (1 unmeasured)");
    }

    #[test]
    fn no_history_at_all_is_unknown_not_zero() {
        let mut ests = Estimates::default();
        ests.targets.insert("test:a".into(), None);
        let p = progress(&Timeline::new(), &ests);
        assert_eq!(p.eta_secs, None);
        assert!(p.to_string().ends_with("ETA unknown"));
    }

    #[test]
    fn compiling_phase_and_extra_targets() {
        let mut ests = Estimates {
            compile: est(300.0),
            ..Estimates::default()
        };
        ests.targets.insert("test:a".into(), est(10.0));
        let mut t = Timeline::new();
        t.record(0, &Event::CompileStarted);
        t.record(
            100_000,
            &Event::Panic {
                target: String::new(),
                message: String::new(),
            },
        );
        let p = progress(&t, &ests);
        assert_eq!(p.current.as_deref(), Some("compile"));
        assert_eq!(p.eta_secs, Some(200.0 + 10.0));
        assert!(p.to_string().starts_with("compiling; ETA"));
        // a target not in the expected set still counts in the total
        t.record(
            100_000,
            &Event::CompileFinished {
                profile: "t".into(),
            },
        );
        start(&mut t, 100_000, "surprise");
        assert_eq!(progress(&t, &ests).total_targets, 2);
    }
}
