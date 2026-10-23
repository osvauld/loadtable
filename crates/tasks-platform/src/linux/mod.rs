//! Linux implementations of the platform traits (`/proc`, PSI, systemd).

mod memory;
mod power;
mod pressure;
mod procs;
mod service;
mod table;

use crate::{
    Child, Disk, GroupId, InhibitGuard, InstallPlan, Memory, MemorySnapshot, PlatformError,
    PowerReading, PowerSource, Pressure, PressureSnapshot, PressureThresholds, ProcInfo,
    ProcessTable, Processes, Service, ServiceStatus, ServiceUnit, Signal, SleepGuard, SpawnSpec,
    Usage, disk, groups,
};
use std::path::Path;

pub use memory::{parse_meminfo, parse_stat_cpu, parse_vmstat_swap};
pub use pressure::{PsiFile, PsiLine, parse_psi};
pub use procs::{ProcStat, parse_proc_stat};
pub use service::parse_is_active;
pub use table::{ProcFsTable, parse_proc_io};

/// The Linux platform implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct Native;

impl Memory for Native {
    fn snapshot(&self) -> Result<MemorySnapshot, PlatformError> {
        memory::read()
    }
}
impl Pressure for Native {
    fn snapshot(&self, t: &PressureThresholds) -> Result<PressureSnapshot, PlatformError> {
        pressure::read(t)
    }
}
impl Processes for Native {
    fn spawn_group(&self, cmd: &SpawnSpec) -> Result<Child, PlatformError> {
        groups::spawn(cmd, &procs::start_time)
    }
    fn signal_group(&self, group: GroupId, sig: Signal) -> Result<(), PlatformError> {
        groups::signal(group, sig, &procs::start_time)
    }
    fn usage(&self, group: GroupId) -> Result<Usage, PlatformError> {
        procs::usage(group)
    }
    fn peek_exit(&self, child: &Child) -> Result<Option<i32>, PlatformError> {
        groups::peek_exit(child)
    }
    fn try_wait(&self, child: &Child) -> Result<Option<i32>, PlatformError> {
        groups::try_wait(child)
    }
}
impl ProcessTable for Native {
    fn snapshot(&self) -> Result<Vec<ProcInfo>, PlatformError> {
        ProcFsTable.snapshot()
    }
    fn io_of(&self, pid: i32) -> Option<(u64, u64)> {
        ProcFsTable.io_of(pid)
    }
}
impl Service for Native {
    fn install(&self, unit: &ServiceUnit) -> Result<InstallPlan, PlatformError> {
        service::install(unit)
    }
    fn status(&self, name: &str) -> Result<ServiceStatus, PlatformError> {
        service::status(name)
    }
}
impl Disk for Native {
    fn free_bytes(&self, path: &Path) -> Result<u64, PlatformError> {
        disk::free_bytes(path)
    }
}
impl PowerSource for Native {
    fn power(&self) -> Result<PowerReading, PlatformError> {
        power::read()
    }
}
impl SleepGuard for Native {
    fn keep_awake(&self, why: &str) -> Result<InhibitGuard, PlatformError> {
        power::inhibit(why)
    }
}
