//! Native Linux admission. Stable flock inodes interoperate with existing Python
//! clients; holder/wait notes are diagnostics, never authority. No lock deletion.
use crate::{Result, execution::now, worker};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
// Conservative trial headroom, not a reservation or running-process quota.
const MIN_MEMORY: u64 = 8 * 1024 * 1024 * 1024;
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
    fn blocker(&self) -> Option<&'static str> {
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
            Some("invalid pressure metric")
        } else if self.available_bytes < MIN_MEMORY {
            Some("available memory below 8 GiB")
        } else if self.cpu_some_avg10 > 40.0 {
            Some("CPU PSI some avg10 above 40%")
        } else if self.memory_some_avg10 > 1.0 {
            Some("memory PSI some avg10 above 1%")
        } else if self.memory_full_avg10 > 0.5 {
            Some("memory PSI full avg10 above 0.5%")
        } else if self.io_some_avg10 > 20.0 {
            Some("I/O PSI some avg10 above 20%")
        } else if self.io_full_avg10 > 10.0 {
            Some("I/O PSI full avg10 above 10%")
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
    pub pressure_blocker: Option<&'static str>,
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
            if blocker != previous_blocker {
                if let Some(reason) = blocker {
                    eprintln!("Native admission waiting: {reason}; inspect tasks admission");
                }
                previous_blocker = blocker;
            }
            p.permits()
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
    status_at(&root(), Pressure::read()?)
}
fn status_at(root: &Path, pressure: Pressure) -> Result<Status> {
    let mut status = Status {
        backend: "rust-flock-v1",
        pressure_blocker: pressure.blocker(),
        pressure,
        resources: vec![],
        waiting: vec![],
        warnings: vec![],
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
