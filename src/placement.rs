//! Machine placement: which daemon runs one live command, this machine's or a peer's
//! (forwarded over the iroh peer link, `peer_link::forwarded_spec`).
//!
//! The scoring ([`decide`]) is pure and tested from snapshot fixtures. Inputs are this
//! machine's cluster snapshot and the latest snapshot of every worker/both peer, which the
//! daemon's peer runtime keeps from each peer's `status/0` stream (`Action::Peers` ->
//! `snapshots`). No ssh, no `tasks-offload`.
//!
//! Per candidate machine: expected total = wait + run.
//! * wait: the compile slot holder's remaining ETA, plus every compile job queued ahead with
//!   its ETA ([`UNKNOWN_S`] when unknown); a pressure blocker adds [`BLOCKED_S`] (a blocked
//!   machine is slower, not excluded); a peer also waits for this machine's offload backlog
//!   (its bridge job takes a local offload slot).
//! * run: median wall time of passed runs of the same command kind on that machine (for a
//!   peer also this machine's forwarded runs of it); with no history there, another
//!   machine's median scaled by the Cargo jobs ratio; with no history anywhere, the same
//!   guess everywhere, so the wait decides.
//!
//! Ties go to the machine with more compile slots free, then to a worker peer (keeping this
//! machine free). Offline,
//! never reached and stale peers are skipped (with a note), and so is a peer with a
//! missing-capability mark the command needs (`health::capability`: e.g. a Cargo command
//! with `bindgen` in its lock file on a peer whose earlier job failed with "Unable to find
//! libclang"); a named peer still gets the job, with a note. The device class always stays
//! local; the run class stays local unless a peer is named. Advisory only: admission on the
//! chosen machine still applies every lock and gate, and the receiving peer re-validates the
//! command (its own Cargo jobs cap and device denylist).
use crate::Phase;
use crate::metrics::{SlotKind, Snapshot, command::normalize};
use crate::peer_link::PeerSnapshot;
use serde::Serialize;
use std::path::Path;

/// Seconds assumed for a slot holder or queued job without an ETA, and for a run time with no
/// history anywhere.
pub const UNKNOWN_S: f64 = 600.0;
/// Extra wait for a machine whose admission currently reports a pressure blocker.
pub const BLOCKED_S: f64 = 600.0;
/// Cargo jobs assumed for the ratio when a machine's cap is unknown (admission's default).
pub const DEFAULT_JOBS: u64 = crate::admission::DEFAULT_MAX_CARGO_JOBS;

/// What the caller asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// This machine, exactly as before (no command rewrite).
    Local,
    /// Placement decides among this machine and every usable worker/both peer.
    Anywhere,
    /// Forward to this peer (`tasks run --peer NAME`).
    Peer(String),
}

impl Request {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "local" => Ok(Self::Local),
            "anywhere" => Ok(Self::Anywhere),
            name if task_manager_peer::config::valid_name(name) => Ok(Self::Peer(name.into())),
            other => Err(format!(
                "placement {other:?} is not \"local\", \"anywhere\" or a peer name \
                 ([A-Za-z0-9._-]{{1,64}})"
            )),
        }
    }
    pub fn text(&self) -> String {
        match self {
            Self::Local => "local".into(),
            Self::Anywhere => "anywhere".into(),
            Self::Peer(name) => name.clone(),
        }
    }
}

/// Admission class of the command (where it runs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Class {
    Compile,
    Run,
    Device(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum Target {
    Local,
    Peer(String),
}

/// One worker/both peer as placement sees it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Peer {
    pub name: String,
    /// `worker`, `both`, `host`, `observer`.
    pub role: String,
    /// `connecting`, `online`, `offline`, `status_error` (see `PeerSnapshot`).
    pub state: String,
    pub error: Option<String>,
    pub age_s: Option<f64>,
    pub stale: bool,
    pub snapshot: Option<Snapshot>,
    /// Why the snapshot could not be read, if it could not.
    pub unreadable: Option<String>,
    /// Missing-capability marks about this peer ([`marks_for`]); advisory, see the module doc.
    pub missing: Vec<crate::health::capability::CapabilityMark>,
}

/// The marks about `peer`: the controller's own (by the peer's configured name or machine
/// id, from classified forwarded-job logs) plus those the peer publishes about itself in its
/// snapshot's `health.capabilities`. One per capability (the controller's first).
pub fn marks_for(
    peer: &Peer,
    local_marks: &[crate::health::capability::CapabilityMark],
) -> Vec<crate::health::capability::CapabilityMark> {
    let id = peer.snapshot.as_ref().map(|s| s.machine.id.as_str());
    let own = peer
        .snapshot
        .as_ref()
        .and_then(|s| s.health.as_ref())
        .map(|h| h.capabilities.as_slice())
        .unwrap_or_default();
    let mut out: Vec<crate::health::capability::CapabilityMark> = vec![];
    for m in local_marks.iter().chain(own) {
        let about = m.node == peer.name || Some(m.node.as_str()) == id;
        if about && !out.iter().any(|o| o.capability == m.capability) {
            out.push(m.clone());
        }
    }
    out
}

/// Keeps only the marks `argv` (run from `cwd`) needs, so [`decide`] (which has no cwd)
/// skips only for capabilities this command uses (e.g. libclang only with `bindgen`).
/// Prefixes where a path that exists here is private to this machine (another machine has its
/// own home and `/tmp`); system and toolchain paths exist on every machine.
const PRIVATE_PREFIXES: &[&str] = &["/tmp/", "/var/tmp/", "/home/", "/root/", "/mnt/", "/media/"];
/// Toolchain homes every worker has its own copy of.
const SHARED_HOME_DIRS: &[&str] = &["/.cargo/", "/.rustup/", "/.local/bin/", "/.nix-profile/"];

/// The first argument naming an absolute path that exists on this machine but would not on
/// another: a script in a scratchpad or `/tmp`, or a file under the home directory. Only the
/// checkout is synced, and it lives at a different path on the peer, so the command would
/// exit 127 or fail to open it there. Looks into `NAME=/path` and `--flag=/path` too.
pub fn local_only_path(argv: &[String]) -> Option<String> {
    argv.iter().skip(1).find_map(|arg| {
        let path = arg.rsplit_once('=').map_or(arg.as_str(), |(_, v)| v);
        let private = PRIVATE_PREFIXES.iter().any(|p| path.starts_with(p))
            && !SHARED_HOME_DIRS.iter().any(|d| path.contains(d));
        (private && Path::new(path).exists()).then(|| path.to_string())
    })
}

pub fn retain_needed(peers: &mut [Peer], argv: &[String], cwd: Option<&Path>) {
    for p in peers {
        p.missing
            .retain(|m| crate::health::capability::needed(&m.family, argv, cwd));
    }
}

impl Peer {
    pub fn from_report(s: &PeerSnapshot) -> Self {
        let (snapshot, unreadable) = match &s.snapshot {
            None => (None, None),
            Some(v) => match serde_json::from_value::<Snapshot>(v.clone()) {
                Ok(snap) => (Some(snap), None),
                Err(e) => (None, Some(e.to_string())),
            },
        };
        Self {
            name: s.name.clone(),
            role: s.role.clone(),
            state: s.state.clone(),
            error: s.error.clone(),
            age_s: s.age_s,
            stale: s.stale,
            snapshot,
            unreadable,
            missing: vec![],
        }
    }
    fn runs_jobs(&self) -> bool {
        matches!(self.role.as_str(), "worker" | "both")
    }
    fn online(&self) -> bool {
        matches!(
            self.state.as_str(),
            crate::peer_link::STREAM_ONLINE | crate::peer_link::STREAM_STATUS_ERROR
        )
    }
    /// Why `anywhere` cannot consider it, or None when it is a candidate.
    pub fn unusable(&self) -> Option<String> {
        let error = self
            .error
            .as_deref()
            .map(|e| format!(": {e}"))
            .unwrap_or_default();
        if !self.runs_jobs() {
            return Some(format!("role {} does not run forwarded jobs", self.role));
        }
        if self.state == crate::peer_link::STREAM_CONNECTING {
            return Some("no status from it yet (connecting)".into());
        }
        if !self.online() {
            return Some(format!("offline{error}"));
        }
        if let Some(why) = &self.unreadable {
            return Some(format!("unreadable snapshot: {why}"));
        }
        if self.snapshot.is_none() || self.stale {
            return Some(match self.age_s {
                Some(age) => format!("stale snapshot ({age:.0}s old){error}"),
                None => format!("no snapshot yet{error}"),
            });
        }
        None
    }
}

/// One candidate's expected time to finish the command.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Score {
    /// Peer name, or this machine's hostname.
    pub machine: String,
    pub target: Target,
    pub wait_s: f64,
    pub wait_basis: String,
    pub run_s: f64,
    pub run_basis: String,
    pub total_s: f64,
    /// That machine's Cargo jobs cap (None: unknown).
    pub cargo_jobs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Skipped {
    pub machine: String,
    pub why: String,
}

/// The placement of one command.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    /// `local`, `anywhere` or the named peer.
    pub requested: String,
    pub target: Target,
    /// Peer name, or this machine's hostname.
    pub machine: String,
    /// The chosen machine's snapshot id (hostname), when known.
    pub machine_id: Option<String>,
    pub reason: String,
    /// The Cargo jobs cap applied to the command (None: not rewritten).
    pub cargo_jobs: Option<u64>,
    /// The command as it runs on the chosen machine.
    pub argv: Vec<String>,
    pub scores: Vec<Score>,
    pub skipped: Vec<Skipped>,
    pub notes: Vec<String>,
}

impl Decision {
    /// A machine choice was actually made (peers were involved); a plain local submission
    /// without peers is not worth recording.
    pub fn decided(&self) -> bool {
        self.peer().is_some() || !self.scores.is_empty() || !self.skipped.is_empty()
    }
    pub fn peer(&self) -> Option<&str> {
        match &self.target {
            Target::Peer(name) => Some(name),
            Target::Local => None,
        }
    }
    /// One line for people, e.g. `placement anywhere -> beta (~0s wait + ~10m run, no
    /// history) vs laptop (~10m wait + ~10m run, no history): beta sooner`.
    pub fn line(&self) -> String {
        let mut out = format!("placement {} -> {}", self.requested, self.machine);
        let chosen = self.scores.iter().find(|s| s.target == self.target);
        if let Some(s) = chosen {
            out.push_str(&format!(" ({})", score_text(s)));
        }
        for s in self.scores.iter().filter(|s| s.target != self.target) {
            out.push_str(&format!(" vs {} ({})", s.machine, score_text(s)));
        }
        for s in &self.skipped {
            out.push_str(&format!("; {} skipped: {}", s.machine, s.why));
        }
        out.push_str(&format!(": {}", self.reason));
        if let Some(jobs) = self.cargo_jobs {
            out.push_str(&format!("; CARGO_BUILD_JOBS={jobs}"));
        }
        out
    }
}

fn score_text(s: &Score) -> String {
    format!(
        "~{} wait [{}] + ~{} run [{}]",
        duration(s.wait_s),
        s.wait_basis,
        duration(s.run_s),
        s.run_basis
    )
}

pub fn duration(s: f64) -> String {
    if s < 60.0 {
        format!("{}s", s.round() as u64)
    } else {
        format!("{}m", (s / 60.0).round() as u64)
    }
}

fn base(word: &str) -> Option<&str> {
    Path::new(word).file_name().and_then(|n| n.to_str())
}

/// Index of `cargo` in a plain `cargo ...` or `env [--] NAME=value... cargo ...` argv.
fn cargo_index(argv: &[String]) -> Option<usize> {
    let mut index = 0;
    if base(argv.first()?)? == "env" {
        index = 1;
        if argv.get(index)? == "--" {
            index += 1;
        }
        while argv.get(index)?.contains('=') {
            index += 1;
        }
    }
    (base(argv.get(index)?)? == "cargo").then_some(index)
}

/// Words of `argv` without Cargo parallelism (`-j N`, `-jN`, `--jobs N`, `--jobs=N`, and a
/// `CARGO_BUILD_JOBS=` assignment in an `env` prefix). Only a direct Cargo command
/// (optionally after `env NAME=value...`) is rewritten; anything after Cargo's own `--` is
/// the test harness's and is kept.
pub fn strip_jobs(argv: &[String]) -> Vec<String> {
    let Some(cargo) = cargo_index(argv) else {
        return argv.to_vec();
    };
    let mut out: Vec<String> = argv[..cargo]
        .iter()
        .filter(|w| !w.starts_with("CARGO_BUILD_JOBS="))
        .cloned()
        .collect();
    if out.len() == 1 && out[0] == "env" || out.len() == 2 && out[0] == "env" && out[1] == "--" {
        out.clear();
    }
    out.push(argv[cargo].clone());
    let mut rest = argv[cargo + 1..].iter();
    while let Some(word) = rest.next() {
        if word == "--" {
            out.push(word.clone());
            out.extend(rest.by_ref().cloned());
            break;
        }
        if word == "-j" || word == "--jobs" {
            rest.next();
        } else if word.starts_with("--jobs=")
            || (word.len() > 2
                && word.starts_with("-j")
                && word[2..].bytes().all(|b| b.is_ascii_digit()))
        {
        } else {
            out.push(word.clone());
        }
    }
    out
}

/// The parallelism a direct Cargo command asks for (`-j N`, `-jN`, `--jobs N`, `--jobs=N`, or a
/// `CARGO_BUILD_JOBS=` assignment in an `env` prefix; a flag wins), `None` when it asks for none.
pub fn requested_jobs(argv: &[String]) -> Option<u64> {
    let cargo = cargo_index(argv)?;
    let mut jobs = argv[..cargo]
        .iter()
        .rev()
        .find_map(|w| w.strip_prefix("CARGO_BUILD_JOBS="))
        .and_then(|v| v.parse().ok());
    let mut rest = argv[cargo + 1..].iter();
    while let Some(word) = rest.next() {
        let value = match word.as_str() {
            "--" => break,
            "-j" | "--jobs" => rest.next().map(String::as_str),
            w => w.strip_prefix("--jobs=").or_else(|| w.strip_prefix("-j")),
        };
        if let Some(n) = value.and_then(|v| v.parse().ok()) {
            jobs = Some(n);
        }
    }
    jobs
}

/// A Cargo command with the user's parallelism replaced by `CARGO_BUILD_JOBS=jobs` in an
/// `env` prefix (merged into an existing one). Anything that is not a plain `cargo` or
/// `env NAME=value... cargo` command is returned unchanged.
pub fn apply_jobs(argv: &[String], jobs: u64) -> Vec<String> {
    if cargo_index(argv).is_none() {
        return argv.to_vec();
    }
    let stripped = strip_jobs(argv);
    let assignment = format!("CARGO_BUILD_JOBS={jobs}");
    let mut out = Vec::with_capacity(stripped.len() + 2);
    match stripped.first().and_then(|w| base(w)) {
        Some("env") => {
            out.push(stripped[0].clone());
            let mut rest = stripped[1..].iter().peekable();
            if rest.peek().is_some_and(|w| *w == "--") {
                out.push("--".into());
                rest.next();
            }
            out.push(assignment);
            out.extend(rest.cloned());
        }
        _ => {
            out.push("env".into());
            out.push(assignment);
            out.extend(stripped);
        }
    }
    out
}

/// Whether `argv` is a Cargo command placement rewrites.
pub fn is_cargo(argv: &[String]) -> bool {
    cargo_index(argv).is_some()
}

/// Cargo subcommands whose result is the log and exit status, not a file the submitter uses
/// next: `tasks run` places these anywhere by default once a worker peer is configured.
/// `build`, `run`, scripts and everything else stay on this machine unless the submitter
/// asks (`--anywhere`, `--peer`), because their outputs would stay on the machine that built
/// them. Interim: moves to the policy file's `workflows::`/`tools::`.
pub const PLACED_BY_DEFAULT: &[&str] = &["check", "test", "clippy", "nextest"];

/// The first word after Cargo that is not `+toolchain` or a flag is the subcommand. A
/// global flag with a value (`--color always test`) yields the value, which matches no
/// subcommand and so stays local.
pub fn placed_by_default(argv: &[String]) -> bool {
    if gradle_tasks(argv).is_some_and(|t| !t.is_empty() && t.iter().all(|t| !makes_artifact(t))) {
        return true;
    }
    let Some(cargo) = cargo_index(argv) else {
        return false;
    };
    argv[cargo + 1..]
        .iter()
        .find(|w| !w.starts_with('-') && !w.starts_with('+'))
        .is_some_and(|sub| PLACED_BY_DEFAULT.contains(&sub.as_str()))
}

/// Task names of a direct `gradlew`/`gradle` command (the last `:`-segment of each, so
/// `:app:compileDebugKotlin` is `compileDebugKotlin`); None for any other command. Options and
/// the values of `-p`/`--project-dir`/`-c` are skipped.
pub fn gradle_tasks(argv: &[String]) -> Option<Vec<String>> {
    let first = base(argv.first()?)?;
    if first != "gradlew" && first != "gradle" {
        return None;
    }
    let mut tasks = vec![];
    let mut words = argv[1..].iter();
    while let Some(w) = words.next() {
        if matches!(
            w.as_str(),
            "-p" | "--project-dir" | "-c" | "--settings-file"
        ) {
            words.next();
        } else if !w.starts_with('-') {
            tasks.push(w.rsplit(':').next().unwrap_or(w).to_string());
        }
    }
    Some(tasks)
}

/// Tasks whose product is a file the submitter wants (an APK, a bundle, an install): placed
/// elsewhere, that output would stay on the worker unless the submission fetches it.
fn makes_artifact(task: &str) -> bool {
    [
        "assemble", "bundle", "install", "build", "package", "publish", "export",
    ]
    .iter()
    .any(|p| task.starts_with(p))
}

/// A gradle command that builds an artifact (so it is not placed by default).
pub fn gradle_builds_artifact(argv: &[String]) -> bool {
    gradle_tasks(argv).is_some_and(|t| t.iter().any(|t| makes_artifact(t)))
}

fn waiting(phase: &Phase) -> bool {
    matches!(
        phase,
        Phase::Queued | Phase::Approved | Phase::Preparing | Phase::WaitingBuild
    )
}

/// Share of `snapshot`'s compile slots in use (0 = all free): the tie-break between machines
/// that would finish at the same time, so an idle laptop is used before a busy worker.
fn compile_load(snapshot: &Snapshot) -> f64 {
    let compile = || {
        snapshot
            .machine
            .slots
            .iter()
            .filter(|s| s.kind == SlotKind::Compile)
    };
    compile().filter(|s| s.busy).count() as f64 / compile().count().max(1) as f64
}

/// Seconds until `snapshot`'s compile slot is free for a new job, and how that was found:
/// the holder's remaining ETA plus each compile job queued for the slot (jobs awaiting
/// approval have no queue position and are not ahead), plus [`BLOCKED_S`] under a blocker.
pub fn compile_wait(snapshot: &Snapshot) -> (f64, String) {
    let eta = |id: i64| {
        snapshot
            .jobs
            .iter()
            .find(|j| j.id == id)
            .and_then(|j| j.estimate.as_ref())
            .map(|e| e.remaining_s.max(0.0))
    };
    let mut parts = Vec::new();
    // Holders: busy compile slots, plus a building compile job the slot scan missed.
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
    let slots = snapshot
        .machine
        .slots
        .iter()
        .filter(|s| s.kind == SlotKind::Compile)
        .count()
        .max(1);
    // With every compile slot held the soonest holder to finish decides when one frees; a
    // free slot means no wait on holders.
    let mut remaining_s: Vec<f64> = holders
        .iter()
        .map(|(_, r)| r.unwrap_or(UNKNOWN_S))
        .collect();
    remaining_s.sort_by(f64::total_cmp);
    let mut wait = if holders.len() >= slots {
        remaining_s[holders.len() - slots]
    } else {
        0.0
    };
    for (id, remaining) in &holders {
        parts.push(match (id, remaining) {
            (Some(id), Some(r)) => format!("slot held by #{id}, ~{} left", duration(*r)),
            (Some(id), None) => format!("slot held by #{id}, ETA unknown"),
            (None, _) => "slot held, ETA unknown".to_string(),
        });
    }
    let mut queued = 0;
    for job in &snapshot.jobs {
        if job.resource == SlotKind::Compile
            && waiting(&job.phase)
            && job.queue.as_ref().is_some_and(|q| q.position.is_some())
        {
            wait += job.estimate.as_ref().map_or(UNKNOWN_S, |e| e.total_s);
            queued += 1;
        }
    }
    if queued > 0 {
        parts.push(format!("{queued} queued ahead"));
    }
    if let Some(blocker) = &snapshot.machine.blocker {
        wait += BLOCKED_S;
        parts.push(format!("blocked: {blocker} (+{})", duration(BLOCKED_S)));
    }
    if parts.is_empty() {
        parts.push("compile slot free".into());
    }
    (wait, parts.join(", "))
}

/// Local offload jobs still waiting for a local offload slot are ahead of a forwarded job
/// too, and with every local offload slot held a new bridge waits for one.
pub fn offload_backlog(local: &Snapshot) -> (f64, Option<String>) {
    let ahead: Vec<f64> = local
        .jobs
        .iter()
        .filter(|j| j.resource == SlotKind::Offload && waiting(&j.phase))
        .filter(|j| j.queue.as_ref().is_some_and(|q| q.position.is_some()))
        .map(|j| j.estimate.as_ref().map_or(UNKNOWN_S, |e| e.total_s))
        .collect();
    let mut wait: f64 = ahead.iter().sum();
    let slots: Vec<_> = local
        .machine
        .slots
        .iter()
        .filter(|s| s.kind == SlotKind::Offload)
        .collect();
    let full = !slots.is_empty() && slots.iter().all(|s| s.busy);
    if full {
        wait += UNKNOWN_S;
    }
    let text = match (ahead.len(), full) {
        (0, false) => None,
        (n, false) => Some(format!("{n} local offloads queued")),
        (0, true) => Some("all local offload slots busy".into()),
        (n, true) => Some(format!(
            "{n} local offloads queued, all local offload slots busy"
        )),
    };
    (wait, text)
}

pub fn median(mut values: Vec<f64>) -> Option<f64> {
    values.retain(|v| v.is_finite() && *v > 0.0);
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    })
}

/// Median wall time of passed jobs of `kind` in a snapshot's job list.
pub fn history(snapshot: &Snapshot, kind: &str) -> Option<(f64, usize)> {
    let walls: Vec<f64> = snapshot
        .jobs
        .iter()
        .filter(|j| j.kind_key == kind && j.phase == Phase::Passed)
        .filter_map(|j| j.result.as_ref()?.wall_s)
        .collect();
    let n = walls.len();
    median(walls).map(|m| (m, n))
}

struct Candidate<'a> {
    target: Target,
    machine: String,
    snapshot: &'a Snapshot,
    jobs: Option<u64>,
}

/// Decides where `argv` (of admission class `class`) runs. `local_jobs` is this machine's
/// Cargo jobs cap when its snapshot does not carry one. `peers` are the configured
/// worker/both peers with the runtime's latest view of each (empty: no peer link).
///
/// Errors only for an explicit peer that cannot take the job (unknown, wrong role, offline):
/// nothing should be submitted then.
pub fn decide(
    argv: &[String],
    class: &Class,
    request: &Request,
    local: &Snapshot,
    local_jobs: u64,
    peers: &[Peer],
) -> Result<Decision, String> {
    let local_name = local.machine.id.clone();
    let local_cap = local.machine.cargo_jobs_max.unwrap_or(local_jobs);
    let cap_of = |p: &Peer| p.snapshot.as_ref().and_then(|s| s.machine.cargo_jobs_max);
    let mut decision = Decision {
        requested: request.text(),
        target: Target::Local,
        machine: local_name.clone(),
        machine_id: Some(local_name.clone()),
        reason: String::new(),
        cargo_jobs: None,
        argv: argv.to_vec(),
        scores: vec![],
        skipped: vec![],
        notes: vec![],
    };
    let cargo = is_cargo(argv);
    // The chosen machine's cap goes into a Cargo command (peers involved only).
    let finish = |mut d: Decision, cap: Option<u64>| -> Decision {
        if cargo {
            match cap {
                Some(jobs) => {
                    d.argv = apply_jobs(argv, jobs);
                    d.cargo_jobs = Some(jobs);
                }
                None => d.notes.push(format!(
                    "{}'s Cargo jobs cap is unknown (its snapshot predates cargo_jobs_max); \
                     the command is forwarded unchanged and validated there",
                    d.machine
                )),
            }
        }
        d
    };
    match request {
        Request::Local => {
            decision.reason = "local requested".into();
            return Ok(decision);
        }
        Request::Peer(name) => {
            let Some(peer) = peers.iter().find(|p| &p.name == name) else {
                let known: Vec<_> = peers
                    .iter()
                    .map(|p| format!("{} ({})", p.name, p.role))
                    .collect();
                return Err(format!(
                    "unknown peer {name:?}; worker/both peers configured: {}",
                    if known.is_empty() {
                        "none".into()
                    } else {
                        known.join(", ")
                    }
                ));
            };
            if !peer.runs_jobs() {
                return Err(format!(
                    "peer {name} has role {}; only a worker/both peer runs forwarded jobs",
                    peer.role
                ));
            }
            if !peer.online() && peer.state != crate::peer_link::STREAM_CONNECTING {
                return Err(format!(
                    "peer {name} is offline ({}); nothing was submitted. Retry later, or use \
                     placement \"anywhere\" / \"local\"",
                    peer.error.as_deref().unwrap_or("no status stream")
                ));
            }
            if peer.state == crate::peer_link::STREAM_CONNECTING {
                decision
                    .notes
                    .push(format!("no status from {name} yet; forwarding anyway"));
            }
            decision.target = Target::Peer(name.clone());
            decision.machine = name.clone();
            decision.machine_id = peer.snapshot.as_ref().map(|s| s.machine.id.clone());
            decision.reason = format!("peer {name} named explicitly");
            if let Some(m) = crate::health::capability::blocking(&peer.missing, argv, None) {
                decision.notes.push(format!(
                    "{name} {} ({}); forwarding anyway because it was named",
                    m.lacks(),
                    m.evidence()
                ));
            }
            let cap = cap_of(peer);
            return Ok(finish(decision, cap));
        }
        Request::Anywhere => {}
    }
    if peers.is_empty() {
        decision.reason = "no worker peers configured; same as local".into();
        return Ok(decision);
    }
    match class {
        Class::Device(_) => {
            decision.reason = "the device class always stays local".into();
            return Ok(decision);
        }
        Class::Run => {
            decision.reason =
                "the run class stays local unless a peer is named (it needs this machine's build)"
                    .into();
            return Ok(decision);
        }
        Class::Compile => {}
    }
    let mut candidates = vec![Candidate {
        target: Target::Local,
        machine: local_name.clone(),
        snapshot: local,
        jobs: Some(local_cap),
    }];
    for peer in peers {
        let lacks = crate::health::capability::blocking(&peer.missing, argv, None)
            .map(|m| format!("{} ({}); this command needs it", m.lacks(), m.evidence()));
        match (peer.unusable().or(lacks), &peer.snapshot) {
            (None, Some(snapshot)) => candidates.push(Candidate {
                target: Target::Peer(peer.name.clone()),
                machine: peer.name.clone(),
                snapshot,
                jobs: cap_of(peer),
            }),
            (why, _) => {
                let why = why.unwrap_or_else(|| "no snapshot".into());
                decision.skipped.push(Skipped {
                    machine: peer.name.clone(),
                    why,
                });
            }
        }
    }
    let kind = normalize(&strip_jobs(argv)).kind_key;
    let offload_kind = format!("{}{kind}", crate::metrics::command::OFFLOAD_MARKER);
    // Each candidate's own history: a peer's runs, else this machine's forwarded runs of it
    // (their wall time includes sync/fetch: a fair estimate for a forward).
    let own: Vec<Option<(f64, String)>> = candidates
        .iter()
        .map(|c| match c.target {
            Target::Local => {
                history(c.snapshot, &kind).map(|(m, n)| (m, format!("median of {n} runs here")))
            }
            Target::Peer(_) => history(c.snapshot, &kind)
                .map(|(m, n)| (m, format!("median of {n} runs there")))
                .or_else(|| {
                    history(local, &offload_kind)
                        .map(|(m, n)| (m, format!("median of {n} forwarded runs")))
                }),
        })
        .collect();
    let (backlog, backlog_text) = offload_backlog(local);
    for (i, c) in candidates.iter().enumerate() {
        let (run_s, run_basis) = match &own[i] {
            Some((m, basis)) => (*m, basis.clone()),
            None => match own
                .iter()
                .enumerate()
                .find_map(|(j, o)| o.as_ref().map(|o| (j, o)))
            {
                Some((j, (m, _))) => {
                    let from = candidates[j].jobs.unwrap_or(DEFAULT_JOBS) as f64;
                    let to = c.jobs.unwrap_or(DEFAULT_JOBS) as f64;
                    (
                        m * from / to,
                        format!("{}'s median x{from}/{to} jobs", candidates[j].machine),
                    )
                }
                // Nothing known anywhere: equal guesses, so the wait decides.
                None => (UNKNOWN_S, "no history".to_string()),
            },
        };
        let (mut wait_s, mut wait_basis) = compile_wait(c.snapshot);
        if matches!(c.target, Target::Peer(_))
            && let Some(text) = &backlog_text
        {
            wait_s += backlog;
            wait_basis.push_str(&format!(", {text}"));
        }
        decision.scores.push(Score {
            machine: c.machine.clone(),
            target: c.target.clone(),
            wait_s,
            wait_basis,
            run_s,
            run_basis,
            total_s: wait_s + run_s,
            cargo_jobs: c.jobs,
        });
    }
    // Lowest total; on a tie the machine with more compile slots free, then a peer (worker)
    // over this machine, then config order.
    let load: Vec<f64> = candidates
        .iter()
        .map(|c| compile_load(c.snapshot))
        .collect();
    let best = decision
        .scores
        .iter()
        .enumerate()
        .min_by(|(i, a), (j, b)| {
            a.total_s
                .total_cmp(&b.total_s)
                .then_with(|| load[*i].total_cmp(&load[*j]))
                .then_with(|| {
                    let local = |s: &Score| matches!(s.target, Target::Local);
                    local(a).cmp(&local(b))
                })
                .then(i.cmp(j))
        })
        .map(|(i, _)| i)
        .unwrap_or(0);
    let chosen = decision.scores[best].clone();
    let tie = decision
        .scores
        .iter()
        .enumerate()
        .any(|(i, s)| i != best && s.total_s == chosen.total_s);
    decision.target = chosen.target.clone();
    decision.machine = chosen.machine.clone();
    decision.machine_id = Some(candidates[best].snapshot.machine.id.clone());
    decision.reason = match (&chosen.target, decision.scores.len(), tie) {
        (Target::Local, 1, _) => {
            let why = decision
                .skipped
                .iter()
                .map(|s| format!("{} {}", s.machine, s.why))
                .collect::<Vec<_>>()
                .join("; ");
            format!("no usable worker peer ({why}); running here")
        }
        (_, _, true) if load.iter().any(|l| *l != load[best]) => {
            format!("tie; {} has more compile slots free", chosen.machine)
        }
        (Target::Peer(_), _, true) => format!("tie; {} (worker) preferred", chosen.machine),
        _ => format!("{} sooner", chosen.machine),
    };
    if decision.scores.len() == 1 {
        for s in &decision.skipped {
            decision.notes.push(format!(
                "{} skipped: {}; fell back to local",
                s.machine, s.why
            ));
        }
    }
    let cap = candidates[best].jobs;
    Ok(finish(decision, cap))
}

/// What placement reads from the daemon: this machine's snapshot (only when peers are
/// involved) and the peer runtime's view of every worker/both peer.
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    pub local: Option<Snapshot>,
    pub local_jobs: u64,
    pub peers: Vec<Peer>,
    /// Why there are no peers to consider (no config, link disabled, old daemon).
    pub no_peers: Option<String>,
}

/// Is a peer config visible to this process (`TASKS_PEER_CONFIG`, else the XDG/home path)?
/// The daemon starts its peer runtime only with one; without it placement never asks.
pub fn peer_config_present() -> bool {
    task_manager_peer::PeerConfig::default_path().is_some_and(|p| p.exists())
}

/// Reads the inputs from the daemon at `socket`. A daemon that just started may still be
/// binding its endpoint or connecting to its peers: this waits up to `settle` for that.
#[cfg(unix)]
pub fn gather(socket: &Path, settle: std::time::Duration) -> crate::Result<Inputs> {
    use std::time::{Duration, Instant};
    let mut inputs = Inputs {
        local_jobs: crate::admission::submission_max_cargo_jobs().unwrap_or(DEFAULT_JOBS),
        ..Default::default()
    };
    if !peer_config_present() {
        inputs.no_peers = Some("no peer config".into());
        return Ok(inputs);
    }
    let end = Instant::now() + settle;
    let report = loop {
        let report = match crate::peer_link::request_peers(socket, Duration::from_secs(30)) {
            Ok(r) => r,
            Err(e) => {
                inputs.no_peers = Some(format!("peer link unavailable: {e}"));
                return Ok(inputs);
            }
        };
        let starting = !report.enabled && report.error.is_none();
        let connecting = report
            .snapshots
            .iter()
            .any(|s| s.state == crate::peer_link::STREAM_CONNECTING);
        if !(starting || connecting) || Instant::now() >= end {
            break report;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if !report.enabled {
        inputs.no_peers = Some(format!(
            "peer link not enabled in the daemon ({})",
            report.error.unwrap_or_else(|| "still starting".into())
        ));
        return Ok(inputs);
    }
    // Configured worker/both peers; ones without a status stream (no addr) are listed too,
    // so an explicit name gets a precise error.
    for view in report
        .peers
        .iter()
        .filter(|p| matches!(p.role.as_str(), "worker" | "both"))
    {
        let peer = match report.snapshots.iter().find(|s| s.name == view.name) {
            Some(s) => Peer::from_report(s),
            None => Peer {
                name: view.name.clone(),
                role: view.role.clone(),
                state: crate::peer_link::STREAM_OFFLINE.into(),
                error: Some("no address configured (or the daemon predates peer snapshots)".into()),
                stale: true,
                ..Default::default()
            },
        };
        inputs.peers.push(peer);
    }
    let local_marks = crate::health::capability::Marks::load(&crate::state_dir()).marks;
    for peer in &mut inputs.peers {
        peer.missing = marks_for(peer, &local_marks);
    }
    if inputs.peers.is_empty() {
        inputs.no_peers = Some("no worker/both peer configured".into());
        return Ok(inputs);
    }
    let reply = crate::request_status(socket, Duration::from_secs(60))?;
    inputs.local = Some(reply.status.ok_or_else(|| {
        reply
            .error
            .unwrap_or_else(|| "daemon returned no status".into())
    })?);
    Ok(inputs)
}

/// Decides with gathered inputs. Without peers this is `local` (no status read, no rewrite).
pub fn decide_with(
    argv: &[String],
    class: &Class,
    request: &Request,
    inputs: &Inputs,
) -> Result<Decision, String> {
    match (&inputs.local, request) {
        (Some(local), _) => decide(
            argv,
            class,
            request,
            local,
            inputs.local_jobs,
            &inputs.peers,
        ),
        (None, Request::Peer(name)) => Err(format!(
            "cannot forward to peer {name:?}: {}",
            inputs.no_peers.as_deref().unwrap_or("no peers")
        )),
        (None, _) => Ok(Decision {
            requested: request.text(),
            target: Target::Local,
            machine: "local".into(),
            machine_id: None,
            reason: match request {
                Request::Local => "local requested".into(),
                _ => format!(
                    "{}; same as local",
                    inputs.no_peers.as_deref().unwrap_or("no peers")
                ),
            },
            cargo_jobs: None,
            argv: argv.to_vec(),
            scores: vec![],
            skipped: vec![],
            notes: vec![],
        }),
    }
}

/// The job to submit for a decision: a validated local live command, or the local
/// forwarding job of `peer_link::forwarded_spec` (the peer validates the inner command).
/// `fetch` is ignored when the job stays here (its outputs are already here).
// The submission's parts as `tasks run` has them; grouping them would only move the list.
#[allow(clippy::too_many_arguments)]
pub fn job_spec(
    decision: &Decision,
    resource: crate::local_command::Resource,
    broad_test: bool,
    owner: String,
    cwd: std::path::PathBuf,
    fetch: &[String],
    state: &Path,
    exe: &Path,
) -> crate::Result<crate::JobSpec> {
    let recipe = crate::local_command::Recipe {
        broad_test,
        argv: decision.argv.clone(),
        resource,
    };
    match decision.peer() {
        None => recipe.spec(owner, cwd),
        Some(peer) => crate::peer_link::forwarded_spec(
            &recipe.unchecked_spec(owner, cwd)?,
            peer,
            fetch,
            state,
            exe,
        ),
    }
}

/// `<state>/placement/<job>.json`: the decision behind a submitted job, so `job` can say
/// where it went and why (small, written once, never pruned).
pub fn record_path(state: &Path, job: i64) -> std::path::PathBuf {
    state.join("placement").join(format!("{job}.json"))
}

pub fn record(state: &Path, job: i64, decision: &Decision) -> std::io::Result<()> {
    crate::peer::records::write_json(&record_path(state, job), decision)
}

pub fn recorded(state: &Path, job: i64) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(record_path(state, job)).ok()?).ok()
}

/// One log line for the job's recorded decision: where it ran, why, and the Cargo jobs it was
/// given, so an agent reading only the job log can tell why `--anywhere` stayed here.
pub fn recorded_line(state: &Path, job: i64) -> Option<String> {
    let r = recorded(state, job)?;
    let text = |key: &str| r[key].as_str().map(str::to_owned);
    let mut out = format!(
        "tasks: placement {} -> {}: {}",
        text("requested")?,
        text("machine")?,
        text("reason")?
    );
    if let Some(jobs) = r["cargo_jobs"].as_u64() {
        out.push_str(&format!("; CARGO_BUILD_JOBS={jobs}"));
    }
    for s in r["scores"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "\ntasks:   {}: wait {:.0}s [{}]",
            s["machine"].as_str().unwrap_or("?"),
            s["wait_s"].as_f64().unwrap_or(0.0),
            s["wait_basis"].as_str().unwrap_or("")
        ));
    }
    Some(out + "\n")
}

#[cfg(test)]
#[path = "placement_tests.rs"]
mod tests;
