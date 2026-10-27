//! The run-policy data model, as written in `.loadtable/run-policy.toml`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The schema version this crate reads and writes.
pub const SCHEMA_VERSION: u32 = 1;

/// A repo's run policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Schema version; must equal [`SCHEMA_VERSION`].
    pub version: u32,
    /// Named suites.
    #[serde(default)]
    pub suites: BTreeMap<String, Suite>,
    /// What travels with a checkout when a job is forwarded to another machine.
    #[serde(default, skip_serializing_if = "SyncSection::is_empty")]
    pub sync: SyncSection,
    /// Where this repo's jobs may run when the submitter names no machine.
    #[serde(default, skip_serializing_if = "PlacementSection::is_empty")]
    pub placement: PlacementSection,
}

/// `[placement]`: defaults for jobs that may run anywhere.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementSection {
    /// The OS a job submitted `--anywhere` (or a plan step that names no machine) needs: a
    /// build whose output is used on this OS, or that needs its native libraries. A job that
    /// names a peer, or gives its own OS, is not changed.
    #[serde(default)]
    pub os: Option<PolicyOs>,
}

impl PlacementSection {
    /// True when nothing is set.
    pub fn is_empty(&self) -> bool {
        self.os.is_none()
    }
}

/// An OS as written in the policy: `linux` or `macos`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyOs {
    /// Linux.
    Linux,
    /// macOS.
    Macos,
}

impl From<PolicyOs> for tasks_core::Os {
    fn from(o: PolicyOs) -> Self {
        match o {
            PolicyOs::Linux => tasks_core::Os::Linux,
            PolicyOs::Macos => tasks_core::Os::Macos,
        }
    }
}

/// `[sync]`: files beyond the git working tree that a forwarded job needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncSection {
    /// Gitignored files that must travel (e.g. `local.properties`), as relative paths. `*` and
    /// `?` match inside one path component; a trailing `/` takes a whole directory. Anything
    /// not listed here and ignored by git never leaves the machine, and a file that looks like
    /// a secret is refused by name.
    #[serde(default)]
    pub extra: Vec<String>,
}

impl SyncSection {
    /// True when nothing is listed.
    pub fn is_empty(&self) -> bool {
        self.extra.is_empty()
    }
}

/// Most `[sync] extra` patterns.
pub const MAX_SYNC_EXTRA: usize = 64;

/// A named group of steps, e.g. `core-tests`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    /// Free text for agents: when to run this suite, what it needs.
    #[serde(default)]
    pub notes: Option<String>,
    /// Tools the suite needs on the machine (informational hints for placement).
    #[serde(default)]
    pub tools: Vec<String>,
    /// The steps, in the order they are written.
    #[serde(default)]
    pub steps: Vec<StepDef>,
    /// Per-target notes keyed by history label (`test:lifecycle`, `lib:p2pcore`).
    #[serde(default)]
    pub targets: BTreeMap<String, TargetNote>,
}

/// How a step is split into per-target steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitMode {
    /// One build step plus one step per cargo test executable.
    #[serde(rename = "cargo-test")]
    CargoTest,
}

/// One step of a suite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepDef {
    /// Unique name within the suite.
    pub name: String,
    /// Command and arguments.
    pub command: Vec<String>,
    /// Steps that must pass first.
    #[serde(default)]
    pub after: Vec<String>,
    /// Placement hint, e.g. a machine name.
    #[serde(default)]
    pub placement: Option<String>,
    /// Timeout in seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Split rule.
    #[serde(default)]
    pub split: Option<SplitMode>,
    /// True if a quick check may skip this step.
    #[serde(default)]
    pub skip_in_quick: bool,
    /// Queue class: `background` for warm-ups and long sweeps that may wait behind the rest
    /// (`normal` is the default). `blocking` is not a policy setting; it needs a person's reason
    /// at submit time.
    #[serde(default)]
    pub priority: Option<tasks_core::Priority>,
    /// Known to run for a long time: starts first among steps that become ready together.
    #[serde(default)]
    pub long: bool,
    /// Must run alone on its machine (e.g. it binds fixed network ports). Not with `light`.
    #[serde(default)]
    pub isolate: bool,
    /// Uses little CPU but runs long: ignores the CPU pressure gate, takes no run slot.
    /// For a split step, a target's own `light` or `isolate` decides for that target.
    #[serde(default)]
    pub light: bool,
    /// Run the failed tests of this step once more, alone, when it fails under load.
    #[serde(default)]
    pub retry_alone: bool,
}

/// Notes about one test target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetNote {
    /// Known to run for a long time: schedule it first.
    #[serde(default)]
    pub long: bool,
    /// Must run alone (e.g. it binds fixed network ports).
    #[serde(default)]
    pub isolate: bool,
    /// Uses little CPU but runs long (timers, network): ignores the CPU pressure gate and takes
    /// no run slot. Not with `isolate`.
    #[serde(default)]
    pub light: bool,
    /// When it fails under load, run the failed tests once more, alone.
    #[serde(default)]
    pub retry_alone: bool,
    /// Free text.
    #[serde(default)]
    pub note: Option<String>,
}

impl Suite {
    /// The step called `name`.
    pub fn step(&self, name: &str) -> Option<&StepDef> {
        self.steps.iter().find(|s| s.name == name)
    }
}
