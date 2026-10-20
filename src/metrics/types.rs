//! Cluster snapshot v1 wire types. Field names are the contract in
//! docs/design/cluster-snapshot.md; readers ignore unknown fields (no
//! `deny_unknown_fields`), and unknown values are `null`, never guessed.
use serde::{Deserialize, Serialize};

pub const SNAPSHOT_VERSION: u32 = 1;
/// Ring-buffer length: 150 points at 2 s = 5 minutes.
pub const HISTORY_POINTS: usize = 150;
pub const SAMPLE_INTERVAL_S: f64 = 2.0;
/// `jobs` holds every live job plus this many newest finished ones.
pub const FINISHED_JOBS: usize = 50;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub taken_at: f64,
    pub machine: Machine,
    pub jobs: Vec<JobView>,
    /// This machine's health (alerts, disks, missing-capability marks); additive, absent
    /// from daemons that predate health monitoring and before the first check pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<crate::health::NodeHealth>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Worker,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    /// Hostname; matches the `@host` suffix offload clients put on owners.
    pub id: String,
    pub role: Role,
    pub logical_cpus: u32,
    /// Whole machine over the last sample interval; null before two samples.
    pub cpu_busy_cores: Option<f64>,
    pub mem_total_bytes: Option<u64>,
    pub mem_available_bytes: Option<u64>,
    pub swap_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    /// Null when PSI is unavailable.
    pub pressure: Option<Pressure>,
    /// Admission's current (compile-gate) pressure blocker.
    pub blocker: Option<String>,
    pub slots: Vec<Slot>,
    /// Always written; a reader accepts a document without it (peer snapshots kept for
    /// placement drop the rings).
    #[serde(default)]
    pub history: MachineHistory,
    /// Largest Cargo `-j`/`CARGO_BUILD_JOBS` this machine's admission accepts (the worker's
    /// recorded `TASKS_MAX_CARGO_JOBS`, else the daemon's own). Additive and optional: absent
    /// in older snapshots and in the fixtures, where readers must treat it as unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo_jobs_max: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pressure {
    pub cpu_some_avg10: f64,
    pub memory_some_avg10: f64,
    pub memory_full_avg10: f64,
    pub io_some_avg10: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotKind {
    Compile,
    Run,
    Offload,
    Device,
    Service,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    pub kind: SlotKind,
    /// Lock file stem, e.g. `p2p-build`, `p2p-run-2`, `p2p-device-SERIAL`.
    pub name: String,
    pub busy: bool,
    pub holder_job: Option<i64>,
    pub holder_who: Option<String>,
    /// Additive: when the slot is busy but no live holder note names it, the
    /// process(es) that actually hold the flock (e.g. a detached compiler-cache
    /// server that inherited a finished job's lease). Absent otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holder_processes: Vec<HolderProcess>,
}

/// A process found holding an admission flock without a live holder note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderProcess {
    pub pid: u32,
    /// Bounded command line (argv joined by spaces), or `[comm]`.
    pub cmd: String,
    /// `TASKS_JOB_ID` from its environment: the job it was started under.
    pub tasks_job: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MachineHistory {
    pub interval_s: f64,
    pub cpu_busy_cores: Vec<f64>,
    pub mem_used_bytes: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobView {
    pub id: i64,
    pub owner: String,
    /// The ledger phase, snake_case (`waiting_build`, `building`, `passed`...).
    pub phase: crate::Phase,
    pub resource: SlotKind,
    pub device: Option<String>,
    /// The checkout as its owner knows it: for a worker job synced by
    /// tasks-offload, the laptop path from the `<id>.identity` file.
    pub checkout: String,
    pub checkout_name: String,
    pub command: String,
    pub kind_key: String,
    pub argv: Vec<String>,
    pub submitted_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// Null unless the job is waiting (for approval, pickup, disk or a slot).
    pub queue: Option<Queue>,
    /// Null unless the job is executing (building/running).
    pub live: Option<Live>,
    /// Cargo commands only.
    pub progress: Option<Progress>,
    pub warmth: Warmth,
    pub warmth_reason: String,
    pub estimate: Option<EtaEstimate>,
    /// Finished jobs only.
    pub result: Option<JobResult>,
    /// Laptop-side offload linkage, filled by feature/status-stream; null here.
    pub offload: Option<OffloadLink>,
    /// Live compile attribution (feature/log-search): the compiler/build-tool processes in
    /// the job's tree at the last sample, largest elapsed first. Optional and additive:
    /// absent for jobs that are not executing and in older snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiling: Option<Vec<super::compile::Compiling>>,
    /// `true` when the command is a `cargo test` with a name filter and no target selector:
    /// it builds every test target of the package (feature/log-search). Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broad_test_build: Option<bool>,
    /// The move of this waiting job to a peer (`reroute`); absent when it never moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reroute: Option<RerouteView>,
}

/// What the snapshot shows about a rerouted job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RerouteView {
    /// `moving`, `moved` or `returned`.
    pub state: String,
    pub moved_from: String,
    pub moved_to: String,
    pub reason: String,
    pub at: i64,
    #[serde(default)]
    pub remote_job_id: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Queue {
    /// 1-based position among waiters for the same slot class; null when the
    /// job is not in a slot queue (approval, worker pickup, disk reserve).
    pub position: Option<u32>,
    pub waiting_for: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Live {
    /// Null until two samples of this job exist.
    pub cpu_cores: Option<f64>,
    pub rss_bytes: u64,
    pub processes: u32,
    #[serde(default)]
    pub history: LiveHistory,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LiveHistory {
    pub interval_s: f64,
    pub cpu_cores: Vec<f64>,
    pub rss_bytes: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub crates_compiled: u64,
    pub crates_expected: Option<u64>,
    /// Test targets built so far (distinct test binaries in the log, or `rustc --test`
    /// crates seen live, whichever is larger). Absent until one is seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_targets_built: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Warmth {
    Cold,
    Warm,
    #[default]
    Unknown,
}
impl Warmth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Warm => "warm",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EtaEstimate {
    pub total_s: f64,
    pub remaining_s: f64,
    pub samples: u32,
    pub basis: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobResult {
    pub exit_code: Option<i32>,
    /// Execution time of the command stages, excluding queue/admission waits.
    pub wall_s: Option<f64>,
    pub cpu_user_s: Option<f64>,
    pub cpu_sys_s: Option<f64>,
    pub peak_rss_bytes: Option<u64>,
    pub crates_compiled: Option<u64>,
    pub warmth: Warmth,
    /// See [`JobMetrics::test_targets_built`]; absent when not measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_targets_built: Option<u64>,
    /// See [`JobMetrics::broad_test_build`]; absent when not measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broad_test_build: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OffloadLink {
    pub worker: String,
    pub remote_job_id: Option<i64>,
    pub transport: String,
    pub sync_s: Option<f64>,
    pub fetch_s: Option<f64>,
}

/// Persisted per finished job as `Execution::metrics` (optional, additive:
/// older releases ignore it; rows without it are history with unknown warmth).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JobMetrics {
    #[serde(default)]
    pub warmth: Warmth,
    #[serde(default)]
    pub warmth_reason: String,
    #[serde(default)]
    pub wall_s: Option<f64>,
    #[serde(default)]
    pub cpu_user_s: Option<f64>,
    #[serde(default)]
    pub cpu_sys_s: Option<f64>,
    /// Largest summed RSS of the job's process tree seen by the daemon's
    /// sampler, or the largest single process (getrusage) when larger/only.
    #[serde(default)]
    pub peak_rss_bytes: Option<u64>,
    #[serde(default)]
    pub crates_compiled: Option<u64>,
    /// Test targets the command built (Cargo only): distinct test binaries named by the log's
    /// `Running`/`Executable` lines plus `Doc-tests` crates, or the `rustc --test` crates the
    /// sampler saw, whichever is larger. Additive (feature/log-search); older rows lack it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_targets_built: Option<u64>,
    /// `cargo test` commands only: true when a name filter came without a target selector,
    /// so every test target of the package was built (see `metrics::testscope`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broad_test_build: Option<bool>,
}

/// `tasks status` reply on the daemon socket. A separate shape from
/// `Response` so existing clients and request kinds are unchanged; an older
/// daemon answers the unknown action with an error in the same `error` field.
#[derive(Debug, Serialize, Deserialize)]
pub struct StatusResponse {
    pub version: u32,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub status: Option<Snapshot>,
}
