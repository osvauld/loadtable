//! Builders shared by the unit tests.

use crate::gates::Readings;
use crate::{AdmissionInput, Gate, SlotState, Thresholds, Waiter};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use tasks_core::{JobId, SlotKind};
use tasks_platform::{
    Disk, FlockSlots, Level, Memory, MemorySnapshot, PlatformError, Pressure, PressureSnapshot,
    PressureThresholds, SlotGuard, SlotLocks,
};

const GIB: u64 = 1 << 30;

pub fn thresholds() -> Thresholds {
    let gate = Gate {
        max_cpu: Some(40.0),
        max_io: Some(40.0),
        max_memory: Level::Normal,
    };
    Thresholds {
        min_available_bytes: 4 * GIB,
        compile: gate,
        run: gate,
        offload: gate,
        transfer: Gate {
            max_cpu: None,
            ..gate
        },
        cache_reserve_bytes: 20 * GIB,
        state_reserve_bytes: GIB,
        short_job_max_secs: 60.0,
        short_job_lane_slots: 1,
    }
}

pub fn pressure() -> PressureSnapshot {
    PressureSnapshot {
        memory: Level::Normal,
        memory_some: None,
        memory_full: None,
        cpu: Some(5.0),
        io: Some(5.0),
    }
}

pub fn psi() -> PressureThresholds {
    PressureThresholds {
        memory_some_warn: 10.0,
        memory_some_critical: 40.0,
        memory_full_warn: 5.0,
        memory_full_critical: 20.0,
    }
}

pub fn readings() -> Readings {
    Readings {
        pressure: pressure(),
        available_memory: 8 * GIB,
        cache_free: 100 * GIB,
        state_free: 50 * GIB,
    }
}

/// An input with `free` free slots of each of compile, run and offload, a one-slot short lane
/// and the given waiters.
pub fn input(free: u32, waiters: Vec<Waiter>) -> AdmissionInput {
    let r = readings();
    let state = |n: u32| SlotState {
        free: (0..n).collect(),
        busy: vec![],
    };
    AdmissionInput {
        slots: BTreeMap::from([
            (SlotKind::Compile, state(free)),
            (SlotKind::Run, state(free)),
            (SlotKind::Offload, state(free)),
        ]),
        short_lane: state(1),
        devices: BTreeMap::new(),
        pressure: r.pressure,
        available_memory: r.available_memory,
        cache_free: r.cache_free,
        state_free: r.state_free,
        thresholds: thresholds(),
        waiters,
    }
}

pub fn waiter(job: i64, kind: SlotKind, queued: u64) -> Waiter {
    Waiter::new(JobId(job), kind, queued)
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A local fake of the platform traits with real flock slots in a private directory.
pub struct Fake {
    pub pressure: PressureSnapshot,
    pub available: u64,
    pub disk_free: u64,
    slots: FlockSlots,
}

impl Fake {
    pub fn new() -> Fake {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tasks-admission-{}-{n}", std::process::id()));
        Fake {
            pressure: pressure(),
            available: 8 << 30,
            disk_free: 100 << 30,
            slots: FlockSlots::new(dir),
        }
    }
}

impl Memory for Fake {
    fn snapshot(&self) -> Result<MemorySnapshot, PlatformError> {
        Ok(MemorySnapshot {
            total_bytes: 16 << 30,
            available_bytes: self.available,
        })
    }
}
impl Pressure for Fake {
    fn snapshot(&self, _: &PressureThresholds) -> Result<PressureSnapshot, PlatformError> {
        Ok(self.pressure)
    }
}
impl Disk for Fake {
    fn free_bytes(&self, _: &Path) -> Result<u64, PlatformError> {
        Ok(self.disk_free)
    }
}
impl SlotLocks for Fake {
    fn try_acquire(&self, kind: SlotKind, index: u32) -> Result<Option<SlotGuard>, PlatformError> {
        self.slots.try_acquire(kind, index)
    }
}
