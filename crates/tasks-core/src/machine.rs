//! Machine description types.

use crate::{DeviceId, MachineName, Tool};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Os {
    /// Linux.
    Linux,
    /// macOS.
    Macos,
}

impl Os {
    /// The OS this binary was compiled for.
    #[cfg(target_os = "linux")]
    pub fn current() -> Os {
        Os::Linux
    }
    /// The OS this binary was compiled for.
    #[cfg(target_os = "macos")]
    pub fn current() -> Os {
        Os::Macos
    }
}

/// CPU architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Arch {
    /// x86-64.
    X86_64,
    /// 64-bit ARM.
    Aarch64,
}

impl Arch {
    /// The architecture this binary was compiled for.
    #[cfg(target_arch = "x86_64")]
    pub fn current() -> Arch {
        Arch::X86_64
    }
    /// The architecture this binary was compiled for.
    #[cfg(target_arch = "aarch64")]
    pub fn current() -> Arch {
        Arch::Aarch64
    }
}

/// A machine that can run jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    /// Machine name.
    pub name: MachineName,
    /// Operating system.
    pub os: Os,
    /// CPU architecture.
    pub arch: Arch,
    /// Logical core count.
    pub cores: u32,
    /// Total memory in bytes.
    pub memory_bytes: u64,
    /// Tools available.
    pub tools: BTreeSet<Tool>,
    /// Devices attached.
    pub devices: BTreeSet<DeviceId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_matches_host() {
        let _ = Os::current();
        let _ = Arch::current();
        #[cfg(target_os = "linux")]
        assert_eq!(Os::current(), Os::Linux);
    }
}
