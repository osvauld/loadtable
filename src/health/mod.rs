//! Node health: periodic checks, alerts and disk monitoring (feature/health-alerts).
//!
//! Every daemon runs a health monitor on its own background threads (`monitor`): a check
//! pass every `TASKS_HEALTH_INTERVAL_S` (default 30 s) and a bounded disk-usage scan every
//! `TASKS_HEALTH_DISK_SCAN_S` (default 10 min). The result, [`NodeHealth`], is the optional
//! `health` field of this machine's cluster snapshot, so it reaches the controller over the
//! peer status stream unchanged. The controller aggregates its own and every peer's health
//! (`cluster`) for `tasks alerts`, MCP `alerts`/`cluster_status`/`submit`, the dashboard
//! banner and a rate-limited desktop notifier.
//!
//! Alerts (`alert`) have a stable id (`KIND:NODE[:SUBJECT]`), a severity, the node, a summary,
//! evidence and a suggested fix, first/last seen; repeats are deduplicated and an alert
//! clears when its condition ends. The checks themselves are pure (`checks`); classified
//! environment failures in failed job logs (`classify`) become per-node missing-capability
//! marks (`capability`) that placement consults.
pub mod alert;
pub mod capability;
pub mod checks;
pub mod classify;
pub mod cluster;
pub mod config;
pub mod disk;
pub mod node;
pub mod notify;
pub mod procs;

pub mod monitor;

pub use alert::{Alert, AlertBook, Observation, Severity};

use serde::{Deserialize, Serialize};

/// Cleared alerts carried in a snapshot (the full list stays in the daemon).
pub const SNAPSHOT_RECENT: usize = 10;

/// One machine's health: the snapshot's optional `health` field (cluster snapshot v1,
/// additive; older readers ignore it, older daemons omit it).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeHealth {
    /// This machine's id (hostname), as in `machine.id`.
    pub node: String,
    /// When the last check pass ran (Unix seconds); 0 before the first one.
    pub checked_at: f64,
    pub interval_s: f64,
    /// Active alerts raised by this machine (about itself or about its peers), most severe first.
    #[serde(default)]
    pub alerts: Vec<Alert>,
    /// Recently cleared alerts, newest last.
    #[serde(default)]
    pub recent: Vec<Alert>,
    /// Every filesystem the task manager writes to, with the top consumers of the last scan.
    #[serde(default)]
    pub disks: Vec<disk::DiskStatus>,
    /// The last bounded disk scan and its cost.
    #[serde(default)]
    pub scan: Option<disk::ScanSummary>,
    /// Missing-capability marks this machine holds (about itself, or about a peer by name).
    #[serde(default)]
    pub capabilities: Vec<capability::CapabilityMark>,
    /// Capability probes of this daemon's environment (declared and known ones; `node`).
    #[serde(default)]
    pub probes: Vec<node::ProbeResult>,
}
