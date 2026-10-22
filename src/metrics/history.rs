//! Run history from the ledger: ETA estimates and expected crate counts.
//!
//! Every terminal ledger row is history, including rows written before this
//! module existed ("backfill"): they have argv (so a kind_key) and per-stage
//! seconds, but no persisted `metrics`, so their warmth is unknown and their
//! crate count missing. Nothing is rewritten; the backfill is computed on read.
use super::command::normalize;
use super::{JobMetrics, Warmth};
use crate::local_command::{self, Recipe, Resource};
use crate::{Job, Phase};
use std::collections::HashMap;

/// Newest matching runs considered by a median.
pub const MAX_SAMPLES: usize = 20;

/// The job's argv without re-validating it against this process's caps
/// (history rows may carry e.g. `-j 8` approved under another configuration).
pub fn recipe(job: &Job) -> Option<Recipe> {
    (job.spec.suite == local_command::SUITE)
        .then(|| serde_json::from_str(&job.spec.configuration).ok())
        .flatten()
}

pub fn argv(job: &Job) -> Vec<String> {
    recipe(job).map(|r| r.argv).unwrap_or_default()
}

/// Coarse ETA grouping key (see `command::normalize`).
pub fn coarse_key(job: &Job) -> String {
    match recipe(job) {
        Some(r) => normalize(&r.argv).coarse_key,
        None => names(job).1,
    }
}

/// (command, kind_key). Typed suites (self-suite, Android trial) have no
/// argv; they are named by suite and configuration.
pub fn names(job: &Job) -> (String, String) {
    match recipe(job) {
        Some(r) => {
            let n = normalize(&r.argv);
            (n.command, n.kind_key)
        }
        None => {
            let name = format!("{} {}", job.spec.suite, job.spec.configuration);
            (name.clone(), name)
        }
    }
}

pub fn resource(job: &Job) -> Resource {
    recipe(job).map_or(Resource::Compile, |r| r.resource)
}

/// Seconds spent executing stages (admission waits excluded); None unless
/// every recorded stage has a duration.
pub fn exec_seconds(job: &Job) -> Option<f64> {
    let e = job.execution.as_ref()?;
    if e.stages.is_empty() {
        return None;
    }
    e.stages.iter().map(|s| s.seconds).sum()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub id: i64,
    pub kind_key: String,
    pub coarse_key: String,
    pub warmth: Warmth,
    pub crates: Option<u64>,
    pub exec_s: Option<f64>,
    pub passed: bool,
}

pub fn run_of(job: &Job) -> Option<Run> {
    if !job.phase.terminal() {
        return None;
    }
    let metrics: Option<&JobMetrics> = job.execution.as_ref().and_then(|e| e.metrics.as_ref());
    Some(Run {
        id: job.id,
        kind_key: names(job).1,
        coarse_key: coarse_key(job),
        warmth: metrics.map_or(Warmth::Unknown, |m| m.warmth),
        crates: metrics.and_then(|m| m.crates_compiled),
        exec_s: exec_seconds(job),
        passed: job.phase == Phase::Passed,
    })
}

pub fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Eta {
    pub total_s: f64,
    pub samples: u32,
    pub basis: String,
}

#[derive(Clone, Debug, Default)]
pub struct History {
    /// Newest first.
    by_kind: HashMap<String, Vec<Run>>,
    /// Same runs grouped by coarse key, newest first.
    by_coarse: HashMap<String, Vec<Run>>,
}

impl History {
    pub fn from_jobs(jobs: &[Job]) -> Self {
        let mut by_kind: HashMap<String, Vec<Run>> = HashMap::new();
        let mut by_coarse: HashMap<String, Vec<Run>> = HashMap::new();
        for run in jobs.iter().filter_map(run_of) {
            by_coarse
                .entry(run.coarse_key.clone())
                .or_default()
                .push(run.clone());
            by_kind.entry(run.kind_key.clone()).or_default().push(run);
        }
        for runs in by_kind.values_mut().chain(by_coarse.values_mut()) {
            runs.sort_by_key(|r| std::cmp::Reverse(r.id));
        }
        Self { by_kind, by_coarse }
    }

    fn runs<'a>(&'a self, kind: &str, exclude: i64) -> impl Iterator<Item = &'a Run> + 'a {
        self.by_kind
            .get(kind)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .filter(move |r| r.id != exclude)
    }

    /// `estimate` without the coarse fallback.
    pub fn estimate(&self, kind: &str, warmth: Warmth, exclude: i64) -> Option<Eta> {
        self.estimate_with(kind, None, warmth, exclude)
    }

    /// Median execution time of successful runs of `kind` with the same
    /// warmth, else of any warmth (said in `basis`); then, with a `coarse`
    /// key, the same two steps over similar commands. None without history.
    pub fn estimate_with(
        &self,
        kind: &str,
        coarse: Option<&str>,
        warmth: Warmth,
        exclude: i64,
    ) -> Option<Eta> {
        let passed = |same: bool| -> Vec<f64> {
            self.runs(kind, exclude)
                .filter(|r| r.passed && (!same || r.warmth == warmth))
                .filter_map(|r| r.exec_s)
                .take(MAX_SAMPLES)
                .collect()
        };
        let label = |n: usize| if n == 1 { "run" } else { "runs" };
        let mut same = passed(true);
        if let Some(total) = median(&mut same) {
            let n = same.len();
            let basis = match warmth {
                Warmth::Unknown => format!(
                    "median of {n} {} of this kind on this machine (warmth unknown)",
                    label(n)
                ),
                w => format!(
                    "median of {n} {} {} of this kind on this machine",
                    w.as_str(),
                    label(n)
                ),
            };
            return Some(Eta {
                total_s: total,
                samples: n as u32,
                basis,
            });
        }
        let mut any = passed(false);
        if let Some(total) = median(&mut any) {
            let n = any.len();
            return Some(Eta {
                total_s: total,
                samples: n as u32,
                basis: format!(
                    "median of {n} {} of this kind on this machine, any warmth (no {} history)",
                    label(n),
                    warmth.as_str()
                ),
            });
        }
        let coarse = coarse?;
        let similar = |same: bool| -> Vec<f64> {
            self.by_coarse
                .get(coarse)
                .map(Vec::as_slice)
                .unwrap_or(&[])
                .iter()
                .filter(|r| r.id != exclude && r.passed && (!same || r.warmth == warmth))
                .filter_map(|r| r.exec_s)
                .take(MAX_SAMPLES)
                .collect()
        };
        let mut same = similar(true);
        let (mut found, note) = if median(&mut same).is_some() {
            let note = match warmth {
                Warmth::Unknown => " (warmth unknown)".to_string(),
                w => format!(", {}", w.as_str()),
            };
            (same, note)
        } else {
            (similar(false), ", any warmth".to_string())
        };
        let total = median(&mut found)?;
        let n = found.len();
        Some(Eta {
            total_s: total,
            samples: n as u32,
            basis: format!(
                "median of {n} {} of similar commands ({coarse}) on this machine{note}",
                label(n)
            ),
        })
    }

    /// Median crate count of earlier successful runs of `kind` (a failed
    /// build may stop early): cold runs when `warmth` is cold, else warm runs.
    /// None without such history.
    pub fn crates_expected(&self, kind: &str, warmth: Warmth, exclude: i64) -> Option<u64> {
        let want = if warmth == Warmth::Cold {
            Warmth::Cold
        } else {
            Warmth::Warm
        };
        let mut counts: Vec<f64> = self
            .runs(kind, exclude)
            .filter(|r| r.passed && r.warmth == want)
            .filter_map(|r| r.crates)
            .take(MAX_SAMPLES)
            .map(|c| c as f64)
            .collect();
        median(&mut counts).map(|m| m.round() as u64)
    }

    /// Reference size R for the warmth rule: median of cold runs' crate
    /// counts, else the largest count of any run. None without counts.
    pub fn reference_crates(&self, kind: &str, exclude: i64) -> Option<u64> {
        let mut cold: Vec<f64> = self
            .runs(kind, exclude)
            .filter(|r| r.passed && r.warmth == Warmth::Cold)
            .filter_map(|r| r.crates)
            .take(MAX_SAMPLES)
            .map(|c| c as f64)
            .collect();
        if let Some(m) = median(&mut cold) {
            return Some(m.round() as u64);
        }
        self.runs(kind, exclude)
            .filter(|r| r.passed)
            .filter_map(|r| r.crates)
            .filter(|&c| c > 0)
            .max()
    }
}
