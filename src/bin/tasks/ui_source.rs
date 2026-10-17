//! Where the window's snapshots come from (docs/design/cluster-snapshot.md). Every
//! source does its I/O on its own background thread and hands results over a
//! channel: `poll` never blocks the UI thread.
//!
//! - [`LocalSource`]: this machine. `tasks status --json` (feature/metrics); until
//!   that exists, the temporary adapter in `ui_legacy.rs` builds a partial
//!   snapshot from the daemon's `list` reply and /proc.
//! - [`RemoteSource`]: a long-running subprocess printing snapshots as JSON lines
//!   (`lan-peer status-watch ...`, `scripts/status-watch-ssh`), or a one-shot
//!   command printing one snapshot (`ssh worker tasks status --json`), which is
//!   then re-run as a poll. Command from `TASKS_UI_REMOTE_CMD` or the first line
//!   of `~/.config/tasks/ui-remote-cmd`.
//! - [`FixtureSource`]: snapshot files for demos and tests (`tasks ui --fixtures DIR`).
use crate::ui_model::Snapshot;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const REMOTE_CMD_ENV: &str = "TASKS_UI_REMOTE_CMD";
const LOCAL_INTERVAL: Duration = Duration::from_secs(2);
/// After `tasks status --json` failed (older daemon/binary), retry it this often.
const STATUS_RETRY: Duration = Duration::from_secs(60);
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_RESTART: Duration = Duration::from_secs(5);
const REMOTE_POLL_AGAIN: Duration = Duration::from_secs(2);
const MAX_DOCUMENT: usize = 8 << 20;

pub enum Update {
    /// `age_s`: how old the snapshot already was when it arrived (fixtures replay
    /// their recorded offsets; live sources send 0).
    Snapshot {
        snapshot: Box<Snapshot>,
        age_s: f64,
    },
    Error(String),
}

pub trait SnapshotSource {
    /// Card name until a snapshot names the machine ("laptop", "worker").
    fn label(&self) -> &str;
    /// The local daemon's machine: its jobs are cancellable from the window.
    fn local(&self) -> bool;
    /// False when nothing is configured; `poll` then explains how to configure it.
    fn configured(&self) -> bool {
        true
    }
    /// Everything that arrived since the last call. Never blocks.
    fn poll(&mut self) -> Vec<Update>;
}

fn drain(rx: &Receiver<Update>) -> Vec<Update> {
    rx.try_iter().collect()
}

// ---------------------------------------------------------------------------
// Local
// ---------------------------------------------------------------------------

pub struct LocalSource {
    rx: Receiver<Update>,
}

impl LocalSource {
    /// Starts the polling thread. `repaint` is called after each update.
    pub fn start(socket: PathBuf, repaint: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || local_loop(socket, tx, repaint));
        Self { rx }
    }
}

impl SnapshotSource for LocalSource {
    fn label(&self) -> &str {
        "laptop"
    }
    fn local(&self) -> bool {
        true
    }
    fn poll(&mut self) -> Vec<Update> {
        drain(&self.rx)
    }
}

fn local_loop(socket: PathBuf, tx: Sender<Update>, repaint: impl Fn()) {
    let mut status_failed_at: Option<Instant> = None;
    let mut sampler = crate::ui_legacy::Sampler::default();
    loop {
        let started = Instant::now();
        let mut update = None;
        if status_failed_at.is_none_or(|at| at.elapsed() >= STATUS_RETRY) {
            match status_json() {
                Ok(snapshot) => {
                    status_failed_at = None;
                    update = Some(Update::Snapshot {
                        snapshot: Box::new(snapshot),
                        age_s: 0.0,
                    });
                }
                Err(_) => status_failed_at = Some(Instant::now()),
            }
        }
        let update = update.unwrap_or_else(|| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::ui_legacy::poll(&socket, &mut sampler)
            })) {
                Ok(Ok(snapshot)) => Update::Snapshot {
                    snapshot: Box::new(snapshot),
                    age_s: 0.0,
                },
                Ok(Err(error)) => Update::Error(format!("local daemon: {error}")),
                Err(_) => Update::Error("local snapshot thread panicked".into()),
            }
        });
        if tx.send(update).is_err() {
            return; // window closed
        }
        repaint();
        std::thread::sleep(LOCAL_INTERVAL.saturating_sub(started.elapsed()));
    }
}

/// `tasks status --json` from this same binary (feature/metrics). Fails on a
/// binary without the subcommand; the caller then uses the legacy adapter.
fn status_json() -> Result<Snapshot, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(exe);
    command.args(["status", "--json"]);
    let text = output_with_timeout(command, STATUS_TIMEOUT)?;
    Snapshot::parse_str(&text)
}

fn output_with_timeout(mut command: Command, timeout: Duration) -> Result<String, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout
            .by_ref()
            .take(MAX_DOCUMENT as u64)
            .read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let text = reader.join().map_err(|_| "reader panicked")?;
    if status.success() {
        Ok(text)
    } else {
        Err(format!("exited with {status}"))
    }
}

// ---------------------------------------------------------------------------
// Remote: JSON-lines subprocess
// ---------------------------------------------------------------------------

/// The remote status command: `TASKS_UI_REMOTE_CMD`, else the first non-comment
/// line of `$XDG_CONFIG_HOME/tasks/ui-remote-cmd` (default `~/.config`).
pub fn remote_command() -> Option<String> {
    let env = std::env::var(REMOTE_CMD_ENV).ok();
    let config = || {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        std::fs::read_to_string(base.join("tasks/ui-remote-cmd")).ok()
    };
    pick_command(env.as_deref(), config().as_deref())
}

pub fn pick_command(env: Option<&str>, config: Option<&str>) -> Option<String> {
    env.map(str::trim)
        .filter(|c| !c.is_empty())
        .map(String::from)
        .or_else(|| {
            config?
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .map(String::from)
        })
}

pub struct RemoteSource {
    rx: Option<Receiver<Update>>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    reported: bool,
}

impl RemoteSource {
    /// `command` runs under `sh -c` in its own process group; None = not configured.
    pub fn start(command: Option<String>, repaint: impl Fn() + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let rx = command.map(|command| {
            let (tx, rx) = mpsc::channel();
            let (stop, child) = (stop.clone(), child.clone());
            std::thread::spawn(move || remote_loop(command, tx, stop, child, repaint));
            rx
        });
        Self {
            rx,
            stop,
            child,
            reported: false,
        }
    }
}

impl Drop for RemoteSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut child) = self.child.lock()
            && let Some(child) = child.as_mut()
        {
            kill_group(child);
        }
    }
}

fn kill_group(child: &mut Child) {
    // Own process group (see spawn), so ssh and friends go too.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGTERM);
    }
    let _ = child.kill();
}

impl SnapshotSource for RemoteSource {
    fn label(&self) -> &str {
        "worker"
    }
    fn local(&self) -> bool {
        false
    }
    fn configured(&self) -> bool {
        self.rx.is_some()
    }
    fn poll(&mut self) -> Vec<Update> {
        match &self.rx {
            Some(rx) => drain(rx),
            None if !self.reported => {
                self.reported = true;
                vec![Update::Error(format!(
                    "no remote status command: set {REMOTE_CMD_ENV} (e.g. `lan-peer status-watch …` or scripts/status-watch-ssh)"
                ))]
            }
            None => Vec::new(),
        }
    }
}

/// Splits a byte stream into JSON documents: one per line (JSON lines) or one
/// pretty-printed document spread over several lines.
#[derive(Default)]
pub struct DocumentSplitter {
    buffer: String,
}

impl DocumentSplitter {
    /// Feeds one line; returns a complete document's parse result, if any.
    pub fn push(&mut self, line: &str) -> Option<Result<Snapshot, String>> {
        if self.buffer.is_empty() && line.trim().is_empty() {
            return None;
        }
        self.buffer.push_str(line);
        self.buffer.push('\n');
        match serde_json::from_str::<serde_json::Value>(&self.buffer) {
            Ok(value) => {
                self.buffer.clear();
                Some(Snapshot::parse(value))
            }
            Err(e) if e.is_eof() && self.buffer.len() < MAX_DOCUMENT => None,
            Err(e) => {
                self.buffer.clear();
                Some(Err(format!("unreadable status line: {e}")))
            }
        }
    }
}

fn remote_loop(
    command: String,
    tx: Sender<Update>,
    stop: Arc<AtomicBool>,
    slot: Arc<Mutex<Option<Child>>>,
    repaint: impl Fn(),
) {
    let send = |update| {
        let sent = tx.send(update).is_ok();
        repaint();
        sent
    };
    while !stop.load(Ordering::SeqCst) {
        let spawned = Command::new("sh")
            .args(["-c", &command])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                if !send(Update::Error(format!("remote status command: {e}"))) {
                    return;
                }
                sleep_unless(&stop, REMOTE_RESTART);
                continue;
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        let last_err = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = stderr {
            let last_err = last_err.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        *last_err.lock().unwrap_or_else(|e| e.into_inner()) = line;
                    }
                }
            });
        }
        let mut documents = 0;
        let mut splitter = DocumentSplitter::default();
        if let Some(stdout) = stdout {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let update = match splitter.push(&line) {
                    None => continue,
                    Some(Ok(snapshot)) => {
                        documents += 1;
                        Update::Snapshot {
                            snapshot: Box::new(snapshot),
                            age_s: 0.0,
                        }
                    }
                    Some(Err(e)) => Update::Error(e),
                };
                if !send(update) {
                    return;
                }
            }
        }
        let status = slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .map(|mut c| c.wait());
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let ok = matches!(&status, Some(Ok(s)) if s.success());
        if ok && documents > 0 {
            // A one-shot command (ssh … tasks status --json): poll it again.
            sleep_unless(&stop, REMOTE_POLL_AGAIN);
            continue;
        }
        let stderr = last_err.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let status = match status {
            Some(Ok(s)) => s.to_string(),
            Some(Err(e)) => e.to_string(),
            None => "unknown status".into(),
        };
        let detail = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        };
        if !send(Update::Error(format!(
            "remote status command {status}{detail}; restarting in {} s",
            REMOTE_RESTART.as_secs()
        ))) {
            return;
        }
        sleep_unless(&stop, REMOTE_RESTART);
    }
}

fn sleep_unless(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// One snapshot file, replayed every few seconds like a live stream. The newest
/// `taken_at` across the directory counts as "now", so an older file reads as a
/// machine last seen that long ago (the offline fixture).
pub struct FixtureSource {
    label: String,
    local: bool,
    snapshot: Snapshot,
    age_s: f64,
    last: Option<Instant>,
}

impl FixtureSource {
    /// Every `*.json` in `dir`, sorted by name; host role first.
    pub fn open_dir(dir: &Path) -> Result<Vec<Self>, String> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        let mut sources = Vec::new();
        for file in files {
            let text =
                std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let snapshot =
                Snapshot::parse_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
            let local = snapshot.machine.role.as_deref() == Some("host");
            let label = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            sources.push(Self {
                label,
                local,
                snapshot,
                age_s: 0.0,
                last: None,
            });
        }
        if sources.is_empty() {
            return Err(format!("{}: no *.json snapshot files", dir.display()));
        }
        let newest = sources
            .iter()
            .filter_map(|s| s.snapshot.taken_at)
            .fold(f64::MIN, f64::max);
        for source in &mut sources {
            source.age_s = source
                .snapshot
                .taken_at
                .map(|t| (newest - t).max(0.0))
                .unwrap_or(0.0);
        }
        sources.sort_by_key(|s| !s.local);
        Ok(sources)
    }
}

impl SnapshotSource for FixtureSource {
    fn label(&self) -> &str {
        &self.label
    }
    fn local(&self) -> bool {
        self.local
    }
    fn poll(&mut self) -> Vec<Update> {
        if self.last.is_some_and(|at| at.elapsed() < LOCAL_INTERVAL) {
            return Vec::new();
        }
        self.last = Some(Instant::now());
        vec![Update::Snapshot {
            snapshot: Box::new(self.snapshot.clone()),
            age_s: self.age_s,
        }]
    }
}
