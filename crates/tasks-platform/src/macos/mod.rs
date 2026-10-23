//! macOS implementations of the platform traits (sysctl, vm_stat, libproc, launchd).

mod bsdinfo;
mod memory;
mod pressure;
mod procs;
mod service;
mod table;

use crate::{
    Child, Disk, GroupId, InstallPlan, Memory, MemorySnapshot, PlatformError, Pressure,
    PressureSnapshot, PressureThresholds, ProcInfo, ProcessTable, Processes, Service,
    ServiceStatus, ServiceUnit, Signal, SpawnSpec, Usage, disk, groups,
};
use std::path::Path;
use std::process::Command;

/// The macOS platform implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct Native;

/// Runs a system tool by absolute path and returns its stdout; nonzero exit is an error.
pub(crate) fn run(program: &str, args: &[&str]) -> Result<String, PlatformError> {
    let command = format!("{program} {}", args.join(" "));
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| PlatformError::Command {
            command: command.clone(),
            detail: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(PlatformError::Command {
            command,
            detail: format!(
                "{}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Memory for Native {
    fn snapshot(&self) -> Result<MemorySnapshot, PlatformError> {
        memory::read()
    }
}
impl Pressure for Native {
    /// Thresholds are ignored: macOS reports a native level, not PSI.
    fn snapshot(&self, _t: &PressureThresholds) -> Result<PressureSnapshot, PlatformError> {
        pressure::read()
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
        table::snapshot()
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
