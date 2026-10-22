//! Pure, serde-serializable domain types for the `tasks` workload coordinator. No I/O.

mod error;
mod ids;
mod job;
mod machine;
mod phase;
pub mod plan;
mod requirements;

pub use error::CoreError;
pub use ids::{DeviceId, JobId, MachineName, Owner, Tool};
pub use job::{DENIED_DEVICES, JobSpec, Kind, Placement};
pub use machine::{Arch, Machine, Os};
pub use phase::{Phase, SlotKind, WaitReason};
pub use requirements::{Requirements, Unmet};
