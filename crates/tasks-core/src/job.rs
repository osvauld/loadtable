//! Job specification and validation.

use crate::{CoreError, JobId, Owner, Priority, Requirements};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Device serials that must never appear in a job's argv.
pub const DENIED_DEVICES: &[&str] = &["ZD22257P6G"];

/// Longest wall-clock limit a job may ask for: seven days.
pub const MAX_TIMEOUT_SECS: u64 = 604_800;

/// Most output paths one job may ask to have fetched back (the peer's list request limit).
pub const MAX_FETCH_PATHS: usize = 256;

/// Longest output path, in bytes.
pub const MAX_FETCH_PATH_BYTES: usize = 1024;

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

/// The CPU share a job gets when jobs compete: compiles yield to tests, which are measuring
/// timing and must not be starved by a build that happens to run beside them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CpuClass {
    /// Low priority: builds and packaging.
    Compile,
    /// High priority: tests and runs.
    Run,
    /// The default share.
    Normal,
}

impl Kind {
    /// The kind as a lowercase word, as shown to people.
    pub fn word(&self) -> &'static str {
        match self {
            Kind::Compile => "compile",
            Kind::Test => "test",
            Kind::Run => "run",
            Kind::Package => "package",
            Kind::Device => "device",
            Kind::Probe => "probe",
        }
    }

    /// How much CPU the machine should give a job of this kind when jobs compete (R-710).
    pub fn cpu_class(&self) -> CpuClass {
        match self {
            Kind::Compile | Kind::Package => CpuClass::Compile,
            Kind::Test | Kind::Run => CpuClass::Run,
            Kind::Device | Kind::Probe => CpuClass::Normal,
        }
    }

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
    /// On the submitting machine, explicitly pinned (`--pin`): never rerouted.
    Pin,
    /// On a named peer.
    Peer(crate::MachineName),
    /// On any capable machine.
    Anywhere,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.word())
    }
}

impl std::fmt::Display for Placement {
    /// Words for people: `here`, `here (pinned)`, `peer nixos (asked)`, `anywhere`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Placement::Here => f.write_str("here"),
            Placement::Pin => f.write_str("here (pinned)"),
            Placement::Peer(m) => write!(f, "peer {} (asked)", m.0),
            Placement::Anywhere => f.write_str("anywhere (chosen by the scheduler)"),
        }
    }
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
    /// Wall-clock limit in seconds, enforced by the supervisor: a job over it ends `TimedOut`.
    /// A plan step's `hints.timeout_secs` fills it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// The submitter passed the broad-test escape (`--broad-test`, R-191). Carried on the spec
    /// so every receiver of the job (a peer, a replayed plan) gives the same answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub broad_test_escape: bool,
    /// Queue class. `blocking` needs `priority_reason`; a plan step's `hints.priority` fills it.
    #[serde(default, skip_serializing_if = "Priority::is_normal")]
    pub priority: Priority,
    /// Why the job is `blocking`, kept with the job and in the audit log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_reason: Option<String>,
    /// Output files, relative to the checkout root, to bring back when the job ran on a peer
    /// (`--fetch`). Older senders omit it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetch: Vec<String>,
}

/// Checks one `--fetch` path: relative to the checkout, no `.`/`..`/empty component, no `.git`.
pub fn check_fetch_path(p: &str) -> Result<(), CoreError> {
    let bad = |why: &str| CoreError::BadFetchPath(p.to_string(), why.to_string());
    if p.is_empty() {
        return Err(bad("empty path"));
    }
    if p.len() > MAX_FETCH_PATH_BYTES {
        return Err(bad("path is too long"));
    }
    if p.starts_with('/') {
        return Err(bad(
            "absolute paths are not allowed; give a path inside the checkout",
        ));
    }
    if p.contains('\0') || p.contains('\\') {
        return Err(bad("NUL or backslash"));
    }
    for c in p.split('/') {
        match c {
            "" | "." | ".." => return Err(bad("`.`, `..` and empty components are not allowed")),
            ".git" => return Err(bad("paths inside .git cannot be fetched")),
            _ => {}
        }
    }
    Ok(())
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
        if let Some(t) = self.timeout_secs
            && !(1..=MAX_TIMEOUT_SECS).contains(&t)
        {
            return Err(CoreError::BadTimeout(t));
        }
        if self.fetch.len() > MAX_FETCH_PATHS {
            return Err(CoreError::TooManyFetchPaths(self.fetch.len()));
        }
        for p in &self.fetch {
            check_fetch_path(p)?;
        }
        crate::check_reason(self.priority, self.priority_reason.as_deref())?;
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
            timeout_secs: None,
            broad_test_escape: false,
            priority: Priority::Normal,
            priority_reason: None,
            fetch: Vec::new(),
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
    fn timeout_must_be_between_one_second_and_a_week() {
        for (t, ok) in [
            (0, false),
            (1, true),
            (MAX_TIMEOUT_SECS, true),
            (MAX_TIMEOUT_SECS + 1, false),
        ] {
            let mut s = spec();
            s.timeout_secs = Some(t);
            assert_eq!(s.validate().is_ok(), ok, "{t}");
        }
        let mut s = spec();
        s.timeout_secs = Some(0);
        assert_eq!(s.validate(), Err(CoreError::BadTimeout(0)));
    }

    #[test]
    fn denied_device() {
        let mut s = spec();
        s.argv = vec!["adb".into(), "-s".into(), "ZD22257P6G".into()];
        assert!(matches!(s.validate(), Err(CoreError::DeniedDevice(_))));
    }

    #[test]
    fn new_spec_fields_default_when_absent_and_stay_off_the_wire_when_unset() {
        let s = spec();
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("timeout_secs") && !json.contains("broad_test_escape"));
        // a spec written by an older build parses with the defaults
        let back: JobSpec = serde_json::from_str(&json).unwrap();
        assert_eq!((back.timeout_secs, back.broad_test_escape), (None, false));
        let mut t = s;
        t.timeout_secs = Some(30);
        t.broad_test_escape = true;
        let back: JobSpec = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn r339_a_blocking_spec_needs_a_reason_and_the_default_stays_off_the_wire() {
        let mut s = spec();
        assert!(!serde_json::to_string(&s).unwrap().contains("priority"));
        s.priority = Priority::Blocking;
        assert_eq!(s.validate(), Err(CoreError::BlockingNeedsReason));
        s.priority_reason = Some("release is waiting".into());
        assert_eq!(s.validate(), Ok(()));
        let back: JobSpec = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let mut bg = spec();
        bg.priority = Priority::Background;
        assert_eq!(bg.validate(), Ok(()));
    }

    #[test]
    fn r343_a_spec_has_no_field_that_claims_the_short_lane() {
        let mut s = spec();
        s.timeout_secs = Some(5);
        s.broad_test_escape = true;
        s.priority = Priority::Blocking;
        s.priority_reason = Some("why".into());
        let v = serde_json::to_value(&s).unwrap();
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert!(keys.iter().all(|k| !k.contains("short")), "{keys:?}");
        // a client that sends one anyway is not believed: the field is simply not read
        let mut with = v;
        with["short"] = serde_json::json!(true);
        with["lane"] = serde_json::json!("short");
        let back: JobSpec = serde_json::from_value(with).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn probe_not_eta() {
        assert!(!Kind::Probe.counts_for_eta());
        assert!(Kind::Compile.counts_for_eta());
    }

    #[test]
    fn r500_fetch_paths_are_relative_and_inside_the_checkout() {
        let mut s = spec();
        s.fetch = vec!["android/app/build/outputs/apk/debug/app-debug.apk".into()];
        assert_eq!(s.validate(), Ok(()));
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            "a/",
            ".git/config",
            "a\\b",
        ] {
            s.fetch = vec![bad.into()];
            assert!(
                matches!(s.validate(), Err(CoreError::BadFetchPath(..))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn r500_the_number_of_fetch_paths_is_bounded() {
        let mut s = spec();
        s.fetch = (0..=MAX_FETCH_PATHS).map(|i| format!("f{i}")).collect();
        assert_eq!(
            s.validate(),
            Err(CoreError::TooManyFetchPaths(MAX_FETCH_PATHS + 1))
        );
    }

    #[test]
    fn r501_an_older_spec_without_fetch_still_parses() {
        let mut v = serde_json::to_value(spec()).unwrap();
        assert!(v.get("fetch").is_none());
        v.as_object_mut().unwrap().remove("fetch");
        let back: JobSpec = serde_json::from_value(v).unwrap();
        assert!(back.fetch.is_empty());
    }

    // R-710
    #[test]
    fn r710_builds_yield_cpu_to_tests_and_runs() {
        assert_eq!(Kind::Compile.cpu_class(), CpuClass::Compile);
        assert_eq!(Kind::Package.cpu_class(), CpuClass::Compile);
        assert_eq!(Kind::Test.cpu_class(), CpuClass::Run);
        assert_eq!(Kind::Run.cpu_class(), CpuClass::Run);
        assert_eq!(Kind::Device.cpu_class(), CpuClass::Normal);
        assert_eq!(Kind::Probe.cpu_class(), CpuClass::Normal);
    }
}
