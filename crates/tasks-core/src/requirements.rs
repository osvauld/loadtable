//! Job requirements and matching against machines.

use crate::{Arch, DeviceId, Machine, Os, Tool};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// What a job needs from the machine that runs it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Requirements {
    /// Required OS.
    pub os: Option<Os>,
    /// Required architecture.
    pub arch: Option<Arch>,
    /// Required tools.
    pub tools: BTreeSet<Tool>,
    /// Required attached device.
    pub device: Option<DeviceId>,
    /// Minimum total memory in bytes.
    pub min_memory_bytes: Option<u64>,
    /// The job only reads the synced checkout it runs in (a test step that follows its plan's
    /// build): jobs like it may run in the same remote copy at the same time (R-700). Set when a
    /// plan is expanded into jobs, never by a submitter.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub shared_checkout: bool,
}

/// One requirement a machine fails to meet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unmet {
    /// Wrong OS.
    Os {
        /// Required.
        want: Os,
        /// Machine has.
        have: Os,
    },
    /// Wrong architecture.
    Arch {
        /// Required.
        want: Arch,
        /// Machine has.
        have: Arch,
    },
    /// Missing tool.
    Tool(Tool),
    /// Missing device.
    Device(DeviceId),
    /// Not enough memory.
    Memory {
        /// Required bytes.
        want: u64,
        /// Machine has bytes.
        have: u64,
    },
}

impl Requirements {
    /// Checks the machine meets every requirement, listing each unmet one otherwise.
    pub fn satisfied_by(&self, m: &Machine) -> Result<(), Vec<Unmet>> {
        let mut unmet = Vec::new();
        if let Some(want) = self.os.filter(|w| *w != m.os) {
            unmet.push(Unmet::Os { want, have: m.os });
        }
        if let Some(want) = self.arch.filter(|w| *w != m.arch) {
            unmet.push(Unmet::Arch { want, have: m.arch });
        }
        unmet.extend(
            self.tools
                .iter()
                .filter(|t| !m.tools.contains(*t))
                .cloned()
                .map(Unmet::Tool),
        );
        if let Some(d) = self.device.as_ref().filter(|d| !m.devices.contains(*d)) {
            unmet.push(Unmet::Device(d.clone()));
        }
        if let Some(want) = self.min_memory_bytes.filter(|w| *w > m.memory_bytes) {
            unmet.push(Unmet::Memory {
                want,
                have: m.memory_bytes,
            });
        }
        if unmet.is_empty() { Ok(()) } else { Err(unmet) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MachineName;

    fn machine() -> Machine {
        Machine {
            name: MachineName("m".into()),
            os: Os::Linux,
            arch: Arch::X86_64,
            cores: 8,
            memory_bytes: 16 << 30,
            tools: [Tool("cargo".into())].into(),
            devices: [DeviceId("abc".into())].into(),
        }
    }

    #[test]
    fn empty_requirements_pass() {
        assert_eq!(Requirements::default().satisfied_by(&machine()), Ok(()));
    }

    #[test]
    fn full_match_passes() {
        let r = Requirements {
            os: Some(Os::Linux),
            arch: Some(Arch::X86_64),
            tools: [Tool("cargo".into())].into(),
            device: Some(DeviceId("abc".into())),
            min_memory_bytes: Some(16 << 30),
            ..Requirements::default()
        };
        assert_eq!(r.satisfied_by(&machine()), Ok(()));
    }

    #[test]
    fn every_mismatch_is_reported() {
        let r = Requirements {
            os: Some(Os::Macos),
            arch: Some(Arch::Aarch64),
            tools: [Tool("cargo".into()), Tool("xcodebuild".into())].into(),
            device: Some(DeviceId("zzz".into())),
            min_memory_bytes: Some(32 << 30),
            ..Requirements::default()
        };
        let u = r.satisfied_by(&machine()).unwrap_err();
        assert_eq!(
            u,
            vec![
                Unmet::Os {
                    want: Os::Macos,
                    have: Os::Linux
                },
                Unmet::Arch {
                    want: Arch::Aarch64,
                    have: Arch::X86_64
                },
                Unmet::Tool(Tool("xcodebuild".into())),
                Unmet::Device(DeviceId("zzz".into())),
                Unmet::Memory {
                    want: 32 << 30,
                    have: 16 << 30
                },
            ]
        );
    }
}
