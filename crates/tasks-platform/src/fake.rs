//! In-memory fakes of the platform traits for tests.

use crate::{
    Child, Disk, GroupId, Level, Memory, MemorySnapshot, PlatformError, Pressure, PressureSnapshot,
    Processes, Service, ServiceStatus, ServiceUnit, Signal, SlotGuard, SlotLocks, SpawnSpec, Usage,
};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;
use tasks_core::SlotKind;

fn poisoned<T>(_: T) -> PlatformError {
    PlatformError::Parse("fake lock poisoned".into())
}

/// A fake with settable readings that implements every platform trait.
#[derive(Debug)]
pub struct Fake {
    /// Memory reading returned.
    pub memory: MemorySnapshot,
    /// Pressure reading returned.
    pub pressure: PressureSnapshot,
    /// Free disk bytes returned.
    pub disk_free: u64,
    /// Signals received, in order.
    pub signals: Mutex<Vec<(GroupId, Signal)>>,
    held: Mutex<BTreeSet<(SlotKind, u32)>>,
    status: Mutex<ServiceStatus>,
}

impl Default for Fake {
    fn default() -> Self {
        Fake {
            memory: MemorySnapshot {
                total_bytes: 16 << 30,
                available_bytes: 8 << 30,
            },
            pressure: PressureSnapshot {
                memory: Level::Normal,
                cpu: None,
                io: None,
            },
            disk_free: 100 << 30,
            signals: Mutex::new(Vec::new()),
            held: Mutex::new(BTreeSet::new()),
            status: Mutex::new(ServiceStatus::NotInstalled),
        }
    }
}

impl Memory for Fake {
    fn snapshot(&self) -> Result<MemorySnapshot, PlatformError> {
        Ok(self.memory)
    }
}
impl Pressure for Fake {
    fn snapshot(&self) -> Result<PressureSnapshot, PlatformError> {
        Ok(self.pressure)
    }
}
impl Disk for Fake {
    fn free_bytes(&self, _path: &Path) -> Result<u64, PlatformError> {
        Ok(self.disk_free)
    }
}
impl Processes for Fake {
    fn spawn_group(&self, _cmd: &SpawnSpec) -> Result<Child, PlatformError> {
        Ok(Child {
            pid: 1,
            group: GroupId(1),
        })
    }
    fn signal_group(&self, group: GroupId, sig: Signal) -> Result<(), PlatformError> {
        self.signals.lock().map_err(poisoned)?.push((group, sig));
        Ok(())
    }
    fn usage(&self, _group: GroupId) -> Result<Usage, PlatformError> {
        Ok(Usage {
            cpu_user_s: 0.0,
            cpu_sys_s: 0.0,
            rss_bytes: 0,
            processes: 1,
        })
    }
}
impl Service for Fake {
    fn install(&self, _unit: &ServiceUnit) -> Result<(), PlatformError> {
        *self.status.lock().map_err(poisoned)? = ServiceStatus::Running;
        Ok(())
    }
    fn status(&self) -> Result<ServiceStatus, PlatformError> {
        Ok(*self.status.lock().map_err(poisoned)?)
    }
}
impl SlotLocks for Fake {
    /// Fake slots are tracked in memory and never released; guards are real temp-file locks.
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
        let f = Fake::default();
        assert_eq!(Memory::snapshot(&f).unwrap().total_bytes, 16 << 30);
        f.signal_group(GroupId(3), Signal::Term).unwrap();
        assert_eq!(f.signals.lock().unwrap().len(), 1);
        assert!(f.try_acquire(SlotKind::Run, 0).unwrap().is_some());
        assert!(f.try_acquire(SlotKind::Run, 0).unwrap().is_none());
        assert_eq!(Service::status(&f).unwrap(), ServiceStatus::NotInstalled);
    }
}
