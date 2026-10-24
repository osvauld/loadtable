//! The values `decide` works on. Nothing here reads a clock, file or environment variable.

use crate::{Running, Waiter};
use std::collections::BTreeMap;
use tasks_core::{DeviceId, JobId, SlotKind};
use tasks_platform::{Level, PressureSnapshot};

/// Slot indices of the short lane start here, so they never collide with normal slots.
pub const SHORT_LANE_BASE: u32 = 1000;
/// Offload jobs only need min(reserve, this) of memory and fetch space (R-060, R-065).
pub const OFFLOAD_FLOOR_CAP_BYTES: u64 = 2 << 30;

/// Pressure limits for one slot kind. A reading strictly above a limit blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gate {
    /// Max CPU pressure percent; `None` means the gate is off (macOS). A limit with no reading blocks.
    pub max_cpu: Option<f32>,
    /// Max I/O pressure percent; `None` means the gate is off.
    pub max_io: Option<f32>,
    /// Highest memory pressure level that still admits.
    pub max_memory: Level,
}

/// Machine policy, always passed in by the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    /// Minimum available memory in bytes.
    pub min_available_bytes: u64,
    /// Limits for compile jobs.
    pub compile: Gate,
    /// Limits for run jobs (may be looser than compile, R-059).
    pub run: Gate,
    /// Limits for offload jobs; only `max_memory` is used.
    pub offload: Gate,
    /// Limits for transfers (checkout sync): the I/O limit and the memory level apply, the CPU
    /// limit is off. A sync is local disk work, so unlike offload it waits for the I/O gate.
    pub transfer: Gate,
    /// Free bytes required on the cache filesystem for a job that creates new output.
    pub cache_reserve_bytes: u64,
    /// Free bytes required on the state filesystem.
    pub state_reserve_bytes: u64,
    /// A waiter predicted to take at most this many seconds may use the short lane.
    pub short_job_max_secs: f64,
    /// ...and whose predicted peak memory is at most this many bytes.
    pub short_job_max_memory_bytes: u64,
    /// Size of the short lane.
    pub short_job_lane_slots: u32,
    /// A waiting job rises one class after this long, ms; 0 turns aging off.
    pub aging_ms: u64,
}

impl Thresholds {
    /// The gate for `kind` (devices have none and use the run gate's shape, unused).
    pub fn gate(&self, kind: SlotKind) -> Gate {
        match kind {
            SlotKind::Compile => self.compile,
            SlotKind::Offload => self.offload,
            SlotKind::Transfer => self.transfer,
            SlotKind::Run | SlotKind::Device => self.run,
        }
    }
}

/// Free and busy slots of one kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotState {
    /// Free slot indices.
    pub free: Vec<u32>,
    /// Busy slot indices with their holder, when known.
    pub busy: Vec<(u32, Option<JobId>)>,
}

/// One attached device's lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceState {
    /// Slot index of the device lock.
    pub index: u32,
    /// True when nobody holds it.
    pub free: bool,
    /// Holder, when busy and known.
    pub holder: Option<JobId>,
}

/// Everything `decide` needs.
#[derive(Debug, Clone)]
pub struct AdmissionInput {
    /// Compile, run and offload slots.
    pub slots: BTreeMap<SlotKind, SlotState>,
    /// Short lane; indices are `0..short_job_lane_slots`.
    pub short_lane: SlotState,
    /// Device locks by id.
    pub devices: BTreeMap<DeviceId, DeviceState>,
    /// Current pressure.
    pub pressure: PressureSnapshot,
    /// Available memory in bytes.
    pub available_memory: u64,
    /// Free bytes where cache output lands.
    pub cache_free: u64,
    /// Free bytes where state lives.
    pub state_free: u64,
    /// Machine policy.
    pub thresholds: Thresholds,
    /// The clock, ms since the epoch: aging is measured against it.
    pub now_ms: u64,
    /// When each owner last started a job, ms; owners with none are served first.
    pub last_started: BTreeMap<String, u64>,
    /// Jobs running on this machine now (not offloaded ones), for `isolate`.
    pub running: Vec<Running>,
    /// Waiting jobs, in any order; `decide` orders them ([`crate::sequence`]).
    pub waiters: Vec<Waiter>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::thresholds;

    #[test]
    fn gate_lookup_per_kind() {
        let mut t = thresholds();
        t.run.max_cpu = Some(80.0);
        assert_eq!(t.gate(SlotKind::Run).max_cpu, Some(80.0));
        assert_eq!(t.gate(SlotKind::Compile).max_cpu, Some(40.0));
    }
}
