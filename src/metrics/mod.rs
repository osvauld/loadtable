//! Machine and job metrics behind `tasks status` (cluster snapshot v1, see
//! docs/design/cluster-snapshot.md). Sampling is daemon-side and in memory;
//! only a finished job's result (`Execution::metrics`) is persisted, inside the
//! existing execution JSON, so older releases keep reading the ledger.
pub mod command;
pub mod compile;
pub mod history;
pub mod procfs;
pub mod ring;
pub mod testscope;
pub mod types;
pub mod units;
pub mod warmth;

#[cfg(target_os = "linux")]
pub mod sampler;
#[cfg(target_os = "linux")]
pub mod slots;
#[cfg(target_os = "linux")]
pub mod snapshot;

pub use types::*;

#[cfg(all(test, target_os = "linux"))]
mod tests;

#[cfg(test)]
mod units_tests;
