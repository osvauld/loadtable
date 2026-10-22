//! The platform traits.

use crate::{
    Child, GroupId, InstallPlan, MemorySnapshot, PlatformError, PressureSnapshot,
    PressureThresholds, ServiceStatus, ServiceUnit, Signal, SlotGuard, SpawnSpec, Usage,
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
    /// Current pressure (Linux PSI avg10 mapped with `thresholds`; macOS
    /// memorystatus level, which ignores `thresholds`). Fails closed on error.
    fn snapshot(&self, thresholds: &PressureThresholds) -> Result<PressureSnapshot, PlatformError>;
}

/// Spawns and supervises process groups.
pub trait Processes {
    /// Starts a command in a new session and process group, output to the log file.
    fn spawn_group(&self, cmd: &SpawnSpec) -> Result<Child, PlatformError>;
    /// Sends a signal to a whole group. Fails with `Gone` if the leader's start
    /// time no longer matches, and refuses group ids <= 1 and our own group.
    fn signal_group(&self, group: GroupId, sig: Signal) -> Result<(), PlatformError>;
    /// Resource usage of a group (live, non-zombie processes). `Gone` on PID reuse.
    fn usage(&self, group: GroupId) -> Result<Usage, PlatformError>;
    /// The leader's exit code without reaping it: `Some(code)` once it exited
    /// (128 + n when killed by signal n), `None` while running. The unreaped
    /// leader keeps the group id reserved, so descendants can still be cleaned.
    fn peek_exit(&self, child: &Child) -> Result<Option<i32>, PlatformError>;
    /// Reaps the leader without blocking, with the same codes as
    /// [`Processes::peek_exit`]. Call only after descendants are cleaned.
    fn try_wait(&self, child: &Child) -> Result<Option<i32>, PlatformError>;
}

/// Exclusive slot locks held via file locks.
pub trait SlotLocks {
    /// Tries to take slot `index` of `kind`; `None` when already held.
    fn try_acquire(&self, kind: SlotKind, index: u32) -> Result<Option<SlotGuard>, PlatformError>;
}

/// Installs and queries the background service.
pub trait Service {
    /// Writes the unit file (systemd user unit or launchd agent) without
    /// starting anything; never overwrites a different file (`Exists`).
    fn install(&self, unit: &ServiceUnit) -> Result<InstallPlan, PlatformError>;
    /// Reports whether the named service is installed and running.
    fn status(&self, name: &str) -> Result<ServiceStatus, PlatformError>;
}

/// Reports free disk space.
pub trait Disk {
    /// Free bytes on the filesystem holding `path`.
    fn free_bytes(&self, path: &Path) -> Result<u64, PlatformError>;
}
