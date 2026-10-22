//! Explicit Linux local worker. The stage supervisor retains worker.lock while cleaning up
//! its own process group, even if the worker dies. No source is a security sandbox.
use crate::execution::{LogTail, ProcessIdentity, StageTiming, now};
use crate::ledger::{self, CancelProbe, CancelState};
use crate::{
    Phase, Result, Store, admission, android, android_build, cache, catalog, retention, snapshot,
    state_dir, workspace_snapshot,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_LOG: u64 = 8 * 1024 * 1024;
const TAIL: u64 = 32 * 1024;
/// A long log's start (command line, first errors, build plan) is kept with its tail.
const HEAD: u64 = 16 * 1024;
static STOP: AtomicBool = AtomicBool::new(false);
/// Whether a job thread's error is an infrastructure fault that makes the worker stop
/// claiming. A busy ledger or a lost stage supervisor belongs to one job (its record is
/// already final), so the worker keeps serving the others.
fn stops_claims(error: &(dyn std::error::Error + 'static)) -> bool {
    !(ledger::is_transient(error) || error.is::<SupervisorLost>())
}
#[derive(Debug)]
struct SupervisorLost(String);
impl std::fmt::Display for SupervisorLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SupervisorLost {}
extern "C" fn stop(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}
fn signals() {
    // Signal handlers only perform a lock-free atomic store.
    unsafe {
        libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
    }
}

pub fn identity(pid: u32) -> Result<ProcessIdentity> {
    Ok(task_manager_platform::identity(pid)?)
}
pub fn alive(process: &ProcessIdentity) -> bool {
    task_manager_platform::alive(process)
}
fn state() -> Result<PathBuf> {
    Ok(fs::canonicalize(state_dir())?)
}
fn job_dir(state: &Path, id: i64) -> PathBuf {
    state.join("jobs").join(id.to_string())
}
fn database(state: &Path) -> Result<Store> {
    Store::open(&state.join("tasks.sqlite"))
}
fn worker_lock(state: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join("worker.lock"))?)
}
/// Whether a worker (or its cleanup supervisor) holds `worker.lock`, read from /proc/locks.
/// Probing with a `try_lock` of our own would hold the lock for an instant, long enough for a
/// worker starting then to fail its own lock and exit.
pub fn running(state: &Path) -> bool {
    lock_held_by_anyone(&state.join("worker.lock"))
}

/// `<state>/execution-intent.json`: the last explicit `tasks start-worker` (enabled) or
/// `tasks stop-worker` (disabled). It survives daemon/machine restarts: a daemon that starts
/// with intent enabled and approved jobs waiting starts the worker itself, and a stopped queue
/// stays stopped until the next explicit start. Absent (older releases, fresh state) means
/// "never decided": workers claim as before, but nothing starts one automatically.
pub const INTENT_FILE: &str = "execution-intent.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecutionIntent {
    pub enabled: bool,
    pub changed_at: i64,
    /// `start-worker` or `stop-worker`.
    pub by: String,
}

pub fn intent(state: &Path) -> Option<ExecutionIntent> {
    let path = state.join(INTENT_FILE);
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > 4096 {
        return None;
    }
    load(&path).ok()
}

pub fn set_intent(state: &Path, enabled: bool, by: &str) -> Result<()> {
    if intent(state).is_some_and(|i| i.enabled == enabled && i.by == by) {
        return Ok(()); // unchanged: no rewrite/fsync per `tasks run`
    }
    save(
        &state.join(INTENT_FILE),
        &ExecutionIntent {
            enabled,
            changed_at: now(),
            by: by.into(),
        },
    )
}

/// True only after an explicit `tasks stop-worker` or `tasks drain` (not when no intent
/// was ever recorded).
pub fn stopped(state: &Path) -> bool {
    intent(state).is_some_and(|i| !i.enabled)
}

/// `by` of the intent `tasks drain` records: stopped until the daemon next starts.
pub const DRAIN: &str = "drain";

/// `tasks drain` is in effect: no claims, no worker start (not even from `tasks run`), and
/// the next daemon start resumes execution. This is what makes "drain, then restart" free of
/// the window a "no live jobs?" check followed by `systemctl restart` leaves.
pub fn draining(state: &Path) -> bool {
    intent(state).is_some_and(|i| !i.enabled && i.by == DRAIN)
}

/// Why approved jobs are not being claimed on purpose, for the queue text: who switched claiming
/// off, how long ago, and how to resume. None when claiming is not switched off.
pub fn stopped_reason(state: &Path, now: i64) -> Option<String> {
    let i = intent(state).filter(|i| !i.enabled)?;
    let ago = now.saturating_sub(i.changed_at).max(0);
    let ago = if ago >= 3600 {
        format!("{}h{:02}m", ago / 3600, ago % 3600 / 60)
    } else {
        format!("{}m", ago / 60)
    };
    Some(if i.by == DRAIN {
        format!(
            "operator drain for a daemon restart, since {ago} ago (tasks drain --cancel resumes)"
        )
    } else {
        format!(
            "claiming stopped by tasks stop-worker, since {ago} ago (tasks start-worker resumes)"
        )
    })
}

/// `tasks drain`: like stop-worker, but resumed by the next daemon start.
pub fn drain_requested() -> Result<()> {
    set_intent(&state()?, false, DRAIN)
}

/// `tasks start-worker`: records the enabled intent (first, so a worker it spawns never
/// sees a stale stop), then starts a worker if none runs. Returns whether one was spawned.
pub fn start_requested() -> Result<bool> {
    let state = state()?;
    if draining(&state) {
        // Approved jobs stay approved; the restarted daemon runs them (`tasks drain --cancel`
        // resumes without a restart).
        return Ok(false);
    }
    set_intent(&state, true, "start-worker")?;
    start_detached_inner()
}

/// `tasks drain --cancel`: resume now, without the restart.
pub fn drain_cancelled() -> Result<bool> {
    set_intent(&state()?, true, "start-worker")?;
    start_detached_inner()
}

/// `tasks stop-worker`: the worker claims no new jobs (running jobs finish normally, nothing
/// is cancelled) and exits when idle; no daemon starts one until `tasks start-worker`.
pub fn stop_requested() -> Result<()> {
    set_intent(&state()?, false, "stop-worker")
}

/// Daemon start: resume execution when it was enabled before the restart and approved jobs
/// wait. Returns a log line, or `None` when nothing applies.
pub fn resume_after_restart(state: &Path, approved: usize) -> Option<String> {
    let mut intent = intent(state)?;
    if !intent.enabled && intent.by == DRAIN {
        // The restart the drain was for has happened: execution resumes.
        if let Err(e) = set_intent(state, true, "drain-restart") {
            return Some(format!(
                "drained before this start, but could not resume: {e}"
            ));
        }
        intent.enabled = true;
    }
    if !intent.enabled {
        return Some(format!(
            "execution stays stopped (tasks stop-worker at unix {}); {approved} approved job(s) wait for tasks start-worker",
            intent.changed_at
        ));
    }
    if approved == 0 || running(state) {
        return None;
    }
    Some(match start_detached_inner() {
        Ok(_) => format!(
            "execution was enabled before this start (tasks start-worker at unix {}); worker started for {approved} approved job(s)",
            intent.changed_at
        ),
        Err(e) => {
            format!("execution was enabled before this start, but the worker did not start: {e}")
        }
    })
}

pub fn start_detached() -> Result<()> {
    start_detached_inner().map(|_| ())
}

fn start_detached_inner() -> Result<bool> {
    let state = state()?;
    // Report invalid TASKS_MIN_* configuration to the requesting client now.
    admission::thresholds()?;
    // Likewise an invalid TASKS_LOCK_DIR (fails closed; default /tmp/claude-$UID).
    admission::root()?;
    let lease = worker_lock(&state)?;
    if lease.try_lock().is_ok() {
        lease.unlock()?;
        admission::Pressure::read()?;
    }
    if running(&state) {
        return Ok(false);
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.join("worker.log"))?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("worker")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()?;
    std::thread::spawn(move || child.wait());
    // Let the reply's execution_enabled reflect the new worker (it takes worker.lock first).
    // Watched through /proc/locks only: probing with our own try_lock would make a worker
    // starting in that instant fail its lock and exit ("another worker ... is active").
    let until = Instant::now() + Duration::from_secs(2);
    while !lock_held_by_anyone(&state.join("worker.lock")) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(true)
}

/// Whether some process holds a FLOCK on `path`, from /proc/locks, without taking it.
pub fn lock_held_by_anyone(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    fs::read_to_string("/proc/locks").is_ok_and(|t| {
        crate::metrics::procfs::flock_held(&crate::metrics::procfs::parse_flocks(&t), &meta)
    })
}

pub fn logs(id: i64) -> Result<LogTail> {
    let state = state()?;
    database(&state)?.job(id)?;
    let path = job_dir(&state, id).join("job.log");
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LogTail {
                job_id: id,
                text: "No execution log yet".into(),
                truncated: false,
            });
        }
        Err(e) => return Err(e.into()),
    };
    let len = file.metadata()?.len();
    Ok(LogTail {
        job_id: id,
        text: head_and_tail(&mut file, len, HEAD, TAIL)?,
        truncated: len > HEAD + TAIL,
    })
}

/// Most important lines kept from an elided middle (the first and the last half of them
/// when there are more), each cut to `IMPORTANT_LINE` bytes: bounds the stored extra text.
const IMPORTANT_KEPT: usize = 200;
const IMPORTANT_LINE: usize = 300;

/// A line an agent needs even from the elided middle of a long log: a cargo test binary
/// (`Running`), its `test result:`, failures, panics and compiler errors.
fn important(line: &str) -> bool {
    let start = line.trim_start();
    start.starts_with("Running ")
        || start.starts_with("test result:")
        || start.starts_with("error[")
        || start.starts_with("error:")
        || line.contains(" FAILED")
        || line.contains("panicked at")
        || line.contains("BUILD FAILED")
}

/// The important lines of `middle` (line-aligned bytes): the first and last
/// `IMPORTANT_KEPT / 2` when there are more than `IMPORTANT_KEPT`, how many were found, and
/// how many lines the text has.
fn important_lines(middle: &[u8]) -> (Vec<String>, Vec<String>, usize, usize) {
    let half = IMPORTANT_KEPT / 2;
    let (mut first, mut last) = (Vec::new(), VecDeque::new());
    let (mut found, mut lines) = (0, 0);
    for raw in middle.split_inclusive(|b| *b == b'\n') {
        lines += 1;
        let text = String::from_utf8_lossy(raw);
        let line = text.trim_end_matches(['\n', '\r']);
        if !important(line) {
            continue;
        }
        found += 1;
        let mut end = line.len().min(IMPORTANT_LINE);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        let kept = line[..end].to_owned();
        if first.len() < half {
            first.push(kept);
        } else {
            if last.len() == half {
                last.pop_front();
            }
            last.push_back(kept);
        }
    }
    (first, last.into(), found, lines)
}

/// The whole text when it fits in `head + tail` bytes, else the first `head` and last `tail`
/// bytes (cut at line boundaries) around one marker line naming the omitted byte and line
/// counts, followed by the important lines of the omitted part (see [`important`]; at most
/// `IMPORTANT_KEPT` lines of `IMPORTANT_LINE` bytes, so the result stays within
/// `head + tail + IMPORTANT_KEPT * (IMPORTANT_LINE + 1)` plus the marker lines).
pub fn head_and_tail(file: &mut File, len: u64, head: u64, tail: u64) -> Result<String> {
    let mut read = |from: u64, n: u64| -> Result<Vec<u8>> {
        file.seek(SeekFrom::Start(from))?;
        let mut bytes = Vec::new();
        (&mut *file).take(n).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    if len <= head + tail {
        return Ok(String::from_utf8_lossy(&read(0, len)?).into_owned());
    }
    let mut first = read(0, head)?;
    if let Some(end) = first.iter().rposition(|b| *b == b'\n') {
        first.truncate(end + 1);
    }
    let mut last = read(len - tail, tail)?;
    if let Some(start) = last.iter().position(|b| *b == b'\n') {
        last.drain(..=start);
    }
    let omitted = len - first.len() as u64 - last.len() as u64;
    let middle = read(first.len() as u64, omitted)?;
    let (kept_first, kept_last, found, lines) = important_lines(&middle);
    let mut text = String::from_utf8_lossy(&first).into_owned();
    text.push_str(&format!(
        "\n[... tasks: {omitted} bytes of this log omitted here ({lines} lines; head and tail shown); the full retained log is the job's job.log ...]\n"
    ));
    if found > 0 {
        text.push_str(&format!(
            "[... tasks: {found} important lines (Running, test result:, FAILED, panicked at, error) of the omitted part follow ...]\n"
        ));
        for line in &kept_first {
            text.push_str(line);
            text.push('\n');
        }
        let skipped = found - kept_first.len() - kept_last.len();
        if skipped > 0 {
            text.push_str(&format!(
                "[... tasks: {skipped} more important lines omitted here (first {} and last {} kept) ...]\n",
                kept_first.len(),
                kept_last.len()
            ));
        }
        for line in &kept_last {
            text.push_str(line);
            text.push('\n');
        }
    }
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&last));
    Ok(text)
}

/// Appends to a job's log under the same size cap and lock as the worker's own notes
/// (the controller-side reconciliation note of an interrupted offload job).
pub(crate) fn append_job_log(state: &Path, id: i64, text: &[u8]) {
    let dir = job_dir(state, id);
    if dir.is_dir() {
        append_log(&dir, text);
    }
}

fn append_log(dir: &Path, text: &[u8]) {
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("job.log"))
    {
        if f.lock().is_err() {
            return;
        }
        let left = MAX_LOG.saturating_sub(f.metadata().map(|m| m.len()).unwrap_or(MAX_LOG));
        let _ = f.write_all(&text[..text.len().min(left as usize)]);
        let _ = f.unlock();
    }
}
fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temp = path.with_extension("tmp");
    let mut f = File::create(&temp)?;
    serde_json::to_writer(&mut f, value)?;
    f.sync_all()?;
    fs::rename(temp, path)?;
    Ok(())
}
fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Artifact {
    path: PathBuf,
    digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StageResult {
    timing: StageTiming,
    artifacts: Vec<Artifact>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Admitted {
    at: i64,
    wait_seconds: f64,
}

fn monotonic() -> Result<f64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(time.tv_sec as f64 + time.tv_nsec as f64 / 1_000_000_000.0)
}
fn digest(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn target() -> Result<PathBuf> {
    let path = cache::target_path()?;
    fs::create_dir_all(&path)?;
    Ok(fs::canonicalize(path)?)
}
pub fn run() -> Result<()> {
    signals();
    let state = state()?;
    let lock = worker_lock(&state)?;
    lock.try_lock()
        .map_err(|_| "another worker or its cleanup supervisor is active")?;
    admission::Pressure::read()?;
    let thresholds = admission::thresholds()?;
    eprintln!("worker: effective admission thresholds {thresholds:?}");
    // Fail closed on an invalid TASKS_CANCEL_GRACE_MS before claiming any job.
    let grace = admission::cancel_grace()?;
    eprintln!(
        "worker: cancel grace {} ms (SIGTERM to SIGKILL)",
        grace.as_millis()
    );
    // Diagnostic only: lets `tasks admission` in other shells report these values.
    if let Err(error) = admission::record_worker_thresholds(&state, &thresholds) {
        eprintln!("worker: could not record admission thresholds: {error}");
    }
    let lock_dir = admission::root()?;
    eprintln!("worker: admission lock directory {}", lock_dir.display());
    if let Err(error) = admission::record_worker_lock_dir(&state, &lock_dir) {
        eprintln!("worker: could not record admission lock directory: {error}");
    }
    let worker = identity(std::process::id())?;
    let mut store = database(&state)?;
    let interrupted = store.reconcile_interrupted()?;
    fs::create_dir_all(state.join("jobs"))?;
    if let Some((bytes, chars)) = own_io() {
        eprintln!(
            "worker: startup done ({interrupted} interrupted job(s) recorded); own threads wrote {bytes} bytes ({chars} via write calls)"
        );
    }
    let mut idle = Instant::now();
    // One compile + four run slots can be occupied without a long run blocking
    // another agent's compile. SQLite claims are serialized; executions are not.
    // Offload clients have their own claim budget (one per offload slot), so
    // waiting offloads never use up the general five and vice versa.
    let mut active: Vec<(std::thread::JoinHandle<Result<()>>, bool)> = vec![];
    let mut failure: Option<String> = None;
    // `tasks stop-worker`: checked about once a second, not per 100 ms tick.
    let mut paused = stopped(&state);
    let mut paused_checked = Instant::now();
    if paused {
        eprintln!("worker: execution stopped (tasks stop-worker); claiming nothing");
    }
    loop {
        if paused_checked.elapsed() >= Duration::from_secs(1) {
            paused_checked = Instant::now();
            let now_paused = stopped(&state);
            if now_paused && !paused {
                eprintln!(
                    "worker: tasks stop-worker: claiming no new jobs; {} running job(s) finish",
                    active.len()
                );
            }
            paused = now_paused;
        }
        let mut i = 0;
        while i < active.len() {
            if active[i].0.is_finished() {
                match active.swap_remove(i).0.join() {
                    Ok(Ok(())) => (),
                    // A job's own transient ledger error or lost supervisor is already on that
                    // job's record; it must not stop every other approved job being claimed.
                    Ok(Err(e)) if !stops_claims(e.as_ref()) => {
                        eprintln!("worker: job ended with an error, still claiming: {e}");
                    }
                    Ok(Err(e)) => {
                        failure.get_or_insert(e.to_string());
                    }
                    Err(_) => {
                        failure.get_or_insert(
                            "execution thread panicked; inspect retained state before retry".into(),
                        );
                    }
                }
            } else {
                i += 1;
            }
        }
        let offloads = active.iter().filter(|(_, offload)| *offload).count();
        let general = active.len() - offloads < 5;
        let offload = offloads < admission::OFFLOAD_SLOTS;
        if !STOP.load(Ordering::SeqCst) && failure.is_none() && !paused && (general || offload) {
            match store.claim_class(worker.clone(), general, offload) {
                Ok(Some(job)) => {
                    let is_offload = crate::local_command::is_offload(&job.spec);
                    idle = Instant::now();
                    let state = state.clone();
                    let lock = lock.try_clone()?;
                    let worker = worker.clone();
                    let handle = std::thread::spawn(move || {
                        let result = (|| -> Result<()> {
                            let mut store = database(&state)?;
                            if let Err(error) = execute(&state, &lock, &mut store, job.id, &worker)
                            {
                                append_log(
                                    &job_dir(&state, job.id),
                                    format!("worker error: {error}\n").as_bytes(),
                                );
                                let phase = if error.is::<SupervisorLost>() {
                                    Phase::Interrupted
                                } else {
                                    Phase::Failed
                                };
                                let detail = error.to_string();
                                with_ledger(&state, job.id, "final failure record", || {
                                    store.finish(job.id, phase.clone(), &detail)
                                })?;
                                return Err(error);
                            }
                            Ok(())
                        })();
                        // This thread's own I/O, before it leaves the per-thread table.
                        if let Ok(text) = fs::read_to_string("/proc/thread-self/io") {
                            let (bytes, chars) = parse_io(&text);
                            FINISHED_THREADS_IO.0.fetch_add(bytes, Ordering::Relaxed);
                            FINISHED_THREADS_IO.1.fetch_add(chars, Ordering::Relaxed);
                        }
                        result
                    });
                    active.push((handle, is_offload));
                    continue;
                }
                Ok(None) => (),
                // A busy ledger is not a worker failure: try the claim again later.
                Err(e) if ledger::is_transient(e.as_ref()) => {
                    eprintln!("worker: ledger busy while claiming ({e}); retrying");
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => {
                    failure = Some(e.to_string());
                }
            }
        }
        // Stop taking new claims after an infrastructure error, but let other
        // already-approved jobs finish. Signals propagate cancellation to all.
        if active.is_empty()
            && (STOP.load(Ordering::SeqCst)
                || failure.is_some()
                || idle.elapsed() > Duration::from_secs(2))
        {
            // A client that approves a job then asks for a worker sees this one still holding
            // worker.lock and spawns nothing. Release the lock first, then look again: any
            // approval committed before that client's check is visible here, and any later
            // one finds the lock free and spawns a worker.
            let leaving = STOP.load(Ordering::SeqCst) || failure.is_some() || paused;
            if !leaving
                && lock.unlock().is_ok()
                && store.approved_waiting().unwrap_or(0) > 0
                && lock.try_lock().is_ok()
            {
                idle = Instant::now();
                continue;
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some(end) = own_io() {
        // /proc/self/io of this process also counts every job process it reaped (stage
        // supervisors and, through them, cargo/rustc/gradle/ssh). These are the worker's
        // OWN threads only, so a burst can be attributed to the jobs, not the worker.
        eprintln!(
            "worker: exiting; own threads wrote {} bytes to storage ({} via write calls) since start; jobs' processes are counted in /proc/PID/io, not here",
            end.0, end.1
        );
    }
    if let Some(error) = failure {
        return Err(error.into());
    }
    Ok(())
}

/// (write_bytes, wchar) of finished execution threads (their per-thread counters vanish with
/// them, merged with reaped children's in `/proc/self/io`).
static FINISHED_THREADS_IO: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));

/// (write_bytes, wchar) of this process's own threads, live and finished: the worker's own
/// I/O, excluding reaped children (which `/proc/self/io` includes).
pub fn own_io() -> Option<(u64, u64)> {
    let mut total = (
        FINISHED_THREADS_IO.0.load(Ordering::Relaxed),
        FINISHED_THREADS_IO.1.load(Ordering::Relaxed),
    );
    for entry in fs::read_dir("/proc/self/task").ok()?.flatten() {
        let Ok(text) = fs::read_to_string(entry.path().join("io")) else {
            continue;
        };
        let (bytes, chars) = parse_io(&text);
        total.0 += bytes;
        total.1 += chars;
    }
    Some(total)
}

/// (write_bytes, wchar) of one `/proc/.../io` text.
pub fn parse_io(text: &str) -> (u64, u64) {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    };
    (field("write_bytes:"), field("wchar:"))
}

fn execute(
    state: &Path,
    lock: &File,
    store: &mut Store,
    id: i64,
    worker: &ProcessIdentity,
) -> Result<()> {
    let job = store.job(id)?;
    catalog::validate_claimed(&job)?;
    if job.spec.suite == crate::local_command::SUITE {
        return execute_local(state, lock, store, &job, worker);
    }
    if job.spec.suite == catalog::ANDROID_SUITE {
        return execute_android(state, lock, store, &job, worker);
    }
    let dir = job_dir(state, id);
    fs::create_dir(&dir)?;
    append_log(
        &dir,
        format!(
            "job {id}: suite={} revision={} source={}\n",
            job.spec.suite, job.spec.revision, job.spec.source
        )
        .as_bytes(),
    );
    let target = target()?;
    if dir.starts_with(&target) || target.starts_with(&dir) {
        return Err("target must not overlap job metadata or source snapshots".into());
    }
    let source_root = fs::canonicalize(Path::new(&job.spec.source).join("tools/task-manager"))?;
    if target.starts_with(&source_root) || source_root.starts_with(&target) {
        return Err("target must not overlap the selected source workspace".into());
    }
    let compile_cache = cache::CompileCache::discover()?;
    save(&dir.join("compile-cache.json"), &compile_cache)?;
    save(&dir.join("target.json"), &target)?;
    append_log(
        &dir,
        format!(
            "compile cache: {}\n",
            serde_json::to_string(&compile_cache)?
        )
        .as_bytes(),
    );
    let Some(target_lease) = wait_for_target(store, id, state, &target, &compile_cache, &dir)?
    else {
        store.finish(
            id,
            Phase::Cancelled,
            "Cancelled while waiting for target/disk admission",
        )?;
        return Ok(());
    };
    let inventory = retention::require_budget(&target)?;
    let usage = retention::begin_use(&target_lease, &target)?;
    append_log(
        &dir,
        format!(
            "target budget: {} allocated bytes / {}; ownership={}\n",
            inventory.allocated_bytes,
            retention::TARGET_BUDGET_BYTES,
            if usage.is_some() {
                "manager"
            } else {
                "legacy/unknown (never auto-adopted)"
            },
        )
        .as_bytes(),
    );
    let _usage = usage; // Final usage recording runs before the target lease drops.
    let snap = snapshot::capture(
        Path::new(&job.spec.source),
        &job.spec.revision,
        &dir.join("snapshot"),
    )?;
    save(&dir.join("snapshot.json"), &snap)?;
    let mut e = store.execution(id)?.ok_or("missing execution")?;
    e.snapshot = Some(snap);
    store.update_execution(id, Phase::Preparing, e)?;
    for stage in ["build", "run"] {
        if cancelled(store, id)? || STOP.load(Ordering::SeqCst) {
            store.finish(id, Phase::Cancelled, "Cancelled before stage admission")?;
            return Ok(());
        }
        let result = supervise(state, lock, target_lease.file(), store, id, stage, worker)?;
        let mut e = store.execution(id)?.ok_or("missing execution")?;
        e.supervisor = None;
        e.stages.push(result.timing.clone());
        let phase = if stage == "build" {
            Phase::Building
        } else {
            Phase::Running
        };
        store.update_execution(id, phase, e)?;
        if cancelled(store, id)? || STOP.load(Ordering::SeqCst) {
            store.finish(
                id,
                Phase::Cancelled,
                "Cancelled; owned stage process group stopped",
            )?;
            return Ok(());
        }
        if result.timing.exit_code != Some(0) {
            store.finish(
                id,
                Phase::Failed,
                &format!(
                    "{stage} failed: exit={:?} signal={:?}; inspect logs",
                    result.timing.exit_code, result.timing.signal
                ),
            )?;
            return Ok(());
        }
    }
    // Commit completion and successful observations together; cancellation must
    // not teach history if it races with final completion.
    store.complete_success(id)?;
    Ok(())
}
fn execute_local(
    state: &Path,
    lock: &File,
    store: &mut Store,
    job: &crate::Job,
    worker: &ProcessIdentity,
) -> Result<()> {
    let recipe = crate::local_command::Recipe::from_spec(&job.spec)?;
    let dir = job_dir(state, job.id);
    fs::create_dir(&dir)?;
    let lease = File::create(dir.join("live-command-lease"))?;
    append_log(
        &dir,
        b"TRUSTED LIVE COMMAND: cwd is NOT snapshotted; no immutable-source/artifact guarantee\n",
    );
    // A job rerouted before this directory existed gets its move line now.
    crate::reroute::flush_pending_line(state, store, job.id);
    // Metrics: cold/unknown build state BEFORE output placement can create a
    // target link. Diagnostic only; a failure to record it never fails the job.
    let start = crate::metrics::warmth::at_start(&recipe.argv, Path::new(&job.spec.source));
    let _ = save(&dir.join(crate::metrics::warmth::START_FILE), &start);
    if !prepare_local_outputs(store, state, &dir, job, &recipe)? {
        return store.finish(
            job.id,
            Phase::Cancelled,
            "Cancelled while waiting for disk reserve",
        );
    }
    // The spec can change under a waiting job: a reroute turns it into the forward to a peer
    // (its local admission withdraws), a forward that submitted nothing turns it back.
    // Re-read: a reroute may have claimed the job while it waited for the disk reserve.
    let mut current = with_ledger(state, job.id, "job re-read", || store.job(job.id))?;
    'spec: loop {
        let recipe = crate::local_command::Recipe::from_spec(&current.spec)?;
        let ran = crate::reroute::spec_key(&current.spec)?;
        for stage in recipe.stages() {
            if cancelled(store, job.id)? || STOP.load(Ordering::SeqCst) {
                return store.finish(
                    job.id,
                    Phase::Cancelled,
                    "Cancelled before local command stage",
                );
            }
            let result = supervise(state, lock, &lease, store, job.id, stage, worker)?;
            if let crate::reroute::Next::Respec(note) =
                with_ledger(state, job.id, "reroute check", || {
                    crate::reroute::after_stage(state, store, job.id, &ran, stage)
                })?
            {
                append_log(&dir, note.as_bytes());
                current = with_ledger(state, job.id, "job re-read", || store.job(job.id))?;
                continue 'spec;
            }
            with_ledger(state, job.id, "stage result", || {
                let mut e = store.execution(job.id)?.ok_or("missing execution")?;
                e.supervisor = None;
                e.stages.push(result.timing.clone());
                store.update_execution(
                    job.id,
                    if stage == "build" {
                        Phase::Building
                    } else {
                        Phase::Running
                    },
                    e,
                )
            })?;
            if cancelled(store, job.id)? || STOP.load(Ordering::SeqCst) {
                return with_ledger(state, job.id, "cancel record", || {
                    store.finish(
                        job.id,
                        Phase::Cancelled,
                        "Cancelled local command; owned group stopped",
                    )
                });
            }
            if result.timing.exit_code != Some(0) {
                return with_ledger(state, job.id, "failure record", || {
                    store.finish(
                        job.id,
                        Phase::Failed,
                        "Local command failed; inspect retained log",
                    )
                });
            }
        }
        return with_ledger(state, job.id, "completion", || {
            store.complete_success(job.id)
        });
    }
}

const DISK_WAIT_LOG_INTERVAL: Duration = Duration::from_secs(60);
/// Runs in the worker BEFORE any shared compile/run/device admission: creates
/// the git-ignored Android `local.properties` a fresh worktree lacks (or fails
/// naming it), then places cache output links under the disk reserves.
/// A reserve shortfall WAITS here, logged and cancellable, like the snapshot
/// path's `wait_for_target`. No shared slot is held while waiting, so jobs that
/// need no new space (existing links, run-only work) are never blocked behind
/// it. `run_command` re-places after admission; that is normally a no-op.
/// Returns false if cancelled or stopping while waiting.
fn prepare_local_outputs(
    store: &mut Store,
    state: &Path,
    dir: &Path,
    job: &crate::Job,
    recipe: &crate::local_command::Recipe,
) -> Result<bool> {
    use crate::local_command::{self as local, Placed};
    let cwd = Path::new(&job.spec.source);
    if let Some(note) =
        local::ensure_android_local_properties(&recipe.argv, cwd, |name| std::env::var_os(name))?
    {
        append_log(dir, note.as_bytes());
    }
    let mut waited_on: Option<PathBuf> = None;
    let mut logged = Instant::now();
    let placement = local::wait_for_placement(
        || {
            let state_space = cache::state_space(state)?;
            if !state_space.admitted() {
                return Ok(Placed::Blocked(state_space));
            }
            if recipe.resource == local::Resource::Offload {
                // No local output; admission applies the offload disk gate.
                return Ok(Placed::Done(local::offload_placement()));
            }
            local::try_place_outputs(cwd, &recipe.argv)
        },
        |space| {
            if cancelled(store, job.id)? || STOP.load(Ordering::SeqCst) {
                return Ok(false);
            }
            // Log when the blocking filesystem changes, then once a minute.
            if waited_on.as_ref() != Some(&space.path) || logged.elapsed() >= DISK_WAIT_LOG_INTERVAL
            {
                let detail = format!("Waiting for disk reserve: {}", space.detail());
                let mut e = store.execution(job.id)?.ok_or("missing execution")?;
                e.detail = detail.clone();
                store.update_execution(job.id, Phase::Preparing, e)?;
                append_log(dir, format!("{detail}\n").as_bytes());
                waited_on = Some(space.path.clone());
                logged = Instant::now();
            }
            std::thread::sleep(Duration::from_secs(1));
            Ok(true)
        },
    )?;
    let Some(placement) = placement else {
        return Ok(false);
    };
    if waited_on.is_some() {
        append_log(
            dir,
            b"Disk reserve available; continuing to shared admission\n",
        );
    }
    append_log(dir, placement.detail().as_bytes());
    Ok(true)
}

fn execute_android(
    state: &Path,
    lock: &File,
    store: &mut Store,
    job: &crate::Job,
    worker: &ProcessIdentity,
) -> Result<()> {
    let id = job.id;
    let abis = android::Abis::parse(catalog::android_contract(&job.spec)?)?;
    let source = Path::new(&job.spec.source);
    let dir = job_dir(state, id);
    fs::create_dir(&dir)?;
    append_log(
        &dir,
        b"Android debug build-only; no tests/install/launch/publish requested\n",
    );
    let target = android_build::root(&android_build::lane_path(abis, source)?)?;
    if dir.starts_with(&target)
        || target.starts_with(&dir)
        || target.starts_with(fs::canonicalize(source)?)
    {
        return Err("Android lane overlaps source or results".into());
    }
    let compile_cache = cache::CompileCache::discover()?;
    let Some(lease) = wait_for_target(store, id, state, &target, &compile_cache, &dir)? else {
        store.finish(
            id,
            Phase::Cancelled,
            "Cancelled during Android lane/disk admission",
        )?;
        return Ok(());
    };
    retention::require_budget(&target)?;
    let _usage = retention::begin_android_use(&lease, &target)?;
    // Restrict only after empty-root initialization/owned-marker verification;
    // never chmod an unowned populated cache. Working source may be private.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
    let plan = android::preflight(source, &job.spec.revision, abis)?;
    if !plan.libaom_tarball_verified || plan.toolchain_markers.iter().any(|m| !m.present) {
        return Err("Android preflight missing verified native archive/toolchain markers; no fetch performed".into());
    }
    let host = android_build::Host::discover(source, abis)?;
    let snap = workspace_snapshot::capture(source, &job.spec.revision, &dir.join("snapshot"))?;
    if snap.digest != plan.snapshot.digest {
        return Err("Android source changed between preflight and capture".into());
    }
    let script_hash =
        android_build::file_hash(&dir.join("snapshot/scripts/build_libaom.py"), 1024 * 1024)?.0;
    let native_key = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            &script_hash,
            &host.sdk,
            host.identities
                .iter()
                .filter(|(path, _)| path.starts_with(host.sdk.join("ndk/28.2.13676358")))
                .collect::<Vec<_>>(),
        ))?)
    );
    let layout =
        android_build::Layout::prepare(&target, &dir.join("snapshot"), &snap.digest, &native_key)?;
    let archive = plan.libaom_tarball.path;
    let archive_hash = plan
        .libaom_tarball
        .sha256
        .ok_or("missing archive identity")?;
    save(&dir.join("android-host.json"), &host)?;
    save(&dir.join("android-layout.json"), &layout)?;
    save(
        &dir.join("android-native.json"),
        &(archive, archive_hash, plan.libaom_version),
    )?;
    save(&dir.join("snapshot.json"), &snap)?;
    let mut e = store.execution(id)?.ok_or("missing execution")?;
    e.snapshot = Some(snap);
    e.detail = "Verified Android capsule materialized; outputs exclusively leased".into();
    store.update_execution(id, Phase::Preparing, e)?;
    if cancelled(store, id)? || STOP.load(Ordering::SeqCst) {
        store.finish(
            id,
            Phase::Cancelled,
            "Cancelled before Android compile admission",
        )?;
        return Ok(());
    }
    let result = supervise(state, lock, lease.file(), store, id, "build", worker)?;
    let mut e = store.execution(id)?.ok_or("missing execution")?;
    e.supervisor = None;
    e.stages.push(result.timing.clone());
    store.update_execution(id, Phase::Building, e)?;
    if cancelled(store, id)? || STOP.load(Ordering::SeqCst) {
        store.finish(
            id,
            Phase::Cancelled,
            "Cancelled Android build; owned group stopped",
        )?;
    } else if result.timing.exit_code != Some(0) {
        store.finish(
            id,
            Phase::Failed,
            "Android build failed; inspect retained stage logs",
        )?;
    } else {
        let artifact = load(&dir.join("android-artifact.json"))?;
        store.complete_build_success(id, artifact)?;
    }
    Ok(())
}

fn wait_for_target(
    store: &mut Store,
    id: i64,
    state: &Path,
    target: &Path,
    compile_cache: &cache::CompileCache,
    dir: &Path,
) -> Result<Option<cache::TargetLease>> {
    let mut previous = String::new();
    loop {
        if cancelled(store, id)? || STOP.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let mut disks = vec![cache::state_space(state)?, cache::disk_space(target)?];
        if compile_cache.wrapper.is_some() {
            disks.push(cache::disk_space(&compile_cache.directory)?);
        }
        let detail = if let Some(disk) = disks.iter().find(|d| !d.admitted()) {
            format!("Waiting for disk reserve: {}", disk.detail())
        } else if let Some(lease) = cache::TargetLease::try_acquire(target)? {
            append_log(
                dir,
                format!("Exclusive target lease acquired: {}\n", target.display()).as_bytes(),
            );
            return Ok(Some(lease));
        } else {
            format!("Waiting for exclusive target lease: {}", target.display())
        };
        if detail != previous {
            let mut e = store.execution(id)?.ok_or("missing execution")?;
            e.detail = detail.clone();
            store.update_execution(id, Phase::Preparing, e)?;
            append_log(dir, format!("{detail}\n").as_bytes());
            previous = detail;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Worker-thread cancellation check. A busy ledger is retried (the stage keeps
/// running in its own supervisor meanwhile); only a committed cancel returns true.
/// Past the ledger deadline this fails with `LedgerUnavailable`, never "cancelled".
fn cancelled(store: &Store, id: i64) -> Result<bool> {
    let state = state()?;
    with_ledger(&state, id, "cancellation check", || {
        ledger::cancel_requested(store, id)
    })
}

/// Retries busy/locked ledger calls up to `ledger::LEDGER_UNAVAILABLE_DEADLINE`,
/// noting the wait in the job log. Other errors return at once.
fn with_ledger<T>(state: &Path, id: i64, what: &str, f: impl FnMut() -> Result<T>) -> Result<T> {
    let dir = job_dir(state, id);
    ledger::retry(
        what,
        ledger::LEDGER_UNAVAILABLE_DEADLINE,
        &mut |note| append_log(&dir, format!("worker: {note}").as_bytes()),
        f,
    )
}

fn supervise(
    state: &Path,
    lock: &File,
    target_lease: &File,
    store: &mut Store,
    id: i64,
    stage: &str,
    worker: &ProcessIdentity,
) -> Result<StageResult> {
    let dir = job_dir(state, id);
    let phase = if stage == "build" {
        Phase::WaitingBuild
    } else {
        Phase::WaitingRun
    };
    let mut e = store.execution(id)?.ok_or("missing execution")?;
    e.detail = format!("Waiting for shared {stage} admission");
    store.update_execution(id, phase, e)?;
    save(&dir.join(format!("{stage}-waiting.json")), &monotonic()?)?;
    let mut child = Command::new(std::env::current_exe()?)
        // Admission's checkout auto-placement must not inspect/mutate the caller's
        // unrelated Git cwd. All workload commands choose their own absolute cwd.
        .current_dir("/")
        .env("TASK_MANAGER_STATE_DIR", state)
        .args([
            "__stage",
            &id.to_string(),
            stage,
            &worker.pid.to_string(),
            &worker.start_ticks.to_string(),
        ])
        .stdin(lock.try_clone()?)
        // Internal stdout is a second inherited lease channel, NOT user output.
        // Standard descriptors retain worker/target leases across native exec.
        // All workload output is separately logged, never written on this fd.
        .stdout(target_lease.try_clone()?)
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let supervisor = identity(child.id())?;
    let mut e = store.execution(id)?.ok_or("missing execution")?;
    e.supervisor = Some(supervisor.clone());
    store.update_execution(
        id,
        if stage == "build" {
            Phase::WaitingBuild
        } else {
            Phase::WaitingRun
        },
        e,
    )?;
    let start = Instant::now();
    let mut admitted = None;
    loop {
        if STOP.load(Ordering::SeqCst) {
            with_ledger(state, id, "stop request", || store.cancel(id))?;
        }
        if admitted.is_none() && dir.join(format!("{stage}-admitted.json")).exists() {
            let marker: Admitted = load(&dir.join(format!("{stage}-admitted.json")))?;
            admitted = Some(marker.wait_seconds);
            with_ledger(state, id, "admission detail", || {
                let mut e = store.execution(id)?.ok_or("missing execution")?;
                e.detail = format!("Shared admission granted; {stage} executing");
                store.update_execution(
                    id,
                    if stage == "build" {
                        Phase::Building
                    } else {
                        Phase::Running
                    },
                    e,
                )
            })?;
        }
        if let Some(status) = child.try_wait()? {
            let path = dir.join(format!("{stage}-result.json"));
            if !path.exists() {
                return Err(SupervisorLost(format!(
                    "stage supervisor exited without result ({status}); not retried"
                ))
                .into());
            }
            let mut result: StageResult = load(&path)?;
            result.timing.wait_seconds = admitted.unwrap_or_else(|| start.elapsed().as_secs_f64());
            return Ok(result);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Internal stage supervisor: owns only its newly spawned Rust admission group.
/// stdin retains worker.lock; stdout retains the target lease through cleanup.
pub fn stage(id: i64, stage: &str, worker: ProcessIdentity) -> Result<()> {
    signals();
    if !matches!(stage, "build" | "run") {
        return Err("unknown internal stage".into());
    }
    let state = state()?;
    let dir = job_dir(&state, id);
    // Fail closed before launching anything if the grace setting is invalid.
    let grace = admission::cancel_grace()?;
    let store = database(&state)?;
    let job = with_ledger(&state, id, "stage job read", || store.job(id))?;
    catalog::validate_claimed(&job)?;
    if stage == "run" && job.spec.suite == catalog::ANDROID_SUITE {
        return Err("Android is build-only; no run admission allowed".into());
    }
    let e = job.execution.ok_or("missing execution")?;
    if e.worker.as_ref() != Some(&worker) || !job.phase.active() {
        return Err("stage lacks a claimed worker".into());
    }
    let supervisor = identity(std::process::id())?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args([
        "__admit",
        &id.to_string(),
        stage,
        &supervisor.pid.to_string(),
        &supervisor.start_ticks.to_string(),
    ]);
    // Admission killed by a supervisor crash cannot strand the command unnoticed.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // Pass the lease to admission and its command so crash windows stay serialized.
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};
    let fd = unsafe {
        libc::fcntl(
            std::io::stdin().as_fd().as_raw_fd(),
            libc::F_DUPFD_CLOEXEC,
            3,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let lease = unsafe { File::from_raw_fd(fd) };
    // Preserve the target lease on stdout through native admission and __command.
    // Neither internal entry point emits user output on this descriptor.
    let target_fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    if target_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let target_lease = unsafe { File::from_raw_fd(target_fd) };
    cmd.stdin(Stdio::from(lease))
        .stdout(Stdio::from(target_lease))
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn()?;
    let group = identity(child.id())?;
    let err = drain(child.stderr.take().ok_or("missing stderr")?, dir.clone());
    let mut stopped = false;
    // A failed ledger read is "unknown": the stage keeps running and the read is
    // retried. Only a committed cancel, a stop signal, worker loss, or a ledger
    // unreadable past its deadline stops the group.
    let mut probe = CancelProbe::default();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let reason = if STOP.load(Ordering::SeqCst) {
            Some("stop signal".to_string())
        } else if !alive(&worker) {
            Some("worker gone".to_string())
        } else {
            match probe.poll(|| ledger::cancel_requested(&store, id), &mut |note| {
                append_log(&dir, format!("supervisor: {note}").as_bytes())
            }) {
                CancelState::Continue => None,
                CancelState::Cancelled => Some("cancel requested".to_string()),
                CancelState::LedgerUnavailable(why) => {
                    Some(format!("ledger unavailable, NOT a cancellation: {why}"))
                }
            }
        };
        if let Some(reason) = reason {
            append_log(
                &dir,
                format!(
                    "supervisor: stopping owned stage group ({reason}); SIGTERM, SIGKILL after {} ms\n",
                    grace.as_millis()
                )
                .as_bytes(),
            );
            terminate_group(&mut child, &group, grace)?;
            stopped = true;
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = err.join();
    let result_path = dir.join(format!("{stage}-result.json"));
    if !result_path.exists() {
        save(
            &result_path,
            &StageResult {
                timing: StageTiming {
                    stage: stage.into(),
                    wait_seconds: 0.0,
                    seconds: None,
                    exit_code: status.code(),
                    signal: status.signal(),
                },
                artifacts: vec![],
            },
        )?;
    }
    if stopped && !alive(&worker) {
        append_log(
            &dir,
            b"supervisor: worker disappeared; job requires reconciliation, never rerun\n",
        );
    }
    Ok(())
}

/// Internal Rust-only admission boundary, the process-group leader for commands.
/// Existing Python clients coordinate through the same flock files, not RPC.
pub fn admit(id: i64, stage: &str, supervisor: ProcessIdentity) -> Result<()> {
    signals();
    if !matches!(stage, "build" | "run") {
        return Err("unknown native admission stage".into());
    }
    let state = state()?;
    let mut store = database(&state)?;
    let job = with_ledger(&state, id, "admission job read", || store.job(id))?;
    catalog::validate_claimed(&job)?;
    // The spec this admission is for: a reroute that claims the job meanwhile changes it, and
    // then this admission withdraws (waiting) or does not commit (slot already taken).
    let ran = crate::reroute::spec_key(&job.spec)?;
    let rerouted = std::cell::Cell::new(false);
    let checked = std::cell::Cell::new(Instant::now());
    // Admission may wait a long time; a busy ledger during that wait is retried,
    // never read as a cancel. This process's stderr is the job log.
    let probe = std::cell::RefCell::new(CancelProbe::default());
    let unavailable = std::cell::RefCell::new(None::<String>);
    let stop_waiting = || {
        if checked.get().elapsed() >= Duration::from_millis(500) {
            checked.set(Instant::now());
            if store.spec_changed(id, &ran).unwrap_or(false) {
                rerouted.set(true);
                return true;
            }
        }
        let state = probe
            .borrow_mut()
            .poll(|| ledger::cancel_requested(&store, id), &mut |note| {
                eprint!("admission: {note}")
            });
        match state {
            CancelState::Continue => false,
            CancelState::Cancelled => true,
            CancelState::LedgerUnavailable(why) => {
                *unavailable.borrow_mut() = Some(why);
                true
            }
        }
    };
    let not_admitted = |what: &str| -> Box<dyn std::error::Error + Send + Sync> {
        if rerouted.get() {
            return format!(
                "{what} withdrawn: job {id} was rerouted to a peer before it started here"
            )
            .into();
        }
        match unavailable.borrow().as_ref() {
            Some(why) => ledger::LedgerUnavailable(format!("{what} abandoned: {why}")).into(),
            None => format!("{what} cancelled or owner disappeared").into(),
        }
    };
    let e = job.execution.as_ref().ok_or("missing execution")?;
    let worker = e.worker.as_ref().ok_or("missing worker")?;
    if !job.phase.active()
        || identity(unsafe { libc::getppid() } as u32)? != supervisor
        || unsafe { libc::getpgrp() } != std::process::id() as i32
        || (stage == "run" && job.spec.suite == catalog::ANDROID_SUITE)
    {
        return Err("native admission requires its owned supervisor/group and valid stage".into());
    }
    let mut offload = false;
    let mut service = false;
    let mut fetches = true;
    let _device = if job.spec.suite == crate::local_command::SUITE {
        let recipe = crate::local_command::Recipe::from_spec(&job.spec)?;
        recipe.argv_for(stage)?;
        offload = recipe.resource == crate::local_command::Resource::Offload;
        service = recipe.resource == crate::local_command::Resource::Service;
        fetches = crate::local_command::offload_fetches(&recipe.argv);
        if let crate::local_command::Resource::Device(serial) = recipe.resource {
            let Some(d) = admission::acquire(
                admission::Kind::Device(serial),
                &format!("{}:job-{id}", job.spec.owner),
                "approved local device command",
                || {
                    STOP.load(Ordering::SeqCst)
                        || !alive(&supervisor)
                        || !alive(worker)
                        || stop_waiting()
                },
            )?
            else {
                return Err(not_admitted("device admission"));
            };
            d.inherit()?;
            Some(d)
        } else {
            None
        }
    } else {
        None
    };
    let kind = if stage == "build" {
        admission::Kind::Compile
    } else if offload {
        // Validated recipe: only an allowlisted offload client, run stage only.
        admission::Kind::Offload
    } else if service {
        admission::Kind::Service
    } else {
        admission::Kind::Run
    };
    // A job back from a failed reroute queues with its first wait time (original position).
    let since = crate::reroute::queued_since(&job_dir(&state, id), stage);
    let submitted_at = job.submitted_at;
    let urgent = || {
        store
            .execution(id)
            .ok()
            .flatten()
            .filter(|e| e.urgent())
            .map(|_| admission::urgent_time(submitted_at))
    };
    // A forwarded job queues here, slot-free, while its checkout is in use (peer::checkout_gate).
    let gate = matches!(kind, admission::Kind::Offload)
        .then(|| crate::peer::checkout_gate::Gate::for_job(&job))
        .flatten();
    let hold = || {
        gate.as_ref()
            .and_then(|g| g.check(|| store.jobs().unwrap_or_default()))
    };
    let Some(lease) = admission::acquire_offload_prio(
        kind,
        fetches,
        &format!("{}:job-{id}", job.spec.owner),
        &format!(
            "{} __command {id} {stage} {} {}",
            job.spec.suite, supervisor.pid, supervisor.start_ticks
        ),
        since,
        urgent,
        hold,
        || STOP.load(Ordering::SeqCst) || !alive(&supervisor) || !alive(worker) || stop_waiting(),
    )?
    else {
        return Err(not_admitted("native admission"));
    };
    #[cfg(debug_assertions)]
    crate::reroute::test_admit_hold(id, stage);
    // Commit under the ledger write lock: from here on no reroute can claim this job. If one
    // claimed it first, release the slot without running anything.
    let committed = with_ledger(&state, id, "admission commit", || {
        store.commit_admission(&state, id, stage, &ran)
    })?;
    if !committed {
        drop(lease);
        append_log(
            &job_dir(&state, id),
            format!(
                "tasks: {stage} slot taken just as job {id} was rerouted; released it unused (nothing ran here)\n"
            )
            .as_bytes(),
        );
        rerouted.set(true);
        return Err(not_admitted("native admission"));
    }
    lease.inherit()?;
    append_log(
        &job_dir(&state, id),
        b"Native Rust admission granted (shared flock protocol)\n",
    );
    use std::os::fd::AsFd;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args([
        "__command",
        &id.to_string(),
        stage,
        &supervisor.pid.to_string(),
        &supervisor.start_ticks.to_string(),
    ])
    .stdin(File::from(std::io::stdin().as_fd().try_clone_to_owned()?))
    .stdout(File::from(std::io::stdout().as_fd().try_clone_to_owned()?));
    // Managed output mappings already exist; never auto-place another checkout's
    // outputs. Cargo uses machine configuration, not a wrapper override here.
    if std::env::var_os("GRADLE_USER_HOME").is_none() {
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME required")?);
        let gradle = home.join("build-cache/gradle");
        if gradle.is_dir() {
            cmd.env("GRADLE_USER_HOME", gradle);
        }
    }
    let status = cmd.status()?;
    if !status.success() {
        // The command's own exit status was already logged once by __command and
        // is the job's recorded exit code; this is only the internal wrapper's.
        let wrapper = status
            .code()
            .map_or_else(|| status.to_string(), |c| format!("exit {c}"));
        return Err(format!(
            "{stage} stage did not succeed (internal __command wrapper: {wrapper}; this is not the command's exit status)"
        )
        .into());
    }
    Ok(())
}

fn signal_group(group: &ProcessIdentity, sig: i32) -> Result<()> {
    if !identity(group.pid).is_ok_and(|p| p == *group) {
        return Ok(());
    }
    if unsafe { libc::getpgid(group.pid as i32) } != group.pid as i32 {
        return Err("refusing to signal a group not owned by stage child".into());
    }
    if unsafe { libc::kill(-(group.pid as i32), sig) } < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            return Err(e.into());
        }
    }
    Ok(())
}
/// SIGTERM, then SIGKILL after `grace` (TASKS_CANCEL_GRACE_MS) or as soon as the
/// group leader has exited, whichever is first; the SIGKILL still reaches any
/// remaining group members.
fn terminate_group(child: &mut Child, group: &ProcessIdentity, grace: Duration) -> Result<()> {
    // Keep the group leader unreaped until escalation, preventing PID/group reuse.
    signal_group(group, libc::SIGTERM)?;
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && !leader_exited(child) {
        std::thread::sleep(Duration::from_millis(20));
    }
    signal_group(group, libc::SIGKILL)?;
    let _ = child.wait()?;
    Ok(())
}
/// True once the child has exited, WITHOUT reaping it (WNOWAIT).
fn leader_exited(child: &Child) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    rc == 0 && unsafe { info.si_pid() } != 0
}
fn drain<R: Read + Send + 'static>(mut reader: R, dir: PathBuf) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => append_log(&dir, &buf[..n]),
            }
        }
    })
}

const DEFAULT_CARGO_BUILD_JOBS: &str = "2";
/// `admission::cargo_jobs_when_busy`: while another compile-class job holds a slot on this
/// machine, a Cargo command's `-j` (or the injected default) above `min(cap, when_busy)` is
/// replaced by it. A lone job, and any other command, runs as submitted.
fn lower_busy_jobs(
    dir: &Path,
    resource: &crate::local_command::Resource,
    argv: Vec<String>,
) -> Vec<String> {
    use crate::local_command::Resource;
    if *resource != Resource::Compile || !crate::placement::is_cargo(&argv) {
        return argv;
    }
    let Ok(Some(when_busy)) = crate::policy::effective()
        .policy
        .admission
        .cargo_jobs_when_busy()
    else {
        return argv;
    };
    let Ok(thresholds) = admission::Thresholds::from_env() else {
        return argv;
    };
    let others = admission::other_compile_jobs_running(unsafe { libc::getppid() } as u32);
    let default = DEFAULT_CARGO_BUILD_JOBS.parse().unwrap_or(2);
    let Some(jobs) = admission::lowered_cargo_jobs(
        crate::placement::requested_jobs(&argv),
        default,
        thresholds.max_cargo_jobs,
        Some(when_busy),
        others > 0,
    ) else {
        return argv;
    };
    append_log(
        dir,
        format!(
            "tasks: {others} other compile job(s) running; Cargo -j lowered to {jobs} \
             (admission::cargo_jobs_when_busy)\n"
        )
        .as_bytes(),
    );
    crate::placement::apply_jobs(&argv, jobs)
}
/// The only daemon environment variables an admitted command inherits.
const PASSTHROUGH_ENV: [&str; 12] = [
    "HOME",
    "PATH",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "XDG_RUNTIME_DIR",
    "LANG",
    // Host toolchain wiring set by the daemon's own environment (e.g. the
    // NixOS worker unit); absent on hosts that don't set them.
    "PKG_CONFIG_PATH",
    "NIX_LD",
    "NIX_LD_LIBRARY_PATH",
    // Android/Gradle wiring from the worker unit: Gradle finds the SDK through
    // ANDROID_HOME (or the deprecated ANDROID_SDK_ROOT) when local.properties
    // has no sdk.dir, and the JDK through JAVA_HOME. Paths, not credentials.
    "ANDROID_HOME",
    "ANDROID_SDK_ROOT",
    "JAVA_HOME",
];
fn sanitized(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    for name in PASSTHROUGH_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    // One admitted Cargo process can otherwise launch many simultaneous linkers.
    // This is a cooperative default, not a limit on arbitrary trusted scripts.
    // Deliberately NOT TASKS_MAX_CARGO_JOBS: a raised cap only permits larger
    // explicit -j/--jobs/CARGO_BUILD_JOBS; default behaviour is the same on every host.
    command.env("CARGO_BUILD_JOBS", DEFAULT_CARGO_BUILD_JOBS);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Device commands: `ANDROID_SERIAL` names the locked device, so adb, Gradle
/// install tasks and ddmlib target it even without `-s` (an explicit `-s` or a
/// job's own `env ANDROID_SERIAL=` still overrides; denylisted serials in argv
/// are refused at validation). The adb server is started here, before the
/// command, with no inherited descriptors: adb daemonizes into its own session
/// (outside the owned process group), and a server first spawned by the command
/// itself would keep this job's device and run lock descriptors held forever.
/// Failure to start it is logged, not fatal (the command reports its own error).
fn prepare_device_command(cmd: &mut Command, dir: &Path, lock: &str) {
    cmd.env("ANDROID_SERIAL", crate::device::adb_serial_for_lock(lock));
    let env: Vec<(&str, std::ffi::OsString)> = PASSTHROUGH_ENV
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|v| (*name, v)))
        .collect();
    let note = match crate::device::start_adb_server(&env, &dir.join("adb-start-server.log")) {
        Ok(text) if text.trim().is_empty() => "tasks: adb server already running\n".to_string(),
        Ok(text) => format!(
            "tasks: adb start-server (detached, no lock descriptors): {}\n",
            text.trim()
        ),
        Err(e) => format!(
            "tasks: warning: {e}; the command's own adb calls may start a server that holds this job's locks\n"
        ),
    };
    append_log(dir, note.as_bytes());
}

/// When the command's RUSTC_WRAPPER is sccache, start (or touch) its server
/// from here with no inherited descriptors and its own session, before the
/// command runs: a server the command's first wrapper call autostarts would
/// detach from the job, inherit its admission lease and keep the slot held
/// until its idle timeout (cache_server.rs). The server gets the command's
/// own environment (`cmd`, already sanitized) minus TASKS_JOB_ID. Logged,
/// never fatal.
fn prepare_compiler_cache_server(dir: &Path, cmd: &Command) {
    let env: Vec<(std::ffi::OsString, std::ffi::OsString)> = cmd
        .get_envs()
        .filter(|(k, _)| *k != "TASKS_JOB_ID")
        .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
        .collect();
    let Some(wrapper) = env
        .iter()
        .find(|(k, _)| k == "RUSTC_WRAPPER")
        .map(|(_, v)| v.clone())
        .filter(|w| crate::cache_server::is_server_wrapper(w))
    else {
        return;
    };
    let outcome = crate::cache_server::ensure(&wrapper, &env, dir);
    append_log(dir, outcome.note().as_bytes());
}

/// The soft open-file limit admitted commands get: a normal login shell or CI runner has far more
/// than a service unit's 1024, and real-network test suites start many sockets.
const NOFILE_TARGET: libc::rlim_t = 65536;

/// Raises this process's soft `RLIMIT_NOFILE` to `NOFILE_TARGET` (never above the hard limit,
/// never lowering it); children inherit it. Returns the effective (soft, hard) limits.
fn raise_nofile() -> Option<(libc::rlim_t, libc::rlim_t)> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return None;
    }
    let want = NOFILE_TARGET.min(limit.rlim_max);
    if limit.rlim_cur < want {
        let raised = libc::rlimit {
            rlim_cur: want,
            rlim_max: limit.rlim_max,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
            limit.rlim_cur = want;
        }
    }
    Some((limit.rlim_cur, limit.rlim_max))
}

/// Admitted command runner. No arbitrary command/spec arguments are accepted.
/// If its supervisor dies it kills its own dedicated group, not another agent's group.
pub fn command(id: i64, stage: &str, supervisor: ProcessIdentity) -> Result<()> {
    let state = state()?;
    let dir = job_dir(&state, id);
    let store = database(&state)?;
    let job = with_ledger(&state, id, "command job read", || store.job(id))?;
    catalog::validate_claimed(&job)?;
    if !job.phase.active() {
        return Err("command requires active claimed job".into());
    }
    if let Some(line) = crate::placement::recorded_line(&state, id) {
        append_log(&dir, line.as_bytes());
    }
    if let Some((soft, hard)) = raise_nofile() {
        append_log(
            &dir,
            format!("tasks: open-file limit nofile={soft} (hard {hard})\n").as_bytes(),
        );
    }
    let worker = job
        .execution
        .as_ref()
        .and_then(|e| e.worker.clone())
        .ok_or("missing worker")?;
    let parent = identity(unsafe { libc::getppid() } as u32)?;
    if unsafe { libc::getpgid(0) } != parent.pid as i32 {
        return Err("command must be in its own admission process group".into());
    }
    let watchdog_dir = dir.clone();
    let watcher = Arc::new(AtomicBool::new(false));
    let done = watcher.clone();
    let watchdog = std::thread::spawn(move || {
        while !done.load(Ordering::SeqCst) {
            if !alive(&supervisor) || !alive(&worker) || !alive(&parent) {
                append_log(
                    &watchdog_dir,
                    b"command: supervisor/worker lost; terminating own admitted group\n",
                );
                unsafe {
                    libc::kill(-libc::getpgrp(), libc::SIGKILL);
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let result = run_command(&state, &dir, &job, stage);
    watcher.store(true, Ordering::SeqCst);
    let _ = watchdog.join();
    result
}

fn run_command(state: &Path, dir: &Path, job: &crate::Job, stage: &str) -> Result<()> {
    if job.spec.suite == crate::local_command::SUITE {
        let recipe = crate::local_command::Recipe::from_spec(&job.spec)?;
        let argv = recipe.argv_for(stage)?;
        if stage == "run" && recipe.stages().len() == 2 {
            let built: StageResult = load(&dir.join("build-result.json"))?;
            if built.timing.exit_code != Some(0) {
                return Err("local Cargo test requires successful compile stage".into());
            }
        }
        cache::require_state_space(state)?;
        let placement = if recipe.resource == crate::local_command::Resource::Offload {
            crate::local_command::offload_placement()
        } else {
            crate::local_command::place_outputs(Path::new(&job.spec.source), &argv)?
        };
        save(
            &dir.join(format!("{stage}-admitted.json")),
            &Admitted {
                at: now(),
                wait_seconds: (monotonic()?
                    - load::<f64>(&dir.join(format!("{stage}-waiting.json")))?)
                .max(0.0),
            },
        )?;
        append_log(
            dir,
            format!(
                "{}live cwd={} argv={}\n",
                placement.detail(),
                job.spec.source,
                serde_json::to_string(&argv)?
            )
            .as_bytes(),
        );
        let argv = lower_busy_jobs(dir, &recipe.resource, argv);
        let mut cmd = sanitized(Path::new(&argv[0]));
        cmd.args(&argv[1..]).current_dir(&job.spec.source);
        // tasks-offload names its per-job sidecar after this (cluster-snapshot.md).
        cmd.env("TASKS_JOB_ID", job.id.to_string());
        // Host wiring from the daemon's own environment (the worker unit), never the client's:
        // compile cache, Gradle home, and libclang for bindgen (NixOS has no libclang on a
        // search path; e.g. webrtc-audio-processing's bindgen needs both).
        for name in [
            "RUSTC_WRAPPER",
            "SCCACHE_DIR",
            "SCCACHE_CACHE_SIZE",
            "GRADLE_USER_HOME",
            "LIBCLANG_PATH",
            "BINDGEN_EXTRA_CLANG_ARGS",
        ] {
            if let Some(v) = std::env::var_os(name) {
                cmd.env(name, v);
            }
        }
        if let crate::local_command::Resource::Device(lock) = &recipe.resource {
            prepare_device_command(&mut cmd, dir, lock);
        }
        // An offload client compiles nothing here; every other stage may run
        // the wrapper and so must find a server that holds no lease.
        if recipe.resource != crate::local_command::Resource::Offload {
            prepare_compiler_cache_server(dir, &cmd);
        }
        let start = Instant::now();
        // This process has no other live children, so RUSAGE_CHILDREN around the
        // wait is exactly this command's tree (all waited-for descendants).
        let usage_before = crate::metrics::sampler::children_usage();
        let mut child = cmd.spawn()?;
        let out = drain(child.stdout.take().ok_or("missing stdout")?, dir.into());
        let err = drain(child.stderr.take().ok_or("missing stderr")?, dir.into());
        let status = child.wait()?;
        let _ = out.join();
        let _ = err.join();
        let usage = crate::metrics::sampler::usage_delta(
            &usage_before,
            &crate::metrics::sampler::children_usage(),
        );
        let _ = save(
            &dir.join(format!("{stage}{}", crate::metrics::sampler::USAGE_SUFFIX)),
            &usage,
        );
        save(
            &dir.join(format!("{stage}-result.json")),
            &StageResult {
                timing: StageTiming {
                    stage: stage.into(),
                    seconds: Some(start.elapsed().as_secs_f64()),
                    wait_seconds: 0.0,
                    exit_code: status.code(),
                    signal: status.signal(),
                },
                artifacts: vec![],
            },
        )?;
        if !status.success() {
            return Err(
                format!("command failed: {status} (recorded as the job's exit status)").into(),
            );
        }
        return Ok(());
    }
    if job.spec.suite == catalog::ANDROID_SUITE {
        return run_android(state, dir, job, stage);
    }
    let e = job.execution.as_ref().ok_or("missing execution")?;
    let snap = e.snapshot.as_ref().ok_or("missing source snapshot")?;
    snapshot::verify(&dir.join("snapshot"), &snap.digest)?;
    let target: PathBuf = load(&dir.join("target.json"))?;
    save(
        &dir.join(format!("{stage}-admitted.json")),
        &Admitted {
            at: now(),
            wait_seconds: (monotonic()? - load::<f64>(&dir.join(format!("{stage}-waiting.json")))?)
                .max(0.0),
        },
    )?;
    let started = Instant::now();
    let mut artifacts = Vec::new();
    let mut exit_code = Some(0);
    let mut signal = None;
    match stage {
        "build" => {
            // Recheck after shared admission: space may have changed while queued.
            let compile_cache: cache::CompileCache = load(&dir.join("compile-cache.json"))?;
            cache::require_state_space(state)?;
            cache::require_space(&[&target])?;
            // stdout still carries the exclusive target lease in this process.
            retention::require_budget(&target)?;
            if compile_cache.wrapper.is_some() {
                cache::require_space(&[&compile_cache.directory])?;
            }
            let mut cmd = sanitized(Path::new("cargo"));
            compile_cache.apply(&mut cmd);
            prepare_compiler_cache_server(dir, &cmd);
            cmd.current_dir(dir.join("snapshot"))
                .env("CARGO_TARGET_DIR", &target)
                .args([
                    "test",
                    "--locked",
                    "--offline",
                    "--lib",
                    "--bins",
                    "--tests",
                    "--no-run",
                    "--message-format=json",
                ]);
            append_log(dir, b"command: cargo test --locked --offline --lib --bins --tests --no-run --message-format=json\n");
            let mut child = cmd.spawn()?;
            let err = drain(
                child.stderr.take().ok_or("missing stderr")?,
                dir.to_path_buf(),
            );
            // Cargo JSON output is bounded by line and artifact count; logs are independently capped.
            let collected = Arc::new(Mutex::new(Vec::new()));
            let output = collected.clone();
            let out_dir = dir.to_path_buf();
            let reader = child.stdout.take().ok_or("missing stdout")?;
            let out = std::thread::spawn(move || {
                let mut reader = BufReader::new(reader);
                loop {
                    let mut line = Vec::new();
                    let read = (&mut reader).take(1024 * 1024).read_until(b'\n', &mut line);
                    match read {
                        Ok(0) | Err(_) => break,
                        _ => {}
                    }
                    append_log(&out_dir, &line);
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line)
                        && value["reason"] == "compiler-artifact"
                        && value["profile"]["test"] == true
                        && let Some(path) = value["executable"].as_str()
                        && let Ok(mut paths) = output.lock()
                        && paths.len() < 256
                    {
                        paths.push(PathBuf::from(path));
                    }
                }
            });
            let status = child.wait()?;
            let _ = out.join();
            let _ = err.join();
            exit_code = status.code();
            signal = status.signal();
            if status.success() {
                let paths = collected
                    .lock()
                    .map_err(|_| "artifact collector panicked")?;
                for path in paths.iter() {
                    let path = fs::canonicalize(path)?;
                    if !path.starts_with(&target) || !path.is_file() {
                        return Err("compiler artifact outside dedicated target".into());
                    }
                    artifacts.push(Artifact {
                        digest: digest(&path)?,
                        path,
                    });
                }
                artifacts.sort_by(|a, b| a.path.cmp(&b.path));
                artifacts.dedup_by(|a, b| a.path == b.path);
                if artifacts.is_empty() {
                    return Err("build produced no test harnesses".into());
                }
            }
        }
        "run" => {
            let build: StageResult = load(&dir.join("build-result.json"))?;
            if build.timing.exit_code != Some(0) || build.artifacts.is_empty() {
                return Err("no successful build artifacts".into());
            }
            for artifact in &build.artifacts {
                let path = fs::canonicalize(&artifact.path)?;
                if !path.starts_with(&target) || digest(&path)? != artifact.digest {
                    return Err("test artifact changed since compilation".into());
                }
                append_log(
                    dir,
                    format!("test harness: {}\n", path.display()).as_bytes(),
                );
                let mut cmd = sanitized(&path);
                cmd.current_dir(dir.join("snapshot"));
                let mut child = cmd.spawn()?;
                let out = drain(
                    child.stdout.take().ok_or("missing stdout")?,
                    dir.to_path_buf(),
                );
                let err = drain(
                    child.stderr.take().ok_or("missing stderr")?,
                    dir.to_path_buf(),
                );
                let status = child.wait()?;
                let _ = out.join();
                let _ = err.join();
                if !status.success() {
                    exit_code = status.code();
                    signal = status.signal();
                    break;
                }
            }
            artifacts = build.artifacts;
        }
        _ => return Err("unknown admitted command".into()),
    }
    let timing = StageTiming {
        stage: stage.into(),
        wait_seconds: 0.0,
        seconds: Some(started.elapsed().as_secs_f64()),
        exit_code,
        signal,
    };
    append_log(
        dir,
        format!(
            "{stage} result: exit={exit_code:?} signal={signal:?} runtime={:.3}s\n",
            timing.seconds.unwrap_or(0.0)
        )
        .as_bytes(),
    );
    save(
        &dir.join(format!("{stage}-result.json")),
        &StageResult { timing, artifacts },
    )?;
    // Buildlock should also report failure, rather than disguising a failed harness as success.
    if exit_code != Some(0) {
        return Err(format!("{stage} command failed").into());
    }
    Ok(())
}

/// Preserve admission's machine cache settings, rather than creating another
/// Gradle home or redirecting Cargo away from the lane's canonical target/.
fn android_command(
    program: &Path,
    host: &android_build::Host,
    layout: &android_build::Layout,
) -> Result<Command> {
    if std::env::var_os("CARGO_TARGET_DIR").is_some()
        || std::env::var_os("RUSTC_WORKSPACE_WRAPPER").is_some()
    {
        return Err("Android runner refuses target/workspace-wrapper environment overrides".into());
    }
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME missing")?);
    let gradle_home = home.join("build-cache/gradle");
    if std::env::var_os("GRADLE_USER_HOME")
        .map(PathBuf::from)
        .as_ref()
        != Some(&gradle_home)
    {
        return Err("Android runner requires admission's canonical shared Gradle home".into());
    }
    let mut cmd = sanitized(program);
    for name in [
        "RUSTC_WRAPPER",
        "SCCACHE_DIR",
        "SCCACHE_CACHE_SIZE",
        "GRADLE_USER_HOME",
    ] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    cmd.env("JAVA_HOME", &host.java)
        .env("ANDROID_HOME", &host.sdk)
        .env("ANDROID_SDK_ROOT", &host.sdk)
        .env("ANDROID_NDK_HOME", host.sdk.join("ndk/28.2.13676358"))
        .env("RUSTUP_TOOLCHAIN", &host.toolchain)
        .env("RUSTUP_AUTO_INSTALL", "0")
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_BUILD_JOBS", "2")
        .env("CARGO_INCREMENTAL", "0")
        .env("AOM_CACHE", layout.native())
        .current_dir(&layout.tree);
    Ok(cmd)
}
fn android_step(
    dir: &Path,
    name: &str,
    mut cmd: Command,
    steps: &mut Vec<StageTiming>,
) -> Result<()> {
    append_log(dir, format!("Android step: {name}\n").as_bytes());
    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let out = drain(child.stdout.take().ok_or("missing stdout")?, dir.to_owned());
    let err = drain(child.stderr.take().ok_or("missing stderr")?, dir.to_owned());
    let status = child.wait()?;
    let _ = out.join();
    let _ = err.join();
    steps.push(StageTiming {
        stage: name.into(),
        seconds: Some(start.elapsed().as_secs_f64()),
        exit_code: status.code(),
        signal: status.signal(),
        wait_seconds: 0.0,
    });
    save(&dir.join("android-steps.json"), steps)?;
    if !status.success() {
        return Err(format!("Android {name} failed: {status}").into());
    }
    Ok(())
}
fn run_android(state: &Path, dir: &Path, job: &crate::Job, stage: &str) -> Result<()> {
    if stage != "build" {
        return Err("Android has no run stage".into());
    }
    let abis = android::Abis::parse(catalog::android_contract(&job.spec)?)?;
    let snap = job
        .execution
        .as_ref()
        .and_then(|e| e.snapshot.as_ref())
        .ok_or("missing Android snapshot")?;
    let layout: android_build::Layout = load(&dir.join("android-layout.json"))?;
    let host: android_build::Host = load(&dir.join("android-host.json"))?;
    use std::os::fd::AsFd;
    let inherited_lease = File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
    android_build::verify_lease(&layout.root, &inherited_lease)?;
    workspace_snapshot::verify(&dir.join("snapshot"), &snap.digest)?;
    layout.verify(&snap.digest)?;
    host.verify()?;
    save(
        &dir.join("build-admitted.json"),
        &Admitted {
            at: now(),
            wait_seconds: (monotonic()? - load::<f64>(&dir.join("build-waiting.json"))?).max(0.0),
        },
    )?;
    // Recheck after queue wait, not merely before capture. Never lower reserves.
    cache::require_state_space(state)?;
    cache::require_space(&[&layout.root])?;
    retention::require_budget(&layout.root)?;
    let start = Instant::now();
    let mut steps = Vec::new();
    let mut rustc = android_command(&host.rust.join("bin/rustc"), &host, &layout)?;
    prepare_compiler_cache_server(dir, &rustc);
    rustc.arg("-vV");
    android_step(dir, "rustc-identity", rustc, &mut steps)?;
    let mut java = android_command(&host.java.join("bin/java"), &host, &layout)?;
    java.arg("-version");
    android_step(dir, "java-identity", java, &mut steps)?;
    let mut gradle_version = android_command(&host.gradle, &host, &layout)?;
    gradle_version.args(["--offline", "--no-daemon", "--version"]);
    android_step(dir, "gradle-identity", gradle_version, &mut steps)?;
    let (archive, hash, version): (PathBuf, String, String) =
        load(&dir.join("android-native.json"))?;
    android_build::seed_archive(
        &archive,
        &layout
            .native()
            .join(archive.file_name().ok_or("missing archive name")?),
        &hash,
    )?;
    let mut native_ids = std::collections::BTreeMap::new();
    for triple in abis.triples() {
        let mut cmd = android_command(Path::new("python3"), &host, &layout)?;
        cmd.arg("scripts/build_libaom.py")
            .args(["--target", triple, "--jobs", "2"]);
        android_step(dir, &format!("libaom:{triple}"), cmd, &mut steps)?;
        let prefix = layout.native().join(format!("install-v{version}-{triple}"));
        for path in [
            prefix.join("lib/libaom.a"),
            prefix.join("include/aom/aom_encoder.h"),
        ] {
            native_ids.insert(
                path.clone(),
                android_build::file_hash(&path, 512 * 1024 * 1024)?.0,
            );
        }
    }
    if layout.apk().try_exists()? {
        return Err("APK existed before assembly; refusing stale artifact".into());
    }
    let mut gradle = android_command(&host.gradle, &host, &layout)?;
    gradle
        .current_dir(layout.tree.join("android"))
        .args([
            "--no-daemon",
            "--offline",
            "--console=plain",
            "--max-workers=2",
            "--project-cache-dir",
        ])
        .arg(layout.root.join("project-cache"))
        .args([
            "-Dorg.gradle.java.installations.auto-download=false",
            "-Pandroid.builder.sdkDownload=false",
            ":app:assembleDebug",
        ])
        .arg(format!("-Pabis={}", abis.names()))
        .args([
            "-PrustRelease=false",
            "-PdevTools=true",
            "-PvideoHdCapture=false",
        ])
        .arg(format!("-PaomRoot={}", layout.native().display()));
    // Fresh project cache + source workDir identity force JNI regeneration; Cargo
    // can still reuse dependency compilation in the exclusively leased target/.
    gradle.arg("--rerun-tasks");
    android_step(dir, "assemble-debug", gradle, &mut steps)?;
    android_build::file_hash(&layout.apk(), android_build::MAX_APK)?;
    let mut verify = android_command(Path::new("python3"), &host, &layout)?;
    verify
        .args(["-c", android_build::CHECK_APK])
        .arg(layout.apk())
        .arg(abis.names());
    android_step(dir, "verify-apk-jni", verify, &mut steps)?;
    layout.verify(&snap.digest)?;
    host.verify()?;
    let provenance = serde_json::json!({"schema":1,"source":snap,"configuration":job.spec.configuration,"host":host,"native_archive_sha256":hash,"native_artifacts":native_ids,"steps":steps,"tests_run":false,"installed":false,"published":false});
    let artifact = android_build::publish(
        &layout,
        dir,
        &snap.digest,
        &job.spec.configuration,
        &provenance,
    )?;
    save(&dir.join("android-artifact.json"), &artifact)?;
    save(
        &dir.join("build-result.json"),
        &StageResult {
            timing: StageTiming {
                stage: "build".into(),
                seconds: Some(start.elapsed().as_secs_f64()),
                exit_code: Some(0),
                signal: None,
                wait_seconds: 0.0,
            },
            artifacts: vec![Artifact {
                path: dir.join(&artifact.path),
                digest: artifact.sha256,
            }],
        },
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_stopped_queue_says_who_stopped_it_and_how_to_resume() {
        let dir = std::env::temp_dir().join(format!("stopped-reason-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(stopped_reason(&dir, 1000), None);
        save(
            &dir.join(INTENT_FILE),
            &ExecutionIntent {
                enabled: false,
                changed_at: 1000,
                by: DRAIN.into(),
            },
        )
        .unwrap();
        let drain = stopped_reason(&dir, 1000 + 2 * 3600 + 5 * 60).unwrap();
        assert!(
            drain.contains("drain") && drain.contains("2h05m"),
            "{drain}"
        );
        assert!(drain.contains("tasks drain --cancel"), "{drain}");
        set_intent(&dir, false, "stop-worker").unwrap();
        let stop = stopped_reason(&dir, 1000 + 120).unwrap();
        assert!(
            stop.contains("stop-worker") && stop.contains("tasks start-worker"),
            "{stop}"
        );
        set_intent(&dir, true, "start-worker").unwrap();
        assert_eq!(stopped_reason(&dir, 5000), None);
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn a_jobs_busy_ledger_or_lost_supervisor_does_not_stop_the_worker_claiming() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );
        assert!(!stops_claims(&busy));
        assert!(!stops_claims(&SupervisorLost("exited".into())));
        let other: Box<dyn std::error::Error + Send + Sync> = "disk failure".into();
        assert!(stops_claims(other.as_ref()));
    }
    use super::*;
    fn temp_state(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "task-manager-worker-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    #[test]
    fn execution_intent_persists_and_stop_sticks_until_start() {
        let state = temp_state("intent");
        // Never decided (older release/fresh state): claims allowed, nothing auto-starts.
        assert!(intent(&state).is_none() && !stopped(&state));
        assert!(resume_after_restart(&state, 3).is_none());
        set_intent(&state, false, "stop-worker").unwrap();
        assert!(stopped(&state));
        let note = resume_after_restart(&state, 3).unwrap();
        assert!(
            note.contains("stays stopped") && note.contains("3 approved"),
            "{note}"
        );
        set_intent(&state, true, "start-worker").unwrap();
        assert!(!stopped(&state));
        let first = fs::metadata(state.join(INTENT_FILE))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        // Unchanged intent is not rewritten (no fsync per `tasks run`).
        set_intent(&state, true, "start-worker").unwrap();
        let second = fs::metadata(state.join(INTENT_FILE))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(first, second);
        // Enabled but nothing approved: nothing to start.
        assert!(resume_after_restart(&state, 0).is_none());
        // A garbage or oversized file is "never decided", never "stopped".
        fs::write(state.join(INTENT_FILE), vec![b'x'; 5000]).unwrap();
        assert!(intent(&state).is_none() && !stopped(&state));
        fs::remove_dir_all(state).unwrap();
    }
    #[test]
    fn lock_watch_never_takes_the_lock_it_watches() {
        let state = temp_state("lockwatch");
        let path = state.join("worker.lock");
        let held = File::create(&path).unwrap();
        assert!(!lock_held_by_anyone(&path));
        held.lock().unwrap();
        assert!(lock_held_by_anyone(&path));
        // Watching many times leaves the lock free for whoever starts next.
        held.unlock().unwrap();
        for _ in 0..50 {
            assert!(!lock_held_by_anyone(&path));
        }
        let other = File::open(&path).unwrap();
        assert!(
            other.try_lock().is_ok(),
            "the watch must not hold worker.lock"
        );
        fs::remove_dir_all(state).unwrap();
    }
    #[test]
    fn drain_stops_claims_until_the_next_daemon_start_resumes_it() {
        let state = temp_state("drain");
        set_intent(&state, false, "stop-worker").unwrap();
        assert!(!draining(&state));
        // A drain after a stop is still recorded (same `enabled`, different reason).
        set_intent(&state, false, DRAIN).unwrap();
        assert!(draining(&state) && stopped(&state));
        // The restart the drain was for: execution is enabled again (nothing approved, so
        // no worker is started here).
        assert!(resume_after_restart(&state, 0).is_none());
        let after = intent(&state).unwrap();
        assert!(after.enabled && !draining(&state), "{after:?}");
        assert_eq!(after.by, "drain-restart");
        fs::remove_dir_all(state).unwrap();
    }
    #[test]
    fn long_logs_keep_head_and_tail_around_one_marker() {
        let state = temp_state("headtail");
        let path = state.join("job.log");
        let mut text = String::from("cargo clippy: first warning line\n");
        for i in 0..20_000 {
            text.push_str(&format!("compiling line {i}\n"));
        }
        text.push_str("thread 'main' panicked at src/x.rs:1\ntest result: FAILED\n");
        fs::write(&path, &text).unwrap();
        let len = text.len() as u64;
        let shown = head_and_tail(&mut File::open(&path).unwrap(), len, 1024, 2048).unwrap();
        assert!(
            shown.starts_with("cargo clippy: first warning line\n"),
            "{shown}"
        );
        assert!(shown.ends_with("test result: FAILED\n"), "{shown}");
        assert!(shown.contains("panicked at src/x.rs:1"));
        assert_eq!(shown.matches("bytes of this log omitted").count(), 1);
        assert!(shown.len() < 1024 + 2048 + 200);
        // Whole lines on both sides of the marker.
        for line in shown.lines().filter(|l| l.starts_with("compiling")) {
            assert!(line.split(' ').count() == 3, "{line:?}");
        }
        // Short logs are returned whole, without a marker.
        let small = head_and_tail(&mut File::open(&path).unwrap(), 40, 1024, 2048).unwrap();
        assert_eq!(small, text[..40]);
        fs::remove_dir_all(state).unwrap();
    }
    #[test]
    fn elided_middle_keeps_important_lines_in_order() {
        let state = temp_state("headtailkeep");
        let path = state.join("job.log");
        let mut text = String::from("head line\n");
        for i in 0..5_000 {
            text.push_str(&format!("   Compiling filler{i}\n"));
        }
        text.push_str("     Running unittests src/lib.rs (target/debug/deps/a-1)\n");
        text.push_str("test a::b ... FAILED\n");
        text.push_str("thread 'a::b' panicked at src/a.rs:9:5:\n");
        text.push_str("error[E0308]: mismatched types\n");
        text.push_str("warning: unused variable: `x`\n");
        text.push_str(&format!(
            "test result: FAILED. 1 passed; {}\n",
            "x".repeat(500)
        ));
        for i in 0..5_000 {
            text.push_str(&format!("   Compiling more{i}\n"));
        }
        text.push_str("BUILD FAILED in 3s\n");
        fs::write(&path, &text).unwrap();
        let len = text.len() as u64;
        let shown = head_and_tail(&mut File::open(&path).unwrap(), len, 1024, 2048).unwrap();
        let at = |needle: &str| {
            shown
                .find(needle)
                .unwrap_or_else(|| panic!("{needle}: {shown}"))
        };
        let marker = at("bytes of this log omitted");
        assert!(marker < at("Running unittests"));
        assert!(at("Running unittests") < at("test a::b ... FAILED"));
        assert!(at("test a::b ... FAILED") < at("panicked at src/a.rs"));
        assert!(at("panicked at src/a.rs") < at("error[E0308]"));
        assert!(at("error[E0308]") < at("test result: FAILED. 1 passed;"));
        assert!(at("test result: FAILED. 1 passed;") < at("BUILD FAILED"));
        assert!(!shown.contains("unused variable"));
        // The long result line is cut; the marker counts the dropped lines.
        assert!(!shown.contains(&"x".repeat(300)));
        assert!(shown.contains(" lines; head and tail shown)"), "{shown}");
        assert_eq!(shown.matches("bytes of this log omitted").count(), 1);
        assert!(shown.len() < 1024 + 2048 + 1500, "{}", shown.len());
        fs::remove_dir_all(state).unwrap();
    }
    #[test]
    fn many_important_lines_keep_first_and_last_halves() {
        let state = temp_state("headtailcap");
        let path = state.join("job.log");
        let mut text = "head\n".repeat(100);
        for i in 0..1_000 {
            text.push_str(&format!("test t{i} ... FAILED\n"));
        }
        text.push_str(&"filler line\n".repeat(10_000));
        fs::write(&path, &text).unwrap();
        let len = text.len() as u64;
        let shown = head_and_tail(&mut File::open(&path).unwrap(), len, 64, 256).unwrap();
        let kept = shown.matches("... FAILED").count();
        assert_eq!(kept, IMPORTANT_KEPT, "{shown}");
        assert!(shown.contains("test t0 ... FAILED"));
        assert!(shown.contains("test t99 ... FAILED"));
        assert!(!shown.contains("test t100 ... FAILED"));
        assert!(shown.contains("test t999 ... FAILED"));
        assert!(
            shown.contains("800 more important lines omitted"),
            "{shown}"
        );
        assert!(shown.len() < 64 + 256 + IMPORTANT_KEPT * (IMPORTANT_LINE + 1) + 600);
        fs::remove_dir_all(state).unwrap();
    }
    #[test]
    fn own_io_reads_thread_counters_not_reaped_children() {
        assert_eq!(
            parse_io("rchar: 1\nwchar: 93190\nwrite_bytes: 819200\ncancelled_write_bytes: 0\n"),
            (819200, 93190)
        );
        assert!(own_io().is_some());
        // A reaped child's writes land in /proc/self/io (what `tasks worker`'s 4.9 GB was)
        // but not in this thread's own counter. (Other test threads may write meanwhile, so
        // only this thread is compared.)
        let thread = || parse_io(&fs::read_to_string("/proc/thread-self/io").unwrap());
        let whole = || parse_io(&fs::read_to_string("/proc/self/io").unwrap());
        let (t0, w0) = (thread(), whole());
        let status = Command::new("sh")
            .args(["-c", "head -c 3000000 /dev/zero > /dev/null"])
            .status()
            .unwrap();
        assert!(status.success());
        let (t1, w1) = (thread(), whole());
        assert!(t1.1 - t0.1 < 100_000, "{t0:?} -> {t1:?}");
        assert!(w1.1 - w0.1 >= 3_000_000, "{w0:?} -> {w1:?}");
    }
    #[test]
    fn sanitized_commands_keep_only_allowlisted_toolchain_env() {
        let cmd = sanitized(Path::new("cargo"));
        let names: Vec<_> = cmd.get_envs().map(|(key, _)| key.to_owned()).collect();
        assert!(
            names.iter().all(|key| [
                "HOME",
                "PATH",
                "RUSTUP_HOME",
                "CARGO_HOME",
                "XDG_RUNTIME_DIR",
                "LANG",
                "PKG_CONFIG_PATH",
                "NIX_LD",
                "NIX_LD_LIBRARY_PATH",
                "ANDROID_HOME",
                "ANDROID_SDK_ROOT",
                "JAVA_HOME",
                "CARGO_BUILD_JOBS"
            ]
            .contains(&key.to_str().unwrap())),
            "{names:?}"
        );
    }
    #[test]
    fn sanitized_commands_pass_android_sdk_and_jdk_locations_through() {
        // A worker unit's ANDROID_HOME/JAVA_HOME must reach Gradle, whose cwd
        // may lack local.properties; secrets-looking names stay out.
        for name in ["ANDROID_HOME", "ANDROID_SDK_ROOT", "JAVA_HOME"] {
            assert!(PASSTHROUGH_ENV.contains(&name), "{name}");
        }
        for name in PASSTHROUGH_ENV {
            let upper = name.to_ascii_uppercase();
            assert!(
                !["TOKEN", "SECRET", "PASSWORD", "KEY", "SSH_AUTH_SOCK"]
                    .iter()
                    .any(|bad| upper.contains(bad)),
                "{name}"
            );
        }
        let cmd = sanitized(Path::new("./gradlew"));
        for (key, value) in cmd.get_envs() {
            let key = key.to_str().unwrap();
            assert!(
                key == "CARGO_BUILD_JOBS" || PASSTHROUGH_ENV.contains(&key),
                "{key}"
            );
            if PASSTHROUGH_ENV.contains(&key) {
                assert_eq!(value, std::env::var_os(key).as_deref(), "{key}");
            }
        }
    }
    #[test]
    fn sanitized_commands_default_to_two_cargo_jobs_not_cpu_count() {
        let cmd = sanitized(Path::new("cargo"));
        assert_eq!(
            cmd.get_envs()
                .find(|(key, _)| *key == "CARGO_BUILD_JOBS")
                .unwrap()
                .1,
            Some(std::ffi::OsStr::new("2"))
        );
        // A raised TASKS_MAX_CARGO_JOBS only permits larger explicit -j; the
        // injected default is fixed and never read from the cap.
        assert_eq!(DEFAULT_CARGO_BUILD_JOBS, "2");
    }
    #[test]
    fn open_file_limit_is_raised_to_the_target_within_the_hard_limit() {
        let before = raise_nofile().unwrap();
        assert!(before.0 >= NOFILE_TARGET.min(before.1));
        // Idempotent, and never lowers.
        assert_eq!(raise_nofile().unwrap(), before);
    }
    #[test]
    fn proc_identity_detects_pid_reuse_without_signalling_it() {
        let mut current = identity(std::process::id()).unwrap();
        assert!(alive(&current));
        current.start_ticks += 1;
        assert!(!alive(&current));
        signal_group(&current, libc::SIGTERM).unwrap();
    }
    #[test]
    fn cancellation_only_stops_newly_owned_group() {
        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = identity(child.id()).unwrap();
        terminate_group(&mut child, &group, Duration::from_millis(300)).unwrap();
        assert!(!alive(&group));
        assert!(alive(&identity(std::process::id()).unwrap()));
    }
    /// A group whose leader traps SIGTERM and needs ~0.6 s to clean up.
    fn slow_cleanup_group() -> (Child, ProcessIdentity) {
        let mut child = Command::new("sh")
            .args([
                "-c",
                "trap 'sleep 0.6; exit 7' TERM; echo ready; sleep 30 & wait",
            ])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        // Wait until the trap is installed before signalling.
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let group = identity(child.id()).unwrap();
        (child, group)
    }
    #[test]
    fn default_grace_kills_a_slow_sigterm_handler() {
        let (mut child, group) = slow_cleanup_group();
        terminate_group(&mut child, &group, Duration::from_millis(300)).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
    }
    #[test]
    fn longer_grace_lets_the_sigterm_handler_finish_and_returns_early() {
        let (mut child, group) = slow_cleanup_group();
        let start = Instant::now();
        terminate_group(&mut child, &group, Duration::from_secs(5)).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(7), "{status:?}");
        // Escalation happens once the leader exits, not after the full grace.
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert!(!alive(&group));
    }
}
