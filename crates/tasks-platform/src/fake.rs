//! In-memory fake of the platform traits for tests of other crates.

use crate::{
    Child, Disk, GroupId, InstallPlan, Level, Memory, MemorySnapshot, PlatformError, Pressure,
    PressureSnapshot, PressureThresholds, Processes, Service, ServiceStatus, ServiceUnit, Signal,
    SlotGuard, SlotLocks, SpawnSpec, Usage,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tasks_core::SlotKind;

fn poisoned<T>(_: T) -> PlatformError {
    PlatformError::Poisoned("FakePlatform")
}

/// A fake with settable readings that implements every platform trait.
/// Set the public fields to script memory, pressure and disk; signals are recorded.
#[derive(Debug)]
pub struct FakePlatform {
    /// Memory reading returned.
    pub memory: MemorySnapshot,
    /// Pressure reading returned (thresholds are ignored by the fake).
    pub pressure: PressureSnapshot,
    /// Free disk bytes returned.
    pub disk_free: u64,
    /// Usage returned for any group.
    pub usage: Usage,
    /// Signals received, in order.
    pub signals: Mutex<Vec<(GroupId, Signal)>>,
    held: Mutex<BTreeSet<(SlotKind, u32)>>,
    status: Mutex<ServiceStatus>,
}

impl Default for FakePlatform {
    fn default() -> Self {
        FakePlatform {
            memory: MemorySnapshot {
                total_bytes: 16 << 30,
                available_bytes: 8 << 30,
            },
            pressure: PressureSnapshot {
                memory: Level::Normal,
                memory_some: None,
                memory_full: None,
                cpu: None,
                io: None,
            },
            disk_free: 100 << 30,
            usage: Usage {
                cpu_user_s: 0.0,
                cpu_sys_s: 0.0,
                rss_bytes: 0,
                processes: 1,
            },
            signals: Mutex::new(Vec::new()),
            held: Mutex::new(BTreeSet::new()),
            status: Mutex::new(ServiceStatus::NotInstalled),
        }
    }
}

impl Memory for FakePlatform {
    fn snapshot(&self) -> Result<MemorySnapshot, PlatformError> {
        Ok(self.memory)
    }
}
impl Pressure for FakePlatform {
    fn snapshot(&self, _t: &PressureThresholds) -> Result<PressureSnapshot, PlatformError> {
        Ok(self.pressure)
    }
}
impl Disk for FakePlatform {
    fn free_bytes(&self, _path: &Path) -> Result<u64, PlatformError> {
        Ok(self.disk_free)
    }
}
impl Processes for FakePlatform {
    fn spawn_group(&self, _cmd: &SpawnSpec) -> Result<Child, PlatformError> {
        Ok(Child {
            pid: 2,
            group: GroupId {
                pgid: 2,
                start_time: 1,
            },
        })
    }
    fn signal_group(&self, group: GroupId, sig: Signal) -> Result<(), PlatformError> {
        self.signals.lock().map_err(poisoned)?.push((group, sig));
        Ok(())
    }
    fn usage(&self, _group: GroupId) -> Result<Usage, PlatformError> {
        Ok(self.usage)
    }
    fn peek_exit(&self, _child: &Child) -> Result<Option<i32>, PlatformError> {
        Ok(Some(0))
    }
    fn try_wait(&self, _child: &Child) -> Result<Option<i32>, PlatformError> {
        Ok(Some(0))
    }
}
impl Service for FakePlatform {
    fn install(&self, unit: &ServiceUnit) -> Result<InstallPlan, PlatformError> {
        *self.status.lock().map_err(poisoned)? = ServiceStatus::Stopped;
        Ok(InstallPlan {
            path: PathBuf::from(format!("/fake/{}.service", unit.name)),
            contents_written: true,
            next_command: format!("fake-start {}", unit.name),
        })
    }
    fn status(&self, _name: &str) -> Result<ServiceStatus, PlatformError> {
        Ok(*self.status.lock().map_err(poisoned)?)
    }
}
impl SlotLocks for FakePlatform {
    /// Slots are tracked in memory (never released); guards are real temp-file locks.
    fn try_acquire(&self, kind: SlotKind, index: u32) -> Result<Option<SlotGuard>, PlatformError> {
        let mut held = self.held.lock().map_err(poisoned)?;
        if !held.insert((kind, index)) {
            return Ok(None);
        }
        let dir = std::env::temp_dir().join(format!("tasks-fake-slots-{}", std::process::id()));
        crate::FlockSlots::new(dir).try_acquire(kind, index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_reports_and_records() {
        let mut f = FakePlatform::default();
        f.memory.available_bytes = 1;
        f.pressure.memory = Level::Critical;
        f.disk_free = 7;
        assert_eq!(Memory::snapshot(&f).unwrap().available_bytes, 1);
        let t = PressureThresholds {
            memory_some_warn: 1.0,
            memory_some_critical: 2.0,
            memory_full_warn: 1.0,
            memory_full_critical: 2.0,
        };
        assert_eq!(Pressure::snapshot(&f, &t).unwrap().memory, Level::Critical);
        assert_eq!(f.free_bytes(Path::new("/")).unwrap(), 7);
        let g = GroupId {
            pgid: 3,
            start_time: 9,
        };
        f.signal_group(g, Signal::Term).unwrap();
        assert_eq!(f.signals.lock().unwrap().as_slice(), &[(g, Signal::Term)]);
        assert!(f.try_acquire(SlotKind::Run, 0).unwrap().is_some());
        assert!(f.try_acquire(SlotKind::Run, 0).unwrap().is_none());
        assert_eq!(
            Service::status(&f, "x").unwrap(),
            ServiceStatus::NotInstalled
        );
    }
}
