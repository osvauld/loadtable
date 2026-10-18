#![cfg(target_os = "linux")]
//! Idle budget of the daemon with a realistic history: N finished jobs (and their runtime
//! estimates) in an isolated state dir, lock dir, HOME and no peer config. Checks that the
//! hot requests stay small (reply bytes) and cheap (daemon CPU from /proc/PID/stat), that the
//! cached listing follows commits made by other connections, and that an idle daemon with
//! that history uses next to no CPU.
//!
//! CPU budgets here are deliberately loose so a debug build on a busy machine passes; the
//! release numbers (and the proposed node budgets) come from `scripts/idle_bench.py`.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tinline_task_manager::{
    Action, Cost, JobSpec, PROTOCOL_VERSION, Phase, Request, Response, Store, request,
};

const JOBS: i64 = 730;

struct Daemon {
    root: PathBuf,
    child: Child,
}

impl Daemon {
    fn start(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!("tasks-idle-{label}-{}", std::process::id()));
        assert!(!root.exists(), "refusing to reuse another fixture");
        for dir in ["state", "locks", "home"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        seed(&root.join("state/tasks.sqlite"));
        let child = Command::new(env!("CARGO_BIN_EXE_tasks"))
            .arg("daemon")
            .env("TASK_MANAGER_STATE_DIR", root.join("state"))
            .env("TASKS_LOCK_DIR", root.join("locks"))
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("home/.config"))
            .env("XDG_STATE_HOME", root.join("home/.state"))
            .env("TASKS_PEER_CONFIG", root.join("home/no-peer.toml"))
            .env("TASKS_HEALTH_NOTIFY", "off")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Self { root, child };
        let end = Instant::now() + Duration::from_secs(20);
        while request(&daemon.socket(), Action::Job(1)).is_err() {
            assert!(Instant::now() < end, "daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }
    fn socket(&self) -> PathBuf {
        self.root.join("state/taskd.sock")
    }
    /// One raw request: the reply line's size and its parsed form.
    fn raw(&self, action: Action) -> (usize, serde_json::Value) {
        let mut stream = UnixStream::connect(self.socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut line = serde_json::to_vec(&Request {
            version: PROTOCOL_VERSION,
            action,
        })
        .unwrap();
        line.push(b'\n');
        stream.write_all(&line).unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).unwrap();
        assert!(reply.ends_with('\n'), "incomplete reply");
        (reply.len(), serde_json::from_str(&reply).unwrap())
    }
    /// Daemon CPU (user + system) in seconds.
    fn cpu(&self) -> f64 {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.child.id())).unwrap();
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let ticks: f64 = fields[11].parse::<f64>().unwrap() + fields[12].parse::<f64>().unwrap();
        ticks / unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64
    }
    /// Daemon CPU milliseconds per request of `action`, over `count` requests.
    fn cpu_ms(&self, action: &Action, count: u32) -> f64 {
        let before = self.cpu();
        for _ in 0..count {
            self.raw(action.clone());
        }
        (self.cpu() - before) * 1000.0 / f64::from(count)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Synthetic history shaped like a real ledger: finished local commands with stages and
/// metrics (~1.2 KB of JSON each) and about one runtime estimate per job.
fn seed(path: &Path) {
    drop(Store::open(path).unwrap()); // schema
    let mut db = rusqlite::Connection::open(path).unwrap();
    let tx = db.transaction().unwrap();
    for id in 1..=JOBS {
        let configuration = serde_json::json!({
            "argv": ["cargo", "test", "--locked", "-j", "2", "-p", format!("crate{}", id % 40),
                     format!("filter_{id}")],
            "resource": if id % 3 == 0 { "run" } else { "compile" },
        })
        .to_string();
        let spec = serde_json::json!({
            "suite": "local.command", "owner": format!("synthetic-agent-{}", id % 12),
            "source": format!("/tmp/synthetic-checkouts/repo-{}", id % 9),
            "revision": "live-workspace", "configuration": configuration,
            "cost": "cpu_simulation",
        });
        let phase = ["passed", "failed", "passed", "cancelled"][(id % 4) as usize];
        let at = 1_790_000_000 + id * 60;
        let execution = serde_json::json!({
            "approved_at": at, "started_at": at, "finished_at": at + 50,
            "cancel_requested": false, "worker": {"pid": 100_000 + id, "start_ticks": 7_000_000 + id},
            "supervisor": null, "snapshot": null,
            "stages": [
                {"stage": "build", "wait_seconds": 0.02, "seconds": 41.5, "exit_code": 0, "signal": null},
                {"stage": "run", "wait_seconds": 0.01, "seconds": 8.25, "exit_code": 0, "signal": null}
            ],
            "android_trial": false, "local_command": true,
            "detail": "Local command finished; inspect retained log",
            "metrics": {"warmth": "warm", "warmth_reason": "synthetic", "wall_s": 49.75,
                        "cpu_user_s": 80.5, "cpu_sys_s": 9.25, "peak_rss_bytes": 912_345_678u64,
                        "crates_compiled": 12}
        });
        tx.execute(
            "INSERT INTO jobs(id, spec, phase, submitted_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, spec.to_string(), phase, at],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO executions(job_id, data) VALUES (?1, ?2)",
            rusqlite::params![id, execution.to_string()],
        )
        .unwrap();
        for stage in ["build", "worker.wait_build"] {
            if stage == "build" || id % 10 == 0 {
                tx.execute(
                    "INSERT OR REPLACE INTO runtimes VALUES ('local.command', ?1, ?2, ?3)",
                    rusqlite::params![
                        configuration,
                        stage,
                        r#"{"mean":41.5,"variance":3.5,"samples":2}"#
                    ],
                )
                .unwrap();
            }
        }
    }
    tx.commit().unwrap();
}

fn spec(owner: &str) -> JobSpec {
    JobSpec {
        suite: "local.command".into(),
        owner: owner.into(),
        source: "/tmp/synthetic-checkouts/new".into(),
        revision: "live-workspace".into(),
        configuration: r#"{"argv":["true"],"resource":"run"}"#.into(),
        cost: Cost::CpuSimulation,
    }
}

#[test]
fn hot_replies_are_small_and_the_listing_is_unchanged() {
    let d = Daemon::start("replies");
    // The full listing still carries the whole history and the estimates.
    let list = request(&d.socket(), Action::List).unwrap();
    assert!(list.error.is_none(), "{:?}", list.error);
    assert_eq!(list.jobs.len(), JOBS as usize);
    assert!(list.estimates.len() >= JOBS as usize);
    // Nothing is unfinished: list_active, logs and the worker switch replies stay tiny.
    let (bytes, active) = d.raw(Action::ListActive);
    assert!(active["error"].is_null(), "{active}");
    assert_eq!(active["jobs"].as_array().unwrap().len(), 0);
    assert!(bytes < 1024, "list_active reply {bytes} B");
    let (bytes, logs) = d.raw(Action::Logs(JOBS));
    assert_eq!(
        logs["jobs"].as_array().unwrap().len(),
        1,
        "logs carries its job only"
    );
    assert_eq!(logs["jobs"][0]["id"], JOBS);
    assert!(logs["estimates"].as_array().unwrap().is_empty());
    assert!(bytes < 8 * 1024, "logs reply {bytes} B");
    let (bytes, stopped) = d.raw(Action::StopWorker);
    assert!(stopped["error"].is_null(), "{stopped}");
    assert!(stopped["jobs"].as_array().unwrap().is_empty());
    assert!(bytes < 1024, "stop-worker reply {bytes} B");
    let (bytes, _) = d.raw(Action::Job(JOBS));
    assert!(bytes < 4 * 1024, "job reply {bytes} B");

    // A job submitted through ANOTHER connection (as a client or worker process would) shows
    // up in the cached listing and in list_active; so does its cancellation.
    let mut other = Store::open(&d.root.join("state/tasks.sqlite")).unwrap();
    let job = other.submit(&spec("synthetic-late")).unwrap();
    let list: Response = request(&d.socket(), Action::List).unwrap();
    assert_eq!(list.jobs.len(), JOBS as usize + 1);
    let active = request(&d.socket(), Action::ListActive).unwrap();
    assert_eq!(
        active.jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
        [job.id]
    );
    assert!(active.estimates.is_empty());
    let (_, stopped) = d.raw(Action::StopWorker);
    assert_eq!(
        stopped["jobs"].as_array().unwrap().len(),
        1,
        "unfinished jobs only"
    );
    // A mutation through the daemon's own connection invalidates as well.
    let cancelled = request(&d.socket(), Action::Cancel(job.id)).unwrap();
    assert!(cancelled.error.is_none(), "{:?}", cancelled.error);
    let list = request(&d.socket(), Action::List).unwrap();
    let phase = list
        .jobs
        .iter()
        .find(|j| j.id == job.id)
        .map(|j| j.phase.clone());
    assert_eq!(phase, Some(Phase::Cancelled));
    assert!(
        request(&d.socket(), Action::ListActive)
            .unwrap()
            .jobs
            .is_empty()
    );
}

#[test]
fn idle_and_polled_daemon_stays_within_budget() {
    let d = Daemon::start("budget");
    // Warm the cache and let the sampler/health threads take their first samples.
    request(&d.socket(), Action::List).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let list = d.cpu_ms(&Action::List, 20);
    let active = d.cpu_ms(&Action::ListActive, 40);
    let job = d.cpu_ms(&Action::Job(JOBS), 40);
    let status = d.cpu_ms(&Action::Status, 20);
    // Idle: no client at all for 15 s.
    let before = d.cpu();
    std::thread::sleep(Duration::from_secs(15));
    let idle_per_min = (d.cpu() - before) * 4.0;
    eprintln!(
        "idle budget ({JOBS} jobs, {} build): list {list:.1} ms, list_active {active:.1} ms, \
         job {job:.1} ms, status {status:.1} ms per request; idle {idle_per_min:.2} CPU s/min",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    // Before the cache and buffered replies a release daemon spent ~170-300 ms per list.
    assert!(list < 60.0, "list {list:.1} ms/request");
    assert!(active < 20.0, "list_active {active:.1} ms/request");
    assert!(job < 20.0, "job {job:.1} ms/request");
    assert!(status < 120.0, "status {status:.1} ms/request");
    assert!(idle_per_min < 1.0, "idle {idle_per_min:.2} CPU s/min");
}
