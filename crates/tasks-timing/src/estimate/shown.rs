//! The two kinds of number the rest of the system gets from history.
//!
//! A [`Shown`] is what a person is told ("ETA", "time left", a progress bar): it exists only when
//! passing runs back it, and says which runs ([`EtaBasis`]); with none it says why not. A
//! [`Planning`] is a number the scheduler needs to order a queue or score a machine; it may be
//! a guess and must never be printed as an ETA. The two are different types so a guess cannot
//! reach a screen by accident.
//!
//! A command with passing runs of related work here always gets a number (R-823): its own runs
//! first, then its family's ([`crate::family`]), another machine's, the tool's builds here as a
//! range, and last its own runs of the other warmth as a bound. Runs of the wrong warmth never
//! make a plain estimate: a rebuild is not timed by a cold build, nor the other way round.

use super::{Estimate, MachineInfo, Source, build};
use crate::dur::fmt_eta;
use crate::error::TimingError;
use crate::family::{family, ranged, tool};
use crate::history::{Cache, History, Outcome, Sample, SampleKey};
use std::fmt;

/// The most recent passing runs an estimate shown to a person rests on: few enough to follow
/// the code as it changes, enough that one odd run does not decide it.
pub const RECENT: usize = 6;
/// The runs the p90 looks back over (R-826): six runs cannot see a tail (their "p90" is their
/// slowest), so the spread comes from more of them than the median does.
pub const TAIL: usize = 20;
/// The p90 is never under this times the median (R-826): with few runs the tail is unseen. In
/// the 852 whole-job runs recorded up to 29 Oct 2026 the real time was past the six-run p90
/// one time in three; with the floor, one in six.
pub const P90_FLOOR: f64 = 1.5;

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
    /// It passed here only from a cold build; this run starts warm (R-800).
    NoWarmRunHere,
    /// The job runs on a peer; its progress is not measured here.
    OnPeer,
    /// The timing history could not be read.
    HistoryUnreadable,
    /// The job is interactive (R-799): it runs as long as someone uses it.
    Interactive,
}

impl fmt::Display for NoEstimate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NoEstimate::FirstRun => "first run",
            NoEstimate::NoPassingRun => "no passing run yet",
            NoEstimate::FirstBuildHere => "first build here",
            NoEstimate::NoWarmRunHere => "no warm run here yet",
            NoEstimate::OnPeer => "runs on a peer",
            NoEstimate::HistoryUnreadable => "the timing history could not be read",
            NoEstimate::Interactive => "interactive: runs as long as it is used",
        })
    }
}

/// Which passing runs an estimate rests on, closest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EtaBasis {
    /// This command's runs here, of the wanted warmth.
    Exact,
    /// Runs here of commands in the same family (R-824), of the wanted warmth.
    Similar,
    /// This command's runs on another machine, scaled by core count.
    Peer,
    /// The tool's builds here of the wanted warmth, as a range: `tool` names them.
    Range {
        /// The tool (`gradle`).
        tool: String,
    },
    /// Only cold runs of this work so far and this run starts warm: at most that long.
    AtMost,
    /// Only warm runs of this work so far and this run starts cold: at least that long.
    AtLeast,
}

/// An estimate backed by passing runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Eta {
    /// Median seconds (the bound, for [`EtaBasis::AtMost`] and [`EtaBasis::AtLeast`]).
    pub median_secs: f64,
    /// 90th percentile seconds.
    pub p90_secs: f64,
    /// The fastest of the recent runs (none for a bound).
    pub low_secs: Option<f64>,
    /// The slowest of the recent runs, or the last twenty runs' p90 when that is longer (none
    /// for a bound, R-837).
    pub high_secs: Option<f64>,
    /// Passing runs behind it.
    pub samples: usize,
    /// The machine whose runs these are.
    pub machine: String,
    /// True unless these are this command's own runs here ([`EtaBasis::Exact`]).
    pub rough: bool,
    /// Which runs.
    pub basis: EtaBasis,
}

/// `~4m`, or `~5s–56s` when the fastest and slowest runs round apart (R-837).
pub fn range_text(low: f64, high: f64) -> String {
    let (lo, hi) = (fmt_eta(low), fmt_eta(high));
    if lo == hi {
        format!("~{hi}")
    } else {
        format!("~{lo}–{hi}")
    }
}

impl Eta {
    /// ` (median 16s)` when the estimate is a range, else nothing.
    fn median_note(&self) -> String {
        match (self.low_secs, self.high_secs) {
            (Some(lo), Some(hi)) if fmt_eta(lo) != fmt_eta(hi) => {
                format!(" (median {})", fmt_eta(self.median_secs))
            }
            _ => String::new(),
        }
    }

    /// `~4m`, `~5s–56s`, `~27m (like 6 similar builds here)`, `~8m (rough, from nixos's
    /// runs)`, `~12m–31m (rough: gradle builds here)`, `under ~27m (only cold builds so far)`
    /// or `over ~30s (first build here)`. A range runs from the fastest recent run to the
    /// slowest (R-837): one number would be wrong whenever the amount to rebuild changes.
    pub fn text(&self) -> String {
        let med = fmt_eta(self.median_secs);
        let range = match (self.low_secs, self.high_secs) {
            (Some(lo), Some(hi)) => range_text(lo, hi),
            _ => format!("~{med}"),
        };
        match &self.basis {
            EtaBasis::Exact => range,
            EtaBasis::Similar => {
                let builds = if self.samples == 1 { "build" } else { "builds" };
                format!("{range} (like {} similar {builds} here)", self.samples)
            }
            EtaBasis::Peer => format!("{range} (rough, from {}'s runs)", self.machine),
            EtaBasis::Range { tool } => format!("{range} (rough: {tool} builds here)"),
            EtaBasis::AtMost => format!("under ~{med} (only cold builds so far)"),
            EtaBasis::AtLeast => format!("over ~{med} (first build here)"),
        }
    }
}

/// Time left of a running job against its estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Left {
    /// This many seconds are left.
    Secs(f64),
    /// Between this many seconds (to the fastest recent run, 0 once past it) and that many (to
    /// the slowest) are left (R-837).
    Between {
        /// Seconds to the fastest run; 0 once the run is past it.
        low_secs: f64,
        /// Seconds to the slowest run.
        high_secs: f64,
    },
    /// The run is past every recent passing run: it is not "0s left", it is slower than all of
    /// them (R-675, R-837).
    Over {
        /// The fastest recent run, in seconds (the bound itself for a bound).
        fastest_secs: f64,
        /// The slowest recent run, in seconds: what the run is past.
        slowest_secs: f64,
        /// How far past it the run is.
        by_secs: f64,
    },
}

impl Eta {
    /// What is left after `elapsed` seconds. `progress_left` is the per-target sum when the job
    /// reports targets: while it still sees work ahead it is believed. Otherwise the run is
    /// measured against its recent runs: time to the fastest and to the slowest while it is
    /// under the slowest, and [`Left::Over`] once it is past them all, rather than a zero or
    /// "over" for a run still inside what the recent runs took (R-837).
    pub fn left(&self, progress_left: Option<f64>, elapsed: f64) -> Left {
        if let Some(p) = progress_left
            && p > 0.5
        {
            return Left::Secs(p);
        }
        let (low, high) = match (self.low_secs, self.high_secs) {
            (Some(lo), Some(hi)) => (lo, hi.max(lo)),
            _ => (self.median_secs, self.median_secs),
        };
        if elapsed >= high {
            return Left::Over {
                fastest_secs: low,
                slowest_secs: high,
                by_secs: elapsed - high,
            };
        }
        let floor = progress_left.unwrap_or(0.0).max(0.0);
        let high_left = (high - elapsed).max(floor);
        let low_left = (low - elapsed).max(floor).min(high_left);
        if fmt_eta(low_left) == fmt_eta(high_left) {
            Left::Secs(high_left)
        } else {
            Left::Between {
                low_secs: low_left,
                high_secs: high_left,
            }
        }
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
            Shown::Eta(e) => {
                let runs = if e.samples == 1 { "run" } else { "runs" };
                match e.basis {
                    EtaBasis::Exact => write!(
                        f,
                        "{}{} from {} passing {runs} on {}",
                        e.text(),
                        e.median_note(),
                        e.samples,
                        e.machine
                    ),
                    EtaBasis::Similar | EtaBasis::Peer => {
                        write!(f, "{}{}", e.text(), e.median_note())
                    }
                    EtaBasis::Range { .. } | EtaBasis::AtMost | EtaBasis::AtLeast => write!(
                        f,
                        "{}, from {} passing {runs} on {}",
                        e.text(),
                        e.samples,
                        e.machine
                    ),
                }
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

/// The passing samples of this machine that stand for a run starting with warmth `want`. A
/// measured warmth (a cargo build's output directory, R-800) takes runs of that warmth only, so
/// a rebuild is never timed by a cold build or the other way round; samples whose warmth was
/// not known stand in only while there is no run of the wanted warmth. `Unknown` (the warmth is
/// only guessed from history) takes the warm runs when there are any, else every passing run.
fn fitting_here(here: &[Sample], want: Cache) -> Vec<Sample> {
    let passed = |s: &&Sample| s.outcome == Outcome::Passed && finite(s);
    let wanted = if want == Cache::Unknown {
        if here.iter().filter(passed).any(|s| s.cache == Cache::Warm) {
            Cache::Warm
        } else {
            return here.iter().filter(passed).cloned().collect();
        }
    } else {
        want
    };
    let exact: Vec<Sample> = here
        .iter()
        .filter(passed)
        .filter(|s| s.cache == wanted)
        .cloned()
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    here.iter()
        .filter(passed)
        .filter(|s| s.cache == Cache::Unknown)
        .cloned()
        .collect()
}

/// Another machine's sample counts only when its warmth is known to be the wanted one; a
/// guessed warmth (`Unknown`) takes its warm runs.
fn fits_there(s: &Sample, want: Cache) -> bool {
    let want = if want == Cache::Unknown {
        Cache::Warm
    } else {
        want
    };
    s.outcome == Outcome::Passed && finite(s) && s.cache == want
}

/// The seconds of the most recent runs, each list sorted: the last [`RECENT`], which the
/// median comes from, and the last [`TAIL`], which the p90 comes from.
struct Runs {
    recent: Vec<f64>,
    tail: Vec<f64>,
}

fn window(mut v: Vec<Sample>) -> Runs {
    v.sort_by_key(|s| std::cmp::Reverse(s.at_ms));
    let mut recent: Vec<f64> = v.iter().take(RECENT).map(|s| s.secs).collect();
    let mut tail: Vec<f64> = v.iter().take(TAIL).map(|s| s.secs).collect();
    recent.sort_by(f64::total_cmp);
    tail.sort_by(f64::total_cmp);
    Runs { recent, tail }
}

/// The median of the recent runs and the p90 of the tail, never under [`P90_FLOOR`] times the
/// median, both scaled.
fn spread(runs: &Runs, scale: f64) -> (f64, f64) {
    let (median, _) = build(&runs.recent, scale);
    let (_, tail_p90) = build(&runs.tail, scale);
    (median, tail_p90.max(median * P90_FLOOR))
}

fn eta(runs: &Runs, scale: f64, machine: &str, basis: EtaBasis) -> Shown {
    let (median_secs, p90_secs) = spread(runs, scale);
    Shown::Eta(Eta {
        median_secs,
        p90_secs,
        low_secs: runs.recent.first().map(|s| s * scale),
        // the slowest recent run, or the tail's measured p90 (not the floor) when it is longer:
        // six runs alone were beaten one run in three (R-826, R-837)
        high_secs: runs
            .recent
            .last()
            .map(|s| (s * scale).max(build(&runs.tail, scale).1)),
        samples: runs.recent.len(),
        machine: machine.to_string(),
        rough: basis != EtaBasis::Exact,
        basis,
    })
}

/// A bound from this work's runs of the other warmth: a rebuild takes at most what the cold
/// builds took (their median), a first build at least what the rebuilds took (their slowest).
fn bound(here: &[Sample], want: Cache, machine: &str) -> Option<Shown> {
    let (other, basis) = match want {
        Cache::Warm => (Cache::Cold, EtaBasis::AtMost),
        Cache::Cold => (Cache::Warm, EtaBasis::AtLeast),
        Cache::Unknown => return None,
    };
    let secs = window(
        here.iter()
            .filter(|s| s.outcome == Outcome::Passed && finite(s) && s.cache == other)
            .copied()
            .collect(),
    )
    .recent;
    let at = match basis {
        EtaBasis::AtMost => build(&secs, 1.0).0,
        _ => secs.last().copied()?,
    };
    (!secs.is_empty()).then(|| {
        Shown::Eta(Eta {
            median_secs: at,
            p90_secs: at,
            low_secs: None,
            high_secs: None,
            samples: secs.len(),
            machine: machine.to_string(),
            rough: true,
            basis,
        })
    })
}

/// What to show as the duration of `key` run on `machine` from build state `want`.
///
/// Only passing runs count, and only of the wanted warmth while any exist (R-800). In order
/// (R-823): this command's own runs here; its family's runs here ("like 6 similar builds",
/// R-824); its runs on another machine, scaled by cores; the tool's builds here as a range;
/// its own or its family's runs of the other warmth as a bound ("under ~27m"). Otherwise
/// nothing, with the reason. The kind's default is never offered.
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
    let passed_here = here.iter().any(|s| s.outcome == Outcome::Passed);
    let mut any_passed = passed_here;
    let mine = window(fitting_here(&here, want));
    if !mine.recent.is_empty() {
        return Ok(eta(&mine, 1.0, &machine.name, EtaBasis::Exact));
    }
    // relatives: the family's whole-job runs here, and (for a tool whose builds make a range,
    // R-827) the tool's builds that have a family
    let fam = key.target.is_none().then(|| family(&key.recipe)).flatten();
    let (kin, builds): (Vec<Sample>, Vec<Sample>) = match &fam {
        Some(f) => {
            let all = history.machine_samples(&machine.name, tool(&key.recipe))?;
            let with: Vec<(Option<String>, Sample)> =
                all.into_iter().map(|(r, s)| (family(&r), s)).collect();
            (
                with.iter()
                    .filter(|(g, _)| g.as_ref() == Some(f))
                    .map(|(_, s)| *s)
                    .collect(),
                with.iter()
                    .filter(|(g, _)| g.is_some() && ranged(tool(&key.recipe)))
                    .map(|(_, s)| *s)
                    .collect(),
            )
        }
        None => (Vec::new(), Vec::new()),
    };
    let similar = window(fitting_here(&kin, want));
    if !similar.recent.is_empty() {
        return Ok(eta(&similar, 1.0, &machine.name, EtaBasis::Similar));
    }
    let mut best: Option<(Runs, &MachineInfo)> = None;
    for o in others
        .iter()
        .filter(|o| o.name != machine.name && o.cores > 0)
    {
        let theirs = history.samples(&at(&o.name))?;
        any_sample |= !theirs.is_empty();
        any_passed |= theirs.iter().any(|s| s.outcome == Outcome::Passed);
        let runs = window(theirs.into_iter().filter(|s| fits_there(s, want)).collect());
        if !runs.recent.is_empty()
            && best
                .as_ref()
                .is_none_or(|(b, _)| runs.recent.len() > b.recent.len())
        {
            best = Some((runs, o));
        }
    }
    if let Some((runs, o)) = best {
        let scaled_by = f64::from(o.cores) / f64::from(machine.cores.max(1));
        return Ok(eta(&runs, scaled_by, &o.name, EtaBasis::Peer));
    }
    // the tool's builds here of the wanted warmth only: a range, never a mix of warmths
    let wanted = if want == Cache::Unknown {
        Cache::Warm
    } else {
        want
    };
    let range = window(
        builds
            .iter()
            .filter(|s| s.outcome == Outcome::Passed && finite(s) && s.cache == wanted)
            .copied()
            .collect(),
    );
    if !range.recent.is_empty() {
        let tool = tool(&key.recipe).to_string();
        return Ok(eta(&range, 1.0, &machine.name, EtaBasis::Range { tool }));
    }
    let own_or_kin = if here.iter().any(|s| s.outcome == Outcome::Passed) {
        &here
    } else {
        &kin
    };
    if let Some(b) = bound(own_or_kin, want, &machine.name) {
        return Ok(b);
    }
    Ok(Shown::Absent(if passed_here && want != Cache::Cold {
        NoEstimate::NoWarmRunHere
    } else if any_passed {
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
    // no passing run of its own anywhere: its family's runs here are measured work of the same
    // cost, and the tool's builds here a better guess than the kind's (R-823)
    if basis != Basis::Passed
        && let Shown::Eta(s) = shown(history, key, machine, &[], Cache::Unknown)?
        && matches!(s.basis, EtaBasis::Similar | EtaBasis::Range { .. })
    {
        return Ok(Planning {
            median_secs: s.median_secs,
            p90_secs: s.p90_secs,
            samples: s.samples,
            basis: if s.basis == EtaBasis::Similar {
                Basis::Passed
            } else {
                Basis::Guess
            },
        });
    }
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
