//! The two kinds of number the rest of the system gets from history.
//!
//! A [`Shown`] is what a person is told ("ETA", "time left", a progress bar): it exists only when
//! passing runs back it, and otherwise says why not. A [`Planning`] is a number the scheduler
//! needs to order a queue or score a machine; it may be a guess and must never be printed as
//! an ETA. The two are different types so a guess cannot reach a screen by accident.

use super::{Estimate, MachineInfo, Source, build};
use crate::dur::fmt_eta;
use crate::error::TimingError;
use crate::history::{Cache, History, Outcome, Sample, SampleKey};
use std::fmt;

/// Why there is no estimate to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoEstimate {
    /// Nothing has ever finished for this command, anywhere.
    FirstRun,
    /// It has run before but never passed (failed runs stop early and say nothing about the
    /// time a good run takes).
    NoPassingRun,
    /// It passed elsewhere, but not here with the build state this run starts from.
    FirstBuildHere,
    /// The job runs on a peer; its progress is not measured here.
    OnPeer,
    /// The timing history could not be read.
    HistoryUnreadable,
}

impl fmt::Display for NoEstimate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NoEstimate::FirstRun => "first run",
            NoEstimate::NoPassingRun => "no passing run yet",
            NoEstimate::FirstBuildHere => "first build here",
            NoEstimate::OnPeer => "runs on a peer",
            NoEstimate::HistoryUnreadable => "the timing history could not be read",
        })
    }
}

/// An estimate backed by passing runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Eta {
    /// Median seconds.
    pub median_secs: f64,
    /// 90th percentile seconds.
    pub p90_secs: f64,
    /// Passing runs behind it.
    pub samples: usize,
    /// The machine whose runs these are.
    pub machine: String,
    /// True when they are another machine's runs, scaled by core count.
    pub rough: bool,
}

impl Eta {
    /// `4m`, or `~4m (rough, from nixos's runs)`.
    pub fn text(&self) -> String {
        if self.rough {
            format!(
                "~{} (rough, from {}'s runs)",
                fmt_eta(self.median_secs),
                self.machine
            )
        } else {
            format!("~{}", fmt_eta(self.median_secs))
        }
    }
}

/// Time left of a running job against its estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Left {
    /// This many seconds are left.
    Secs(f64),
    /// The run is past what passing runs took: it is not "0s left", it is slower than usual.
    Over {
        /// The usual (median) seconds.
        usual_secs: f64,
        /// How far past it the run is.
        by_secs: f64,
    },
}

impl Eta {
    /// What is left after `elapsed` seconds. `progress_left` is the per-target sum when the job
    /// reports targets: while it still sees work ahead it is believed; once it has nothing (or
    /// nothing positive) left and the run is past the usual time, the answer is [`Left::Over`]
    /// rather than a zero.
    pub fn left(&self, progress_left: Option<f64>, elapsed: f64) -> Left {
        if let Some(p) = progress_left
            && p > 0.5
        {
            return Left::Secs(p);
        }
        if elapsed >= self.median_secs {
            return Left::Over {
                usual_secs: self.median_secs,
                by_secs: elapsed - self.median_secs,
            };
        }
        Left::Secs(progress_left.map_or(self.median_secs - elapsed, |p| {
            p.max(self.median_secs - elapsed)
        }))
    }
}

/// What may be shown as the duration of a run: an estimate, or the reason there is none.
#[derive(Debug, Clone, PartialEq)]
pub enum Shown {
    /// Passing runs back this number.
    Eta(Eta),
    /// Nothing honest to show.
    Absent(NoEstimate),
}

impl Shown {
    /// The estimate, when there is one.
    pub fn eta(&self) -> Option<&Eta> {
        match self {
            Shown::Eta(e) => Some(e),
            Shown::Absent(_) => None,
        }
    }

    /// The reason there is none.
    pub fn why_not(&self) -> Option<NoEstimate> {
        match self {
            Shown::Eta(_) => None,
            Shown::Absent(w) => Some(*w),
        }
    }
}

impl fmt::Display for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Shown::Eta(e) if e.rough => write!(f, "{} (p90 {})", e.text(), fmt_eta(e.p90_secs)),
            Shown::Eta(e) => {
                let runs = if e.samples == 1 { "run" } else { "runs" };
                write!(
                    f,
                    "{} (p90 {}) from {} passing {runs} on {}",
                    e.text(),
                    fmt_eta(e.p90_secs),
                    e.samples,
                    e.machine
                )
            }
            Shown::Absent(w) => write!(f, "no estimate: {w}"),
        }
    }
}

/// What a planning number rests on, weakest last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Passing runs of this command (here or elsewhere).
    Passed,
    /// Only failed runs, or the kind's history: a rough guide for ordering, nothing to show.
    Guess,
    /// The built-in default for the kind.
    BuiltIn,
}

/// A duration the scheduler plans with. Never shown as an ETA.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Planning {
    /// Median seconds.
    pub median_secs: f64,
    /// 90th percentile seconds.
    pub p90_secs: f64,
    /// Runs behind it (0 for the built-in default).
    pub samples: usize,
    /// What it rests on.
    pub basis: Basis,
}

impl Planning {
    /// True when passing runs of the command back the number (placement may call it known).
    pub fn is_measured(&self) -> bool {
        self.basis == Basis::Passed
    }
}

/// The warmth a run starts from: warm once this machine has a passing run of the command,
/// cold before that. Runs are recorded with it, and estimates ask for it.
pub fn wanted_cache(local: &[Sample]) -> Cache {
    if local.iter().any(|s| s.outcome == Outcome::Passed) {
        Cache::Warm
    } else {
        Cache::Cold
    }
}

fn finite(s: &Sample) -> bool {
    s.secs.is_finite() && s.secs >= 0.0
}

/// A passing sample of this machine that stands for a run starting with warmth `want`: a warm
/// run is bounded by any passing run here (a cold one only overestimates), a cold run only by
/// cold ones; a sample recorded before warmth was kept (`Unknown`) counts for both.
fn fits_here(s: &Sample, want: Cache) -> bool {
    s.outcome == Outcome::Passed
        && finite(s)
        && match want {
            Cache::Warm => true,
            Cache::Cold | Cache::Unknown => matches!(s.cache, Cache::Cold | Cache::Unknown),
        }
}

/// Another machine's sample counts only when its warmth is known to be the wanted one.
fn fits_there(s: &Sample, want: Cache) -> bool {
    s.outcome == Outcome::Passed && finite(s) && s.cache == want
}

fn window(mut v: Vec<Sample>) -> Vec<f64> {
    v.sort_by_key(|s| std::cmp::Reverse(s.at_ms));
    v.truncate(super::WINDOW);
    let mut secs: Vec<f64> = v.iter().map(|s| s.secs).collect();
    secs.sort_by(f64::total_cmp);
    secs
}

/// What to show as the duration of `key` run on `machine` from build state `want`.
///
/// Only passing runs count. This machine's own come first, exact; otherwise another machine's
/// of the same warmth, scaled by cores and marked rough; otherwise nothing, with the reason.
/// The kind's default is never offered.
pub fn shown(
    history: &dyn History,
    key: &SampleKey,
    machine: &MachineInfo,
    others: &[MachineInfo],
    want: Cache,
) -> Result<Shown, TimingError> {
    let at = |m: &str| SampleKey {
        recipe: key.recipe.clone(),
        target: key.target.clone(),
        machine: m.to_string(),
    };
    let here = history.samples(&at(&machine.name))?;
    let mut any_sample = !here.is_empty();
    let mut any_passed = here.iter().any(|s| s.outcome == Outcome::Passed);
    let mine = window(here.into_iter().filter(|s| fits_here(s, want)).collect());
    if !mine.is_empty() {
        let (median_secs, p90_secs) = build(&mine, 1.0);
        return Ok(Shown::Eta(Eta {
            median_secs,
            p90_secs,
            samples: mine.len(),
            machine: machine.name.clone(),
            rough: false,
        }));
    }
    let mut best: Option<(Vec<f64>, &MachineInfo)> = None;
    for o in others
        .iter()
        .filter(|o| o.name != machine.name && o.cores > 0)
    {
        let theirs = history.samples(&at(&o.name))?;
        any_sample |= !theirs.is_empty();
        any_passed |= theirs.iter().any(|s| s.outcome == Outcome::Passed);
        let secs = window(theirs.into_iter().filter(|s| fits_there(s, want)).collect());
        if !secs.is_empty() && best.as_ref().is_none_or(|(b, _)| secs.len() > b.len()) {
            best = Some((secs, o));
        }
    }
    if let Some((secs, o)) = best {
        let scaled_by = f64::from(o.cores) / f64::from(machine.cores.max(1));
        let (median_secs, p90_secs) = build(&secs, scaled_by);
        return Ok(Shown::Eta(Eta {
            median_secs,
            p90_secs,
            samples: secs.len(),
            machine: o.name.clone(),
            rough: true,
        }));
    }
    Ok(Shown::Absent(if any_passed {
        NoEstimate::FirstBuildHere
    } else if any_sample {
        NoEstimate::NoPassingRun
    } else {
        NoEstimate::FirstRun
    }))
}

/// A number to plan with for `key` on `machine`: passing runs here, then another machine's
/// scaled, then failed runs, then the kind's history, then the built-in default. Always
/// answers; check [`Planning::basis`] before treating it as known.
pub fn planning(
    history: &dyn History,
    key: &SampleKey,
    machine: &MachineInfo,
    others: &[MachineInfo],
) -> Result<Planning, TimingError> {
    let e = super::estimate_with_cache(history, key, machine, others, None)?;
    let passed = |m: &str| -> Result<bool, TimingError> {
        let k = SampleKey {
            machine: m.to_string(),
            ..key.clone()
        };
        Ok(history
            .samples(&k)?
            .iter()
            .any(|s| s.outcome == Outcome::Passed && finite(s)))
    };
    let basis = match &e.source {
        super::Source::KindDefault if e.samples == 0 => Basis::BuiltIn,
        super::Source::KindDefault => Basis::Guess,
        _ if passed(&e.machine)? => Basis::Passed,
        _ => Basis::Guess,
    };
    Ok(Planning {
        median_secs: e.median_secs,
        p90_secs: e.p90_secs,
        samples: e.samples,
        basis,
    })
}

impl From<&Eta> for Estimate {
    /// For the per-target progress sums, which take [`Estimate`]s.
    fn from(e: &Eta) -> Estimate {
        Estimate {
            median_secs: e.median_secs,
            p90_secs: e.p90_secs,
            samples: e.samples,
            source: if e.rough {
                Source::OtherMachine {
                    name: e.machine.clone(),
                    scaled_by: 1.0,
                }
            } else {
                Source::SameMachine
            },
            machine: e.machine.clone(),
            cache_fallback: false,
        }
    }
}
