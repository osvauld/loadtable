//! Daemon: ties ledger, admission, placement, runner and peer together into the long-running
//! coordinator service and its local API.
//!
//! The daemon owns one machine's ledger and slots. Clients speak [`api`] over a Unix socket
//! in the state directory. [`scheduler::Scheduler`] is the core and is driven by tests through
//! `tick()`; [`server::Daemon`] adds the process: the one-daemon lock, startup reconciliation,
//! the socket and the loop. The `tasks2d` binary is a thin wrapper over [`entry`].

pub mod alerts;
pub mod api;
pub mod cargo_jobs;
pub mod config;
pub mod entry;
pub mod error;
pub mod forwarder;
mod handler;
pub mod holders;
pub mod logs;
pub mod machine_policy;
mod place;
pub mod platform;
pub mod queue_estimate;
pub mod regex_lite;
mod runjob;
pub mod scheduler;
pub mod server;
pub mod startup;
mod status;
pub mod stuck;
mod supervise;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;

pub use error::DaemonError;
pub use scheduler::{Scheduler, SchedulerParts, TickReport};
pub use server::{Daemon, DaemonOptions};
pub use status::job_view;
pub use supervise::slot_lock_path;
