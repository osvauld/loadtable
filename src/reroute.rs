//! Rerouting queued jobs: a waiting, not-yet-started local job moves to a worker peer when that
//! peer would clearly finish it sooner. Placement (`placement.rs`) decides once, at submit; this
//! re-scores jobs that are still waiting here, so an agent never has to cancel and resubmit.
//!
//! * **Who may move.** A live command (`local.command`) of the compile class that has not
//!   started: phase approved / preparing / waiting_build / waiting_run with no stage executed and
//!   no local admission committed. Never the device class, never the run class (it needs this
//!   machine's build, like placement), never an offload client or a job that is already a
//!   forward, never a pinned job (`tasks run --pin`, MCP `pin: true`, MCP `placement: "local"`,
//!   or a named peer), never a job with a cancel request, and at most [`MAX_MOVES`] move per
//!   job (a failed move counts too, so a job never flaps).
//! * **When.** The daemon's peer runtime re-scores every [`Policy::interval`], and early when a
//!   peer comes online or the compile slot holder's ETA grows (`peer::reroute`).
//! * **Score.** Here: the job's remaining wait (slot holder ETA, compile jobs queued ahead of it,
//!   a pressure blocker) plus its run estimate (the snapshot's warmth-aware ETA). There:
//!   placement's compile wait of the peer plus this machine's offload backlog plus the run on
//!   the peer, where a peer that never ran this command for this checkout pays a COLD build
//!   ([`peer_run`]). Move only when here >= `min_ratio` x there AND here - there >= `min_gain_s`.
//! * **Atomic with admission.** [`Store::claim_move`] and [`Store::commit_admission`] both run in an
//!   IMMEDIATE ledger transaction. A claim requires the job's spec to be unchanged and no
//!   `STAGE-admission-committed` marker in its job directory; it rewrites the job's spec into
//!   `peer_link::forwarded_spec` (the same job id becomes the local mirror of the forward).
//!   Native admission, once it holds the slot, commits only if the spec is still the one it
//!   read and then writes that marker. Whichever transaction is first wins; the loser backs off
//!   (a withdrawn admission releases the slot without running anything).
//! * **Failure.** If the forward submits nothing (peer offline, or it refused the job), the
//!   worker puts the original spec back and the job waits here again at its original queue
//!   position (the admission wait note keeps its first timestamp), with a note. An uncertain
//!   submission is never reverted: the peer link's idempotent key decides, nothing runs twice.
//!
//! * **Pull-back.** The other way: a job that was forwarded (a local `tasks peer-forward` bridge)
//!   and still WAITS on the peer, blocked by a pressure gate (PSI, available memory, CPU), comes
//!   back when this machine's compile slot is free and unpressured and the same `min_ratio` /
//!   `min_gain_s` rule holds (here = run now, there = the peer's remaining wait + run). Only a
//!   placement-chosen forward (`--anywhere`: its placement record says `anywhere`) may come
//!   back, never one sent to a named peer or pinned, and a pull-back counts as a move
//!   ([`MAX_MOVES`]). The peer's copy is cancelled FIRST over the peer link; only a confirmed
//!   cancel lets [`Store::claim_pull_back`] restore the local spec (the bridge then ends and
//!   the worker queues the job here, like a failed move). An uncertain cancel restores nothing:
//!   the job is never run twice. See [`decide_pull_back`]; knob `pull_back` / [`PULL_BACK_ENV`].
//!
//! The decisions ([`decide`], [`decide_pull_back`]) are pure and unit-tested; the daemon side is
//! `peer::reroute`.
use crate::local_command::{Recipe, Resource};
use crate::metrics::{JobView, SlotKind, Snapshot, Warmth, command::normalize};
use crate::placement::{self, BLOCKED_S, Peer, UNKNOWN_S, duration};
use crate::{Job, JobSpec, Phase, Result, Store};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `0`, `off`, `false` or `no` disables rerouting. Default: on, but the loop only exists in a
/// daemon whose peer runtime runs (a peer config) and has at least one worker/both peer.
pub const ENABLE_ENV: &str = "TASKS_REROUTE";
/// Seconds between re-scorings, 1..=3600 (default [`DEFAULT_INTERVAL_S`]).
pub const INTERVAL_ENV: &str = "TASKS_REROUTE_INTERVAL_S";
/// Minimum ratio here/there, 1.0..=100 (default [`DEFAULT_MIN_RATIO`]).
pub const MIN_RATIO_ENV: &str = "TASKS_REROUTE_MIN_RATIO";
/// Minimum seconds saved, 0..=86400 (default [`DEFAULT_MIN_GAIN_S`]).
pub const MIN_GAIN_ENV: &str = "TASKS_REROUTE_MIN_GAIN_S";
/// `0`, `off`, `false` or `no` stops pulling a waiting forward back from a pressured peer
/// (default: on; see [`decide_pull_back`]).
pub const PULL_BACK_ENV: &str = "TASKS_REROUTE_PULL_BACK";
pub const DEFAULT_INTERVAL_S: u64 = 20;
pub const DEFAULT_MIN_RATIO: f64 = 2.0;
pub const DEFAULT_MIN_GAIN_S: f64 = 300.0;
/// A slot holder whose remaining ETA grows by more than this triggers an early re-scoring.
pub const ETA_GROWTH_S: f64 = 60.0;
/// Moves per job, failed attempts included (anti-flap).
pub const MAX_MOVES: u32 = 1;
/// A peer's run time when it never built this command for this checkout and no cold run of it
/// is known anywhere: a cold build is assumed to take this long (placement's unknown guess).
pub const COLD_GUESS_S: f64 = UNKNOWN_S;
// The same knobs can be set in the policy file's `reroute::` section ([`ReroutePolicy`];
// src/policy.rs): `enabled`, `interval_s`, `min_ratio`, `min_gain_s`, `pull_back`. The
// variables above win.

/// Test-only (debug builds): `1` moves every eligible job at each tick, to the best usable
/// worker peer or else the first configured one, ignoring the thresholds and its link state.
pub const TEST_FORCE_ENV: &str = "TASKS_REROUTE_TEST_FORCE";
/// Test-only (debug builds): a directory; the loop re-scores only when a new file appears in it
/// (an injected scheduler tick) and then writes `NAME.done` with what it did.
pub const TEST_TICKS_ENV: &str = "TASKS_REROUTE_TEST_TICKS";
/// Test-only (debug builds): a directory; native admission of job ID stage STAGE pauses after
/// taking the slot and before committing while `ID-STAGE.hold` exists in it, announcing
/// `ID-STAGE.held`, until `ID-STAGE.release` appears.
pub const TEST_ADMIT_HOLD_ENV: &str = "TASKS_TEST_ADMIT_HOLD";

pub const STATE_MOVING: &str = "moving";
pub const STATE_MOVED: &str = "moved";
pub const STATE_RETURNED: &str = "returned";
/// The forward was cancelled on its peer and the job runs here (see [`decide_pull_back`]).
pub const STATE_PULLED_BACK: &str = "pulled_back";

/// The `reroute::` section of the policy file. Every key is optional; absent = not set there.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReroutePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_gain_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_back: Option<bool>,
}

impl ReroutePolicy {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Rerouting knobs: the daemon's environment over the policy file's `reroute::` section.
#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    pub enabled: bool,
    pub interval: Duration,
    pub min_ratio: f64,
    pub min_gain_s: f64,
    /// Pull a forward that still waits on a pressured peer back here ([`decide_pull_back`]).
    pub pull_back: bool,
    /// Test-only, see [`TEST_FORCE_ENV`].
    pub force: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: Duration::from_secs(DEFAULT_INTERVAL_S),
            min_ratio: DEFAULT_MIN_RATIO,
            min_gain_s: DEFAULT_MIN_GAIN_S,
            pull_back: true,
            force: false,
        }
    }
}

impl Policy {
    /// Environment lookups only (no policy file).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> std::result::Result<Self, String> {
        Self::from_sources(&ReroutePolicy::default(), get)
    }

    /// `get` (the environment) wins over `file`. An invalid value disables rerouting (the
    /// caller logs "reroute: DISABLED: ..."), whichever source it came from.
    pub fn from_sources(
        file: &ReroutePolicy,
        get: impl Fn(&str) -> Option<String>,
    ) -> std::result::Result<Self, String> {
        let mut p = Self::default();
        if let Some(v) = get(ENABLE_ENV) {
            p.enabled = match v.trim().to_ascii_lowercase().as_str() {
                "1" | "on" | "true" | "yes" => true,
                "0" | "off" | "false" | "no" => false,
                other => return Err(format!("{ENABLE_ENV} must be on or off, got {other:?}")),
            };
        } else if let Some(enabled) = file.enabled {
            p.enabled = enabled;
        }
        if let Some(v) = get(PULL_BACK_ENV) {
            p.pull_back = match v.trim().to_ascii_lowercase().as_str() {
                "1" | "on" | "true" | "yes" => true,
                "0" | "off" | "false" | "no" => false,
                other => return Err(format!("{PULL_BACK_ENV} must be on or off, got {other:?}")),
            };
        } else if let Some(pull_back) = file.pull_back {
            p.pull_back = pull_back;
        }
        let number = |name: &str,
                      key: &str,
                      from_file: Option<f64>,
                      lo: f64,
                      hi: f64|
         -> std::result::Result<Option<f64>, String> {
            match (get(name), from_file) {
                (Some(v), _) => match v.trim().parse::<f64>() {
                    Ok(n) if n.is_finite() && (lo..=hi).contains(&n) => Ok(Some(n)),
                    _ => Err(format!(
                        "{name} must be a number from {lo} to {hi}, got {v:?}"
                    )),
                },
                (None, Some(n)) if n.is_finite() && (lo..=hi).contains(&n) => Ok(Some(n)),
                (None, Some(n)) => Err(format!(
                    "policy file reroute.{key} must be a number from {lo} to {hi}, got {n}"
                )),
                (None, None) => Ok(None),
            }
        };
        if let Some(s) = number(INTERVAL_ENV, "interval_s", file.interval_s, 1.0, 3600.0)? {
            p.interval = Duration::from_secs_f64(s);
        }
        if let Some(r) = number(MIN_RATIO_ENV, "min_ratio", file.min_ratio, 1.0, 100.0)? {
            p.min_ratio = r;
        }
        if let Some(g) = number(MIN_GAIN_ENV, "min_gain_s", file.min_gain_s, 0.0, 86400.0)? {
            p.min_gain_s = g;
        }
        p.force = cfg!(debug_assertions) && get(TEST_FORCE_ENV).is_some_and(|v| v == "1");
        Ok(p)
    }
    /// The daemon's knobs: its environment over the policy file (read when the peer runtime
    /// starts the reroute task).
    pub fn from_env() -> std::result::Result<Self, String> {
        let file = crate::policy::effective().policy.reroute;
        Self::from_sources(&file, |name| std::env::var(name).ok())
    }
}

/// What the ledger records about a move (`execution.reroute`; optional and additive like
/// `metrics`/`peer`: older releases ignore it). `update_execution` never drops or rewinds it:
/// only [`Store::claim_move`], [`Store::set_reroute`] and [`Store::revert_reroute`] write it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reroute {
    /// `moving` (claimed; the forward is starting), `moved` (the peer has the job),
    /// `returned` (the forward submitted nothing; the job runs here again).
    pub state: String,
    /// This machine (snapshot machine id, the hostname).
    pub moved_from: String,
    /// The peer (configured name).
    pub moved_to: String,
    /// The decision as one line, the same text as in the job log.
    pub reason: String,
    /// Unix seconds of the move.
    pub at: i64,
    pub moves: u32,
    #[serde(default)]
    pub local_s: Option<f64>,
    #[serde(default)]
    pub peer_s: Option<f64>,
    #[serde(default)]
    pub remote_job_id: Option<i64>,
    /// Why the job came back, when it did.
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub returned_at: Option<i64>,
    /// The line is not in the job log yet (moved before the worker created the job directory).
    #[serde(default)]
    pub log_pending: bool,
    /// The spec as submitted and approved (restored if the move fails).
    #[serde(default)]
    pub original_spec: Option<JobSpec>,
}

impl Reroute {
    /// One line for people (`tasks run`, MCP, UI).
    pub fn summary(&self) -> String {
        match self.state.as_str() {
            STATE_RETURNED => format!(
                "reroute {} -> {} failed, back in the local queue: {}",
                self.moved_from,
                self.moved_to,
                self.note.as_deref().unwrap_or("nothing was submitted")
            ),
            STATE_PULLED_BACK => format!(
                "pulled back {} -> {}: {}",
                self.moved_from, self.moved_to, self.reason
            ),
            STATE_MOVED => match self.remote_job_id {
                Some(remote) => format!("{} [job {remote} on {}]", self.reason, self.moved_to),
                None => self.reason.clone(),
            },
            _ => self.reason.clone(),
        }
    }
}

// ---- capability marks --------------------------------------------------------------------------

/// The hook for node capability marks (e.g. "nixos lacks libclang"): reroute asks it before
/// moving a job to a peer, the same marks placement consults (`placement::marks_for`).
/// feature/health-alerts provides them ([`crate::health::capability::PeerMarks`]).
pub trait Capabilities: Send + Sync {
    /// Why `node` cannot run `argv` (run from `cwd`, when known), or None when nothing is
    /// known against it. The text follows "lacks ", e.g. `libclang (bindgen) (job #3: ...)`.
    fn lacks(&self, node: &str, argv: &[String], cwd: Option<&Path>) -> Option<String>;
}

/// No marks: every node is assumed capable.
pub struct NoCapabilityMarks;

impl Capabilities for NoCapabilityMarks {
    fn lacks(&self, _node: &str, _argv: &[String], _cwd: Option<&Path>) -> Option<String> {
        None
    }
}

/// The capability marks a reroute round uses: this machine's own marks (`state`, from
/// classified forwarded-job logs) plus those each peer publishes in its snapshot, exactly as
/// placement sees them.
pub fn capabilities(state: &Path, peers: &[Peer]) -> Box<dyn Capabilities> {
    Box::new(crate::health::capability::PeerMarks::load(state, peers))
}

// ---- pins ---------------------------------------------------------------------------------------

pub fn pin_path(state: &Path, id: i64) -> PathBuf {
    state.join("reroute/pinned").join(format!("{id}.json"))
}

/// Pins job `id` where it is (written by the submitting client BEFORE approval).
pub fn pin(state: &Path, id: i64, why: &str) -> std::io::Result<()> {
    let path = pin_path(state, id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(
            &serde_json::json!({"job": id, "why": why, "at": crate::execution::now()}),
        )?,
    )?;
    std::fs::rename(tmp, path)
}

/// Why job `id` must stay where it is, if it is pinned: an explicit pin, or a placement record
/// that asked for a specific machine (`local` or a peer name) rather than `anywhere`.
pub fn pinned(state: &Path, id: i64) -> Option<String> {
    if let Ok(bytes) = std::fs::read(pin_path(state, id)) {
        let why = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v["why"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "pinned".into());
        return Some(why);
    }
    let requested = placement::recorded(state, id)?["requested"]
        .as_str()?
        .to_owned();
    (requested != "anywhere").then(|| format!("placement {requested:?} requested at submit"))
}

// ---- eligibility --------------------------------------------------------------------------------

/// One local job as reroute sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub id: i64,
    pub phase: Phase,
    pub argv: Vec<String>,
    pub resource: Resource,
    /// The job's cwd (the checkout as this machine knows it).
    pub source: String,
    pub owner: String,
    /// Stages already executed.
    pub stages: usize,
    pub cancel_requested: bool,
    pub moves: u32,
    pub pinned: Option<String>,
    /// A local admission was committed for it (its command may have started).
    pub admitted_here: bool,
}

impl Candidate {
    /// None for anything that is not a live command.
    pub fn from_job(job: &Job, pinned: Option<String>, admitted_here: bool) -> Option<Self> {
        let recipe = Recipe::from_spec_unvalidated(&job.spec).ok()?;
        let e = job.execution.as_ref();
        Some(Self {
            id: job.id,
            phase: job.phase.clone(),
            argv: recipe.argv,
            resource: recipe.resource,
            source: job.spec.source.clone(),
            owner: job.spec.owner.clone(),
            stages: e.map_or(0, |e| e.stages.len()),
            cancel_requested: e.is_some_and(|e| e.cancel_requested),
            moves: e
                .and_then(|e| e.reroute.as_ref())
                .map_or(0, |r| r.moves.max(1)),
            pinned,
            admitted_here,
        })
    }
}

/// Phases before any stage was admitted.
pub fn movable_phase(phase: &Phase) -> bool {
    matches!(
        phase,
        Phase::Approved | Phase::Preparing | Phase::WaitingBuild | Phase::WaitingRun
    )
}

/// Why `c` never moves, or None when it may.
pub fn ineligible(c: &Candidate) -> Option<String> {
    match &c.resource {
        Resource::Device(_) => return Some("the device class never moves".into()),
        Resource::Run => {
            return Some("the run class stays here (it needs this machine's build)".into());
        }
        Resource::Offload => {
            return Some("already a forward or an offload client".into());
        }
        Resource::Service => return Some("a service never moves".into()),
        Resource::Compile => {}
    }
    if let Some(why) = &c.pinned {
        return Some(format!("pinned ({why})"));
    }
    if c.moves >= MAX_MOVES {
        return Some("already moved once".into());
    }
    if c.cancel_requested {
        return Some("cancel requested".into());
    }
    if c.phase == Phase::Queued {
        return Some("not approved yet".into());
    }
    if !movable_phase(&c.phase) || c.stages > 0 || c.admitted_here {
        return Some(format!("already started ({:?})", c.phase));
    }
    None
}

// ---- decision -----------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PeerScore {
    pub peer: String,
    pub wait_s: f64,
    pub wait_basis: String,
    pub run_s: f64,
    pub run_basis: String,
    /// `cold build`, `warm run` or `run`.
    pub run_label: String,
    pub total_s: f64,
    pub cargo_jobs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Move {
        peer: String,
        /// The command as it runs there (Cargo `-j` replaced by the peer's cap).
        argv: Vec<String>,
        cargo_jobs: Option<u64>,
    },
    Stay {
        why: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Verdict {
    pub job: i64,
    pub action: Action,
    pub local_wait_s: f64,
    pub local_wait_basis: String,
    pub local_run_s: f64,
    pub local_run_basis: String,
    pub peers: Vec<PeerScore>,
    /// `rerouted: here → there (...)` for a move; the reason to stay otherwise.
    pub line: String,
}

impl Verdict {
    pub fn moves(&self) -> Option<&str> {
        match &self.action {
            Action::Move { peer, .. } => Some(peer),
            Action::Stay { .. } => None,
        }
    }
}

fn waiting_view(phase: &Phase) -> bool {
    matches!(
        phase,
        Phase::Approved | Phase::Preparing | Phase::WaitingBuild
    )
}

/// (queue position, id): positioned waiters first, in position order, then by id.
fn order_key(view: Option<&JobView>, id: i64) -> (u32, i64) {
    (
        view.and_then(|v| v.queue.as_ref())
            .and_then(|q| q.position)
            .unwrap_or(u32::MAX),
        id,
    )
}

/// The slot holders' remaining time on `snapshot` (one compile slot: the longest decides).
fn holder_wait(snapshot: &Snapshot) -> (f64, Vec<String>) {
    let eta = |id: i64| {
        snapshot
            .jobs
            .iter()
            .find(|j| j.id == id)
            .and_then(|j| j.estimate.as_ref())
            .map(|e| e.remaining_s.max(0.0))
    };
    let mut holders: Vec<(Option<i64>, Option<f64>)> = snapshot
        .machine
        .slots
        .iter()
        .filter(|s| s.kind == SlotKind::Compile && s.busy)
        .map(|s| (s.holder_job, s.holder_job.and_then(eta)))
        .collect();
    for job in &snapshot.jobs {
        if job.resource == SlotKind::Compile
            && job.phase == Phase::Building
            && !holders.iter().any(|(id, _)| *id == Some(job.id))
        {
            holders.push((Some(job.id), eta(job.id)));
        }
    }
    let mut wait = 0.0f64;
    let mut parts = vec![];
    for (id, remaining) in &holders {
        wait = wait.max(remaining.unwrap_or(UNKNOWN_S));
        parts.push(match (id, remaining) {
            (Some(id), Some(r)) => format!("slot held by #{id}, ~{} left", duration(*r)),
            (Some(id), None) => format!("slot held by #{id}, ETA unknown"),
            (None, _) => "slot held, ETA unknown".to_string(),
        });
    }
    (wait, parts)
}

/// The waiting job's remaining wait here: slot holders, compile jobs ahead of it (not moved in
/// this round), a pressure blocker.
fn local_wait(local: &Snapshot, id: i64, gone: &[i64]) -> (f64, String) {
    let mine = local.jobs.iter().find(|j| j.id == id);
    let my_key = order_key(mine, id);
    let (mut wait, mut parts) = holder_wait(local);
    let mut ahead = 0;
    for j in &local.jobs {
        if j.id == id
            || gone.contains(&j.id)
            || j.resource != SlotKind::Compile
            || !waiting_view(&j.phase)
            || order_key(Some(j), j.id) >= my_key
        {
            continue;
        }
        wait += j.estimate.as_ref().map_or(UNKNOWN_S, |e| e.total_s);
        ahead += 1;
    }
    if ahead > 0 {
        parts.push(format!("{ahead} queued ahead"));
    }
    if let Some(blocker) = &local.machine.blocker {
        wait += BLOCKED_S;
        parts.push(format!("blocked: {blocker} (+{})", duration(BLOCKED_S)));
    }
    if parts.is_empty() {
        parts.push("compile slot free".into());
    }
    (wait, parts.join(", "))
}

fn local_run(local: &Snapshot, id: i64, kind: &str) -> (f64, String) {
    let view = local.jobs.iter().find(|j| j.id == id);
    if let Some(e) = view.and_then(|v| v.estimate.as_ref()) {
        let warmth = view.map_or(Warmth::Unknown, |v| v.warmth);
        return (e.total_s, format!("{} ({})", e.basis, warmth.as_str()));
    }
    match placement::history(local, kind) {
        Some((m, n)) => (m, format!("median of {n} runs here")),
        None => (UNKNOWN_S, "no history".into()),
    }
}

fn walls<'a>(jobs: impl Iterator<Item = &'a JobView>) -> Vec<f64> {
    jobs.filter_map(|j| j.result.as_ref()?.wall_s).collect()
}

/// The run time on a peer, with the cold-build penalty: warm only when the peer already ran
/// this command for this checkout (its snapshot shows the laptop path of synced checkouts).
/// Returns (seconds, basis, label).
pub fn peer_run(
    kind: &str,
    checkout: &str,
    local: &Snapshot,
    peer: &Snapshot,
    local_jobs: u64,
    peer_jobs: Option<u64>,
) -> (f64, String, String) {
    let passed = |j: &&JobView| j.kind_key == kind && j.phase == Phase::Passed;
    let warm = walls(
        peer.jobs
            .iter()
            .filter(passed)
            .filter(|j| j.checkout == checkout),
    );
    let n = warm.len();
    if let Some(m) = placement::median(warm) {
        return (
            m,
            format!("median of {n} runs of this checkout there"),
            "warm run".into(),
        );
    }
    let cold_there = walls(
        peer.jobs
            .iter()
            .filter(passed)
            .filter(|j| j.result.as_ref().is_some_and(|r| r.warmth == Warmth::Cold)),
    );
    let n = cold_there.len();
    if let Some(m) = placement::median(cold_there) {
        return (
            m,
            format!("median of {n} cold runs there"),
            "cold build".into(),
        );
    }
    let cold_here = walls(
        local
            .jobs
            .iter()
            .filter(passed)
            .filter(|j| j.result.as_ref().is_some_and(|r| r.warmth == Warmth::Cold)),
    );
    let n = cold_here.len();
    if let Some(m) = placement::median(cold_here) {
        let from = local.machine.cargo_jobs_max.unwrap_or(local_jobs) as f64;
        let to = peer_jobs.unwrap_or(placement::DEFAULT_JOBS) as f64;
        return (
            m * from / to,
            format!("median of {n} cold runs here x{from}/{to} jobs"),
            "cold build".into(),
        );
    }
    (
        COLD_GUESS_S,
        "never built there for this checkout, no cold history".into(),
        "cold build".into(),
    )
}

/// Re-scores every candidate (in queue order) against the usable worker peers. Moves decided
/// earlier in the same round are accounted for: the moved job no longer waits ahead here, and
/// its run is added to that peer's wait.
pub fn decide(
    candidates: &[Candidate],
    local: &Snapshot,
    local_jobs: u64,
    peers: &[Peer],
    policy: &Policy,
    caps: &dyn Capabilities,
) -> Vec<Verdict> {
    let here = local.machine.id.clone();
    let mut order: Vec<&Candidate> = candidates.iter().collect();
    order.sort_by_key(|c| order_key(local.jobs.iter().find(|j| j.id == c.id), c.id));
    let (backlog, backlog_text) = placement::offload_backlog(local);
    let mut gone: Vec<i64> = vec![];
    let mut extra: Vec<f64> = vec![0.0; peers.len()];
    let mut out = vec![];
    for c in order {
        let stay = |why: String| Verdict {
            job: c.id,
            action: Action::Stay { why: why.clone() },
            local_wait_s: 0.0,
            local_wait_basis: String::new(),
            local_run_s: 0.0,
            local_run_basis: String::new(),
            peers: vec![],
            line: why,
        };
        if let Some(why) = ineligible(c) {
            out.push(stay(why));
            continue;
        }
        let kind = normalize(&placement::strip_jobs(&c.argv)).kind_key;
        let (lw, lw_basis) = local_wait(local, c.id, &gone);
        let (lr, lr_basis) = local_run(local, c.id, &kind);
        let local_total = lw + lr;
        let mut scores = vec![];
        let mut skipped = vec![];
        for (i, p) in peers.iter().enumerate() {
            let why = p.unusable().or_else(|| {
                caps.lacks(&p.name, &c.argv, Some(Path::new(&c.source)))
                    .map(|w| format!("lacks {w}"))
            });
            let snap = p.snapshot.as_ref();
            let (Some(snap), None) = (snap, why.clone()) else {
                skipped.push((i, why.unwrap_or_else(|| "no snapshot".into())));
                continue;
            };
            let cap = snap.machine.cargo_jobs_max;
            let (mut w, mut w_basis) = placement::compile_wait(snap);
            if let Some(text) = &backlog_text {
                w += backlog;
                w_basis.push_str(&format!(", {text}"));
            }
            if extra[i] > 0.0 {
                w += extra[i];
                w_basis.push_str(&format!(", +{} moved there now", duration(extra[i])));
            }
            let (r, r_basis, label) = peer_run(&kind, &c.source, local, snap, local_jobs, cap);
            scores.push((
                i,
                PeerScore {
                    peer: p.name.clone(),
                    wait_s: w,
                    wait_basis: w_basis,
                    run_s: r,
                    run_basis: r_basis,
                    run_label: label,
                    total_s: w + r,
                    cargo_jobs: cap,
                },
            ));
        }
        let best = scores
            .iter()
            .min_by(|a, b| a.1.total_s.total_cmp(&b.1.total_s).then(a.0.cmp(&b.0)))
            .cloned();
        let mut verdict = Verdict {
            job: c.id,
            action: Action::Stay { why: String::new() },
            local_wait_s: lw,
            local_wait_basis: lw_basis,
            local_run_s: lr,
            local_run_basis: lr_basis,
            peers: scores.iter().map(|(_, s)| s.clone()).collect(),
            line: String::new(),
        };
        // Test-only: the best usable peer, else the first worker peer whatever its state.
        let forced = policy.force.then(|| {
            best.clone().or_else(|| {
                peers
                    .iter()
                    .position(|p| matches!(p.role.as_str(), "worker" | "both"))
                    .map(|i| {
                        (
                            i,
                            PeerScore {
                                peer: peers[i].name.clone(),
                                wait_s: 0.0,
                                wait_basis: "forced (test)".into(),
                                run_s: 0.0,
                                run_basis: "forced (test)".into(),
                                run_label: "run".into(),
                                total_s: 0.0,
                                cargo_jobs: None,
                            },
                        )
                    })
            })
        });
        let chosen = match (forced.flatten(), &best) {
            (Some(f), _) => Some(f),
            (None, Some((i, s))) => {
                let ratio_ok = local_total >= policy.min_ratio * s.total_s;
                let gain_ok = local_total - s.total_s >= policy.min_gain_s;
                if ratio_ok && gain_ok {
                    Some((*i, s.clone()))
                } else {
                    verdict.line = format!(
                        "stays: ~{} here vs ~{} on {} (needs >= {}x and >= {} sooner)",
                        duration(local_total),
                        duration(s.total_s),
                        s.peer,
                        policy.min_ratio,
                        duration(policy.min_gain_s)
                    );
                    None
                }
            }
            (None, None) => {
                verdict.line = if skipped.is_empty() {
                    "stays: no worker peer".into()
                } else {
                    format!(
                        "stays: no usable worker peer ({})",
                        skipped
                            .iter()
                            .map(|(i, why)| format!("{} {why}", peers[*i].name))
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                };
                None
            }
        };
        match chosen {
            Some((i, s)) => {
                let argv = match (placement::is_cargo(&c.argv), s.cargo_jobs) {
                    (true, Some(jobs)) => placement::apply_jobs(&c.argv, jobs),
                    _ => c.argv.clone(),
                };
                verdict.line = format!(
                    "rerouted: {here} → {} (~{} wait + ~{} run here vs ~{} wait + ~{} {} there){}",
                    s.peer,
                    duration(lw),
                    duration(lr),
                    duration(s.wait_s),
                    duration(s.run_s),
                    s.run_label,
                    if policy.force { " [forced]" } else { "" }
                );
                verdict.action = Action::Move {
                    peer: s.peer.clone(),
                    argv,
                    cargo_jobs: s.cargo_jobs.filter(|_| placement::is_cargo(&c.argv)),
                };
                gone.push(c.id);
                extra[i] += s.run_s;
            }
            None => {
                verdict.action = Action::Stay {
                    why: verdict.line.clone(),
                };
            }
        }
        out.push(verdict);
    }
    out
}

/// The local forwarding spec of a move: the original live command (argv as decided) wrapped by
/// `peer_link::forwarded_spec`, owner and cwd unchanged. The submitter's recorded
/// `--broad-test` escape (feature/log-search) moves with the job: the forward spec is checked
/// against this host's submission policy again, and the peer applies its own.
pub fn moved_spec(
    original: &JobSpec,
    argv: &[String],
    peer: &str,
    state: &Path,
    exe: &Path,
) -> Result<JobSpec> {
    let inner = Recipe {
        argv: argv.to_vec(),
        resource: Resource::Compile,
        broad_test: Recipe::from_spec_unvalidated(original).is_ok_and(|r| r.broad_test),
    }
    .unchecked_spec(original.owner.clone(), PathBuf::from(&original.source))?;
    crate::peer_link::forwarded_spec(&inner, peer, &[], state, exe)
}

// ---- pull-back ----------------------------------------------------------------------------------

/// Remote phases of a job that has not started executing there.
pub fn waiting_remote_phase(phase: &str) -> bool {
    matches!(
        phase,
        "approved" | "preparing" | "waiting_build" | "waiting_run"
    )
}

/// Whether an admission `waiting_for` text names a pressure gate (PSI, available memory, CPU)
/// rather than a busy slot or a queue position.
pub fn pressure_blocker(text: &str) -> bool {
    text.contains("PSI")
        || text.starts_with("available memory below")
        || text.starts_with("invalid pressure metric")
}

/// One forwarded job (the local bridge) as pull-back sees it, with what the link record and
/// the placement record know about the remote copy.
#[derive(Clone, Debug, PartialEq)]
pub struct Forwarded {
    pub id: i64,
    pub phase: Phase,
    /// The peer it was forwarded to.
    pub peer: String,
    /// The command as forwarded (the peer's Cargo `-j`).
    pub argv: Vec<String>,
    /// The class the forward carries; only a compile job comes back.
    pub resource: Resource,
    pub source: String,
    pub owner: String,
    pub cancel_requested: bool,
    pub moves: u32,
    pub pinned: Option<String>,
    /// The `requested` field of the job's placement record: `anywhere`, `local`, or a peer
    /// name. None when nothing was recorded (`tasks run --peer NAME` records nothing).
    pub requested: Option<String>,
    pub remote_job_id: Option<i64>,
    /// The link record's last remote phase and state (`attached`, `uncertain`, ...).
    pub remote_phase: Option<String>,
    pub remote_state: Option<String>,
}

impl Forwarded {
    /// None for anything that is not a `peer-forward` bridge.
    pub fn from_job(
        job: &Job,
        pinned: Option<String>,
        requested: Option<String>,
        remote: (Option<i64>, Option<String>, Option<String>),
    ) -> Option<Self> {
        let recipe = Recipe::from_spec_unvalidated(&job.spec).ok()?;
        let args = crate::peer_link::ForwardArgs::from_recipe_argv(&recipe.argv)?.ok()?;
        let e = job.execution.as_ref();
        Some(Self {
            id: job.id,
            phase: job.phase.clone(),
            peer: args.peer,
            argv: args.argv,
            resource: match args.resource {
                crate::peer_link::ForwardResource::Compile => Resource::Compile,
                crate::peer_link::ForwardResource::Run => Resource::Run,
                crate::peer_link::ForwardResource::Device(serial) => Resource::Device(serial),
            },
            source: job.spec.source.clone(),
            owner: job.spec.owner.clone(),
            cancel_requested: e.is_some_and(|e| e.cancel_requested),
            moves: e
                .and_then(|e| e.reroute.as_ref())
                .map_or(0, |r| r.moves.max(1)),
            pinned,
            requested,
            remote_job_id: remote.0,
            remote_phase: remote.1,
            remote_state: remote.2,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PullAction {
    Pull {
        peer: String,
        /// The command as it runs here (Cargo `-j` replaced by this machine's cap).
        argv: Vec<String>,
        /// The peer's pressure gate that holds the job there.
        blocker: String,
    },
    Stay {
        why: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PullVerdict {
    pub job: i64,
    pub action: PullAction,
    pub here_s: f64,
    pub there_s: f64,
    /// The reason to stay, or the decision as one line for a pull-back.
    pub line: String,
}

impl PullVerdict {
    pub fn pulls(&self) -> bool {
        matches!(self.action, PullAction::Pull { .. })
    }
}

/// Why forward `f` is never pulled back, or None when it may be (peer state not looked at).
pub fn pull_back_ineligible(f: &Forwarded) -> Option<String> {
    if f.resource != Resource::Compile {
        return Some("only the compile class comes back".into());
    }
    if f.phase != Phase::Building {
        return Some(format!("the bridge is not running ({:?})", f.phase));
    }
    // Only a forward the placement chose may come back: a named peer, `--peer NAME` (no
    // record at all) or a local request is the submitter's wish.
    match f.requested.as_deref() {
        Some("anywhere") => {}
        Some(other) => return Some(format!("sent to {other:?} on request")),
        None => return Some("no placement record: sent to a named peer".into()),
    }
    if let Some(why) = &f.pinned {
        return Some(format!("pinned ({why})"));
    }
    if f.moves >= MAX_MOVES {
        return Some("already moved once".into());
    }
    if f.cancel_requested {
        return Some("cancel requested".into());
    }
    if f.remote_job_id.is_none() {
        return Some("not submitted to the peer yet".into());
    }
    match f.remote_state.as_deref() {
        None | Some("attached") => {}
        Some(other) => return Some(format!("the remote state is {other}")),
    }
    if !f.remote_phase.as_deref().is_some_and(waiting_remote_phase) {
        return Some(format!(
            "already started there ({})",
            f.remote_phase.as_deref().unwrap_or("unknown")
        ));
    }
    None
}

/// Why this machine cannot start a compile job right now, or None when its compile slot is free
/// and nothing waits ahead or is blocked.
pub fn local_busy(local: &Snapshot) -> Option<String> {
    if let Some(blocker) = &local.machine.blocker {
        return Some(format!("blocked here too: {blocker}"));
    }
    let (_, held) = holder_wait(local);
    let slots = local
        .machine
        .slots
        .iter()
        .filter(|s| s.kind == SlotKind::Compile)
        .count()
        .max(1);
    if held.len() >= slots {
        return Some(held.join(", "));
    }
    local
        .jobs
        .iter()
        .find(|j| {
            j.resource == SlotKind::Compile
                && waiting_view(&j.phase)
                && j.queue.as_ref().is_some_and(|q| q.position.is_some())
        })
        .map(|j| format!("compile job #{} waits here first", j.id))
}

/// The remaining wait of forward `remote` on the peer: placement's compile wait minus the job's
/// own run (placement counts every positioned waiter, this one included).
fn remote_wait(snapshot: &Snapshot, remote: i64) -> (f64, String) {
    let (mut wait, basis) = placement::compile_wait(snapshot);
    if let Some(job) = snapshot.jobs.iter().find(|j| j.id == remote)
        && job.resource == SlotKind::Compile
        && waiting_view(&job.phase)
        && job.queue.as_ref().is_some_and(|q| q.position.is_some())
    {
        wait = (wait - job.estimate.as_ref().map_or(UNKNOWN_S, |e| e.total_s)).max(0.0);
    }
    (wait, basis)
}

/// Decides, for each forwarded job, whether to pull it back from its peer. It comes back when
/// ALL hold: it is a placement-chosen, unpinned compile forward that never moved before; the
/// peer's copy has not started (remote phase approved / preparing / waiting_build /
/// waiting_run, in the link record and in the peer's snapshot) and the peer's queue text for
/// it names a pressure blocker; this machine's compile slot is free with no pressure blocker;
/// and running here now beats the peer's remaining wait plus run by `min_ratio` and
/// `min_gain_s`. One job per round (the slot goes to it).
pub fn decide_pull_back(
    forwards: &[Forwarded],
    local: &Snapshot,
    local_jobs: u64,
    peers: &[Peer],
    policy: &Policy,
) -> Vec<PullVerdict> {
    let busy = local_busy(local);
    let mut taken: Option<i64> = None;
    let mut out = vec![];
    for f in forwards {
        let stay = |why: String| PullVerdict {
            job: f.id,
            action: PullAction::Stay { why: why.clone() },
            here_s: 0.0,
            there_s: 0.0,
            line: why,
        };
        if !policy.pull_back {
            out.push(stay("pull-back is off".into()));
            continue;
        }
        if let Some(why) = pull_back_ineligible(f) {
            out.push(stay(why));
            continue;
        }
        if let Some(why) = &busy {
            out.push(stay(format!("stays on {}: {why}", f.peer)));
            continue;
        }
        if let Some(id) = taken {
            out.push(stay(format!(
                "stays on {}: the compile slot goes to #{id}",
                f.peer
            )));
            continue;
        }
        let Some(peer) = peers.iter().find(|p| p.name == f.peer) else {
            out.push(stay(format!("peer {} is not configured", f.peer)));
            continue;
        };
        if let Some(why) = peer.unusable() {
            out.push(stay(format!("peer {} {why}", f.peer)));
            continue;
        }
        let (Some(snap), Some(remote)) = (peer.snapshot.as_ref(), f.remote_job_id) else {
            out.push(stay("no snapshot of the peer".into()));
            continue;
        };
        let Some(view) = snap.jobs.iter().find(|j| j.id == remote) else {
            out.push(stay(format!(
                "job {remote} is not in {}'s snapshot",
                f.peer
            )));
            continue;
        };
        if !movable_phase(&view.phase) || view.live.is_some() {
            out.push(stay(format!(
                "already started on {} ({:?})",
                f.peer, view.phase
            )));
            continue;
        }
        let Some(blocker) = view
            .queue
            .as_ref()
            .and_then(|q| q.waiting_for.clone())
            .filter(|w| pressure_blocker(w))
        else {
            out.push(stay(format!(
                "stays on {}: not blocked by pressure ({})",
                f.peer,
                view.queue
                    .as_ref()
                    .and_then(|q| q.waiting_for.as_deref())
                    .unwrap_or("no queue text")
            )));
            continue;
        };
        let kind = normalize(&placement::strip_jobs(&f.argv)).kind_key;
        let (run, run_basis) = match placement::history(local, &kind) {
            Some((m, n)) => (m, format!("median of {n} runs here")),
            None => (UNKNOWN_S, "no history".into()),
        };
        let (wait, _) = remote_wait(snap, remote);
        let (peer_run_s, _, label) = peer_run(
            &kind,
            &f.source,
            local,
            snap,
            local_jobs,
            snap.machine.cargo_jobs_max,
        );
        // The peer's pressure gate (+ BLOCKED_S) is part of `wait` only when its machine
        // blocker says so; the job's own queue text is the evidence here, so add it once.
        let there = wait
            + peer_run_s
            + if snap.machine.blocker.is_none() {
                BLOCKED_S
            } else {
                0.0
            };
        let ratio_ok = run * policy.min_ratio <= there;
        let gain_ok = there - run >= policy.min_gain_s;
        if !(ratio_ok && gain_ok) {
            let mut v = stay(format!(
                "stays on {}: ~{} here vs ~{} there (needs >= {}x and >= {} sooner)",
                f.peer,
                duration(run),
                duration(there),
                policy.min_ratio,
                duration(policy.min_gain_s)
            ));
            (v.here_s, v.there_s) = (run, there);
            out.push(v);
            continue;
        }
        let cap = local.machine.cargo_jobs_max.unwrap_or(local_jobs);
        let argv = if placement::is_cargo(&f.argv) {
            placement::apply_jobs(&f.argv, cap)
        } else {
            f.argv.clone()
        };
        taken = Some(f.id);
        out.push(PullVerdict {
            job: f.id,
            action: PullAction::Pull {
                peer: f.peer.clone(),
                argv,
                blocker: blocker.clone(),
            },
            here_s: run,
            there_s: there,
            line: format!(
                "pulled back {} → {}: {blocker} (~{} run here, {run_basis}, vs ~{} {label} there incl. wait)",
                f.peer,
                local.machine.id,
                duration(run),
                duration(there)
            ),
        });
    }
    out
}

/// The local spec a pull-back restores: the forwarded command (as decided) run here, owner
/// and cwd unchanged, the `--broad-test` escape kept.
pub fn pulled_spec(forward: &JobSpec, argv: &[String]) -> Result<JobSpec> {
    Recipe {
        argv: argv.to_vec(),
        resource: Resource::Compile,
        broad_test: Recipe::from_spec_unvalidated(forward).is_ok_and(|r| r.broad_test),
    }
    .unchecked_spec(forward.owner.clone(), PathBuf::from(&forward.source))
}

/// `<job dir>/pull-back-pending`: the daemon is cancelling the peer's copy of job `id` and may
/// restore its local spec next. The worker, when the bridge ends, waits briefly for that.
pub fn pull_back_marker(state: &Path, id: i64) -> PathBuf {
    job_dir(state, id).join("pull-back-pending")
}

// ---- ledger transactions ------------------------------------------------------------------------

/// The canonical text of a spec, for "is it still the spec I read" comparisons.
pub fn spec_key(spec: &JobSpec) -> Result<String> {
    Ok(serde_json::to_string(spec)?)
}

fn stored_key(text: &str) -> Result<String> {
    spec_key(&serde_json::from_str::<JobSpec>(text)?)
}

pub fn job_dir(state: &Path, id: i64) -> PathBuf {
    state.join("jobs").join(id.to_string())
}

fn committed_marker(state: &Path, id: i64, stage: &str) -> PathBuf {
    job_dir(state, id).join(format!("{stage}-admission-committed"))
}

/// A local admission was committed for job `id` (any stage).
pub fn admitted_here(state: &Path, id: i64) -> bool {
    ["build", "run"]
        .iter()
        .any(|s| committed_marker(state, id, s).exists())
}

/// Result of a claim attempt.
#[derive(Clone, Debug, PartialEq)]
pub enum Claim {
    Claimed,
    Refused(String),
}

impl Store {
    /// Live jobs that could still be waiting (approved / preparing / waiting_*), by id.
    pub fn waiting_jobs(&self) -> Result<Vec<Job>> {
        let ids: Vec<i64> = {
            let mut statement = self.0.prepare(
                "SELECT id FROM jobs WHERE phase IN \
                 ('approved','preparing','waiting_build','waiting_run') ORDER BY id",
            )?;
            statement
                .query_map([], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?
        };
        let mut jobs = vec![];
        for id in ids {
            if let Ok(job) = self.job(id) {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    /// Live jobs whose stage executes (building), by id: where a forward's bridge runs.
    pub fn building_jobs(&self) -> Result<Vec<Job>> {
        let ids: Vec<i64> = {
            let mut statement = self
                .0
                .prepare("SELECT id FROM jobs WHERE phase='building' ORDER BY id")?;
            statement
                .query_map([], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?
        };
        Ok(ids.into_iter().filter_map(|id| self.job(id).ok()).collect())
    }

    /// The reroute claim: in ONE immediate transaction, checks that job `id` is still exactly
    /// `expected`, has not started and has no committed local admission, then rewrites its spec
    /// into `moved` and records `record`. Native admission commits through
    /// [`Store::commit_admission`] under the same write lock, so exactly one of them wins.
    pub fn claim_move(
        &mut self,
        state: &Path,
        id: i64,
        expected: &JobSpec,
        moved: &JobSpec,
        record: &Reroute,
    ) -> Result<Claim> {
        use rusqlite::{OptionalExtension, TransactionBehavior, params};
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String)> = tx
            .query_row("SELECT phase, spec FROM jobs WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((phase, spec)) = row else {
            return Ok(Claim::Refused("no such job".into()));
        };
        let phase = Phase::from_db(&phase)?;
        if !movable_phase(&phase) {
            return Ok(Claim::Refused(format!("phase is now {phase:?}")));
        }
        if stored_key(&spec)? != spec_key(expected)? {
            return Ok(Claim::Refused("its spec changed".into()));
        }
        let data: Option<String> = tx
            .query_row("SELECT data FROM executions WHERE job_id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(data) = data else {
            return Ok(Claim::Refused("not approved".into()));
        };
        let mut e: crate::execution::Execution = serde_json::from_str(&data)?;
        if e.cancel_requested {
            return Ok(Claim::Refused("cancel requested".into()));
        }
        if !e.stages.is_empty() {
            return Ok(Claim::Refused("a stage already ran".into()));
        }
        if e.reroute.as_ref().is_some_and(|r| r.moves >= MAX_MOVES) {
            return Ok(Claim::Refused("already moved once".into()));
        }
        // Checked under the write lock: admission writes this marker inside its own immediate
        // transaction, so it is either visible now or that admission will see the new spec.
        if admitted_here(state, id) {
            return Ok(Claim::Refused("local admission already committed".into()));
        }
        let mut record = record.clone();
        record.moves = e.reroute.as_ref().map_or(0, |r| r.moves) + 1;
        record.original_spec = Some(serde_json::from_str(&spec)?);
        e.detail = format!("{}; local admission withdrawn", record.reason);
        e.reroute = Some(record);
        tx.execute(
            "UPDATE jobs SET spec=?2 WHERE id=?1",
            params![id, serde_json::to_string(moved)?],
        )?;
        tx.execute(
            "UPDATE executions SET data=?2 WHERE job_id=?1",
            params![id, serde_json::to_string(&e)?],
        )?;
        tx.commit()?;
        Ok(Claim::Claimed)
    }

    /// The pull-back claim: in ONE immediate transaction, checks that forward `id` is still
    /// exactly `expected` (its bridge running, no cancel request, no earlier move) and rewrites
    /// its spec into `pulled`, recording `record`. Call it only after the peer's copy was
    /// cancelled: when the bridge returns, the worker sees the new spec and queues the job
    /// here, as for a failed move. At most [`MAX_MOVES`] counts this one.
    pub fn claim_pull_back(
        &mut self,
        id: i64,
        expected: &JobSpec,
        pulled: &JobSpec,
        record: &Reroute,
    ) -> Result<Claim> {
        use rusqlite::{OptionalExtension, TransactionBehavior, params};
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String)> = tx
            .query_row("SELECT phase, spec FROM jobs WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((phase, spec)) = row else {
            return Ok(Claim::Refused("no such job".into()));
        };
        let phase = Phase::from_db(&phase)?;
        if phase != Phase::Building {
            return Ok(Claim::Refused(format!("phase is now {phase:?}")));
        }
        if stored_key(&spec)? != spec_key(expected)? {
            return Ok(Claim::Refused("its spec changed".into()));
        }
        let data: Option<String> = tx
            .query_row("SELECT data FROM executions WHERE job_id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(data) = data else {
            return Ok(Claim::Refused("not approved".into()));
        };
        let mut e: crate::execution::Execution = serde_json::from_str(&data)?;
        if e.cancel_requested {
            return Ok(Claim::Refused("cancel requested".into()));
        }
        if e.reroute.as_ref().is_some_and(|r| r.moves >= MAX_MOVES) {
            return Ok(Claim::Refused("already moved once".into()));
        }
        let mut record = record.clone();
        record.moves = e.reroute.as_ref().map_or(0, |r| r.moves) + 1;
        record.original_spec = Some(serde_json::from_str(&spec)?);
        e.detail = format!("{}; running here", record.reason);
        e.reroute = Some(record);
        tx.execute(
            "UPDATE jobs SET spec=?2 WHERE id=?1",
            params![id, serde_json::to_string(pulled)?],
        )?;
        tx.execute(
            "UPDATE executions SET data=?2 WHERE job_id=?1",
            params![id, serde_json::to_string(&e)?],
        )?;
        tx.commit()?;
        Ok(Claim::Claimed)
    }

    /// Native admission's commit, after it took the slot: true when job `id` still has the
    /// spec `expected` it read (then the `STAGE-admission-committed` marker is written inside
    /// the transaction and no reroute can claim the job any more); false when a reroute claimed
    /// it meanwhile (the caller releases the slot without running anything).
    pub fn commit_admission(
        &mut self,
        state: &Path,
        id: i64,
        stage: &str,
        expected: &str,
    ) -> Result<bool> {
        use rusqlite::TransactionBehavior;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let spec: String = tx.query_row("SELECT spec FROM jobs WHERE id=?1", [id], |r| r.get(0))?;
        if stored_key(&spec)? != expected {
            return Ok(false);
        }
        let marker = committed_marker(state, id, stage);
        if let Some(dir) = marker.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&marker, crate::execution::now().to_string())?;
        tx.commit()?;
        Ok(true)
    }

    /// Whether job `id`'s spec is no longer `expected` (a reroute claimed it).
    pub fn spec_changed(&self, id: i64, expected: &str) -> Result<bool> {
        let spec: String = self
            .0
            .query_row("SELECT spec FROM jobs WHERE id=?1", [id], |r| r.get(0))?;
        Ok(stored_key(&spec)? != expected)
    }

    /// Updates the reroute record of job `id` (no-op without one).
    pub fn set_reroute(&mut self, id: i64, update: impl FnOnce(&mut Reroute)) -> Result<()> {
        use rusqlite::{TransactionBehavior, params};
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let data: String =
            tx.query_row("SELECT data FROM executions WHERE job_id=?1", [id], |r| {
                r.get(0)
            })?;
        let mut e: crate::execution::Execution = serde_json::from_str(&data)?;
        let Some(record) = e.reroute.as_mut() else {
            return Ok(());
        };
        update(record);
        tx.execute(
            "UPDATE executions SET data=?2 WHERE job_id=?1",
            params![id, serde_json::to_string(&e)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A move that submitted nothing: the original spec comes back (same job, still active) and
    /// the record says why. The move still counts ([`MAX_MOVES`]).
    pub fn revert_reroute(&mut self, id: i64, note: &str) -> Result<()> {
        use rusqlite::{TransactionBehavior, params};
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let phase: String =
            tx.query_row("SELECT phase FROM jobs WHERE id=?1", [id], |r| r.get(0))?;
        if !Phase::from_db(&phase)?.active() {
            return Err("only an active job can return to the local queue".into());
        }
        let data: String =
            tx.query_row("SELECT data FROM executions WHERE job_id=?1", [id], |r| {
                r.get(0)
            })?;
        let mut e: crate::execution::Execution = serde_json::from_str(&data)?;
        let record = e.reroute.as_mut().ok_or("job was not rerouted")?;
        let original = record
            .original_spec
            .clone()
            .ok_or("reroute record lacks the original spec")?;
        record.state = STATE_RETURNED.into();
        record.note = Some(note.to_owned());
        record.returned_at = Some(crate::execution::now());
        e.detail = format!("Back in the local queue: {note}");
        tx.execute(
            "UPDATE jobs SET spec=?2 WHERE id=?1",
            params![id, serde_json::to_string(&original)?],
        )?;
        tx.execute(
            "UPDATE executions SET data=?2 WHERE job_id=?1",
            params![id, serde_json::to_string(&e)?],
        )?;
        tx.commit()?;
        Ok(())
    }
}

// ---- job directory helpers (worker side) --------------------------------------------------------

/// Appends to a job log the way the worker does (flock, 8 MiB cap). Fails if the job directory
/// does not exist yet.
pub fn append_job_log(state: &Path, id: i64, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(job_dir(state, id).join("job.log"))
        .or_else(|e| {
            if job_dir(state, id).is_dir() {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(job_dir(state, id).join("job.log"))
            } else {
                Err(e)
            }
        })?;
    f.lock()?;
    let left = (8u64 * 1024 * 1024).saturating_sub(f.metadata()?.len());
    let bytes = text.as_bytes();
    let result = f.write_all(&bytes[..bytes.len().min(left as usize)]);
    let _ = f.unlock();
    result
}

/// Renames the files of an aborted `stage` attempt (`STAGE-waiting.json`, `-result.json`,
/// `-admitted.json`, usage, committed marker) out of the way: a later attempt of the same stage
/// must not read them. `keep_queued_at` keeps the first admission wait time (queue position).
pub fn archive_stage_files(dir: &Path, stage: &str, tag: &str, keep_queued_at: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&format!("{stage}-")) || name.contains(".reroute-") {
            continue;
        }
        if keep_queued_at && name == format!("{stage}-queued-at.json") {
            continue;
        }
        let _ = std::fs::rename(entry.path(), dir.join(format!("{name}.reroute-{tag}")));
    }
}

/// How long the worker waits, after the bridge of a pull-back ended, for the spec to change.
const PULL_BACK_WAIT: Duration = Duration::from_secs(20);

/// What the worker does after a stage of a live command returned.
#[derive(Clone, Debug, PartialEq)]
pub enum Next {
    /// Record the stage as usual.
    Continue,
    /// The job's spec changed (moved away, or back); re-read it and run its stages. The text
    /// goes into the job log.
    Respec(String),
}

/// After a stage: was the job moved while it waited (its admission withdrew), or was this the
/// forward of a move that submitted nothing (back to the local queue)?
pub fn after_stage(
    state: &Path,
    store: &mut Store,
    id: i64,
    ran: &str,
    stage: &str,
) -> Result<Next> {
    let mut job = store.job(id)?;
    let dir = job_dir(state, id);
    // A pull-back cancels the peer's copy (this stage's bridge ends) before it restores the
    // spec: give it a moment instead of recording the bridge's exit as the job's result.
    let end = std::time::Instant::now() + PULL_BACK_WAIT;
    while spec_key(&job.spec)? == ran
        && pull_back_marker(state, id).exists()
        && std::time::Instant::now() < end
    {
        std::thread::sleep(Duration::from_millis(100));
        job = store.job(id)?;
    }
    let record = job.execution.as_ref().and_then(|e| e.reroute.clone());
    let cancel = job.execution.as_ref().is_some_and(|e| e.cancel_requested);
    if spec_key(&job.spec)? != ran {
        archive_stage_files(&dir, stage, "moved", true);
        if let Some(r) = record.as_ref().filter(|r| r.state == STATE_PULLED_BACK) {
            let link_path = state.join("peer/links").join(format!("{id}.json"));
            let _ = std::fs::rename(
                &link_path,
                link_path.with_extension("json.reroute-pulled-back"),
            );
            return Ok(Next::Respec(format!(
                "tasks: the copy on {} is cancelled; job {id} now queues here\n",
                r.moved_from
            )));
        }
        let to = record.as_ref().map_or("a peer", |r| r.moved_to.as_str());
        return Ok(Next::Respec(format!(
            "tasks: local {stage} admission withdrawn before anything ran here: job {id} was rerouted to {to}\n"
        )));
    }
    let Some(record) = record.filter(|r| r.state == STATE_MOVING) else {
        return Ok(Next::Continue);
    };
    let link_path = state.join("peer/links").join(format!("{id}.json"));
    let link: Option<serde_json::Value> = std::fs::read(&link_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let remote = link.as_ref().and_then(|l| l["remote_job_id"].as_i64());
    let remote_state = link
        .as_ref()
        .and_then(|l| l["remote_state"].as_str().map(str::to_owned));
    // The forward writes its idempotency key into the link record before the first submit
    // attempt: no record at all, or a finished one that never got a remote id and says
    // not_submitted/refused, means the peer has nothing. Uncertain is never reverted.
    let nothing_submitted = remote.is_none()
        && match &link {
            None => true,
            Some(l) => {
                l["stage"] == "done"
                    && matches!(remote_state.as_deref(), Some("not_submitted" | "refused"))
            }
        };
    if nothing_submitted && !cancel {
        let why = link
            .as_ref()
            .and_then(|l| l["error"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "nothing was submitted".into());
        let note = format!(
            "the forward to {} submitted nothing ({why})",
            record.moved_to
        );
        store.revert_reroute(id, &note)?;
        archive_stage_files(&dir, stage, "returned", false);
        let _ = std::fs::rename(
            &link_path,
            link_path.with_extension("json.reroute-returned"),
        );
        return Ok(Next::Respec(format!(
            "tasks: reroute {} → {} failed: {note}; job {id} is back in the local queue at its original position\n",
            record.moved_from, record.moved_to
        )));
    }
    store.set_reroute(id, |r| {
        r.state = STATE_MOVED.into();
        r.remote_job_id = remote;
    })?;
    Ok(Next::Continue)
}

/// Writes the move line into the job log if the job moved before its directory existed.
pub fn flush_pending_line(state: &Path, store: &mut Store, id: i64) {
    let Ok(job) = store.job(id) else { return };
    let Some(record) = job.execution.and_then(|e| e.reroute) else {
        return;
    };
    if !record.log_pending || !job_dir(state, id).is_dir() {
        return;
    }
    // Take the flag in one transaction so the daemon and the worker never both write it.
    let took = std::cell::Cell::new(false);
    if store
        .set_reroute(id, |r| {
            took.set(r.log_pending);
            r.log_pending = false;
        })
        .is_err()
        || !took.get()
    {
        return;
    }
    if append_job_log(state, id, &format!("{}\n", record.reason)).is_err() {
        let _ = store.set_reroute(id, |r| r.log_pending = true);
    }
}

/// The first time job `id` waited for `stage` admission (Unix seconds as f64), recorded on the
/// first attempt; a later attempt (back from a failed move) queues with the same time.
pub fn queued_since(dir: &Path, stage: &str) -> Option<std::time::SystemTime> {
    let path = dir.join(format!("{stage}-queued-at.json"));
    if let Some(secs) = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<f64>(&b).ok())
        .filter(|s| s.is_finite() && *s > 0.0)
    {
        return Some(std::time::UNIX_EPOCH + Duration::from_secs_f64(secs));
    }
    let now = std::time::SystemTime::now();
    if let Ok(d) = now.duration_since(std::time::UNIX_EPOCH) {
        let _ = std::fs::write(
            &path,
            serde_json::to_vec(&d.as_secs_f64()).unwrap_or_default(),
        );
    }
    None
}

/// Test hook (debug builds only, [`TEST_ADMIT_HOLD_ENV`]): pauses native admission of job `id`
/// between taking the slot and committing, so a test can fire a reroute at exactly that moment.
#[cfg(debug_assertions)]
pub fn test_admit_hold(id: i64, stage: &str) {
    let Some(dir) = std::env::var_os(TEST_ADMIT_HOLD_ENV).map(PathBuf::from) else {
        return;
    };
    let base = format!("{id}-{stage}");
    if !dir.join(format!("{base}.hold")).exists() {
        return;
    }
    let _ = std::fs::write(dir.join(format!("{base}.held")), b"");
    let end = std::time::Instant::now() + Duration::from_secs(300);
    while !dir.join(format!("{base}.release")).exists() && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
#[path = "reroute_tests.rs"]
mod tests;
