//! Native Linux admission. Stable flock inodes interoperate with existing Python
//! clients; holder/wait notes are diagnostics, never authority. No lock deletion.
use crate::{Result, execution::now, worker};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
// Conservative trial headroom, not a reservation or running-process quota.
/// Default MemAvailable reserve; overridable per daemon/worker by MEMORY_ENV.
pub const MIN_MEMORY: u64 = 8 * 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
/// Whole MiB, 1024..=1048576. Default 8192 (8 GiB).
pub const MEMORY_ENV: &str = "TASKS_MIN_AVAILABLE_MEMORY_MIB";
/// Whole GiB of free space kept on cache/target disks, 1..=65536. Default 20.
pub const CACHE_FREE_ENV: &str = "TASKS_MIN_CACHE_FREE_GIB";
/// Whole MiB of free space kept on the state disk, 64..=1048576. Default 256.
pub const STATE_FREE_ENV: &str = "TASKS_MIN_STATE_FREE_MIB";
/// Largest explicit Cargo `-j`/`--jobs`/`CARGO_BUILD_JOBS` a live command may
/// request, 1..=64. Default 2. Raising it never changes the injected default of 2.
pub const MAX_CARGO_JOBS_ENV: &str = "TASKS_MAX_CARGO_JOBS";
pub const DEFAULT_MAX_CARGO_JOBS: u64 = 2;
/// Written by each worker at start so `tasks admission` in another shell reports
/// what the worker actually applies, not that shell's own environment.
pub const THRESHOLD_RECORD: &str = "admission-thresholds.json";
// PSI gates are fixed policy; only reported, never configurable here.
const CPU_SOME_MAX: f64 = 40.0;
const MEMORY_SOME_MAX: f64 = 1.0;
const MEMORY_FULL_MAX: f64 = 0.5;
const IO_SOME_MAX: f64 = 20.0;
const IO_FULL_MAX: f64 = 10.0;

struct Limit {
    env: &'static str,
    unit: u64,
    unit_name: &'static str,
    floor: u64,
    max: u64,
}
const MEMORY_LIMIT: Limit = Limit {
    env: MEMORY_ENV,
    unit: MIB,
    unit_name: "MiB",
    floor: 1024,
    max: 1024 * 1024,
};
const CACHE_LIMIT: Limit = Limit {
    env: CACHE_FREE_ENV,
    unit: GIB,
    unit_name: "GiB",
    floor: 1,
    max: 64 * 1024,
};
const STATE_LIMIT: Limit = Limit {
    env: STATE_FREE_ENV,
    unit: MIB,
    unit_name: "MiB",
    floor: 64,
    max: 1024 * 1024,
};
const CARGO_JOBS_LIMIT: Limit = Limit {
    env: MAX_CARGO_JOBS_ENV,
    unit: 1,
    unit_name: "jobs",
    floor: 1,
    max: 64,
};
impl Limit {
    fn parse(&self, raw: Option<OsString>, default: u64) -> Result<u64> {
        let Some(raw) = raw else {
            return Ok(default);
        };
        let error = || -> Box<dyn std::error::Error + Send + Sync> {
            let shown: String = raw.to_string_lossy().chars().take(32).collect();
            format!(
                "{} must be a whole number of {} from {} to {} (no sign, spaces or suffix); got {shown:?}",
                self.env, self.unit_name, self.floor, self.max
            )
            .into()
        };
        let text = raw.to_str().ok_or_else(error)?;
        if text.is_empty() || text.len() > 10 || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(error());
        }
        let value: u64 = text.parse().map_err(|_| error())?;
        if !(self.floor..=self.max).contains(&value) {
            return Err(error());
        }
        Ok(value * self.unit)
    }
    fn admits(&self, bytes: u64) -> bool {
        bytes.is_multiple_of(self.unit) && (self.floor..=self.max).contains(&(bytes / self.unit))
    }
}
/// Effective admission reserves for this daemon/worker process. Defaults are the
/// historical constants; explicit environment overrides are validated, never clamped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thresholds {
    pub min_available_memory_bytes: u64,
    pub min_cache_free_bytes: u64,
    pub min_state_free_bytes: u64,
    /// Records from earlier releases lack this field and mean the default.
    #[serde(default = "default_max_cargo_jobs")]
    pub max_cargo_jobs: u64,
}
fn default_max_cargo_jobs() -> u64 {
    DEFAULT_MAX_CARGO_JOBS
}
impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_available_memory_bytes: MIN_MEMORY,
            min_cache_free_bytes: crate::cache::MIN_FREE_BYTES,
            min_state_free_bytes: crate::cache::MIN_STATE_FREE_BYTES,
            max_cargo_jobs: DEFAULT_MAX_CARGO_JOBS,
        }
    }
}
impl Thresholds {
    pub fn from_lookup(get: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let d = Self::default();
        Ok(Self {
            min_available_memory_bytes: MEMORY_LIMIT
                .parse(get(MEMORY_ENV), d.min_available_memory_bytes)?,
            min_cache_free_bytes: CACHE_LIMIT.parse(get(CACHE_FREE_ENV), d.min_cache_free_bytes)?,
            min_state_free_bytes: STATE_LIMIT.parse(get(STATE_FREE_ENV), d.min_state_free_bytes)?,
            max_cargo_jobs: CARGO_JOBS_LIMIT.parse(get(MAX_CARGO_JOBS_ENV), d.max_cargo_jobs)?,
        })
    }
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var_os(name))
    }
    pub fn overridden(get: impl Fn(&str) -> Option<OsString>) -> bool {
        [
            MEMORY_ENV,
            CACHE_FREE_ENV,
            STATE_FREE_ENV,
            MAX_CARGO_JOBS_ENV,
        ]
        .iter()
        .any(|name| get(name).is_some())
    }
    /// Same bounds as the environment, so a tampered record cannot lower a floor.
    fn valid(&self) -> bool {
        MEMORY_LIMIT.admits(self.min_available_memory_bytes)
            && CACHE_LIMIT.admits(self.min_cache_free_bytes)
            && STATE_LIMIT.admits(self.min_state_free_bytes)
            && CARGO_JOBS_LIMIT.admits(self.max_cargo_jobs)
    }
}
static ACTIVE: OnceLock<std::result::Result<Thresholds, String>> = OnceLock::new();
/// Resolved once per process from its own environment. Invalid values fail closed.
pub fn thresholds() -> Result<Thresholds> {
    ACTIVE
        .get_or_init(|| Thresholds::from_env().map_err(|e| e.to_string()))
        .clone()
        .map_err(Into::into)
}
/// Client-side early check only, so a client shell reports what the worker
/// applies: the last worker's valid record, else this process's own value.
/// Not authority: daemon approval and the worker re-validate with their own.
pub fn submission_max_cargo_jobs() -> Result<u64> {
    let (report, _) = report_thresholds(
        &crate::state_dir(),
        thresholds(),
        Thresholds::overridden(|name| std::env::var_os(name)),
    )?;
    Ok(report.effective.max_cargo_jobs)
}
fn memory_text(bytes: u64) -> String {
    if bytes.is_multiple_of(GIB) {
        format!("{} GiB", bytes / GIB)
    } else {
        format!("{} MiB", bytes / MIB)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ThresholdReport {
    /// "worker-record", "environment" or "default".
    pub source: String,
    #[serde(flatten)]
    pub effective: Thresholds,
    pub cpu_some_avg10_max: f64,
    pub memory_some_avg10_max: f64,
    pub memory_full_avg10_max: f64,
    pub io_some_avg10_max: f64,
    pub io_full_avg10_max: f64,
    pub recorded_by_pid: Option<u32>,
    pub recorded_at: Option<i64>,
}
impl ThresholdReport {
    pub fn new(source: &str, effective: Thresholds, record: Option<&Record>) -> Self {
        Self {
            source: source.into(),
            effective,
            cpu_some_avg10_max: CPU_SOME_MAX,
            memory_some_avg10_max: MEMORY_SOME_MAX,
            memory_full_avg10_max: MEMORY_FULL_MAX,
            io_some_avg10_max: IO_SOME_MAX,
            io_full_avg10_max: IO_FULL_MAX,
            recorded_by_pid: record.map(|r| r.pid),
            recorded_at: record.map(|r| r.recorded_at),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub schema: u32,
    pub pid: u32,
    pub recorded_at: i64,
    pub thresholds: Thresholds,
}
/// Atomic, user-only diagnostic record. Not authority: each process still
/// applies its own environment; this only makes reports match the worker.
pub fn record_worker_thresholds(state: &Path, thresholds: &Thresholds) -> Result<()> {
    let record = Record {
        schema: 1,
        pid: std::process::id(),
        recorded_at: now(),
        thresholds: *thresholds,
    };
    let temp = state.join(format!(
        ".{THRESHOLD_RECORD}.{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    out.write_all(&serde_json::to_vec_pretty(&record)?)?;
    out.sync_all()?;
    fs::rename(&temp, state.join(THRESHOLD_RECORD))?;
    Ok(())
}
fn read_record(state: &Path) -> Result<Option<Record>> {
    let path = state.join(THRESHOLD_RECORD);
    let m = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.len() > 4096 {
        return Err("unsafe/oversized worker threshold record".into());
    }
    let record: Record = serde_json::from_slice(&fs::read(&path)?)?;
    if record.schema != 1 || !record.thresholds.valid() {
        return Err("worker threshold record outside accepted bounds".into());
    }
    Ok(Some(record))
}
/// Thresholds to REPORT from a client shell: the last worker's record if valid,
/// else this process's own configuration. Mismatches become warnings.
pub fn report_thresholds(
    state: &Path,
    own: Result<Thresholds>,
    own_overridden: bool,
) -> Result<(ThresholdReport, Vec<String>)> {
    let mut warnings = vec![];
    match read_record(state) {
        Ok(Some(record)) => {
            match &own {
                Ok(t) if own_overridden && *t != record.thresholds => warnings.push(
                    "this shell's TASKS_MIN_*/TASKS_MAX_CARGO_JOBS settings differ from the worker's recorded thresholds; the worker applies its own environment".into(),
                ),
                Err(e) => warnings.push(format!("this shell's threshold environment is invalid: {e}")),
                _ => (),
            }
            return Ok((
                ThresholdReport::new("worker-record", record.thresholds, Some(&record)),
                warnings,
            ));
        }
        Ok(None) => (),
        Err(e) => warnings.push(format!("{}: {e}", state.join(THRESHOLD_RECORD).display())),
    }
    let own = own?;
    let source = if own_overridden {
        "environment"
    } else {
        "default"
    };
    Ok((ThresholdReport::new(source, own, None), warnings))
}
#[derive(Clone, Debug)]
pub enum Kind {
    Compile,
    Run,
    Device(String),
}
impl Kind {
    fn label(&self) -> String {
        match self {
            Self::Compile => "compile".into(),
            Self::Run => "run".into(),
            Self::Device(s) => format!("device {s}"),
        }
    }
    fn names(&self) -> Result<Vec<String>> {
        Ok(match self {
            Self::Compile => vec!["p2p-build.lock".into()],
            Self::Run => (0..4).map(|i| format!("p2p-run-{i}.lock")).collect(),
            Self::Device(s) => {
                // Do not silently alias two device names to one sanitized identity.
                if s.is_empty()
                    || s.len() > 128
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                {
                    return Err("invalid device serial".into());
                }
                vec![format!("p2p-device-{s}.lock")]
            }
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pressure {
    pub available_bytes: u64,
    pub cpu_some_avg10: f64,
    pub memory_some_avg10: f64,
    pub memory_full_avg10: f64,
    pub io_some_avg10: f64,
    pub io_full_avg10: f64,
}
impl Pressure {
    pub fn read() -> Result<Self> {
        Self::parse(
            &fs::read_to_string("/proc/meminfo")?,
            &fs::read_to_string("/proc/pressure/cpu")?,
            &fs::read_to_string("/proc/pressure/memory")?,
            &fs::read_to_string("/proc/pressure/io")?,
        )
    }
    fn parse(memory: &str, cpu: &str, memory_psi: &str, io_psi: &str) -> Result<Self> {
        let kb: u64 = memory
            .lines()
            .find_map(|l| l.strip_prefix("MemAvailable:"))
            .ok_or("MemAvailable missing")?
            .split_whitespace()
            .next()
            .ok_or("memory value missing")?
            .parse()?;
        fn psi(text: &str, category: &str) -> Result<f64> {
            let value: f64 = text
                .lines()
                .find(|l| l.split_whitespace().next() == Some(category))
                .and_then(|l| l.split_whitespace().find_map(|v| v.strip_prefix("avg10=")))
                .ok_or("required PSI metric missing")?
                .parse()?;
            if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                return Err("invalid PSI metric".into());
            }
            Ok(value)
        }
        Ok(Self {
            available_bytes: kb.checked_mul(1024).ok_or("memory overflow")?,
            cpu_some_avg10: psi(cpu, "some")?,
            memory_some_avg10: psi(memory_psi, "some")?,
            memory_full_avg10: psi(memory_psi, "full")?,
            io_some_avg10: psi(io_psi, "some")?,
            io_full_avg10: psi(io_psi, "full")?,
        })
    }
    fn permits(&self) -> bool {
        self.blocker().is_none()
    }
    fn blocker(&self) -> Option<String> {
        match thresholds() {
            Ok(t) => self.blocker_with(&t),
            Err(e) => Some(format!("invalid admission threshold configuration: {e}")),
        }
    }
    /// Names the effective memory threshold; PSI gates are unchanged policy.
    pub fn blocker_with(&self, thresholds: &Thresholds) -> Option<String> {
        if [
            self.cpu_some_avg10,
            self.memory_some_avg10,
            self.memory_full_avg10,
            self.io_some_avg10,
            self.io_full_avg10,
        ]
        .iter()
        .any(|value| !value.is_finite() || !(0.0..=100.0).contains(value))
        {
            Some("invalid pressure metric".into())
        } else if self.available_bytes < thresholds.min_available_memory_bytes {
            Some(format!(
                "available memory below {}",
                memory_text(thresholds.min_available_memory_bytes)
            ))
        } else if self.cpu_some_avg10 > CPU_SOME_MAX {
            Some("CPU PSI some avg10 above 40%".into())
        } else if self.memory_some_avg10 > MEMORY_SOME_MAX {
            Some("memory PSI some avg10 above 1%".into())
        } else if self.memory_full_avg10 > MEMORY_FULL_MAX {
            Some("memory PSI full avg10 above 0.5%".into())
        } else if self.io_some_avg10 > IO_SOME_MAX {
            Some("I/O PSI some avg10 above 20%".into())
        } else if self.io_full_avg10 > IO_FULL_MAX {
            Some("I/O PSI full avg10 above 10%".into())
        } else {
            None
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Note {
    pub who: String,
    pub cmd: String,
    #[serde(default)]
    pub since: f64,
    pub pid: u32,
    #[serde(default)]
    pub start_ticks: Option<u64>,
    #[serde(default)]
    pub slot: Option<String>,
}
impl Note {
    fn alive(&self) -> bool {
        worker::identity(self.pid)
            .is_ok_and(|p| self.start_ticks.is_none_or(|s| s == p.start_ticks) && worker::alive(&p))
    }
}
#[derive(Debug, Serialize)]
pub struct ResourceStatus {
    pub path: PathBuf,
    pub busy: bool,
    pub holder: Option<Note>,
}
#[derive(Debug, Serialize)]
pub struct Status {
    pub backend: &'static str,
    pub pressure: Pressure,
    pub pressure_blocker: Option<String>,
    pub thresholds: ThresholdReport,
    /// Live cache-disk space (`~/build-cache`) against the effective reserve.
    pub cache_disk: Option<crate::cache::DiskSpace>,
    /// Live state-disk space against the effective reserve.
    pub state_disk: Option<crate::cache::DiskSpace>,
    /// Which disk reserve currently makes jobs wait before admission, if any.
    pub disk_blocker: Option<String>,
    pub resources: Vec<ResourceStatus>,
    pub waiting: Vec<Note>,
    pub warnings: Vec<String>,
}
pub fn root() -> PathBuf {
    PathBuf::from(format!("/tmp/claude-{}", unsafe { libc::geteuid() }))
}
fn check_root(root: &Path) -> Result<()> {
    let m = fs::symlink_metadata(root)?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } {
        return Err("admission directory must be real and user-owned".into());
    }
    Ok(())
}
fn open_lock(path: &Path, create: bool) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = f.metadata()?;
    if !m.is_file() || m.nlink() != 1 || m.uid() != unsafe { libc::geteuid() } {
        return Err("unsafe admission lock inode".into());
    }
    Ok(f)
}
fn take(f: &File) -> Result<bool> {
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if e.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(e.into())
    }
}
fn read_note(path: &Path) -> Result<Note> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.len() > 16384 {
        return Err("unsafe/oversized admission note".into());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
struct Wait {
    path: PathBuf,
    ino: u64,
    dev: u64,
}
impl Wait {
    fn new(root: &Path, note: &Note) -> Result<Self> {
        let path = root.join(format!(
            "p2p-wait-{}-{}-{}.json",
            std::process::id(),
            note.start_ticks
                .ok_or("native wait requires process start identity")?,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        f.write_all(&serde_json::to_vec(note)?)?;
        let m = f.metadata()?;
        Ok(Self {
            path,
            ino: m.ino(),
            dev: m.dev(),
        })
    }
}
impl Drop for Wait {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.ino() == self.ino && m.dev() == self.dev)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
pub struct Lease {
    file: File,
    holder: PathBuf,
    pid: u32,
}
impl Lease {
    /// Retain admission through command exec and parent-loss cleanup, not only
    /// while the admission process is alive. No user output uses this fd.
    pub fn inherit(&self) -> Result<()> {
        let fd = self.file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        // The lock stays held by inherited children until their descriptors close.
        if read_note(&self.holder).is_ok_and(|n| n.pid == self.pid) {
            let _ = fs::remove_file(&self.holder);
        }
    }
}
fn older_waiter(root: &Path, me: &Wait, slot: &str) -> Result<bool> {
    let own = fs::metadata(&me.path)?.modified()?;
    for (i, entry) in fs::read_dir(root)?.enumerate() {
        if i >= 4096 {
            return Err("admission directory exceeds scan bound".into());
        }
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if path == me.path || !name.starts_with("p2p-wait-") || !name.ends_with(".json") {
            continue;
        }
        // Invalid/stale diagnostics do not acquire resources or block indefinitely.
        if let Ok(note) = read_note(&path)
            // Legacy notes have no process-start token: PID reuse could turn a
            // stale note into a permanent queue blocker. Observe, don't order.
            && note.start_ticks.is_some()
            && note.slot.as_deref() == Some(slot)
            && note.alive()
            && let Ok(m) = fs::symlink_metadata(&path)
            && let Ok(t) = m.modified()
            && (t < own || (t == own && path < me.path))
        {
            return Ok(true);
        }
    }
    Ok(false)
}
pub fn acquire(kind: Kind, who: &str, cmd: &str, stop: impl Fn() -> bool) -> Result<Option<Lease>> {
    acquire_at(&root(), kind, who, cmd, Pressure::read, stop)
}
fn acquire_at(
    root: &Path,
    kind: Kind,
    who: &str,
    cmd: &str,
    pressure: impl Fn() -> Result<Pressure>,
    stop: impl Fn() -> bool,
) -> Result<Option<Lease>> {
    fs::create_dir_all(root)?;
    check_root(root)?;
    let names = kind.names()?;
    let files = names
        .iter()
        .map(|n| open_lock(&root.join(n), true))
        .collect::<Result<Vec<_>>>()?;
    let identity = worker::identity(std::process::id())?;
    let note = Note {
        who: who.chars().take(128).collect(),
        cmd: cmd.chars().take(200).collect(),
        since: now() as f64,
        pid: identity.pid,
        start_ticks: Some(identity.start_ticks),
        slot: Some(kind.label()),
    };
    let wait = Wait::new(root, &note)?;
    let mut previous_blocker = None;
    loop {
        if stop() {
            return Ok(None);
        }
        let gated = !matches!(kind, Kind::Device(_));
        let ready = if gated {
            let p = pressure()?;
            let blocker = p.blocker();
            let permits = blocker.is_none();
            if blocker != previous_blocker {
                if let Some(reason) = &blocker {
                    eprintln!("Native admission waiting: {reason}; inspect tasks admission");
                }
                previous_blocker = blocker;
            }
            permits
        } else {
            true
        };
        if ready && !older_waiter(root, &wait, &kind.label())? {
            for (name, f) in names.iter().zip(files.iter()) {
                if !take(f)? {
                    continue;
                }
                if gated && !pressure()?.permits() {
                    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
                    continue;
                }
                let holder = root.join(name).with_extension("holder");
                let temp = root.join(format!(
                    ".native-holder-{}-{}",
                    identity.pid,
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                let mut out = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&temp)?;
                out.write_all(&serde_json::to_vec(&note)?)?;
                fs::rename(&temp, &holder)?;
                return Ok(Some(Lease {
                    file: f.try_clone()?,
                    holder,
                    pid: identity.pid,
                }));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
pub fn status() -> Result<Status> {
    let (report, warnings) = report_thresholds(
        &crate::state_dir(),
        thresholds(),
        Thresholds::overridden(|name| std::env::var_os(name)),
    )?;
    let state = crate::state_dir();
    let mut status = status_with(&root(), Pressure::read()?, report, warnings)?;
    let t = status.thresholds.effective;
    let cache_disk =
        crate::cache::cache_root().and_then(|cache| crate::cache::cache_space_with(&cache, &t));
    let state_disk = crate::cache::state_space_with(&state, &t);
    with_disks(&mut status, cache_disk, state_disk);
    Ok(status)
}
/// Additive disk fields; a measurement error becomes a warning, not a failure.
fn with_disks(
    status: &mut Status,
    cache: Result<crate::cache::DiskSpace>,
    state: Result<crate::cache::DiskSpace>,
) {
    let mut keep = |disk: Result<crate::cache::DiskSpace>, name: &str| match disk {
        Ok(disk) => Some(disk),
        Err(e) => {
            status
                .warnings
                .push(format!("{name} disk not measured: {e}"));
            None
        }
    };
    let cache = keep(cache, "cache");
    let state = keep(state, "state");
    status.disk_blocker = crate::cache::disk_blocker(cache.as_ref(), state.as_ref());
    status.cache_disk = cache;
    status.state_disk = state;
}
#[cfg(test)]
fn status_at(root: &Path, pressure: Pressure) -> Result<Status> {
    status_with(
        root,
        pressure,
        ThresholdReport::new("default", Thresholds::default(), None),
        vec![],
    )
}
fn status_with(
    root: &Path,
    pressure: Pressure,
    thresholds: ThresholdReport,
    warnings: Vec<String>,
) -> Result<Status> {
    let mut status = Status {
        backend: "rust-flock-v1",
        pressure_blocker: pressure.blocker_with(&thresholds.effective),
        pressure,
        thresholds,
        cache_disk: None,
        state_disk: None,
        disk_blocker: None,
        resources: vec![],
        waiting: vec![],
        warnings,
    };
    if !root.try_exists()? {
        return Ok(status);
    }
    check_root(root)?;
    for (i, entry) in fs::read_dir(root)?.enumerate() {
        if i >= 4096 {
            return Err("admission directory exceeds scan bound".into());
        }
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with("p2p-")
            && name.ends_with(".lock")
            && (name == "p2p-build.lock"
                || name.starts_with("p2p-run-")
                || name.starts_with("p2p-device-"))
        {
            let f = open_lock(&path, false)?;
            let busy = !take(&f)?;
            let holder = read_note(&path.with_extension("holder"))
                .ok()
                .filter(|n| busy && n.alive());
            status.resources.push(ResourceStatus { path, busy, holder });
        } else if name.starts_with("p2p-wait-") && name.ends_with(".json") {
            match read_note(&path) {
                Ok(n) if n.alive() => status.waiting.push(n),
                Ok(_) => (),
                Err(e) => status.warnings.push(format!("{}: {e}", path.display())),
            }
        }
    }
    status.resources.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(status)
}
#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
