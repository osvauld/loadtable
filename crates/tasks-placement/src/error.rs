//! Errors for malformed placement inputs.

use tasks_core::MachineName;

/// The input handed to the placement functions is not usable.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PlacementError {
    /// No machine is marked as the local one.
    #[error("no machine is marked local")]
    NoLocalMachine,
    /// More than one machine is marked as the local one.
    #[error("more than one machine is marked local")]
    SeveralLocalMachines,
    /// Two machines share a name.
    #[error("machine {0:?} appears twice")]
    DuplicateMachine(String),
    /// A number is NaN, infinite or negative where that makes no sense.
    #[error("{field} is not a finite non-negative number")]
    BadNumber {
        /// Which field.
        field: &'static str,
    },
    /// The p90 weight is outside 0..=1.
    #[error("p90_weight must be within 0..=1")]
    BadWeight,
    /// The current machine of a reroute query is not in the input.
    #[error("current machine {0:?} is not in the input")]
    UnknownCurrent(String),
}

impl PlacementError {
    pub(crate) fn duplicate(name: &MachineName) -> Self {
        PlacementError::DuplicateMachine(name.0.clone())
    }
}
