//! OS abstraction for `tasks`: traits for memory, pressure, processes, slot locks,
//! services and disk, with Linux and macOS implementations and test fakes.

mod cgroup;
mod disk;
mod error;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
mod fds;
mod groups;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
// The macOS parsers are pure, so they are also compiled under test on Linux.
#[cfg(any(test, target_os = "macos"))]
#[path = "macos/parse.rs"]
mod macos_parse;
mod sample;
mod service;
mod slots;
mod traits;
mod types;

pub use cgroup::{
    CgroupFs, CgroupName, CgroupStatus, CgroupTree, Cgroups, IoTotals, NoCgroups, PsiTotals,
    RealFs, WANTED_CONTROLLERS, delegated_root, native_cgroups, parse_cpu_usage, parse_io_stat,
    parse_psi_totals, parse_self_cgroup, parse_single_u64,
};
pub use error::PlatformError;
pub use fds::close_on_exec_inherited;
pub use sample::{
    OutsideSampler, ProcInfo, ProcessTable, TOP_N, TopOutside, TopProc, TreeMeter,
    clean_program_name, tree_pids,
};
pub use service::{render_launchd_plist, render_systemd_unit};
pub use slots::{FlockSlots, SlotGuard, slot_lock_path};
pub use traits::{Disk, Memory, Pressure, Processes, Service, SlotLocks};
pub use types::{
    Child, GroupId, HostRates, InstallPlan, Level, MemorySnapshot, PLACE_SCRIPT, PressureSnapshot,
    PressureThresholds, ServiceStatus, ServiceUnit, Signal, SpawnSpec, Usage, place_in_cgroup,
};

#[cfg(target_os = "linux")]
pub use linux::Native;
#[cfg(target_os = "macos")]
pub use macos::Native;

/// Returns the implementation for the current OS.
pub fn native() -> Native {
    Native
}
