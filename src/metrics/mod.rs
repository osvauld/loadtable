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
pub mod warmth;

// Snapshot construction and sampler handles are shared Unix code. Native reads inside
// the sampler remain Linux-specific until the macOS monitoring backend is integrated.
pub mod sampler;
pub mod slots;
pub mod snapshot;

pub use types::*;

#[cfg(all(test, target_os = "linux"))]
mod tests;
