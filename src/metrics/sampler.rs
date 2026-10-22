//! Daemon-side sampler: every 2 s, one pass over /proc/stat, /proc/meminfo,
//! PSI, /proc/locks, the admission lock directory and (only while jobs run)
//! one scan of /proc/PID/stat shared by all jobs. In memory only; the single
//! write is one `Execution::metrics` merge per finished job.
use super::history::{self, History};
use super::procfs::{self, CpuTimes, ProcStat};
use super::ring::Ring;
use super::slots::{self, Scan};
use super::snapshot::{self, Blockers, JobExtras};
use super::warmth::{self, CompileCounter, StartWarmth};
use super::{
    HISTORY_POINTS, JobMetrics, Live, LiveHistory, Machine, MachineHistory, Pressure, Role,
    SAMPLE_INTERVAL_S, Snapshot, Warmth,
};
use crate::execution::ProcessIdentity;
use crate::{Job, Phase, Result, Store, admission};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Per-tick read bound of one job log; the rest is read on later ticks.
const LOG_READ_PER_TICK: u64 = 1024 * 1024;
/// Per-stage resource usage written by `__command` (`jobs/ID/STAGE-usage.json`).
pub const USAGE_SUFFIX: &str = "-usage.json";

pub use task_manager_platform::ChildUsage as StageUsage;

/// getrusage(RUSAGE_CHILDREN) of the calling process: waited-for descendants.
pub fn children_usage() -> StageUsage {
    task_manager_platform::children_usage().unwrap_or_default()
}

/// Usage of one command: CPU as the difference around its wait; max RSS as
/// reported after it (earlier children of `__command` are tiny helpers).
pub fn usage_delta(before: &StageUsage, after: &StageUsage) -> StageUsage {
    StageUsage {
        cpu_user_s: (after.cpu_user_s - before.cpu_user_s).max(0.0),
        cpu_sys_s: (after.cpu_sys_s - before.cpu_sys_s).max(0.0),
        max_rss_bytes: after.max_rss_bytes,
    }
}

struct JobLive {
    supervisor: ProcessIdentity,
    prev: Option<(u64, Instant)>,
    cpu: Ring<f64>,
    rss: Ring<u64>,
    last_cpu: Option<f64>,
    last_rss: u64,
    processes: u32,
    peak_rss: u64,
    log_offset: u64,
    counter: CompileCounter,
    /// The daemon's own start check, only for jobs whose worker wrote none.
    start: Option<StartWarmth>,
    /// Group members that are not descendants, from the last full walk.
    orphans: Vec<u32>,
    /// Compiler/build-tool processes at the last sample (live compile attribution).
    compiling: Vec<super::compile::Compiling>,
    /// Per compiler process: (start ticks, cpu ticks, when) of the previous sample.
    proc_cpu: HashMap<u32, (u64, u64, Instant)>,
    /// Distinct crates seen compiled as `rustc --test` (bounded).
    test_crates: std::collections::HashSet<String>,
}

/// What a finished job's sampled state contributes to its persisted metrics.
#[derive(Clone, Debug, Default)]
struct Pending {
    peak: u64,
    start: Option<StartWarmth>,
    test_crates: u64,
}

struct State {
    hostname: String,
    role: Role,
    logical_cpus: u32,
    prev_cpu: Option<CpuTimes>,
    cpu_busy: Option<f64>,
    mem: Option<procfs::MemInfo>,
    pressure: Option<Pressure>,
    blockers: Blockers,
    /// Admission's effective Cargo jobs cap (None while thresholds are invalid).
    cargo_jobs_max: Option<u64>,
    scan: Scan,
    cpu_hist: Ring<f64>,
    mem_hist: Ring<u64>,
    jobs: HashMap<i64, JobLive>,
    /// Highest job id when this daemon first read the ledger. Terminal jobs
    /// above it ran under this daemon and are always finalized; older ones
    /// only when a stage usage file shows a metrics-aware worker ran them.
    startup_max_id: Option<i64>,
    /// Terminal jobs already considered (finalized or not eligible).
    considered: std::collections::HashSet<i64>,
    /// Sampled peak/start of jobs that left the active set, until finalized.
    pending: HashMap<i64, Pending>,
    /// Ticks that sampled running jobs (drives the periodic full walk).
    job_ticks: u64,
    /// False once the kernel lacked `/proc/PID/task/*/children`.
    children_files: bool,
}

#[derive(Clone)]
pub struct Sampler {
    state_dir: PathBuf,
    state: Arc<Mutex<State>>,
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// `TASKS_MACHINE_ROLE=host|worker`, else `worker` when `~/offload/checkouts`
/// exists (the tasks-offload landing directory), else `host`.
pub fn role() -> Role {
    match std::env::var("TASKS_MACHINE_ROLE").as_deref() {
        Ok("worker") => return Role::Worker,
        Ok("host") => return Role::Host,
        _ => (),
    }
    let worker = std::env::var_os("HOME")
        .is_some_and(|home| Path::new(&home).join("offload/checkouts").is_dir());
    if worker { Role::Worker } else { Role::Host }
}

fn clock_ticks() -> f64 {
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 { t as f64 } else { 100.0 }
}
fn page_size() -> u64 {
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if p > 0 { p as u64 } else { 4096 }
}

/// All readable /proc/PID/stat records.
pub fn read_procs() -> Vec<ProcStat> {
    let mut out = vec![];
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some(stat) = procfs::parse_proc_stat(&text)
        {
            out.push(stat);
        }
    }
    out
}

/// Every FULL_WALK_EVERY-th tick reads all of /proc (to find orphans that
/// stayed in a job's process group); other ticks read only each job's tree.
pub const FULL_WALK_EVERY: u64 = 15;

fn read_stat(pid: u32) -> Option<ProcStat> {
    procfs::parse_proc_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Children of `pid` from `/proc/PID/task/*/children` (CONFIG_PROC_CHILDREN).
/// None when the kernel does not provide the file.
fn children_of(pid: u32) -> Option<Vec<u32>> {
    let mut out = vec![];
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    for task in tasks.flatten() {
        match std::fs::read_to_string(task.path().join("children")) {
            Ok(text) => out.extend(
                text.split_whitespace()
                    .filter_map(|c| c.parse::<u32>().ok()),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A thread that just exited, or no CONFIG_PROC_CHILDREN.
                if !task.path().exists() {
                    continue;
                }
                return None;
            }
            Err(_) => continue,
        }
    }
    Some(out)
}

/// Targeted read of one job's tree: the supervisor, its descendants (via the
/// children files) and previously found group orphans still in a job group.
/// None when the children files are unsupported (fall back to a full walk).
pub fn read_tree(supervisor: u32, orphans: &[u32]) -> Option<Vec<ProcStat>> {
    let mut out: Vec<ProcStat> = vec![];
    let mut stack = vec![supervisor];
    let mut seen = std::collections::HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) || seen.len() > 4096 {
            continue;
        }
        let Some(stat) = read_stat(pid) else { continue };
        stack.extend(children_of(pid)?);
        out.push(stat);
    }
    let groups: std::collections::HashSet<u32> = out.iter().map(|p| p.pgrp).collect();
    for &pid in orphans {
        if !seen.contains(&pid)
            && let Some(stat) = read_stat(pid).filter(|p| p.pgrp > 1 && groups.contains(&p.pgrp))
        {
            out.push(stat);
        }
    }
    Some(out)
}

/// Active ledger jobs with their phase and stage supervisor, if any.
fn active_jobs(store: &Store) -> Result<Vec<(i64, Phase, Option<ProcessIdentity>)>> {
    let mut statement = store.0.prepare(
        "SELECT id, phase FROM jobs WHERE phase IN
         ('preparing','waiting_build','building','waiting_run','running') ORDER BY id",
    )?;
    let rows = statement.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = vec![];
    for row in rows {
        let (id, phase) = row?;
        let supervisor = store.execution(id)?.and_then(|e| e.supervisor);
        out.push((id, Phase::from_db(&phase)?, supervisor));
    }
    Ok(out)
}

/// One job row without loading the whole ledger (`Store::job` lists all).
fn job_row(store: &Store, id: i64) -> Result<Job> {
    let (spec, phase, submitted_at): (String, String, i64) = store.0.query_row(
        "SELECT spec, phase, submitted_at FROM jobs WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(Job {
        id,
        spec: serde_json::from_str(&spec)?,
        phase: Phase::from_db(&phase)?,
        submitted_at,
        execution: store.execution(id)?,
    })
}

pub fn job_dir(state_dir: &Path, id: i64) -> PathBuf {
    state_dir.join("jobs").join(id.to_string())
}

fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > 64 * 1024 {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn start_warmth_file(state_dir: &Path, id: i64) -> Option<StartWarmth> {
    load(&job_dir(state_dir, id).join(warmth::START_FILE))
}

/// Sum of the stage usage files of a job; None when there are none.
pub fn stage_usage(state_dir: &Path, id: i64) -> Option<StageUsage> {
    let dir = job_dir(state_dir, id);
    let mut total: Option<StageUsage> = None;
    for stage in ["build", "run"] {
        if let Some(u) = load::<StageUsage>(&dir.join(format!("{stage}{USAGE_SUFFIX}"))) {
            let t = total.get_or_insert_with(StageUsage::default);
            t.cpu_user_s += u.cpu_user_s;
            t.cpu_sys_s += u.cpu_sys_s;
            t.max_rss_bytes = t.max_rss_bytes.max(u.max_rss_bytes);
        }
    }
    total
}

/// `Compiling ` lines and test targets of a job's whole log (bounded by the log cap).
pub fn log_counts(state_dir: &Path, id: i64) -> Option<CompileCounter> {
    let mut file = std::fs::File::open(job_dir(state_dir, id).join("job.log")).ok()?;
    let mut bytes = vec![];
    (&mut file)
        .take(16 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(CompileCounter::of_text(&bytes))
}

/// Count of `Compiling ` lines in a job's whole log (bounded by the log cap).
pub fn log_crates(state_dir: &Path, id: i64) -> Option<u64> {
    log_counts(state_dir, id).map(|c| c.count)
}

/// Adds the test-scope fields (feature/log-search) to a finished Cargo job's metrics:
/// test targets built = max(test binaries in the log, `rustc --test` crates seen live), and
/// the broad `cargo test` flag. Non-Cargo jobs keep both absent.
pub fn add_test_scope(
    metrics: &mut JobMetrics,
    job: &Job,
    log_test_targets: Option<u64>,
    observed_test_crates: u64,
) {
    let argv = history::argv(job);
    metrics.broad_test_build = super::testscope::broad_flag(&argv);
    if super::command::is_cargo(&argv) {
        metrics.test_targets_built = Some(log_test_targets.unwrap_or(0).max(observed_test_crates));
    }
}

/// Seconds since boot (`/proc/uptime`), for process ages.
fn uptime() -> Option<f64> {
    std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Live compile attribution for one job's members (see `metrics::compile`): reads the
/// cmdline only of members whose comm names a compiler or build tool. Updates the job's
/// per-process CPU baselines and its set of `--test` crates.
fn attribute_members(
    entry: &mut JobLive,
    members: &[ProcStat],
    now: Instant,
    uptime: Option<f64>,
    hz: f64,
    page: u64,
) {
    use super::compile::{self, Compiling};
    let mut out: Vec<Compiling> = vec![];
    let mut seen = std::collections::HashSet::new();
    for p in members {
        if !compile::interesting_comm(&p.comm) {
            continue;
        }
        let Some(argv) = compile::read_cmdline(p.pid) else {
            continue;
        };
        let Some(a) = compile::attribute(&p.comm, &argv) else {
            continue;
        };
        seen.insert(p.pid);
        let elapsed = uptime
            .map(|up| (up - p.start_ticks as f64 / hz).max(0.0))
            .unwrap_or(0.0);
        let cpu = match entry.proc_cpu.get(&p.pid) {
            Some(&(start, ticks, at)) if start == p.start_ticks => {
                let dt = now.duration_since(at).as_secs_f64().max(1e-3);
                Some((p.cpu_ticks.saturating_sub(ticks) as f64 / hz / dt).max(0.0))
            }
            _ if elapsed > 0.05 => Some(p.cpu_ticks as f64 / hz / elapsed),
            _ => None,
        };
        entry
            .proc_cpu
            .insert(p.pid, (p.start_ticks, p.cpu_ticks, now));
        if a.kind == "test" && entry.test_crates.len() < 4096 {
            entry.test_crates.insert(a.unit.clone());
        }
        out.push(Compiling {
            crate_name: Some(a.unit.clone()),
            unit: a.unit,
            kind: a.kind.into(),
            tool: "cargo".into(),
            proc_name: a.proc_name,
            pid: p.pid,
            elapsed_s: (elapsed * 10.0).round() / 10.0,
            cpu: cpu.map(|c| (c * 100.0).round() / 100.0),
            rss_bytes: p.rss_pages * page,
        });
    }
    entry.proc_cpu.retain(|pid, _| seen.contains(pid));
    out.sort_by(|a, b| b.elapsed_s.total_cmp(&a.elapsed_s).then(a.pid.cmp(&b.pid)));
    out.truncate(compile::MAX_PER_JOB);
    entry.compiling = out;
}

/// The finished-job record (`Execution::metrics`), from stage usage files,
/// the job log, the start warmth and the daemon's sampled peak RSS.
pub fn finish_metrics(
    job: &Job,
    history: &History,
    usage: Option<StageUsage>,
    crates: Option<u64>,
    start: Option<StartWarmth>,
    sampled_peak: Option<u64>,
) -> JobMetrics {
    let kind = history::names(job).1;
    let argv = history::argv(job);
    let cargo = super::command::is_cargo(&argv);
    let start = start.unwrap_or_else(|| StartWarmth {
        warmth: Warmth::Unknown,
        reason: "start state not recorded".into(),
    });
    let crates = if cargo { crates } else { None };
    let (warmth, reason) = match crates {
        Some(c) => warmth::refine(&start, c, history.reference_crates(&kind, job.id), true),
        None => (start.warmth, start.reason.clone()),
    };
    let peak = match (sampled_peak.filter(|&p| p > 0), usage.as_ref()) {
        (Some(p), Some(u)) => Some(p.max(u.max_rss_bytes)),
        (Some(p), None) => Some(p),
        (None, Some(u)) => Some(u.max_rss_bytes).filter(|&m| m > 0),
        (None, None) => None,
    };
    JobMetrics {
        warmth,
        warmth_reason: reason,
        wall_s: history::exec_seconds(job),
        cpu_user_s: usage.as_ref().map(|u| u.cpu_user_s),
        cpu_sys_s: usage.as_ref().map(|u| u.cpu_sys_s),
        peak_rss_bytes: peak,
        crates_compiled: crates,
        // Filled by `add_test_scope` (it needs the log's test binaries and live crates).
        test_targets_built: None,
        broad_test_build: None,
    }
}

/// Adds `metrics` to a TERMINAL job's execution JSON, keeping every other key
/// as stored (JSON-level merge, so fields unknown to this release survive).
/// Never overwrites existing metrics; the worker no longer writes a terminal
/// job, so this cannot race with completion.
pub fn write_metrics(store: &mut Store, id: i64, metrics: &JobMetrics) -> Result<bool> {
    use rusqlite::{OptionalExtension, TransactionBehavior, params};
    let tx = store
        .0
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let phase: Option<String> = tx
        .query_row("SELECT phase FROM jobs WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    if !phase.is_some_and(|p| Phase::from_db(&p).is_ok_and(|p| p.terminal())) {
        return Ok(false);
    }
    let data: Option<String> = tx
        .query_row("SELECT data FROM executions WHERE job_id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(data) = data else { return Ok(false) };
    let mut value: serde_json::Value = serde_json::from_str(&data)?;
    let Some(object) = value.as_object_mut() else {
        return Ok(false);
    };
    if object.get("metrics").is_some_and(|m| !m.is_null()) {
        return Ok(false);
    }
    object.insert("metrics".into(), serde_json::to_value(metrics)?);
    tx.execute(
        "UPDATE executions SET data=?2 WHERE job_id=?1",
        params![id, serde_json::to_string(&value)?],
    )?;
    tx.commit()?;
    Ok(true)
}

/// The `offload` object of a job forwarded to a peer, from the daemon's link record (live) or
/// the ledger link (`execution.peer`, finished). Other jobs: `None` (tasks-offload sidecars are
/// not read here).
fn peer_offload(state_dir: &Path, job: &Job) -> Option<super::types::OffloadLink> {
    let argv = crate::local_command::Recipe::from_spec_unvalidated(&job.spec)
        .ok()?
        .argv;
    if !crate::peer_link::is_forward_argv(&argv) {
        return None;
    }
    let path = state_dir
        .join("peer/links")
        .join(format!("{}.json", job.id));
    let link: serde_json::Value = std::fs::read(path)
        .ok()
        .filter(|b| b.len() <= 64 * 1024)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let peer = job.execution.as_ref().and_then(|e| e.peer.clone());
    let worker = link["peer"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| peer.as_ref().map(|p| p.peer.clone()))?;
    Some(super::types::OffloadLink {
        worker,
        remote_job_id: link["remote_job_id"]
            .as_i64()
            .or_else(|| peer.as_ref().and_then(|p| p.peer_job_id)),
        transport: "iroh".into(),
        sync_s: link["sync_s"]
            .as_f64()
            .or_else(|| peer.as_ref().and_then(|p| p.sync_s)),
        fetch_s: link["fetch_s"]
            .as_f64()
            .or_else(|| peer.as_ref().and_then(|p| p.fetch_s)),
    })
}

impl Sampler {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            state: Arc::new(Mutex::new(State {
                hostname: hostname(),
                role: role(),
                logical_cpus: std::thread::available_parallelism()
                    .map(|n| n.get() as u32)
                    .unwrap_or(1),
                prev_cpu: None,
                cpu_busy: None,
                mem: None,
                pressure: None,
                blockers: Blockers::default(),
                cargo_jobs_max: None,
                scan: Scan::default(),
                cpu_hist: Ring::new(HISTORY_POINTS),
                mem_hist: Ring::new(HISTORY_POINTS),
                jobs: HashMap::new(),
                startup_max_id: None,
                considered: Default::default(),
                pending: HashMap::new(),
                job_ticks: 0,
                children_files: true,
            })),
        }
    }

    /// Starts the 2-second sampling thread (daemon lifetime).
    pub fn start(state_dir: PathBuf) -> Self {
        let sampler = Self::new(state_dir);
        let worker = sampler.clone();
        std::thread::spawn(move || {
            let mut store = None;
            loop {
                let began = Instant::now();
                if store.is_none() {
                    store = Store::open(&worker.state_dir.join("tasks.sqlite")).ok();
                }
                if let Err(error) = worker.tick(store.as_mut()) {
                    eprintln!("metrics: sample failed: {error}");
                    store = None;
                }
                let interval = Duration::from_secs_f64(SAMPLE_INTERVAL_S);
                std::thread::sleep(interval.saturating_sub(began.elapsed()));
            }
        });
        sampler
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// One sample. Machine values are refreshed even when the ledger is not
    /// readable; job sampling then waits for the next tick.
    pub fn tick(&self, store: Option<&mut Store>) -> Result<()> {
        let stat = std::fs::read_to_string("/proc/stat").ok();
        let mem = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|t| procfs::parse_meminfo(&t));
        let pressure = admission::Pressure::read().ok();
        // The worker's recorded thresholds when present (what admission applies).
        let report = admission::report_thresholds(
            &self.state_dir,
            admission::thresholds(),
            admission::Thresholds::overridden(|n| std::env::var_os(n)),
        );
        let cargo_jobs_max = report
            .as_ref()
            .ok()
            .map(|(r, _)| r.effective.max_cargo_jobs);
        let blockers = match (&pressure, report) {
            (None, _) => Blockers::all("pressure (PSI) not readable; admission fails closed"),
            (_, Err(e)) => Blockers::all(&format!("invalid admission thresholds: {e}")),
            (Some(p), Ok((report, _))) => {
                let t = report.effective;
                Blockers {
                    compile: p.blocker_with(&t),
                    run: p.blocker_for(&t, &admission::Kind::Run),
                    offload: p.blocker_for(&t, &admission::Kind::Offload),
                }
            }
        };
        let lock_root = admission::report_lock_dir(
            &self.state_dir,
            admission::root(),
            std::env::var_os(admission::LOCK_DIR_ENV).is_some(),
        )
        .map(|(r, _)| r.path)
        .ok();
        let held = std::fs::read_to_string("/proc/locks")
            .map(|t| procfs::parse_flocks(&t))
            .unwrap_or_default();
        let scan = lock_root
            .map(|root| slots::scan(&root, &held))
            .unwrap_or_default();
        {
            let mut s = self.lock();
            if let Some((times, cpus)) = stat.as_deref().and_then(procfs::parse_stat) {
                if cpus > 0 {
                    s.logical_cpus = cpus;
                }
                s.cpu_busy = s
                    .prev_cpu
                    .and_then(|prev| procfs::busy_cores(prev, times, s.logical_cpus));
                s.prev_cpu = Some(times);
                if let Some(busy) = s.cpu_busy {
                    s.cpu_hist.push(busy);
                }
            }
            if let Some(m) = mem {
                s.mem_hist.push(m.total.saturating_sub(m.available));
            }
            s.mem = mem;
            s.pressure = pressure.map(|p| Pressure {
                cpu_some_avg10: p.cpu_some_avg10,
                memory_some_avg10: p.memory_some_avg10,
                memory_full_avg10: p.memory_full_avg10,
                io_some_avg10: p.io_some_avg10,
            });
            s.blockers = blockers;
            s.cargo_jobs_max = cargo_jobs_max;
            s.scan = scan;
        }
        let Some(store) = store else { return Ok(()) };
        self.sample_jobs(store)
    }

    fn sample_jobs(&self, store: &mut Store) -> Result<()> {
        let active = active_jobs(store)?;
        let running: Vec<_> = active.iter().filter(|(_, _, s)| s.is_some()).collect();
        let full_walk = !running.is_empty() && {
            let mut s = self.lock();
            s.job_ticks += 1;
            !s.children_files || s.job_ticks % FULL_WALK_EVERY == 1
        };
        let procs = if full_walk { read_procs() } else { vec![] };
        let (hz, page) = (clock_ticks(), page_size());
        let now = Instant::now();
        let up = if running.is_empty() { None } else { uptime() };
        let finished: Vec<(i64, Pending)>;
        {
            let mut s = self.lock();
            let mut unsupported = false;
            for (id, _, supervisor) in &active {
                let entry = s.jobs.entry(*id).or_insert_with(|| JobLive {
                    supervisor: supervisor.clone().unwrap_or(ProcessIdentity {
                        pid: 0,
                        start_ticks: 0,
                    }),
                    prev: None,
                    cpu: Ring::new(HISTORY_POINTS),
                    rss: Ring::new(HISTORY_POINTS),
                    last_cpu: None,
                    last_rss: 0,
                    processes: 0,
                    peak_rss: 0,
                    log_offset: 0,
                    counter: CompileCounter::default(),
                    start: None,
                    orphans: vec![],
                    compiling: vec![],
                    proc_cpu: HashMap::new(),
                    test_crates: Default::default(),
                });
                if let Some(sup) = supervisor
                    && entry.supervisor != *sup
                {
                    // A new stage: new supervisor tree, CPU baseline restarts.
                    entry.supervisor = sup.clone();
                    entry.prev = None;
                }
                let Some(sup) = supervisor else {
                    entry.last_cpu = None;
                    entry.processes = 0;
                    entry.last_rss = 0;
                    entry.compiling.clear();
                    continue;
                };
                let members: Vec<ProcStat> = if full_walk {
                    let members: Vec<ProcStat> = procfs::job_members(&procs, sup.pid)
                        .into_iter()
                        .map(|i| procs[i].clone())
                        .collect();
                    let pids: std::collections::HashSet<u32> =
                        members.iter().map(|p| p.pid).collect();
                    entry.orphans = members
                        .iter()
                        .filter(|p| p.pid != sup.pid && !pids.contains(&p.ppid))
                        .map(|p| p.pid)
                        .collect();
                    members
                } else {
                    match read_tree(sup.pid, &entry.orphans) {
                        Some(members) => members,
                        None => {
                            // No children files: full walks from the next tick on.
                            unsupported = true;
                            vec![]
                        }
                    }
                };
                let identity_ok = members
                    .iter()
                    .any(|p| p.pid == sup.pid && p.start_ticks == sup.start_ticks);
                if !identity_ok {
                    entry.processes = 0;
                    entry.last_rss = 0;
                    entry.last_cpu = None;
                    entry.compiling.clear();
                    continue;
                }
                attribute_members(entry, &members, now, up, hz, page);
                let ticks: u64 = members.iter().map(|p| p.cpu_ticks + p.child_ticks).sum();
                let rss: u64 = members.iter().map(|p| p.rss_pages * page).sum();
                entry.last_cpu = entry.prev.map(|(before, at)| {
                    let dt = now.duration_since(at).as_secs_f64().max(1e-3);
                    (ticks.saturating_sub(before) as f64 / hz / dt).max(0.0)
                });
                entry.prev = Some((ticks, now));
                entry.last_rss = rss;
                entry.processes = members.len() as u32;
                entry.peak_rss = entry.peak_rss.max(rss);
                if let Some(c) = entry.last_cpu {
                    entry.cpu.push(c);
                }
                entry.rss.push(rss);
            }
            if unsupported {
                s.children_files = false;
            }
            let gone: Vec<i64> = s
                .jobs
                .keys()
                .filter(|id| !active.iter().any(|(a, _, _)| a == *id))
                .copied()
                .collect();
            finished = gone
                .into_iter()
                .filter_map(|id| {
                    s.jobs.remove(&id).map(|j| {
                        (
                            id,
                            Pending {
                                peak: j.peak_rss,
                                start: j.start.clone(),
                                test_crates: j.test_crates.len() as u64,
                            },
                        )
                    })
                })
                .collect();
        }
        self.read_logs(&active);
        self.ensure_start_checks(store, &active)?;
        {
            let mut s = self.lock();
            for (id, pending) in finished {
                s.pending.insert(id, pending);
            }
        }
        self.finalize(store)
    }

    /// Feeds new job-log bytes to each active job's `Compiling` counter.
    fn read_logs(&self, active: &[(i64, Phase, Option<ProcessIdentity>)]) {
        for (id, _, _) in active {
            let offset = match self.lock().jobs.get(id) {
                Some(j) => j.log_offset,
                None => continue,
            };
            let path = job_dir(&self.state_dir, *id).join("job.log");
            let Ok(mut file) = std::fs::File::open(&path) else {
                continue;
            };
            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if len <= offset || file.seek(SeekFrom::Start(offset)).is_err() {
                continue;
            }
            let mut bytes = vec![];
            if (&mut file)
                .take(LOG_READ_PER_TICK)
                .read_to_end(&mut bytes)
                .is_err()
            {
                continue;
            }
            if let Some(j) = self.lock().jobs.get_mut(id) {
                j.counter.feed(&bytes);
                j.log_offset = offset + bytes.len() as u64;
            }
        }
    }

    /// For jobs whose worker did not record a start warmth (an older worker
    /// release), check once on first sight, before much can have compiled.
    fn ensure_start_checks(
        &self,
        store: &Store,
        active: &[(i64, Phase, Option<ProcessIdentity>)],
    ) -> Result<()> {
        for (id, _, _) in active {
            let needs = self.lock().jobs.get(id).is_some_and(|j| j.start.is_none())
                && start_warmth_file(&self.state_dir, *id).is_none();
            if !needs {
                continue;
            }
            let job = job_row(store, *id)?;
            let check = warmth::at_start(&history::argv(&job), Path::new(&job.spec.source));
            if let Some(j) = self.lock().jobs.get_mut(id) {
                j.start = Some(check);
            }
        }
        Ok(())
    }

    /// Persists `Execution::metrics` for terminal local commands that lack it:
    /// every job that finished while this daemon ran (seen active or not, so
    /// sub-tick jobs count too), and, once, the newest 50 older ones that ran
    /// under a metrics-aware worker (they have a stage usage file) but
    /// finished while no daemon sampled them. Pre-metrics history rows are
    /// never rewritten; they are read as backfill instead.
    pub(crate) fn finalize(&self, store: &mut Store) -> Result<()> {
        let startup_max = {
            let known = self.lock().startup_max_id;
            match known {
                Some(max) => max,
                None => {
                    let max: i64 =
                        store
                            .0
                            .query_row("SELECT COALESCE(MAX(id), 0) FROM jobs", [], |r| r.get(0))?;
                    self.lock().startup_max_id = Some(max);
                    max
                }
            }
        };
        let candidates: Vec<i64> = {
            let mut statement = store.0.prepare(
                "SELECT id FROM jobs WHERE id > ?1 AND phase IN
                 ('passed','failed','interrupted','cancelled') ORDER BY id",
            )?;
            let rows = statement.query_map([startup_max - 50], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut todo = vec![];
        for id in candidates {
            if !self.lock().considered.insert(id) {
                continue;
            }
            let eligible = store.execution(id)?.is_some_and(|e| e.metrics.is_none())
                && (id > startup_max || stage_usage(&self.state_dir, id).is_some());
            if eligible {
                todo.push(id);
            } else {
                self.lock().pending.remove(&id);
            }
        }
        if todo.is_empty() {
            return Ok(());
        }
        let jobs = store.jobs()?;
        let history = History::from_jobs(&jobs);
        for id in todo {
            let pending = self.lock().pending.remove(&id).unwrap_or_default();
            let Some(job) = jobs.iter().find(|j| j.id == id) else {
                continue;
            };
            if job.spec.suite != crate::local_command::SUITE {
                continue;
            }
            let counts = log_counts(&self.state_dir, id);
            let mut metrics = finish_metrics(
                job,
                &history,
                stage_usage(&self.state_dir, id),
                counts.as_ref().map(|c| c.count),
                start_warmth_file(&self.state_dir, id).or(pending.start),
                Some(pending.peak),
            );
            add_test_scope(
                &mut metrics,
                job,
                counts.as_ref().map(|c| c.test_targets()),
                pending.test_crates,
            );
            if let Err(error) = write_metrics(store, id, &metrics) {
                eprintln!("metrics: job {id}: could not record result: {error}");
            }
        }
        Ok(())
    }

    /// The local machine snapshot for `jobs` (the full ledger listing).
    pub fn snapshot(&self, jobs: &[Job]) -> Snapshot {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let selected = snapshot::select(jobs);
        let s = self.lock();
        let machine = Machine {
            id: s.hostname.clone(),
            role: s.role,
            logical_cpus: s.logical_cpus,
            cpu_busy_cores: s.cpu_busy,
            mem_total_bytes: s.mem.map(|m| m.total),
            mem_available_bytes: s.mem.map(|m| m.available),
            swap_total_bytes: s.mem.map(|m| m.swap_total),
            swap_used_bytes: s.mem.map(|m| m.swap_total.saturating_sub(m.swap_free)),
            pressure: s.pressure.clone(),
            blocker: s.blockers.compile.clone(),
            slots: s.scan.slots.clone(),
            history: MachineHistory {
                interval_s: SAMPLE_INTERVAL_S,
                cpu_busy_cores: s.cpu_hist.to_vec(),
                mem_used_bytes: s.mem_hist.to_vec(),
            },
            cargo_jobs_max: s.cargo_jobs_max,
        };
        let mut extras = HashMap::new();
        for job in &selected {
            let sampled = s.jobs.get(&job.id);
            let live = sampled
                .filter(|_| matches!(job.phase, Phase::Building | Phase::Running))
                .filter(|j| j.processes > 0)
                .map(|j| Live {
                    cpu_cores: j.last_cpu,
                    rss_bytes: j.last_rss,
                    processes: j.processes,
                    history: LiveHistory {
                        interval_s: SAMPLE_INTERVAL_S,
                        cpu_cores: j.cpu.to_vec(),
                        rss_bytes: j.rss.to_vec(),
                    },
                });
            let start = if job.phase.terminal() {
                None
            } else {
                start_warmth_file(&self.state_dir, job.id)
                    .or_else(|| sampled.and_then(|j| j.start.clone()))
            };
            extras.insert(
                job.id,
                JobExtras {
                    live,
                    crates_so_far: sampled.map(|j| j.counter.count),
                    units: super::units::read(&job_dir(&self.state_dir, job.id)),
                    test_targets_so_far: sampled
                        .map(|j| j.counter.test_targets().max(j.test_crates.len() as u64)),
                    compiling: sampled
                        .filter(|_| matches!(job.phase, Phase::Building | Phase::Running))
                        .filter(|j| j.processes > 0)
                        .map(|j| j.compiling.clone()),
                    start,
                    admitted_at: snapshot::admitted_at(&self.state_dir, job),
                    checkout: snapshot::checkout_of(&job.spec.source),
                    offload: peer_offload(&self.state_dir, job),
                },
            );
        }
        let scan = s.scan.clone();
        let blockers = s.blockers.clone();
        drop(s);
        let worker_running = crate::worker::running(&self.state_dir);
        let stopped = crate::worker::stopped_reason(&self.state_dir, now as i64);
        snapshot::assemble(snapshot::Inputs {
            now,
            machine,
            jobs: &selected,
            all_jobs: jobs,
            extras: &extras,
            scan: &scan,
            blockers: &blockers,
            worker_running,
            stopped,
        })
    }
}
