//! Admission control: decides which waiting job may start, given slots, memory, CPU and
//! I/O pressure and disk space.
//!
//! [`decide`] is a pure function of values. [`Admitter`] wires it to the platform traits
//! and re-checks pressure after taking the slot lock. Thresholds are always passed in;
//! nothing here reads host policy, `/proc` or the environment.

mod admitter;
mod decide;
#[cfg(test)]
mod decide_tests;
mod error;
mod gates;
mod input;
mod model;
mod reason;
#[cfg(test)]
mod testkit;

pub use admitter::{Admitter, Attempt, Layout};
pub use decide::decide;
pub use error::AdmissionError;
pub use gates::{Readings, check_gates};
pub use input::{
    AdmissionInput, DeviceState, Gate, OFFLOAD_FLOOR_CAP_BYTES, SHORT_LANE_BASE, SlotState,
    Thresholds,
};
pub use model::{Decision, Hold, Lane, Priority, Verdict, Waiter};
pub use reason::{DiskKind, WaitReason, gib, kind_name};
