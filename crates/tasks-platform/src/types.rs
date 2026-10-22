//! Plain data types shared by the platform traits.

use std::path::PathBuf;

/// Memory totals in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemorySnapshot {
    /// Total memory.
    pub total_bytes: u64,
    /// Memory available to new work.
    pub available_bytes: u64,
}

/// Coarse pressure level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// No pressure.
    Normal,
    /// Elevated pressure.
    Warn,
    /// Severe pressure.
    Critical,
}

/// System pressure reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PressureSnapshot {
    /// Memory pressure level (from PSI thresholds on Linux, the native level on macOS).
    pub memory: Level,
    /// Memory PSI `some` avg10 percent (Linux only).
    pub memory_some: Option<f32>,
    /// Memory PSI `full` avg10 percent (Linux only).
    pub memory_full: Option<f32>,
    /// CPU pressure: PSI `some` avg10 percent on Linux; on macOS an ESTIMATE:
    /// the 1-minute load average divided by logical cores (a ratio where 1.0 means
    /// one runnable task per core, not a percentage), which is not PSI.
    pub cpu: Option<f32>,
    /// I/O PSI `some` avg10 percent (Linux only; `None` on macOS).
    pub io: Option<f32>,
}

/// Thresholds mapping PSI memory percentages (avg10) to a [`Level`].
/// A value at or above a threshold reaches that level. Nothing is hard-coded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PressureThresholds {
    /// `some` avg10 at or above this is `Warn`.
    pub memory_some_warn: f32,
    /// `some` avg10 at or above this is `Critical`.
    pub memory_some_critical: f32,
    /// `full` avg10 at or above this is `Warn`.
    pub memory_full_warn: f32,
    /// `full` avg10 at or above this is `Critical`.
    pub memory_full_critical: f32,
}

impl PressureThresholds {
    /// Rejects non-finite, negative or inverted (warn above critical) values.
    pub fn validate(&self) -> Result<(), crate::PlatformError> {
        let pairs = [
            (
                "memory_some",
                self.memory_some_warn,
                self.memory_some_critical,
            ),
            (
                "memory_full",
                self.memory_full_warn,
                self.memory_full_critical,
            ),
        ];
        for (name, warn, critical) in pairs {
            let ok = warn.is_finite() && critical.is_finite() && warn >= 0.0 && warn <= critical;
            if !ok {
                return Err(crate::PlatformError::InvalidThresholds(format!(
                    "{name}: warn={warn} critical={critical}"
                )));
            }
        }
        Ok(())
    }

    /// Maps memory PSI readings to a level.
    pub fn level(&self, some: f32, full: f32) -> Level {
        if some >= self.memory_some_critical || full >= self.memory_full_critical {
            Level::Critical
        } else if some >= self.memory_some_warn || full >= self.memory_full_warn {
            Level::Warn
        } else {
            Level::Normal
        }
    }
}

/// Process group identity: the group id plus the start time of its leader
/// (process id equals group id). The start time guards against PID reuse.
/// Units are OS-specific (Linux clock ticks since boot, macOS microseconds
/// since the epoch); treat it as an opaque token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupId {
    /// Process group id (equals the leader pid).
    pub pgid: i32,
    /// Opaque start time of the leader.
    pub start_time: u64,
}

/// Signal to send to a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Polite termination request.
    Term,
    /// Forced kill.
    Kill,
    /// Stop (pause).
    Stop,
    /// Continue.
    Cont,
}

/// How to start a process group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// Command and arguments; the first element is the program.
    pub argv: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Complete environment; the parent environment is cleared.
    pub env: Vec<(String, String)>,
    /// File receiving stdout and stderr.
    pub log_path: PathBuf,
}

/// A spawned process group leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Child {
    /// Leader pid.
    pub pid: i32,
    /// Process group id.
    pub group: GroupId,
}

/// Resource usage of a process group.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    /// User CPU seconds.
    pub cpu_user_s: f64,
    /// System CPU seconds.
    pub cpu_sys_s: f64,
    /// Resident memory in bytes.
    pub rss_bytes: u64,
    /// Number of processes.
    pub processes: u32,
}

/// A background service definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceUnit {
    /// Service name.
    pub name: String,
    /// Command and arguments.
    pub argv: Vec<String>,
}

/// What `Service::install` did and what the user should run next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// The unit or plist file path.
    pub path: PathBuf,
    /// True when the file was written now; false when an identical file existed.
    pub contents_written: bool,
    /// Command the user should run to enable or start the service.
    pub next_command: String,
}

/// State of an installed service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
    /// Not installed.
    NotInstalled,
    /// Installed but stopped.
    Stopped,
    /// Running.
    Running,
}
