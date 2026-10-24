//! Fixtures shared by the unit tests: jobs, an in-memory policy file system and a harness that
//! drives `run_cli` end to end against a [`FakeClient`].

use crate::client::{Client, ClientError};
use crate::fake::FakeClient;
use crate::runtime::{Pacer, Signals};
use crate::{Deps, run_cli};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tasks_core::{JobId, Kind, Phase, Placement};
use tasks_daemon::api::{
    AlertView, ApiError, DaemonView, ErrorCode, JobView, PressureView, Request, Response,
    RunningView, SettingView, SlotGroupView, SlotView, Source, StatusSnapshot, WaiterView,
};
use tasks_policy::PolicyFs;

pub const NOW: i64 = 1_800_000_000_000;

pub fn argv(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

pub fn job(id: i64, owner: &str, cwd: &str, phase: Phase, cmd: &str) -> JobView {
    let finished = phase.is_terminal();
    JobView {
        id: JobId(id),
        owner: owner.into(),
        argv: argv(cmd),
        cwd: PathBuf::from(cwd),
        kind: Kind::Test,
        placement: Placement::Here,
        after: vec![],
        exit: match &phase {
            Phase::Passed => Some(0),
            Phase::Failed { exit } | Phase::TimedOut { exit } => *exit,
            _ => None,
        },
        phase,
        wait_text: None,
        summary: None,
        submitted_at_ms: NOW - 100_000,
        started_at_ms: Some(NOW - 90_000),
        finished_at_ms: finished.then_some(NOW - 30_000),
        machine: Some("laptop".into()),
        forwarded_to: None,
        cancel_requested_by: None,
        cancel_reason: None,
        timeout_secs: None,
        remote_phase: None,
        plan: None,
        suite: None,
        tags: Vec::new(),
        log_path: None,
        figures: None,
        priority: Default::default(),
        priority_reason: None,
        remote_wait: None,
        fetch_paths: Vec::new(),
        fetch: Vec::new(),
        sync: None,
    }
}

pub fn api_err(code: ErrorCode, msg: &str) -> ClientError {
    ClientError::Daemon(ApiError::new(code, msg))
}

/// Shares one fake between the test (to read its requests) and the boxed client in `Deps`.
pub struct Shared(pub Arc<FakeClient>);

impl Client for Shared {
    fn call(&self, req: &Request) -> Result<Response, ClientError> {
        self.0.call(req)
    }
}

/// Raises `sig` once `after` polls have been made.
pub struct ScriptedSignals {
    pub polls: Cell<usize>,
    pub after: usize,
    pub sig: i32,
}

impl Signals for ScriptedSignals {
    fn pending(&self) -> Option<i32> {
        self.polls.set(self.polls.get() + 1);
        (self.polls.get() > self.after).then_some(self.sig)
    }
}

pub struct NoPause;

impl Pacer for NoPause {
    fn pause(&self, _d: Duration) {}
}

/// A writer that fails like a closed pipe.
pub struct ClosedPipe;

impl Write for ClosedPipe {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }
}

/// Records each `write_all` as one piece.
#[derive(Default)]
pub struct Pieces(pub Vec<String>);

impl Write for Pieces {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.push(String::from_utf8_lossy(buf).into_owned());
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct MemFs {
    pub files: BTreeMap<PathBuf, String>,
    /// What `cargo metadata` prints here; `None` makes it fail.
    pub metadata: Option<String>,
}

impl PolicyFs for MemFs {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
    fn exists(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }
    fn cargo_metadata(&self, _cwd: &Path) -> io::Result<String> {
        self.metadata
            .clone()
            .ok_or_else(|| io::Error::other("cargo metadata failed: no manifest"))
    }
}

pub struct Harness {
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub files: BTreeMap<PathBuf, String>,
    pub fs: MemFs,
    pub lock_dir: Option<PathBuf>,
    pub signal_after: Option<(usize, i32)>,
}

impl Default for Harness {
    fn default() -> Harness {
        let mut env = BTreeMap::new();
        env.insert("TASKS_WHO".to_string(), "me".to_string());
        env.insert("HOME".to_string(), "/home/u".to_string());
        Harness {
            env,
            cwd: PathBuf::from("/w/repo"),
            files: BTreeMap::new(),
            fs: MemFs::default(),
            lock_dir: Some(PathBuf::from("/state/slots")),
            signal_after: None,
        }
    }
}

pub struct Run {
    pub code: i32,
    pub out: String,
    pub err: String,
}

impl Harness {
    /// Runs `tasks2 ARGS...` against `client`; output goes to strings.
    pub fn run(&self, client: &Arc<FakeClient>, args: &[&str]) -> Run {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = self.run_with(client, args, &mut out, &mut err);
        Run {
            code,
            out: String::from_utf8_lossy(&out).into_owned(),
            err: String::from_utf8_lossy(&err).into_owned(),
        }
    }

    pub fn run_with(
        &self,
        client: &Arc<FakeClient>,
        args: &[&str],
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> i32 {
        let mut full = vec!["tasks2".to_string()];
        full.extend(args.iter().map(|s| s.to_string()));
        let env = |k: &str| self.env.get(k).cloned();
        let now = || NOW;
        let files = &self.files;
        let read = |p: &Path| {
            files
                .get(p)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        };
        let signals = ScriptedSignals {
            polls: Cell::new(0),
            after: self.signal_after.map_or(usize::MAX, |s| s.0),
            sig: self.signal_after.map_or(0, |s| s.1),
        };
        let shared = client.clone();
        let lock = self.lock_dir.clone();
        let client_for = move |_: &crate::args::Cli| -> (Box<dyn Client>, Option<PathBuf>) {
            (Box::new(Shared(shared.clone())), lock.clone())
        };
        let deps = Deps {
            env: &env,
            cwd: self.cwd.clone(),
            now_ms: &now,
            pacer: &NoPause,
            signals: &signals,
            read_input: &read,
            policy_fs: &self.fs,
            history: None,
            client_for: &client_for,
        };
        run_cli(&full, out, err, &deps)
    }
}

pub fn status_snapshot() -> StatusSnapshot {
    StatusSnapshot {
        daemon: DaemonView {
            pid: 42,
            version: "0.1.0".into(),
            machine: "laptop".into(),
            state_dir: "/state".into(),
            lock_dir: "/state/slots".into(),
            policy_file: "/cfg/machine-policy.toml".into(),
            policy_file_loaded: true,
            policy_problems: vec!["slots.compile: 9 is above 8".into()],
            draining: false,
            drained_by: None,
            drained_at_ms: None,
            execution_enabled: true,
            uptime_secs: 3725,
            now_ms: NOW,
        },
        slots: vec![SlotGroupView {
            kind: "compile".into(),
            total: 2,
            free: 0,
            busy: vec![
                SlotView {
                    index: 0,
                    job: Some(JobId(5)),
                    external: false,
                    holder: None,
                },
                SlotView {
                    index: 1,
                    job: None,
                    external: true,
                    holder: Some("pid 9 cargo (from job 3, which has ended)".into()),
                },
            ],
        }],
        waiting: vec![WaiterView {
            id: JobId(6),
            owner: "me".into(),
            command: "cargo test".into(),
            wait_text: "compile slots busy".into(),
            position: 1,
            start_estimate_secs: Some(120.0),
            rough: false,
            class: String::new(),
            class_note: String::new(),
            on_peer: None,
        }],
        waiting_more: 2,
        running: vec![RunningView {
            id: JobId(5),
            owner: "lead".into(),
            command: "cargo build".into(),
            slot: Some(("compile".into(), 0)),
            elapsed_secs: 75.0,
            done_targets: 3,
            total_targets: 10,
            current: Some("tasks-core".into()),
            eta_secs: Some(200.0),
            usual_secs: None,
            over_by_secs: None,
            eta_rough_from: None,
            no_eta_why: None,
            log_bytes: 10,
            figures: None,
            cores: None,
            mem_bytes: None,
        }],
        pressure: PressureView {
            memory: "ok".into(),
            memory_some: Some(1.5),
            memory_full: None,
            memory_stall_limit: None,
            cpu: Some(40.0),
            io: None,
            memory_total: None,
            available_memory: Some(8 << 30),
            cpu_busy_pct: None,
            cpu_cores: None,
            cores_in_use: None,
            swap_total: None,
            swap_used: None,
            swap_in_per_s: None,
            swap_out_per_s: None,
            cache_free: Some(100 << 30),
            state_free: None,
            compile_blocker: Some("memory stalls 12% over 10%".into()),
            warning: None,
        },
        peers: Vec::new(),
        attribution: String::new(),
        transfer: None,
        trips: Vec::new(),
        power: None,
        endpoint: None,
        alerts: vec![AlertView {
            id: "a".into(),
            level: "warn".into(),
            message: "disk low".into(),
            since_ms: 1,
        }],
        settings: vec![
            SettingView {
                key: "slots.compile".into(),
                value: "2".into(),
                source: Source::File,
            },
            SettingView {
                key: "cargo.max_jobs".into(),
                value: "2".into(),
                source: Source::Default,
            },
            SettingView {
                key: "cancel_grace_ms".into(),
                value: "500".into(),
                source: Source::Env,
            },
        ],
    }
}
