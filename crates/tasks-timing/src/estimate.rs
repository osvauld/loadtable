//! Median/p90 estimates from history, with machine and kind fallbacks.

use crate::dur::fmt_eta;
use crate::error::TimingError;
use crate::history::{Cache, History, Outcome, Sample, SampleKey};
use std::fmt;
use tasks_core::Kind;

/// Only the most recent samples count, so the estimate follows changes in the code.
pub const WINDOW: usize = 20;

/// A machine's name and core count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineInfo {
    /// Machine name.
    pub name: String,
    /// Logical cores.
    pub cores: u32,
}

/// Where an estimate came from.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Runs on this machine.
    SameMachine,
    /// Runs on another machine, multiplied by `scaled_by` (other cores / this cores).
    OtherMachine {
        /// The machine the samples came from.
        name: String,
        /// Factor applied to its durations.
        scaled_by: f64,
    },
    /// The kind's history (`kind:<Kind>` recipe) or the built-in default.
    KindDefault,
}

/// A duration estimate with provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    /// Median seconds.
    pub median_secs: f64,
    /// 90th percentile seconds.
    pub p90_secs: f64,
    /// Number of samples behind it (0 for the built-in default).
    pub samples: usize,
    /// Where it came from.
    pub source: Source,
    /// The machine the samples were measured on.
    pub machine: String,
    /// True when no sample had the requested warmth and all were used.
    pub cache_fallback: bool,
}

impl fmt::Display for Estimate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let runs = if self.samples == 1 { "run" } else { "runs" };
        write!(
            f,
            "ETA {} (p90 {}) ",
            fmt_eta(self.median_secs),
            fmt_eta(self.p90_secs)
        )?;
        match &self.source {
            Source::SameMachine => write!(f, "from {} {runs} on {}", self.samples, self.machine)?,
            Source::OtherMachine { name, scaled_by } => write!(
                f,
                "from {} {runs} on {name}, scaled x{scaled_by:.1}",
                self.samples
            )?,
            Source::KindDefault if self.samples == 0 => {
                write!(f, "from the built-in default, no history")?
            }
            Source::KindDefault => write!(
                f,
                "from {} similar {runs} on {}",
                self.samples, self.machine
            )?,
        }
        if self.cache_fallback {
            write!(f, "; no runs with matching warmth")?;
        }
        Ok(())
    }
}

/// Built-in `(median, p90)` seconds per kind, used when nothing has been measured.
pub fn kind_default(kind: Kind) -> (f64, f64) {
    match kind {
        Kind::Compile => (300.0, 900.0),
        Kind::Test => (180.0, 600.0),
        Kind::Run => (60.0, 300.0),
        Kind::Package => (240.0, 600.0),
        Kind::Device => (120.0, 300.0),
        Kind::Probe => (2.0, 5.0),
    }
}

/// The history recipe string under which a store keeps per-kind aggregates.
pub fn kind_recipe(kind: Kind) -> String {
    format!("kind:{kind:?}")
}

/// Guesses the kind from a recipe display string.
fn kind_of(recipe: &str) -> Kind {
    let mut w = recipe.split_whitespace();
    let (tool, sub) = (w.next().unwrap_or(""), w.next().unwrap_or(""));
    match (tool, sub) {
        ("cargo", "test" | "nextest" | "bench") => Kind::Test,
        ("cargo", "run") => Kind::Run,
        ("cargo", _) => Kind::Compile,
        ("true" | "false" | "echo" | "sleep" | "exit" | ":", _) => Kind::Probe,
        ("gradle", _) => {
            let l = recipe.to_ascii_lowercase();
            if l.contains("connected") || l.contains("install") {
                Kind::Device
            } else if l.contains("test") {
                Kind::Test
            } else if l.contains("assemble") || l.contains("bundle") {
                Kind::Package
            } else {
                Kind::Compile
            }
        }
        _ => Kind::Run,
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    let n = sorted.len();
    let rank = (q * n as f64).ceil() as usize;
    sorted
        .get(rank.clamp(1, n.max(1)) - 1)
        .copied()
        .unwrap_or(0.0)
}

/// Usable samples: the most recent window, passed ones if any passed, never cancelled ones.
fn usable(mut v: Vec<Sample>, want: Option<Cache>) -> (Vec<f64>, bool) {
    v.retain(|s| s.outcome != Outcome::Cancelled && s.secs.is_finite() && s.secs >= 0.0);
    if v.iter().any(|s| s.outcome == Outcome::Passed) {
        v.retain(|s| s.outcome == Outcome::Passed);
    }
    let mut fallback = false;
    if let Some(w) = want {
        if v.iter().any(|s| s.cache == w) {
            v.retain(|s| s.cache == w);
        } else if !v.is_empty() {
            fallback = true;
        }
    }
    v.sort_by_key(|s| std::cmp::Reverse(s.at_ms));
    v.truncate(WINDOW);
    let mut secs: Vec<f64> = v.iter().map(|s| s.secs).collect();
    secs.sort_by(f64::total_cmp);
    (secs, fallback)
}

fn build(secs: &[f64], scale: f64) -> (f64, f64) {
    let n = secs.len();
    let med = if n % 2 == 1 {
        secs.get(n / 2).copied().unwrap_or(0.0)
    } else {
        (secs.get((n / 2).saturating_sub(1)).copied().unwrap_or(0.0)
            + secs.get(n / 2).copied().unwrap_or(0.0))
            / 2.0
    };
    (med * scale, percentile(secs, 0.9) * scale)
}

/// Estimates `key` on `machine`, ignoring sample warmth.
pub fn estimate(
    history: &dyn History,
    key: &SampleKey,
    machine: &MachineInfo,
    others: &[MachineInfo],
) -> Result<Estimate, TimingError> {
    estimate_with_cache(history, key, machine, others, None)
}

/// Like [`estimate`], preferring samples with warmth `want` and noting when it falls back.
pub fn estimate_with_cache(
    history: &dyn History,
    key: &SampleKey,
    machine: &MachineInfo,
    others: &[MachineInfo],
    want: Option<Cache>,
) -> Result<Estimate, TimingError> {
    let at = |m: &str, recipe: Option<&str>| SampleKey {
        recipe: recipe.map_or_else(|| key.recipe.clone(), str::to_string),
        target: if recipe.is_some() {
            None
        } else {
            key.target.clone()
        },
        machine: m.to_string(),
    };
    let (secs, fallback) = usable(history.samples(&at(&machine.name, None))?, want);
    if !secs.is_empty() {
        let (median_secs, p90_secs) = build(&secs, 1.0);
        return Ok(Estimate {
            median_secs,
            p90_secs,
            samples: secs.len(),
            source: Source::SameMachine,
            machine: machine.name.clone(),
            cache_fallback: fallback,
        });
    }
    let mut best: Option<(Vec<f64>, bool, &MachineInfo)> = None;
    for o in others
        .iter()
        .filter(|o| o.name != machine.name && o.cores > 0)
    {
        let (s, fb) = usable(history.samples(&at(&o.name, None))?, want);
        if best.as_ref().is_none_or(|(b, _, _)| s.len() > b.len()) && !s.is_empty() {
            best = Some((s, fb, o));
        }
    }
    if let Some((s, fallback, o)) = best {
        let scaled_by = f64::from(o.cores) / f64::from(machine.cores.max(1));
        let (median_secs, p90_secs) = build(&s, scaled_by);
        return Ok(Estimate {
            median_secs,
            p90_secs,
            samples: s.len(),
            source: Source::OtherMachine {
                name: o.name.clone(),
                scaled_by,
            },
            machine: o.name.clone(),
            cache_fallback: fallback,
        });
    }
    let kind = kind_of(&key.recipe);
    let krecipe = kind_recipe(kind);
    let (secs, _) = usable(history.samples(&at(&machine.name, Some(&krecipe)))?, None);
    if !secs.is_empty() {
        let (median_secs, p90_secs) = build(&secs, 1.0);
        return Ok(Estimate {
            median_secs,
            p90_secs,
            samples: secs.len(),
            source: Source::KindDefault,
            machine: machine.name.clone(),
            cache_fallback: false,
        });
    }
    let (median_secs, p90_secs) = kind_default(kind);
    Ok(Estimate {
        median_secs,
        p90_secs,
        samples: 0,
        source: Source::KindDefault,
        machine: machine.name.clone(),
        cache_fallback: false,
    })
}

mod shown;
pub use shown::{
    Basis, Eta, EtaBasis, Left, NoEstimate, Planning, RECENT, Shown, planning, shown, wanted_cache,
};

#[cfg(test)]
mod shown_tests;
#[cfg(test)]
mod tests;
