//! In-memory fakes of the platform traits for tests.

use crate::{
    Child, Disk, FlockSlots, GroupId, Level, Memory, MemorySnapshot, PlatformError, Pressure, PressureSnapshot,
    Processes, Service, ServiceStatus, ServiceUnit, Signal, SlotGuard, SlotLocks, SpawnSpec, Usage,
};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use tasks_core::SlotKind;

fn poisoned<T>(_: T) -> PlatformError {
    PlatformError::Parse("fake lock poisoned".into())
}

static FAKE_COUNTER: AtomicU32 = AtomicU32::new(0);

/// A private lock directory per fake, so tests in one process never share slots.
fn unique_dir() -> std::path::PathBuf {
    let n = FAKE_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("tasks-fake-slots-{}-{n}", std::process::id()))
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
    slots: FlockSlots,
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
            slots: FlockSlots::new(unique_dir()),
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
    /// Fake slots are real flocks in a private directory; dropping the guard releases the slot.
    fn try_acquire(&self, kind: SlotKind, index: u32) -> Result<Option<SlotGuard>, PlatformError> {
        self.slots.try_acquire(kind, index)
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
        let guard = f.try_acquire(SlotKind::Run, 0).unwrap();
        assert!(guard.is_some());
        assert!(f.try_acquire(SlotKind::Run, 0).unwrap().is_none());
        drop(guard);
        assert!(f.try_acquire(SlotKind::Run, 0).unwrap().is_some());
        assert!(Fake::default().try_acquire(SlotKind::Run, 0).unwrap().is_some());
        assert_eq!(Service::status(&f).unwrap(), ServiceStatus::NotInstalled);
    }
}
