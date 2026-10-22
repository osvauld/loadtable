//! Turning timing history into policy suggestions. Never touches files.

use crate::error::PolicyError;
use crate::model::Policy;
use crate::timing::{Candidates, Timing, candidate_key, measure, primary, recipe_of};
use crate::validate::is_cargo_test;
use std::collections::BTreeSet;
use tasks_core::Kind;
use tasks_timing::dur::fmt_precise;
use tasks_timing::estimate::Estimate;

/// Thresholds for the rules. There are no hidden constants: see [`Default`] for starting values.
#[derive(Debug, Clone, PartialEq)]
pub struct SuggestConfig {
    /// Fewest samples a statistic needs before a rule may fire on it.
    pub min_samples: usize,
    /// Split a target out of an unsplit step when its median exceeds this many seconds.
    pub split_secs: f64,
    /// Mark a target `long` when its median exceeds this many seconds.
    pub long_secs: f64,
    /// A step whose median is below this many seconds is tiny.
    pub tiny_secs: f64,
    /// Suggest a merge when a suite has at least this many tiny steps.
    pub min_tiny_steps: usize,
    /// Flag a statistic whose p90 / median exceeds this.
    pub variance_ratio: f64,
    /// Suggest a placement when the best machine's median is at most this fraction of the next
    /// best machine's.
    pub faster_ratio: f64,
}

impl Default for SuggestConfig {
    fn default() -> Self {
        SuggestConfig {
            min_samples: 3,
            split_secs: 100.0,
            long_secs: 60.0,
            tiny_secs: 5.0,
            min_tiny_steps: 2,
            variance_ratio: 2.0,
            faster_ratio: 0.7,
        }
    }
}

/// Which rule produced a suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SuggestionKind {
    /// Split a long target out of an unsplit step.
    SplitOut,
    /// Mark a target `long`.
    MarkLong,
    /// Merge tiny steps.
    MergeTiny,
    /// No history for a step or target.
    NoHistory,
    /// p90 far above the median.
    HighVariance,
    /// One machine is consistently faster.
    Placement,
}

/// A change to the policy text that tasks can describe but never applies.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    /// Add `split = "cargo-test"` to a step.
    SetSplit {
        /// Suite.
        suite: String,
        /// Step.
        step: String,
    },
    /// Add `long = true` to a target's notes.
    MarkLong {
        /// Suite.
        suite: String,
        /// Target label such as `test:lifecycle`.
        target: String,
    },
    /// Add `placement = "MACHINE"` to a step.
    SetPlacement {
        /// Suite.
        suite: String,
        /// Step.
        step: String,
        /// Machine name.
        machine: String,
    },
}

/// One suggestion with its evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    /// The rule.
    pub kind: SuggestionKind,
    /// Suite.
    pub suite: String,
    /// Step.
    pub step: String,
    /// Target label, if about one target.
    pub target: Option<String>,
    /// One-line proposal.
    pub summary: String,
    /// Numbers behind it: sample counts, median, p90, machine.
    pub evidence: String,
    /// The text change, when the rule has a mechanical one.
    pub action: Option<Action>,
}

/// `median 1m56s, p90 2m00s over 5 runs on nixos`.
pub fn describe(e: &Estimate) -> String {
    format!(
        "median {}, p90 {} over {} run{} on {}",
        fmt_precise(e.median_secs),
        fmt_precise(e.p90_secs),
        e.samples,
        if e.samples == 1 { "" } else { "s" },
        e.machine
    )
}

struct Ctx<'a> {
    cfg: &'a SuggestConfig,
    timing: &'a Timing<'a>,
    out: Vec<Suggestion>,
}

impl Ctx<'_> {
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        kind: SuggestionKind,
        suite: &str,
        step: &str,
        target: Option<&str>,
        summary: String,
        evidence: String,
        action: Option<Action>,
    ) {
        self.out.push(Suggestion {
            kind,
            suite: suite.into(),
            step: step.into(),
            target: target.map(str::to_string),
            summary,
            evidence,
            action,
        });
    }

    fn variance(&mut self, suite: &str, step: &str, target: Option<&str>, e: &Estimate) {
        if e.samples >= self.cfg.min_samples
            && e.median_secs > 0.0
            && e.p90_secs / e.median_secs > self.cfg.variance_ratio
        {
            let what = target.unwrap_or(step);
            self.push(
                SuggestionKind::HighVariance,
                suite,
                step,
                target,
                format!(
                    "{what} is unstable: p90 is {:.1}x the median",
                    e.p90_secs / e.median_secs
                ),
                describe(e),
                None,
            );
        }
    }
}

/// Produces suggestions for `policy` from `timing`.
///
/// `candidates` lists the test targets known for cargo-test steps. Probe steps are skipped, and
/// cancelled samples are excluded by `tasks_timing::estimate`.
pub fn suggest(
    policy: &Policy,
    timing: &Timing<'_>,
    candidates: &Candidates,
    cfg: &SuggestConfig,
) -> Result<Vec<Suggestion>, PolicyError> {
    let mut cx = Ctx {
        cfg,
        timing,
        out: Vec::new(),
    };
    for (sname, suite) in &policy.suites {
        let mut tiny: Vec<(String, Estimate)> = Vec::new();
        for step in &suite.steps {
            let Some((recipe, kind)) = recipe_of(&step.command) else {
                continue;
            };
            if kind == Kind::Probe {
                continue;
            }
            let whole = measure(cx.timing, &recipe, None)?;
            let mut labels: BTreeSet<String> = suite.targets.keys().cloned().collect();
            if let Some(c) = candidates.get(&candidate_key(sname, &step.name)) {
                labels.extend(c.iter().cloned());
            }
            if !is_cargo_test(&step.command) {
                labels.clear();
            }
            let mut any_target = false;
            for label in &labels {
                let est = measure(cx.timing, &recipe, Some(label))?;
                let Some(p) = primary(est) else {
                    cx.push(
                        SuggestionKind::NoHistory,
                        sname,
                        &step.name,
                        Some(label),
                        format!(
                            "{label} has no timing history; it is scheduled as long until measured"
                        ),
                        "0 runs".into(),
                        None,
                    );
                    continue;
                };
                any_target = true;
                if p.samples < cx.cfg.min_samples {
                    continue;
                }
                let note_long = suite.targets.get(label).is_some_and(|n| n.long);
                if p.median_secs > cx.cfg.split_secs && step.split.is_none() {
                    cx.push(
                        SuggestionKind::SplitOut,
                        sname,
                        &step.name,
                        Some(label),
                        format!("split test `{label}` out of step {:?}", step.name),
                        describe(&p),
                        Some(Action::SetSplit {
                            suite: sname.clone(),
                            step: step.name.clone(),
                        }),
                    );
                }
                if p.median_secs > cx.cfg.long_secs && !note_long {
                    cx.push(
                        SuggestionKind::MarkLong,
                        sname,
                        &step.name,
                        Some(label),
                        format!("mark `{label}` long so it is scheduled first"),
                        describe(&p),
                        Some(Action::MarkLong {
                            suite: sname.clone(),
                            target: label.clone(),
                        }),
                    );
                }
                cx.variance(sname, &step.name, Some(label), &p);
            }
            if whole.is_empty() && !any_target {
                cx.push(
                    SuggestionKind::NoHistory,
                    sname,
                    &step.name,
                    None,
                    format!("step {:?} has no timing history", step.name),
                    "0 runs on any machine; run the suite a few times, then ask again".into(),
                    None,
                );
                continue;
            }
            if let Some(p) = primary(whole.clone()) {
                cx.variance(sname, &step.name, None, &p);
                if p.samples >= cfg.min_samples
                    && p.median_secs < cfg.tiny_secs
                    && step.split.is_none()
                {
                    tiny.push((step.name.clone(), p));
                }
            }
            let mut ranked: Vec<&Estimate> = whole
                .iter()
                .filter(|e| e.samples >= cfg.min_samples)
                .collect();
            ranked.sort_by(|a, b| a.median_secs.total_cmp(&b.median_secs));
            if step.placement.is_none()
                && let (Some(best), Some(next)) = (ranked.first(), ranked.get(1))
                && best.median_secs <= next.median_secs * cfg.faster_ratio
            {
                cx.push(
                    SuggestionKind::Placement,
                    sname,
                    &step.name,
                    None,
                    format!("prefer {} for step {:?}", best.machine, step.name),
                    format!("{}; vs {}", describe(best), describe(next)),
                    Some(Action::SetPlacement {
                        suite: sname.clone(),
                        step: step.name.clone(),
                        machine: best.machine.clone(),
                    }),
                );
            }
        }
        if tiny.len() >= cfg.min_tiny_steps {
            let names: Vec<&str> = tiny.iter().map(|(n, _)| n.as_str()).collect();
            let ev: Vec<String> = tiny
                .iter()
                .map(|(n, e)| format!("{n}: {}", describe(e)))
                .collect();
            let first = names.first().copied().unwrap_or("");
            cx.push(
                SuggestionKind::MergeTiny,
                sname,
                first,
                None,
                format!("merge tiny steps into one: {}", names.join(", ")),
                ev.join("; "),
                None,
            );
        }
    }
    Ok(cx.out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timing::fixture::{RECIPE, history_4254, seed};
    use crate::validate::parse;
    use std::path::Path;

    const TEXT: &str = r#"version = 1

[suites.core]
notes = "core"

[[suites.core.steps]]
name = "tests"
command = ["cargo", "test", "-p", "p2pcore", "--features", "dev-tools"]
"#;

    fn machines() -> Vec<String> {
        vec!["nixos".into(), "mac".into()]
    }
    fn cands() -> Candidates {
        let all = crate::timing::fixture::T4254
            .iter()
            .map(|(t, _)| t.to_string())
            .collect();
        Candidates::from([(candidate_key("core", "tests"), all)])
    }
    fn run(h: &dyn tasks_timing::History, text: &str) -> Vec<Suggestion> {
        let m = machines();
        let p = parse(text, Path::new("p.toml")).unwrap();
        suggest(
            &p,
            &Timing {
                history: h,
                machines: &m,
            },
            &cands(),
            &SuggestConfig::default(),
        )
        .unwrap()
    }
    fn of(v: &[Suggestion], k: SuggestionKind) -> Vec<&Suggestion> {
        v.iter().filter(|s| s.kind == k).collect()
    }

    #[test]
    fn splits_and_marks_long_from_4254() {
        let h = history_4254();
        let v = run(&h, TEXT);
        let split = of(&v, SuggestionKind::SplitOut);
        let names: Vec<_> = split.iter().filter_map(|s| s.target.as_deref()).collect();
        assert_eq!(names, vec!["test:group_call", "test:lifecycle"]);
        let life = split
            .iter()
            .find(|s| s.target.as_deref() == Some("test:lifecycle"))
            .unwrap();
        assert_eq!(
            life.evidence,
            "median 1m56s, p90 1m58s over 5 runs on nixos"
        );
        assert!(life.summary.contains("split test `test:lifecycle`"));
        let long: Vec<_> = of(&v, SuggestionKind::MarkLong)
            .iter()
            .filter_map(|s| s.target.clone())
            .collect();
        assert_eq!(long, vec!["test:group_call", "test:lifecycle"]);
    }

    #[test]
    fn respects_existing_long_and_split() {
        let text = format!("{TEXT}[suites.core.targets.\"test:lifecycle\"]\nlong = true\n")
            .replace("\"dev-tools\"]", "\"dev-tools\"]\nsplit = \"cargo-test\"");
        let v = run(&history_4254(), &text);
        assert!(of(&v, SuggestionKind::SplitOut).is_empty());
        assert!(
            !of(&v, SuggestionKind::MarkLong)
                .iter()
                .any(|s| s.target.as_deref() == Some("test:lifecycle"))
        );
    }

    #[test]
    fn needs_min_samples() {
        let mut h = tasks_timing::MemHistory::new();
        seed(&mut h, RECIPE, Some("test:lifecycle"), "nixos", 116.0, 2);
        let v = run(&h, TEXT);
        assert!(of(&v, SuggestionKind::SplitOut).is_empty());
    }

    #[test]
    fn flags_no_history() {
        let h = tasks_timing::MemHistory::new();
        let v = run(&h, TEXT);
        assert!(
            of(&v, SuggestionKind::NoHistory)
                .iter()
                .any(|s| s.target.is_none())
        );
        assert!(of(&v, SuggestionKind::NoHistory).len() > 1);
    }

    #[test]
    fn high_variance() {
        let mut h = history_4254();
        let key = tasks_timing::SampleKey {
            recipe: RECIPE.into(),
            target: Some("test:calls".into()),
            machine: "nixos".into(),
        };
        h.add(key.clone(), crate::timing::fixture::sample(90.0, 2000));
        h.add(key, crate::timing::fixture::sample(95.0, 2001));
        let v = run(&h, TEXT);
        let hv = of(&v, SuggestionKind::HighVariance);
        assert_eq!(hv.len(), 1);
        assert_eq!(hv[0].target.as_deref(), Some("test:calls"));
    }

    #[test]
    fn cancelled_samples_excluded() {
        let mut h = tasks_timing::MemHistory::new();
        let key = tasks_timing::SampleKey {
            recipe: RECIPE.into(),
            target: Some("test:lifecycle".into()),
            machine: "nixos".into(),
        };
        for i in 0..5 {
            let mut s = crate::timing::fixture::sample(300.0, i);
            s.outcome = tasks_timing::Outcome::Cancelled;
            h.add(key.clone(), s);
        }
        let v = run(&h, TEXT);
        assert!(of(&v, SuggestionKind::SplitOut).is_empty());
    }

    #[test]
    fn placement_when_one_machine_faster() {
        let mut h = tasks_timing::MemHistory::new();
        seed(&mut h, RECIPE, None, "nixos", 300.0, 5);
        seed(&mut h, RECIPE, None, "mac", 600.0, 4);
        let v = run(&h, TEXT);
        let p = of(&v, SuggestionKind::Placement);
        assert_eq!(p.len(), 1);
        assert_eq!(
            p[0].action,
            Some(Action::SetPlacement {
                suite: "core".into(),
                step: "tests".into(),
                machine: "nixos".into()
            })
        );
        assert!(
            p[0].evidence.contains("5 runs on nixos") && p[0].evidence.contains("4 runs on mac")
        );
        // Not when the machines are close, or a placement is already given.
        let mut h2 = tasks_timing::MemHistory::new();
        seed(&mut h2, RECIPE, None, "nixos", 300.0, 5);
        seed(&mut h2, RECIPE, None, "mac", 330.0, 5);
        assert!(of(&run(&h2, TEXT), SuggestionKind::Placement).is_empty());
        let placed = TEXT.replace("\"dev-tools\"]", "\"dev-tools\"]\nplacement = \"mac\"");
        assert!(of(&run(&h, &placed), SuggestionKind::Placement).is_empty());
    }

    #[test]
    fn merges_tiny_steps_and_skips_probes() {
        let text = r#"version = 1
[suites.s]
[[suites.s.steps]]
name = "fmt"
command = ["cargo", "fmt", "--check"]
[[suites.s.steps]]
name = "lint"
command = ["cargo", "metadata"]
[[suites.s.steps]]
name = "probe"
command = ["true"]
"#;
        let mut h = tasks_timing::MemHistory::new();
        let fmt = recipe_of(&["cargo".into(), "fmt".into(), "--check".into()])
            .unwrap()
            .0;
        let meta = recipe_of(&["cargo".into(), "metadata".into()]).unwrap().0;
        seed(&mut h, &fmt, None, "nixos", 1.5, 4);
        seed(&mut h, &meta, None, "nixos", 2.0, 4);
        let m = machines();
        let p = parse(text, Path::new("p.toml")).unwrap();
        let v = suggest(
            &p,
            &Timing {
                history: &h,
                machines: &m,
            },
            &Candidates::new(),
            &SuggestConfig::default(),
        )
        .unwrap();
        let merge = of(&v, SuggestionKind::MergeTiny);
        assert_eq!(merge.len(), 1);
        assert!(merge[0].summary.contains("fmt") && merge[0].summary.contains("lint"));
        assert!(!v.iter().any(|s| s.step == "probe"));
    }

    #[test]
    fn history_errors_propagate() {
        let h = tasks_timing::MemHistory::failing();
        let m = machines();
        let p = parse(TEXT, Path::new("p.toml")).unwrap();
        assert!(matches!(
            suggest(
                &p,
                &Timing {
                    history: &h,
                    machines: &m
                },
                &cands(),
                &SuggestConfig::default()
            ),
            Err(PolicyError::Timing(_))
        ));
    }
}
