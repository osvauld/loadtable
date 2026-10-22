//! The platform traits.

use crate::{
    Child, GroupId, MemorySnapshot, PlatformError, PressureSnapshot, ServiceStatus, ServiceUnit,
    Signal, SlotGuard, SpawnSpec, Usage,
};
use std::path::Path;
use tasks_core::SlotKind;

/// Reads system memory.
pub trait Memory {
    /// Current total and available memory.
    fn snapshot(&self) -> Result<MemorySnapshot, PlatformError>;
}

/// Reads system pressure.
pub trait Pressure {
    /// Current pressure (Linux PSI avg10; macOS memorystatus level).
    fn snapshot(&self) -> Result<PressureSnapshot, PlatformError>;
}

/// Spawns and supervises process groups.
pub trait Processes {
    /// Starts a command in a new session and process group, output to the log file.
    fn spawn_group(&self, cmd: &SpawnSpec) -> Result<Child, PlatformError>;
    /// Sends a signal to a whole group.
    fn signal_group(&self, group: GroupId, sig: Signal) -> Result<(), PlatformError>;
    /// Resource usage of a group.
    fn usage(&self, group: GroupId) -> Result<Usage, PlatformError>;
}

/// Exclusive slot locks held via file locks.
pub trait SlotLocks {
    /// Tries to take slot `index` of `kind`; `None` when already held.
    fn try_acquire(&self, kind: SlotKind, index: u32) -> Result<Option<SlotGuard>, PlatformError>;
}

/// Installs and queries the background service.
pub trait Service {
    /// Installs the unit (systemd user unit or launchd agent).
    fn install(&self, unit: &ServiceUnit) -> Result<(), PlatformError>;
    /// Reports whether the service is installed and running.
    fn status(&self) -> Result<ServiceStatus, PlatformError>;
}

/// Reports free disk space.
pub trait Disk {
    /// Free bytes on the filesystem holding `path`.
    fn free_bytes(&self, path: &Path) -> Result<u64, PlatformError>;
}
