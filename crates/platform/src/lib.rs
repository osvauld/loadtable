//! Native measurement primitives for Linux and macOS.
//!
//! This is preparation, not a complete macOS worker backend. In particular it
//! does not emulate Linux PSI, inspect flock ownership, or implement parent-death
//! supervision. Resource floors alone are NOT permission to launch a job.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("task-manager-platform currently supports only Linux and macOS");

mod disk;
pub mod fs;
mod memory;
pub mod owned_group;
pub mod process;
mod process_control;
mod telemetry;
pub mod thread;
mod usage;

pub use disk::{DiskSpace, disk_space};
pub use memory::{Memory, MemorySource, memory};
pub use process::{ProcessIdentity, alive, identity};
pub use telemetry::{MachineReading, MachineSampler};
pub use usage::{ChildUsage, children_usage};

/// The native APIs present on this OS. This describes implementation support,
/// not the health of any particular sample or authorization to execute jobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Capabilities {
    pub process_start_identity: bool,
    pub memory: bool,
    pub disk: bool,
    pub linux_psi: bool,
    pub noninvasive_flock_observation: bool,
    pub kernel_parent_death_signal: bool,
}

pub const fn capabilities() -> Capabilities {
    Capabilities {
        process_start_identity: true,
        memory: true,
        disk: true,
        linux_psi: cfg!(target_os = "linux"),
        noninvasive_flock_observation: cfg!(target_os = "linux"),
        kernel_parent_death_signal: cfg!(target_os = "linux"),
    }
}

/// A snapshot of the memory/disk floors, independent of PSI policy. An error
/// reading a required measurement must be propagated, never replaced with zero
/// pressure or an unlimited resource. Linux admission must ALSO check PSI.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ResourceSample {
    pub memory: Memory,
    pub cache_disk: DiskSpace,
    pub state_disk: DiskSpace,
}

impl ResourceSample {
    pub fn read(cache: &std::path::Path, state: &std::path::Path) -> std::io::Result<Self> {
        Ok(Self {
            memory: memory()?,
            cache_disk: disk_space(cache)?,
            state_disk: disk_space(state)?,
        })
    }

    /// Only checks floors. `None` means these three floors pass, NOT that all
    /// admission requirements (pressure, leases, ownership, approvals) pass.
    pub fn floor_blocker(
        &self,
        min_memory: u64,
        min_cache: u64,
        min_state: u64,
    ) -> Option<&'static str> {
        if self.memory.available_bytes < min_memory {
            Some("available memory below reserve")
        } else if self.cache_disk.available_bytes < min_cache {
            Some("cache disk below reserve")
        } else if self.state_disk.available_bytes < min_state {
            Some("state disk below reserve")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floors_are_inclusive_and_independent() {
        let sample = ResourceSample {
            memory: Memory {
                total_bytes: 200,
                available_bytes: 100,
                source: MemorySource::LinuxMemAvailable,
            },
            cache_disk: DiskSpace {
                available_bytes: 50,
            },
            state_disk: DiskSpace {
                available_bytes: 25,
            },
        };
        assert_eq!(sample.floor_blocker(100, 50, 25), None);
        assert_eq!(
            sample.floor_blocker(101, 50, 25),
            Some("available memory below reserve")
        );
        assert_eq!(
            sample.floor_blocker(100, 51, 25),
            Some("cache disk below reserve")
        );
        assert_eq!(
            sample.floor_blocker(100, 50, 26),
            Some("state disk below reserve")
        );
    }

    #[test]
    fn macos_capabilities_do_not_claim_linux_guarantees() {
        let caps = capabilities();
        assert!(caps.process_start_identity && caps.memory && caps.disk);
        assert_eq!(caps.linux_psi, cfg!(target_os = "linux"));
        assert_eq!(
            caps.noninvasive_flock_observation,
            cfg!(target_os = "linux")
        );
        assert_eq!(caps.kernel_parent_death_signal, cfg!(target_os = "linux"));
    }
}
