//! The daemon's peer runtime (iroh daemon-to-daemon link), Linux only.
//!
//! * [`runtime`]: started by `tasks daemon` when a peer config exists; its own thread and tokio
//!   runtime, so iroh never runs on the unix-socket request threads. It hosts the LAN-only
//!   endpoint (status, jobs, source sync, artifact ALPNs), the local bridge socket
//!   `peer.sock` used by forwarded jobs, and a watcher that propagates cancels and links
//!   finished jobs in the ledger.
//! * [`host`]: the receiving side of `jobs/0`: a peer's submission goes through this daemon's
//!   own socket exactly like `tasks enqueue` (+ approve/start-worker when configured `auto`).
//! * [`bridge`]: `tasks peer-forward`, the command of a forwarded job (prints the peer's log,
//!   exits with its exit code, re-attaches across a restart of this daemon).
//! * [`records`]: link records on disk (`<state>/peer/...`).
//! * [`reroute`]: re-scores waiting local jobs and moves one to a worker peer that would
//!   clearly finish it sooner (`crate::reroute`).
pub mod bridge;
pub mod forward;
pub mod host;
pub mod records;
pub mod reroute;
pub mod runtime;

pub use task_manager_peer as net;
