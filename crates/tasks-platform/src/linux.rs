//! Linux implementations of the platform traits (skeleton: not yet implemented).

use crate::{
    Child, Disk, GroupId, Memory, MemorySnapshot, PlatformError, Pressure, PressureSnapshot,
    Processes, Service, ServiceStatus, ServiceUnit, Signal, SpawnSpec, Usage,
};
use std::path::Path;

/// The Linux platform implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct Native;

impl Memory for Native {
    fn snapshot(&self) -> Result<MemorySnapshot, PlatformError> {
        Err(PlatformError::NotYetImplemented("linux memory"))
    }
}
impl Pressure for Native {
    fn snapshot(&self) -> Result<PressureSnapshot, PlatformError> {
        Err(PlatformError::NotYetImplemented("linux pressure"))
    }
}
impl Processes for Native {
    fn spawn_group(&self, _cmd: &SpawnSpec) -> Result<Child, PlatformError> {
        Err(PlatformError::NotYetImplemented("linux spawn_group"))
    }
    fn signal_group(&self, _group: GroupId, _sig: Signal) -> Result<(), PlatformError> {
        Err(PlatformError::NotYetImplemented("linux signal_group"))
    }
    fn usage(&self, _group: GroupId) -> Result<Usage, PlatformError> {
        Err(PlatformError::NotYetImplemented("linux usage"))
    }
}
impl Service for Native {
    fn install(&self, _unit: &ServiceUnit) -> Result<(), PlatformError> {
        Err(PlatformError::NotYetImplemented("linux service install"))
    }
    fn status(&self) -> Result<ServiceStatus, PlatformError> {
        Err(PlatformError::NotYetImplemented("linux service status"))
    }
}
impl Disk for Native {
    fn free_bytes(&self, _path: &Path) -> Result<u64, PlatformError> {
        Err(PlatformError::NotYetImplemented("linux disk"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_is_not_implemented() {
        assert!(matches!(
            Memory::snapshot(&Native),
            Err(PlatformError::NotYetImplemented(_))
        ));
    }
}
