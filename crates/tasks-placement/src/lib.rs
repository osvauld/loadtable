//! Pure placement scoring: which machines can run a job, and which is best.

mod decide;
mod error;
mod explain;
mod input;
mod reroute;

pub use decide::{
    Decision, Exclusion, MachineReport, Refusal, Report, Score, Verdict, WaitBasis, decide,
    slot_for,
};
pub use error::PlacementError;
pub use explain::fmt_secs;
pub use input::{Checkout, Input, Link, MachineView, Policy, Pressure, Request, RunEstimate};
pub use reroute::{Current, JobState, Reroute, StayReason, should_reroute};

use tasks_core::{Machine, MachineName, Requirements, Unmet};

/// Checks every machine against the requirements, keeping the unmet reasons.
pub fn candidates(
    req: &Requirements,
    machines: &[Machine],
) -> Vec<(MachineName, Result<(), Vec<Unmet>>)> {
    machines
        .iter()
        .map(|m| (m.name.clone(), req.satisfied_by(m)))
        .collect()
}

#[cfg(test)]
mod tests;
