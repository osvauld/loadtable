//! In-memory fake of the platform traits for tests of other crates.

use crate::{
    CgroupName, CgroupStatus, Cgroups, Child, Disk, GroupId, InstallPlan, Level, Memory,
    MemorySnapshot, PlatformError, Pressure, PressureSnapshot, PressureThresholds, ProcInfo,
    ProcessTable, Processes, Service, ServiceStatus, ServiceUnit, Signal, SlotGuard, SlotLocks,
    SpawnSpec, Usage,
};
use crate::{InhibitGuard, PowerReading, PowerSource, SleepGuard};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tasks_core::SlotKind;
use tasks_core::{Attribution, JobFigures};

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
    /// The process table returned by [`ProcessTable::snapshot`].
    pub table: Mutex<Vec<ProcInfo>>,
    held: Mutex<BTreeSet<(SlotKind, u32)>>,
    status: Mutex<ServiceStatus>,
    /// Names passed to [`Service::start`], in order.
    pub service_starts: Mutex<Vec<String>>,
    /// Error text [`Service::start`] returns when set.
    pub service_start_error: Mutex<Option<String>>,
}

impl Default for FakePlatform {
    fn default() -> Self {
        FakePlatform {
            memory: MemorySnapshot {
                total_bytes: 16 << 30,
                available_bytes: 8 << 30,
                ..MemorySnapshot::default()
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
            table: Mutex::new(Vec::new()),
            held: Mutex::new(BTreeSet::new()),
            status: Mutex::new(ServiceStatus::NotInstalled),
            service_starts: Mutex::new(Vec::new()),
            service_start_error: Mutex::new(None),
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
impl ProcessTable for FakePlatform {
    fn snapshot(&self) -> Result<Vec<ProcInfo>, PlatformError> {
        Ok(self.table.lock().map_err(poisoned)?.clone())
    }
}

/// One scripted cgroup.
#[derive(Debug, Clone)]
struct FakeGroup {
    pids: Vec<i32>,
    figures: JobFigures,
}

#[derive(Debug, Default)]
struct FakeCgState {
    groups: BTreeMap<CgroupName, FakeGroup>,
    kills: Vec<CgroupName>,
    removed: Vec<CgroupName>,
    weights: BTreeMap<CgroupName, u32>,
}

/// In-memory [`Cgroups`]: groups appear on `create`, hold the pids a test sets, report the
/// figures a test sets, and `kill` empties them. Records every kill and removal.
#[derive(Debug)]
pub struct FakeCgroups {
    delegated: bool,
    reason: String,
    state: Mutex<FakeCgState>,
}

impl FakeCgroups {
    /// Cgroups that work.
    pub fn delegated() -> FakeCgroups {
        FakeCgroups {
            delegated: true,
            reason: String::new(),
            state: Mutex::new(FakeCgState::default()),
        }
    }

    /// Cgroups that are not available, for `reason`.
    pub fn unavailable(reason: &str) -> FakeCgroups {
        FakeCgroups {
            delegated: false,
            reason: reason.to_string(),
            state: Mutex::new(FakeCgState::default()),
        }
    }

    /// Sets the pids listed in a cgroup (creating it).
    pub fn set_pids(&self, name: CgroupName, pids: Vec<i32>) {
        if let Ok(mut s) = self.state.lock() {
            s.groups
                .entry(name)
                .or_insert_with(|| FakeGroup {
                    pids: Vec::new(),
                    figures: JobFigures::empty(Attribution::Cgroup),
                })
                .pids = pids;
        }
    }

    /// Sets the figures a cgroup reports (creating it).
    pub fn set_figures(&self, name: CgroupName, figures: JobFigures) {
        if let Ok(mut s) = self.state.lock() {
            s.groups
                .entry(name)
                .or_insert_with(|| FakeGroup {
                    pids: Vec::new(),
                    figures: JobFigures::empty(Attribution::Cgroup),
                })
                .figures = figures;
        }
    }

    /// Cgroups killed with `cgroup.kill`, in order.
    pub fn kills(&self) -> Vec<CgroupName> {
        self.state
            .lock()
            .map(|s| s.kills.clone())
            .unwrap_or_default()
    }

    /// Cgroups removed, in order.
    pub fn removed(&self) -> Vec<CgroupName> {
        self.state
            .lock()
            .map(|s| s.removed.clone())
            .unwrap_or_default()
    }

    /// The `cpu.weight` last set on a cgroup.
    pub fn cpu_weight(&self, name: CgroupName) -> Option<u32> {
        self.state.lock().ok()?.weights.get(&name).copied()
    }

    /// True while the cgroup exists.
    pub fn exists(&self, name: CgroupName) -> bool {
        self.state
            .lock()
            .map(|s| s.groups.contains_key(&name))
            .unwrap_or(false)
    }
}

impl Cgroups for FakeCgroups {
    fn status(&self) -> CgroupStatus {
        if self.delegated {
            CgroupStatus::Delegated {
                slice: PathBuf::from("/fake/tasks.slice"),
                controllers: vec!["io".into(), "memory".into(), "cpu".into()],
            }
        } else {
            CgroupStatus::Unavailable {
                reason: self.reason.clone(),
            }
        }
    }
    fn create(&self, name: CgroupName) -> Result<PathBuf, PlatformError> {
        if !self.delegated {
            return Err(PlatformError::NoCgroups(self.reason.clone()));
        }
        self.state
            .lock()
            .map_err(poisoned)?
            .groups
            .entry(name)
            .or_insert_with(|| FakeGroup {
                pids: Vec::new(),
                figures: JobFigures::empty(Attribution::Cgroup),
            });
        Ok(PathBuf::from(format!(
            "/fake/tasks.slice/{}/cgroup.procs",
            name.dir_name()
        )))
    }
    fn set_cpu_weight(&self, name: CgroupName, weight: u32) -> Result<bool, PlatformError> {
        if !self.delegated {
            return Ok(false);
        }
        self.state
            .lock()
            .map_err(poisoned)?
            .weights
            .insert(name, weight);
        Ok(true)
    }
    fn pids(&self, name: CgroupName) -> Result<Vec<i32>, PlatformError> {
        Ok(self
            .state
            .lock()
            .map_err(poisoned)?
            .groups
            .get(&name)
            .map(|g| g.pids.clone())
            .unwrap_or_default())
    }
    fn read(&self, name: CgroupName) -> Result<JobFigures, PlatformError> {
        self.state
            .lock()
            .map_err(poisoned)?
            .groups
            .get(&name)
            .map(|g| g.figures.clone())
            .ok_or_else(|| PlatformError::NoCgroups(format!("{} does not exist", name.dir_name())))
    }
    fn kill(&self, name: CgroupName) -> Result<(), PlatformError> {
        let mut s = self.state.lock().map_err(poisoned)?;
        s.kills.push(name);
        if let Some(g) = s.groups.get_mut(&name) {
            g.pids.clear();
        }
        Ok(())
    }
    fn remove(&self, name: CgroupName) -> Result<(), PlatformError> {
        let mut s = self.state.lock().map_err(poisoned)?;
        match s.groups.get(&name) {
            Some(g) if !g.pids.is_empty() => {
                Err(PlatformError::CgroupBusy(PathBuf::from(name.dir_name())))
            }
            Some(_) => {
                s.groups.remove(&name);
                s.removed.push(name);
                Ok(())
            }
            None => Ok(()),
        }
    }
    fn list_jobs(&self) -> Result<Vec<i64>, PlatformError> {
        Ok(self
            .state
            .lock()
            .map_err(poisoned)?
            .groups
            .keys()
            .filter_map(|n| match n {
                CgroupName::Job(id) => Some(*id),
                CgroupName::Transfer => None,
            })
            .collect())
    }
}

impl FakePlatform {
    /// Sets what [`Service::status`] reports.
    pub fn set_service_status(&self, s: ServiceStatus) {
        if let Ok(mut g) = self.status.lock() {
            *g = s;
        }
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
    fn start(&self, name: &str) -> Result<(), PlatformError> {
        self.service_starts
            .lock()
            .map_err(poisoned)?
            .push(name.to_string());
        match self.service_start_error.lock().map_err(poisoned)?.clone() {
            Some(detail) => Err(PlatformError::Command {
                command: format!("fake-start {name}"),
                detail,
            }),
            None => Ok(()),
        }
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

/// A scripted power state and sleep guard: counts acquisitions and releases.
#[derive(Debug)]
pub struct FakePower {
    reading: Mutex<PowerReading>,
    refuse: Mutex<Option<String>>,
    held: Arc<AtomicUsize>,
    acquired: AtomicUsize,
}

impl Default for FakePower {
    fn default() -> Self {
        FakePower {
            reading: Mutex::new(PowerReading::AC),
            refuse: Mutex::new(None),
            held: Arc::new(AtomicUsize::new(0)),
            acquired: AtomicUsize::new(0),
        }
    }
}

impl FakePower {
    /// Sets what [`PowerSource::power`] returns.
    pub fn set_power(&self, r: PowerReading) {
        if let Ok(mut g) = self.reading.lock() {
            *g = r;
        }
    }

    /// Makes [`SleepGuard::keep_awake`] fail with this reason (`None` makes it work again).
    pub fn refuse_with(&self, why: Option<&str>) {
        if let Ok(mut g) = self.refuse.lock() {
            *g = why.map(str::to_string);
        }
    }

    /// Guards currently held.
    pub fn held(&self) -> usize {
        self.held.load(Ordering::SeqCst)
    }

    /// Guards ever taken.
    pub fn acquired(&self) -> usize {
        self.acquired.load(Ordering::SeqCst)
    }
}

impl PowerSource for FakePower {
    fn power(&self) -> Result<PowerReading, PlatformError> {
        Ok(*self.reading.lock().map_err(poisoned)?)
    }
}

impl SleepGuard for FakePower {
    fn keep_awake(&self, _why: &str) -> Result<InhibitGuard, PlatformError> {
        if let Some(why) = self.refuse.lock().map_err(poisoned)?.clone() {
            return Err(PlatformError::Command {
                command: "fake-inhibit".into(),
                detail: why,
            });
        }
        self.held.fetch_add(1, Ordering::SeqCst);
        self.acquired.fetch_add(1, Ordering::SeqCst);
        let held = self.held.clone();
        Ok(InhibitGuard::new(move || {
            held.fetch_sub(1, Ordering::SeqCst);
        }))
    }
}
