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
    /// Memory pressure level.
    pub memory: Level,
    /// CPU pressure (PSI avg10 percent), where available.
    pub cpu: Option<f32>,
    /// I/O pressure (PSI avg10 percent), where available.
    pub io: Option<f32>,
}

/// Process group identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupId(pub i32);

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
    /// Command and arguments.
    pub argv: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Extra environment.
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
