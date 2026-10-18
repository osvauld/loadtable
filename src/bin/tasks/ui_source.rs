//! Where the window's snapshots come from (docs/design/cluster-snapshot.md). Every
//! source does its I/O on its own background thread and hands results over a
//! channel: `poll` never blocks the UI thread.
//!
//! - [`LocalSource`]: this machine. The daemon's `status` request, in-process (with a
//!   daemon that lacks it, the temporary adapter in `ui_legacy.rs` builds a partial
//!   snapshot from the daemon's `list` reply and /proc). Demand-driven: while the
//!   window does not take snapshots (hidden, minimised) nothing is fetched.
//!
//! Every source hands over through a [`Mailbox`] that keeps only the newest
//! snapshot (and a later error), so an undrawn window holds one snapshot per
//! source, never a backlog.
//! - [`RemoteSource`]: a long-running subprocess printing snapshots as JSON lines
//!   (`lan-peer status-watch ...`, `scripts/status-watch-ssh`), or a one-shot
//!   command printing one snapshot (`ssh worker tasks status --json`), which is
//!   then re-run as a poll. Command from `TASKS_UI_REMOTE_CMD` or the first line
//!   of `~/.config/tasks/ui-remote-cmd`.
//! - [`FixtureSource`]: snapshot files for demos and tests (`tasks ui --fixtures DIR`).
use crate::ui_model::Snapshot;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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

// ---------------------------------------------------------------------------
// Mailbox: the latest update only
// ---------------------------------------------------------------------------

/// Hand-over from a source thread to the window that keeps only what the window would end
/// up showing: the newest snapshot and an error that arrived after it. An unbounded channel
/// grew without limit while the window was not drawn (a hidden or minimised Wayland window
/// gets no frames, so nothing drained it): ~hundreds of KB per parsed snapshot, two feeds,
/// every 2 s, GiBs after a few hours. Applying [`Mailbox::take`]'s result in order gives
/// the same `Feed` state as applying every update ever sent.
#[derive(Default)]
pub struct Mailbox {
    pending: Mutex<Pending>,
    taken: Condvar,
}

#[derive(Default)]
struct Pending {
    /// Newest snapshot, its source age and when it was handed over.
    snapshot: Option<(Box<Snapshot>, f64, Instant)>,
    /// An error newer than `snapshot` (an older one is cleared by the snapshot anyway).
    error: Option<String>,
}

impl Mailbox {
    pub fn send(&self, update: Update) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        match update {
            Update::Snapshot { snapshot, age_s } => {
                pending.snapshot = Some((snapshot, age_s, Instant::now()));
                pending.error = None;
            }
            Update::Error(error) => pending.error = Some(error),
        }
    }

    /// Everything the window has not seen yet, at most a snapshot and an error, in order.
    /// A snapshot that waited here counts as older by that wait.
    pub fn take(&self) -> Vec<Update> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::with_capacity(2);
        if let Some((snapshot, age_s, at)) = pending.snapshot.take() {
            out.push(Update::Snapshot {
                snapshot,
                age_s: age_s + at.elapsed().as_secs_f64(),
            });
        }
        if let Some(error) = pending.error.take() {
            out.push(Update::Error(error));
        }
        drop(pending);
        self.taken.notify_all();
        out
    }

    /// Blocks while the last snapshot has not been taken (the window is not drawing), up to
    /// `timeout`; true when nothing is pending any more.
    pub fn wait_taken(&self, timeout: Duration) -> bool {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let (pending, _) = self
            .taken
            .wait_timeout_while(pending, timeout, |p| p.snapshot.is_some())
            .unwrap_or_else(|e| e.into_inner());
        pending.snapshot.is_none()
    }
}

// ---------------------------------------------------------------------------
// Local
// ---------------------------------------------------------------------------

pub struct LocalSource {
    mailbox: Arc<Mailbox>,
    stop: Arc<AtomicBool>,
}

impl LocalSource {
    /// Starts the polling thread. `repaint` is called after each update.
    pub fn start(socket: PathBuf, repaint: impl Fn() + Send + 'static) -> Self {
        let mailbox = Arc::new(Mailbox::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (m, s) = (mailbox.clone(), stop.clone());
        std::thread::spawn(move || local_loop(socket, &m, &s, repaint));
        Self { mailbox, stop }
    }
}

impl Drop for LocalSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
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
        self.mailbox.take()
    }
}

/// How long the local loop waits for the window to take a snapshot before it checks again
/// whether it should stop. While the window is not drawn nothing is fetched at all, so a
/// hidden window costs the daemon nothing.
const UNCONSUMED_RECHECK: Duration = Duration::from_secs(30);

fn local_loop(socket: PathBuf, mailbox: &Mailbox, stop: &AtomicBool, repaint: impl Fn()) {
    let mut status_failed_at: Option<Instant> = None;
    let mut sampler = crate::ui_legacy::Sampler::default();
    while !stop.load(Ordering::SeqCst) {
        let started = Instant::now();
        let mut update = None;
        if status_failed_at.is_none_or(|at| at.elapsed() >= STATUS_RETRY) {
            match status_snapshot(&socket) {
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
        mailbox.send(update);
        repaint();
        std::thread::sleep(LOCAL_INTERVAL.saturating_sub(started.elapsed()));
        // Demand-driven: the next fetch waits until the window took this one.
        while !stop.load(Ordering::SeqCst) && !mailbox.wait_taken(UNCONSUMED_RECHECK) {}
    }
}

/// The daemon's `status` snapshot, requested in-process (it used to spawn `tasks status
/// --json` every 2 s). Fails against a daemon without `status`; the caller then uses the
/// legacy adapter.
fn status_snapshot(socket: &Path) -> Result<Snapshot, String> {
    crate::ensure_daemon(socket).map_err(|e| e.to_string())?;
    let reply =
        tinline_task_manager::request_status(socket, STATUS_TIMEOUT).map_err(|e| e.to_string())?;
    let status = reply.status.ok_or_else(|| {
        reply
            .error
            .unwrap_or_else(|| "daemon returned no status".into())
    })?;
    Snapshot::parse(serde_json::to_value(&status).map_err(|e| e.to_string())?)
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
    mailbox: Option<Arc<Mailbox>>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    reported: bool,
}

impl RemoteSource {
    /// `command` runs under `sh -c` in its own process group; None = not configured.
    pub fn start(command: Option<String>, repaint: impl Fn() + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let mailbox = command.map(|command| {
            let mailbox = Arc::new(Mailbox::default());
            let (m, stop, child) = (mailbox.clone(), stop.clone(), child.clone());
            std::thread::spawn(move || remote_loop(command, &m, stop, child, repaint));
            mailbox
        });
        Self {
            mailbox,
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
        self.mailbox.is_some()
    }
    fn poll(&mut self) -> Vec<Update> {
        match &self.mailbox {
            Some(mailbox) => mailbox.take(),
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
    mailbox: &Mailbox,
    stop: Arc<AtomicBool>,
    slot: Arc<Mutex<Option<Child>>>,
    repaint: impl Fn(),
) {
    let send = |update| {
        mailbox.send(update);
        repaint();
        !stop.load(Ordering::SeqCst)
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

#[cfg(test)]
mod mailbox_tests {
    use super::*;

    fn snapshot(n: u64) -> Update {
        let text = format!(
            r#"{{"version":1,"taken_at":{n},"machine":{{"id":"synthetic-host"}},"jobs":[{{"id":{n},"phase":"running","owner":"synthetic-agent@host","argv":["cargo","test","pad{}"]}}]}}"#,
            "x".repeat((n % 97) as usize)
        );
        Update::Snapshot {
            snapshot: Box::new(Snapshot::parse_str(&text).unwrap()),
            age_s: 0.0,
        }
    }

    fn taken_at(update: &Update) -> Option<f64> {
        match update {
            Update::Snapshot { snapshot, .. } => snapshot.taken_at,
            Update::Error(_) => None,
        }
    }

    /// A window that is not drawn never drains: the mailbox holds one snapshot, not N.
    #[test]
    fn an_undrained_mailbox_keeps_only_the_newest_snapshot() {
        let mailbox = Mailbox::default();
        for n in 1..=10_000 {
            mailbox.send(snapshot(n));
        }
        let updates = mailbox.take();
        assert_eq!(updates.len(), 1);
        assert_eq!(taken_at(&updates[0]), Some(10_000.0));
        assert!(mailbox.take().is_empty(), "taken once");
    }

    /// Coalescing gives the window the same end state as replaying every update in order.
    #[test]
    fn coalesced_updates_end_in_the_same_state_as_the_full_sequence() {
        let cases: Vec<Vec<Option<u64>>> = vec![
            vec![Some(1), None, Some(2)],
            vec![Some(1), Some(2), None],
            vec![None, None],
            vec![None, Some(3)],
            vec![Some(4), None, None],
        ];
        for case in cases {
            let mailbox = Mailbox::default();
            // (snapshot taken_at, error) after replaying the full sequence.
            let mut replayed: (Option<f64>, Option<String>) = (None, None);
            for (i, item) in case.iter().enumerate() {
                match item {
                    Some(n) => {
                        mailbox.send(snapshot(*n));
                        replayed = (Some(*n as f64), None);
                    }
                    None => {
                        mailbox.send(Update::Error(format!("error {i}")));
                        replayed.1 = Some(format!("error {i}"));
                    }
                }
            }
            let mut state: (Option<f64>, Option<String>) = (None, None);
            for update in mailbox.take() {
                match update {
                    Update::Snapshot { snapshot, .. } => state = (snapshot.taken_at, None),
                    Update::Error(e) => state.1 = Some(e),
                }
            }
            assert_eq!(state, replayed, "case {case:?}");
        }
    }

    /// A snapshot that waited for the window is shown as that much older.
    #[test]
    fn a_waiting_snapshot_ages() {
        let mailbox = Mailbox::default();
        mailbox.send(snapshot(1));
        std::thread::sleep(Duration::from_millis(120));
        match &mailbox.take()[0] {
            Update::Snapshot { age_s, .. } => assert!(*age_s >= 0.1, "{age_s}"),
            Update::Error(e) => panic!("{e}"),
        }
    }

    /// The local loop blocks while its last snapshot is unconsumed and wakes when taken.
    #[test]
    fn wait_taken_blocks_until_the_window_takes_the_snapshot() {
        let mailbox = Arc::new(Mailbox::default());
        assert!(
            mailbox.wait_taken(Duration::from_millis(1)),
            "nothing pending"
        );
        mailbox.send(snapshot(1));
        assert!(
            !mailbox.wait_taken(Duration::from_millis(50)),
            "still pending"
        );
        let m = mailbox.clone();
        let taker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            m.take().len()
        });
        let started = Instant::now();
        assert!(mailbox.wait_taken(Duration::from_secs(10)));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(taker.join().unwrap(), 1);
        // Errors alone never block the producer.
        mailbox.send(Update::Error("synthetic".into()));
        assert!(mailbox.wait_taken(Duration::from_millis(1)));
    }
}
