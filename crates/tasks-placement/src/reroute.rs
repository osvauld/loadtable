//! Whether a still-queued job should move.

use crate::decide::{Decision, Report, decide};
use crate::error::PlacementError;
use crate::input::{Input, Policy, Request};
use tasks_core::MachineName;

/// Where the job is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Waiting in a queue.
    Queued,
    /// Started; never moved.
    Running,
}

/// The job's current placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    /// Where it is queued.
    pub machine: MachineName,
    /// Its state.
    pub state: JobState,
    /// Seconds it has been on that machine.
    pub queued_secs: u64,
    /// Times it was moved already.
    pub reroutes: u32,
}

/// Why a job stays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StayReason {
    /// It is running.
    Running,
    /// Only `Anywhere` jobs move.
    NotMovable,
    /// The current machine is still the best, or nothing else can take it.
    NoBetter,
    /// The gain does not exceed the hysteresis margin.
    WithinMargin,
    /// It has not been queued long enough.
    Cooldown,
    /// It has already moved the maximum number of times.
    TooManyReroutes,
}

/// The reroute verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum Reroute {
    /// Leave it where it is.
    Stay(StayReason),
    /// Move it.
    Move {
        /// New machine.
        to: MachineName,
        /// Seconds gained; `None` when the current machine is no longer usable.
        gain_secs: Option<f64>,
        /// The fresh report to log.
        report: Box<Report>,
    },
}

/// Decides whether a queued job should move, given a fresh `Input`.
///
/// The fresh input's numbers for the current machine are those of the job still queued there
/// (its checkout is treated as synced). Running jobs and non-`Anywhere` requests never move.
pub fn should_reroute(
    current: &Current,
    fresh: &Input,
    policy: &Policy,
) -> Result<Reroute, PlacementError> {
    if current.state == JobState::Running {
        return Ok(Reroute::Stay(StayReason::Running));
    }
    if fresh.request != Request::Anywhere {
        return Ok(Reroute::Stay(StayReason::NotMovable));
    }
    let mut input = fresh.clone();
    let cur = input
        .machines
        .iter_mut()
        .find(|v| v.machine.name == current.machine)
        .ok_or_else(|| PlacementError::UnknownCurrent(current.machine.0.clone()))?;
    cur.checkout.synced = true;
    let report = decide(&input, policy)?;
    let Decision::Run(best) = &report.decision else {
        return Ok(Reroute::Stay(StayReason::NoBetter));
    };
    if *best == current.machine {
        return Ok(Reroute::Stay(StayReason::NoBetter));
    }
    let gain = match (report.score_of(&current.machine), report.score_of(best)) {
        (Some(c), Some(b)) => Some(c.total_secs() - b.total_secs()),
        _ => None,
    };
    if let Some(g) = gain {
        if current.reroutes >= policy.max_reroutes {
            return Ok(Reroute::Stay(StayReason::TooManyReroutes));
        }
        if current.queued_secs < policy.reroute_cooldown_secs {
            return Ok(Reroute::Stay(StayReason::Cooldown));
        }
        if g <= policy.reroute_margin_secs {
            return Ok(Reroute::Stay(StayReason::WithinMargin));
        }
    }
    Ok(Reroute::Move {
        to: best.clone(),
        gain_secs: gain,
        report: Box::new(report),
    })
}
