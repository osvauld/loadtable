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
    /// A cgroup's `cgroup.procs` file: the child writes its own pid into it before it execs,
    /// so it and everything it starts are in the cgroup from the first instruction. `None`
    /// spawns directly.
    pub cgroup_procs: Option<PathBuf>,
}

/// The shell script that places a child in a cgroup and then becomes the real command. `$0` is
/// the `cgroup.procs` file and `"$@"` the command. The write may fail (the cgroup vanished, the
/// file is not writable); the command still runs and the caller notices the missing placement.
pub const PLACE_SCRIPT: &str = "{ echo $$ >\"$0\"; } 2>/dev/null; exec \"$@\"";

/// Wraps `argv` so that the process joins the cgroup behind `procs` before it execs `argv`.
///
/// Why a shell and not `pre_exec`: `CommandExt::pre_exec` is `unsafe` (the closure runs between
/// fork and exec in a possibly multi-threaded process) and the workspace denies `unsafe`. Writing
/// the pid from the parent after the spawn would leave a window in which the child could fork
/// children that stay outside the cgroup. `clone3` with `CLONE_INTO_CGROUP` needs `unsafe` too.
/// The shell writes its own pid and then `exec`s, so pid, process-group leadership and start time
/// are the command's, and nothing the command starts is ever outside the cgroup.
pub fn place_in_cgroup(procs: &std::path::Path, argv: &[String]) -> Vec<String> {
    let mut out = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        PLACE_SCRIPT.to_string(),
        procs.display().to_string(),
    ];
    out.extend(argv.iter().cloned());
    out
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
