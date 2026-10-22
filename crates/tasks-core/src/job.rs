//! Job specification and validation.

use crate::{CoreError, JobId, Owner, Requirements};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Device serials that must never appear in a job's argv.
pub const DENIED_DEVICES: &[&str] = &["ZD22257P6G"];

/// What a job does, for scheduling and ETA purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    /// Compilation.
    Compile,
    /// Test run.
    Test,
    /// General run.
    Run,
    /// Packaging.
    Package,
    /// Device interaction.
    Device,
    /// Cheap probe.
    Probe,
}

impl Kind {
    /// Whether jobs of this kind feed ETA estimates (false for probes).
    pub fn counts_for_eta(&self) -> bool {
        !matches!(self, Kind::Probe)
    }
}

/// Where a job may run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Placement {
    /// On the submitting machine.
    Here,
    /// On a named peer.
    Peer(crate::MachineName),
    /// On any capable machine.
    Anywhere,
}

/// A request to run a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// Who submitted it.
    pub owner: Owner,
    /// Command and arguments.
    pub argv: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Job kind.
    pub kind: Kind,
    /// Machine requirements.
    pub requirements: Requirements,
    /// Placement preference.
    pub placement: Placement,
    /// Jobs that must pass first.
    pub after: Vec<JobId>,
}

fn valid_env_name(n: &str) -> bool {
    let mut cs = n.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
        && cs.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

impl JobSpec {
    /// Checks the spec against the submission rules.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.argv.is_empty() {
            return Err(CoreError::EmptyArgv);
        }
        let nul = self.argv.iter().any(|a| a.contains('\0'))
            || self
                .env
                .iter()
                .any(|(k, v)| k.contains('\0') || v.contains('\0'));
        if nul {
            return Err(CoreError::Nul);
        }
        if let Some((k, _)) = self.env.iter().find(|(k, _)| !valid_env_name(k)) {
            return Err(CoreError::BadEnvName(k.clone()));
        }
        if self.after.len() > 64 {
            return Err(CoreError::TooManyDeps(self.after.len()));
        }
        for d in DENIED_DEVICES {
            if self.argv.iter().any(|a| a.contains(d)) {
                return Err(CoreError::DeniedDevice((*d).to_string()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::Placement;

    pub(crate) fn spec() -> JobSpec {
        JobSpec {
            owner: Owner::new("o").unwrap(),
            argv: vec!["echo".into(), "hi".into()],
            cwd: PathBuf::from("/tmp"),
            env: vec![("FOO_1".into(), "x".into())],
            kind: Kind::Run,
            requirements: Requirements::default(),
            placement: Placement::Here,
            after: vec![],
        }
    }

    #[test]
    fn valid_spec() {
        assert_eq!(spec().validate(), Ok(()));
    }

    #[test]
    fn empty_argv() {
        let mut s = spec();
        s.argv.clear();
        assert_eq!(s.validate(), Err(CoreError::EmptyArgv));
    }

    #[test]
    fn nul_rejected() {
        let mut s = spec();
        s.argv[1] = "a\0b".into();
        assert_eq!(s.validate(), Err(CoreError::Nul));
        let mut s = spec();
        s.env[0].1 = "a\0".into();
        assert_eq!(s.validate(), Err(CoreError::Nul));
    }

    #[test]
    fn env_names() {
        for bad in ["", "1A", "a", "A-B"] {
            let mut s = spec();
            s.env = vec![(bad.into(), "v".into())];
            assert_eq!(
                s.validate(),
                Err(CoreError::BadEnvName(bad.into())),
                "{bad}"
            );
        }
        let mut s = spec();
        s.env = vec![("_A9".into(), "v".into())];
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn after_limit() {
        let mut s = spec();
        s.after = (0..64).map(JobId).collect();
        assert_eq!(s.validate(), Ok(()));
        s.after.push(JobId(64));
        assert_eq!(s.validate(), Err(CoreError::TooManyDeps(65)));
    }

    #[test]
    fn denied_device() {
        let mut s = spec();
        s.argv = vec!["adb".into(), "-s".into(), "ZD22257P6G".into()];
        assert!(matches!(s.validate(), Err(CoreError::DeniedDevice(_))));
    }

    #[test]
    fn probe_not_eta() {
        assert!(!Kind::Probe.counts_for_eta());
        assert!(Kind::Compile.counts_for_eta());
    }
}
