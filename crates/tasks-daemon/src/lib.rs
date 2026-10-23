//! Daemon: ties ledger, admission, placement, runner and peer together into the long-running
//! coordinator service and its local API.
//!
//! The daemon owns one machine's ledger and slots. Clients speak [`api`] over a Unix socket
//! in the state directory. [`scheduler::Scheduler`] is the core and is driven by tests through
//! `tick()`; [`server::Daemon`] adds the process: the one-daemon lock, startup reconciliation,
//! the socket and the loop. The `tasks2d` binary is a thin wrapper over [`entry`].

pub mod alerts;
pub mod api;
mod attrib;
#[cfg(test)]
mod attrib_tests;
#[cfg(test)]
mod audit_tests;
pub mod audited;
mod awake;
pub mod build_cache;
pub mod cargo_jobs;
pub mod config;
pub mod entry;
pub mod error;
#[cfg(test)]
mod fetch_tests;
#[cfg(test)]
mod follow_tests;
mod forwarded;
pub mod forwarder;
mod handler;
mod history;
pub mod holders;
pub mod logs;
pub mod machine_policy;
mod metrics;
#[cfg(test)]
mod metrics_tests;
pub mod peer_link;
#[cfg(test)]
mod peer_sleep_tests;
mod peer_wait;
pub mod peers_config;
mod place;
pub mod platform;
pub mod queue_estimate;
mod queue_view;
pub mod regex_lite;
mod runjob;
#[cfg(test)]
mod sched_tests;
pub mod scheduler;
pub mod server;
pub mod startup;
mod status;
pub mod stuck;
pub mod submit;
mod supervise;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;
pub mod trust_control;
pub mod trust_runtime;
#[cfg(test)]
mod trust_tests;
#[cfg(test)]
mod wiring_tests;

pub use attrib::TransferGate;
pub use error::DaemonError;
pub use scheduler::{Scheduler, SchedulerParts, TickReport};
pub use server::{Daemon, DaemonOptions};
pub use status::job_view;
pub use tasks_platform::slot_lock_path;
