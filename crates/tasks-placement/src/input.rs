//! Inputs of the placement decision. All plain data: the caller gathers it, nothing here does I/O.

use crate::error::PlacementError;
use std::collections::BTreeMap;
use std::fmt;
use tasks_core::{Kind, Machine, MachineName, Placement, Requirements, SlotKind, StepHints};
use tasks_timing::Estimate;

/// What the submitter asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// The default: stay on this machine.
    Local,
    /// Explicitly stay on this machine, never moved.
    Pin,
    /// Run on exactly this peer, never moved.
    Peer(MachineName),
    /// Any capable machine; the lowest expected finish wins.
    Anywhere,
}

impl Request {
    /// Maps the core placement of a job spec (`Here` is the default local request).
    pub fn from_placement(p: &Placement) -> Request {
        match p {
            Placement::Here => Request::Local,
            Placement::Pin => Request::Pin,
            Placement::Peer(n) => Request::Peer(n.clone()),
            Placement::Anywhere => Request::Anywhere,
        }
    }

    /// Parses a plan step's placement hint: `local`/`here`, `pin`, `anywhere`, else a peer name.
    pub fn from_hint(hint: &str) -> Request {
        match hint {
            "local" | "here" => Request::Local,
            "pin" => Request::Pin,
            "anywhere" => Request::Anywhere,
            other => Request::Peer(MachineName(other.to_string())),
        }
    }
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Request::Local => f.write_str("local"),
            Request::Pin => f.write_str("pin"),
            Request::Peer(n) => write!(f, "peer {}", n.0),
            Request::Anywhere => f.write_str("anywhere"),
        }
    }
}

/// Whether a machine's state can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// Reachable; the state snapshot is this old.
    Up {
        /// Age of the snapshot in seconds.
        age_secs: u64,
    },
    /// Not reachable.
    Down {
        /// Seconds since it was last seen, if ever.
        last_seen_ago_secs: Option<u64>,
    },
}

/// Pressure summary of a machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pressure {
    /// Admission would block a new job now. Counts as extra wait; and a machine with a closed
    /// gate never wins over one that could start the job at once. For a peer this is its
    /// compile gate (all it reports), so it applies to compile-slot jobs.
    pub blocked: bool,
    /// Human readable summary, e.g. "memory PSI 12%".
    pub summary: String,
}

/// A run time estimate for this job's recipe on one machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunEstimate {
    /// Median seconds.
    pub median_secs: f64,
    /// 90th percentile seconds.
    pub p90_secs: f64,
    /// Number of samples behind it.
    pub samples: usize,
}

impl From<&Estimate> for RunEstimate {
    fn from(e: &Estimate) -> Self {
        RunEstimate {
            median_secs: e.median_secs,
            p90_secs: e.p90_secs,
            samples: e.samples,
        }
    }
}

/// The checkout as seen from one machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Checkout {
    /// The checkout is already synced there (always true for the local machine).
    pub synced: bool,
    /// Bytes that would have to be sent when it is not.
    pub sync_bytes: u64,
}

/// Everything known about one machine for this decision.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineView {
    /// Static facts.
    pub machine: Machine,
    /// This is the machine making the decision. Exactly one view is local.
    pub is_local: bool,
    /// Reachability and staleness (ignored for the local machine).
    pub link: Link,
    /// Free slots by kind; a missing kind means none.
    pub free_slots: BTreeMap<SlotKind, u32>,
    /// Jobs queued there.
    pub queue_depth: u32,
    /// Expected wait for this job in seconds; `None` when the holders have no known ETA.
    pub expected_wait_secs: Option<f64>,
    /// Pressure summary.
    pub pressure: Pressure,
    /// Run time estimate for this recipe on this machine, if any.
    pub estimate: Option<RunEstimate>,
    /// Checkout facts.
    pub checkout: Checkout,
    /// Capability marks that make this machine unusable for the job (evidence text).
    pub marks: Vec<String>,
}

/// One placement question.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    /// Job kind.
    pub kind: Kind,
    /// Hard requirements.
    pub requirements: Requirements,
    /// Scheduling hints (`tools` add to the requirements).
    pub hints: StepHints,
    /// What was asked for.
    pub request: Request,
    /// `Some(why)` when the job cannot be shipped (a path that exists only here, R-138).
    pub local_only_reason: Option<String>,
    /// Every machine, the local one included.
    pub machines: Vec<MachineView>,
}

/// Tunables. No defaults live in this crate: the caller owns every number.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// A peer snapshot older than this is stale and never chosen.
    pub staleness_limit_secs: u64,
    /// Seconds to send one MiB of checkout.
    pub sync_secs_per_mib: f64,
    /// Seconds subtracted from the local score so identical machines stay local.
    pub local_bonus_secs: f64,
    /// Run time assumed when a machine has no estimate.
    pub unknown_run_secs: f64,
    /// Wait assumed when slots are held and the holder's ETA is unknown.
    pub unknown_wait_secs: f64,
    /// Extra wait for a machine whose pressure would block admission.
    pub pressure_wait_secs: f64,
    /// Share of (p90 - median) added to the median run time, 0..=1.
    pub p90_weight: f64,
    /// A queued job moves only when it gains more than this (hysteresis).
    pub reroute_margin_secs: f64,
    /// A queued job must have sat this long on its machine before it may move.
    pub reroute_cooldown_secs: u64,
    /// A job is rerouted at most this many times.
    pub max_reroutes: u32,
}

impl Policy {
    pub(crate) fn validate(&self) -> Result<(), PlacementError> {
        let nums = [
            ("sync_secs_per_mib", self.sync_secs_per_mib),
            ("local_bonus_secs", self.local_bonus_secs),
            ("unknown_run_secs", self.unknown_run_secs),
            ("unknown_wait_secs", self.unknown_wait_secs),
            ("pressure_wait_secs", self.pressure_wait_secs),
            ("reroute_margin_secs", self.reroute_margin_secs),
        ];
        for (field, v) in nums {
            if !v.is_finite() || v < 0.0 {
                return Err(PlacementError::BadNumber { field });
            }
        }
        if !(0.0..=1.0).contains(&self.p90_weight) {
            return Err(PlacementError::BadWeight);
        }
        Ok(())
    }
}

impl Input {
    pub(crate) fn validate(&self) -> Result<(), PlacementError> {
        let mut locals = 0;
        let mut seen = std::collections::BTreeSet::new();
        for v in &self.machines {
            if !seen.insert(&v.machine.name) {
                return Err(PlacementError::duplicate(&v.machine.name));
            }
            locals += usize::from(v.is_local);
            let bad = |x: f64| !x.is_finite() || x < 0.0;
            if v.expected_wait_secs.is_some_and(bad) {
                return Err(PlacementError::BadNumber {
                    field: "expected_wait_secs",
                });
            }
            if v.estimate
                .is_some_and(|e| bad(e.median_secs) || bad(e.p90_secs))
            {
                return Err(PlacementError::BadNumber { field: "estimate" });
            }
        }
        match locals {
            0 => Err(PlacementError::NoLocalMachine),
            1 => Ok(()),
            _ => Err(PlacementError::SeveralLocalMachines),
        }
    }
}
