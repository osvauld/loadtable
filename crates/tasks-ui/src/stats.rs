//! The Stats tab: per-recipe and per-target timing from the History API. Pure.

use std::cmp::Ordering;
use tasks_daemon::api::{HistoryView, SampleView};

/// One line of the stats table.
#[derive(Debug, Clone, PartialEq)]
pub struct StatRow {
    /// The recipe.
    pub recipe: String,
    /// The machine.
    pub machine: String,
    /// The target, or `None` for the whole job.
    pub target: Option<String>,
    /// Samples behind the row.
    pub samples: usize,
    /// How many of them passed.
    pub passed: usize,
    /// Median seconds of passed samples; `None` without any (never zero).
    pub median: Option<f64>,
    /// 90th percentile seconds of passed samples.
    pub p90: Option<f64>,
    /// Seconds of the newest sample.
    pub last: Option<f64>,
    /// Outcome of the newest sample.
    pub last_outcome: Option<String>,
    /// Passed samples that ran on a cold cache.
    pub cold: usize,
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted.get(rank - 1).copied()
}

fn row(recipe: &str, machine: &str, target: Option<&String>, samples: &[SampleView]) -> StatRow {
    let mut passed: Vec<f64> = samples
        .iter()
        .filter(|s| s.outcome == "passed" && s.secs.is_finite())
        .map(|s| s.secs)
        .collect();
    passed.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let newest = samples.iter().max_by_key(|s| s.at_ms);
    StatRow {
        recipe: recipe.to_string(),
        machine: machine.to_string(),
        target: target.cloned(),
        samples: samples.len(),
        passed: passed.len(),
        median: percentile(&passed, 0.5),
        p90: percentile(&passed, 0.9),
        last: newest.map(|s| s.secs),
        last_outcome: newest.map(|s| s.outcome.clone()),
        cold: samples
            .iter()
            .filter(|s| s.outcome == "passed" && s.cache == "cold")
            .count(),
    }
}

/// The rows of a history reply, ordered by recipe, machine, then whole job before targets.
pub fn rows(h: &HistoryView) -> Vec<StatRow> {
    let mut out = Vec::new();
    for r in &h.recipes {
        for m in &r.machines {
            for s in &m.series {
                out.push(row(&r.recipe, &m.machine, s.target.as_ref(), &s.samples));
            }
        }
    }
    out.sort_by(|a, b| {
        (&a.recipe, &a.machine, a.target.is_some(), &a.target).cmp(&(
            &b.recipe,
            &b.machine,
            b.target.is_some(),
            &b.target,
        ))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_daemon::api::{MachineHistory, RecipeHistory, SeriesView};

    fn s(secs: f64, outcome: &str, cache: &str, at: u64) -> SampleView {
        SampleView {
            secs,
            cache: cache.into(),
            outcome: outcome.into(),
            at_ms: at,
        }
    }

    fn history() -> HistoryView {
        HistoryView {
            recipes: vec![RecipeHistory {
                recipe: "cargo-test".into(),
                machines: vec![
                    MachineHistory {
                        machine: "nixos".into(),
                        targets: vec![],
                        series: vec![SeriesView {
                            target: Some("test:b".into()),
                            samples: vec![s(4.0, "passed", "warm", 1)],
                        }],
                    },
                    MachineHistory {
                        machine: "laptop".into(),
                        targets: vec!["test:a".into()],
                        series: vec![
                            SeriesView {
                                target: Some("test:a".into()),
                                samples: vec![s(2.0, "failed", "warm", 5)],
                            },
                            SeriesView {
                                target: None,
                                samples: (1..=10)
                                    .map(|i| {
                                        s(
                                            f64::from(i),
                                            "passed",
                                            if i < 3 { "cold" } else { "warm" },
                                            i as u64,
                                        )
                                    })
                                    .chain([s(99.0, "cancelled", "unknown", 50)])
                                    .collect(),
                            },
                        ],
                    },
                ],
            }],
            truncated: false,
        }
    }

    #[test]
    fn rows_are_ordered_and_use_passed_samples_only() {
        let r = rows(&history());
        let keys: Vec<_> = r
            .iter()
            .map(|x| (x.machine.as_str(), x.target.as_deref()))
            .collect();
        assert_eq!(
            keys,
            [
                ("laptop", None),
                ("laptop", Some("test:a")),
                ("nixos", Some("test:b"))
            ]
        );
        let whole = &r[0];
        assert_eq!((whole.samples, whole.passed, whole.cold), (11, 10, 2));
        assert_eq!(whole.median, Some(5.0));
        assert_eq!(whole.p90, Some(9.0));
        assert_eq!(whole.last, Some(99.0));
        assert_eq!(whole.last_outcome.as_deref(), Some("cancelled"));
    }

    #[test]
    fn a_target_that_never_passed_has_no_median_not_zero() {
        let r = rows(&history());
        let a = &r[1];
        assert_eq!(a.median, None);
        assert_eq!(a.p90, None);
        assert_eq!(a.last_outcome.as_deref(), Some("failed"));
        assert!(
            rows(&HistoryView {
                recipes: vec![],
                truncated: false
            })
            .is_empty()
        );
    }
}
