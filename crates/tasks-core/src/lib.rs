//! Pure, serde-serializable domain types for the `tasks` workload coordinator. No I/O.

mod attribution;
mod error;
mod fetch;
mod ids;
mod job;
mod machine;
mod phase;
pub mod plan;
mod priority;
mod requirements;

pub use attribution::{Attribution, JobFigures, human_bytes};
pub use error::CoreError;
pub use fetch::{FetchResult, FetchStatus, any_failed};
pub use ids::{DeviceId, JobId, MachineName, Owner, Tool};
pub use job::{
    DENIED_DEVICES, JobSpec, Kind, MAX_FETCH_PATH_BYTES, MAX_FETCH_PATHS, MAX_TIMEOUT_SECS,
    Placement, check_fetch_path,
};
pub use machine::{Arch, Machine, Os};
pub use phase::{Phase, SlotKind, WaitReason};
pub use plan::{SplitOrigin, StepHints};
pub use priority::{MAX_PRIORITY_REASON, Priority, UnknownPriority, check_reason};
pub use requirements::{Requirements, Unmet};
