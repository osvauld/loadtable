//! Per-job cgroups (design §9): the trait the runner and daemon use, the cgroup v2 file
//! parsers, the tree operations over an abstract file system, and the "not delegated" stand-in.
//!
//! Everything here is plain file operations under one directory, so it compiles and is tested on
//! every OS; only finding the daemon's own cgroup (`/proc/self/cgroup`) is Linux-specific.

mod figures;
mod fs;
mod tree;

pub use figures::{
    IoTotals, PsiTotals, parse_cpu_usage, parse_io_stat, parse_psi_totals, parse_single_u64,
};
pub use fs::{CgroupFs, RealFs};
pub use tree::{CgroupTree, delegated_root, parse_self_cgroup};

use crate::PlatformError;
use std::path::PathBuf;
use tasks_core::JobFigures;

/// Controllers the daemon asks for in its delegated subtree.
pub const WANTED_CONTROLLERS: [&str; 3] = ["io", "memory", "cpu"];

/// Which cgroup a piece of work lives in: one per job, one for sync and fetch work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CgroupName {
    /// `tasks.slice/job-<id>`.
    Job(i64),
    /// `tasks.slice/transfer`.
    Transfer,
}

impl CgroupName {
    /// The directory name under `tasks.slice`.
    pub fn dir_name(self) -> String {
        match self {
            CgroupName::Job(id) => format!("job-{id}"),
            CgroupName::Transfer => "transfer".to_string(),
        }
    }

    /// Parses a directory name back; `None` for anything that is not ours.
    pub fn from_dir_name(name: &str) -> Option<CgroupName> {
        if name == "transfer" {
            return Some(CgroupName::Transfer);
        }
        name.strip_prefix("job-")
            .and_then(|n| n.parse::<i64>().ok())
            .map(CgroupName::Job)
    }
}

/// Whether the daemon can use cgroups, and if not why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CgroupStatus {
    /// The daemon owns a delegated subtree and created `tasks.slice` in it.
    Delegated {
        /// The `tasks.slice` directory.
        slice: PathBuf,
        /// Controllers enabled for jobs (a subset of io, memory, cpu).
        controllers: Vec<String>,
    },
    /// Process groups only.
    Unavailable {
        /// Why, in words an operator can act on.
        reason: String,
    },
}

impl CgroupStatus {
    /// The one status line: `attribution: exact cgroup (...)` or `attribution: process tree
    /// (no cgroup delegation: ...)`.
    pub fn line(&self) -> String {
        match self {
            CgroupStatus::Delegated { slice, controllers } => format!(
                "attribution: exact cgroup ({}; controllers {})",
                slice.display(),
                if controllers.is_empty() {
                    "none".to_string()
                } else {
                    controllers.join(",")
                }
            ),
            CgroupStatus::Unavailable { reason } => {
                format!("attribution: process tree (no cgroup delegation: {reason})")
            }
        }
    }

    /// True when per-job cgroups work.
    pub fn is_delegated(&self) -> bool {
        matches!(self, CgroupStatus::Delegated { .. })
    }
}

/// Creates, reads, kills and removes the cgroups of jobs and transfers.
pub trait Cgroups: Send + Sync {
    /// Whether cgroups work and where.
    fn status(&self) -> CgroupStatus;
    /// Creates the cgroup (an existing one is reused) and returns its `cgroup.procs` file, which
    /// the child writes its own pid into before it execs. `Err` when not delegated.
    fn create(&self, name: CgroupName) -> Result<PathBuf, PlatformError>;
    /// Sets the cgroup's CPU share (`cpu.weight`, 1..=10000, the kernel default is 100) so that
    /// competing jobs split the CPU in that proportion. `Ok(true)` when set, `Ok(false)` when
    /// the cpu controller is not enabled here (the caller falls back to niceness).
    fn set_cpu_weight(&self, name: CgroupName, weight: u32) -> Result<bool, PlatformError>;
    /// Pids currently in the cgroup (`cgroup.procs`); an absent cgroup has none.
    fn pids(&self, name: CgroupName) -> Result<Vec<i32>, PlatformError>;
    /// Reads the figures: `io.stat`, `memory.peak`, `cpu.stat` and the three pressure totals. A
    /// controller that is not enabled leaves its figures `None`.
    fn read(&self, name: CgroupName) -> Result<JobFigures, PlatformError>;
    /// Kills every process in the cgroup (`cgroup.kill`), including children that left the
    /// process group. The last resort after the group kill.
    fn kill(&self, name: CgroupName) -> Result<(), PlatformError>;
    /// Removes the cgroup if it is empty; an absent one is fine; a populated one is `Busy`.
    fn remove(&self, name: CgroupName) -> Result<(), PlatformError>;
    /// The job cgroups that exist now (for the start-up sweep of stale ones).
    fn list_jobs(&self) -> Result<Vec<i64>, PlatformError>;
}

/// Cgroups when there is no delegation: every operation reports why.
#[derive(Debug, Clone)]
pub struct NoCgroups {
    reason: String,
}

impl NoCgroups {
    /// Process groups only, for `reason`.
    pub fn new(reason: impl Into<String>) -> NoCgroups {
        NoCgroups {
            reason: reason.into(),
        }
    }
}

impl Cgroups for NoCgroups {
    fn status(&self) -> CgroupStatus {
        CgroupStatus::Unavailable {
            reason: self.reason.clone(),
        }
    }
    fn create(&self, _name: CgroupName) -> Result<PathBuf, PlatformError> {
        Err(PlatformError::NoCgroups(self.reason.clone()))
    }
    fn set_cpu_weight(&self, _name: CgroupName, _weight: u32) -> Result<bool, PlatformError> {
        Ok(false)
    }
    fn pids(&self, _name: CgroupName) -> Result<Vec<i32>, PlatformError> {
        Ok(Vec::new())
    }
    fn read(&self, _name: CgroupName) -> Result<JobFigures, PlatformError> {
        Err(PlatformError::NoCgroups(self.reason.clone()))
    }
    fn kill(&self, _name: CgroupName) -> Result<(), PlatformError> {
        Err(PlatformError::NoCgroups(self.reason.clone()))
    }
    fn remove(&self, _name: CgroupName) -> Result<(), PlatformError> {
        Ok(())
    }
    fn list_jobs(&self) -> Result<Vec<i64>, PlatformError> {
        Ok(Vec::new())
    }
}

/// The cgroups for this machine: the daemon's delegated subtree on Linux when it is writable,
/// otherwise [`NoCgroups`] with the reason. Never fails; a failure to set up is the reason.
pub fn native_cgroups() -> Box<dyn Cgroups> {
    #[cfg(target_os = "linux")]
    {
        match tree::setup_native() {
            Ok(t) => Box::new(t),
            Err(reason) => Box::new(NoCgroups::new(reason)),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Box::new(NoCgroups::new("this OS has no cgroups"))
    }
}
