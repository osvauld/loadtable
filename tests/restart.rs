#![cfg(target_os = "linux")]
//! Node policy rule 4, "survives restarts": a daemon that starts after a reboot resumes
//! execution when it was enabled before (no manual `tasks start-worker`), an explicit
//! `tasks stop-worker` sticks across the restart, and both commands print one line.
//!
//! Real `tasks daemon` processes with isolated state, TASKS_LOCK_DIR, HOME and peer config
//! (no slot, ledger or port shared with the host). The assertions stop at the worker's
//! CLAIM (approved -> preparing), which needs no admission headroom; the claimed job is then
//! cancelled and every daemon/worker this test started is waited for (and killed only if it
//! outlives the test). Rust opens every descriptor close-on-exec, so the daemons inherit
//! only their stdio.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tinline_task_manager::local_command::{Recipe, Resource};
use tinline_task_manager::{Action, Phase, Store, request};

struct Node {
    dir: PathBuf,
    daemons: Vec<Child>,
    workers: Vec<u32>,
}

impl Node {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tasks-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        Self {
            dir,
            daemons: vec![],
            workers: vec![],
        }
    }
    fn state(&self) -> PathBuf {
        self.dir.join("state")
    }
    fn socket(&self) -> PathBuf {
        self.state().join("taskd.sock")
    }
    fn env(&self, cmd: &mut Command) {
        cmd.env("TASK_MANAGER_STATE_DIR", self.state())
            .env("TASKS_LOCK_DIR", self.dir.join("locks"))
            .env("TASKS_PEER_CONFIG", self.dir.join("no-peer.toml"))
            .env("HOME", self.dir.join("home"))
            .env_remove("XDG_STATE_HOME");
    }
    fn tasks(&self, args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tasks"));
        cmd.args(args).current_dir(&self.dir).stdin(Stdio::null());
        self.env(&mut cmd);
        cmd.output().unwrap()
    }
    /// Starts `tasks daemon`, stderr to `daemon-N.log`; returns that log's path.
    fn start_daemon(&mut self) -> PathBuf {
        let log = self.dir.join(format!("daemon-{}.log", self.daemons.len()));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tasks"));
        cmd.arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap());
        self.env(&mut cmd);
        let _ = std::fs::remove_file(self.socket());
        self.daemons.push(cmd.spawn().unwrap());
        wait_for("daemon socket", 15, || {
            std::os::unix::net::UnixStream::connect(self.socket()).is_ok()
        });
        log
    }
    /// A reboot as far as the daemon is concerned: it is gone, its socket is stale.
    fn stop_daemon(&mut self) {
        if let Some(mut d) = self.daemons.pop() {
            let _ = d.kill();
            let _ = d.wait();
        }
    }
    fn store(&self) -> Store {
        Store::open(&self.state().join("tasks.sqlite")).unwrap()
    }
    fn approved_run_job(&self) -> i64 {
        let spec = Recipe {
            argv: vec!["/bin/true".into()],
            resource: Resource::Run,
            broad_test: false,
        }
        .spec("restart-survival-test".into(), self.dir.clone())
        .unwrap();
        let id = request(&self.socket(), Action::Submit(spec))
            .unwrap()
            .submitted_id
            .unwrap();
        let r = request(&self.socket(), Action::ApproveCommand(id)).unwrap();
        assert!(r.error.is_none(), "{:?}", r.error);
        id
    }
    fn phase(&self, id: i64) -> Phase {
        self.store().job(id).unwrap().phase
    }
    fn worker_free(&self) -> bool {
        !tinline_task_manager::worker::running(&self.state())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        // Workers exit by themselves once idle; only a straggler is killed.
        let end = Instant::now() + Duration::from_secs(30);
        while !self.worker_free() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(100));
        }
        if !self.worker_free() {
            for pid in &self.workers {
                unsafe { libc::kill(-(*pid as i32), libc::SIGKILL) };
            }
            eprintln!(
                "restart test: killed leftover worker group(s) {:?}",
                self.workers
            );
        }
        for mut d in self.daemons.drain(..) {
            let _ = d.kill();
            let _ = d.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn wait_for(what: &str, secs: u64, mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !ready() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Cancels `id` and waits until it is terminal and the worker went idle.
fn settle(node: &mut Node, id: i64) {
    if let Some(w) = node
        .store()
        .job(id)
        .unwrap()
        .execution
        .and_then(|e| e.worker)
    {
        node.workers.push(w.pid);
    }
    let _ = request(&node.socket(), Action::Cancel(id));
    wait_for("cancelled job to finish", 60, || node.phase(id).terminal());
    wait_for("worker to go idle", 60, || node.worker_free());
}

#[test]
fn restart_resumes_enabled_execution_and_a_stop_sticks() {
    let mut node = Node::new("restart-survival");
    node.start_daemon();
    // Never started on this state: an approved job waits, nothing starts a worker.
    let first = node.approved_run_job();
    // `tasks start-worker` was the last decision before the "reboot" (written as it would
    // be; starting a worker here would run the job before the restart).
    tinline_task_manager::worker::set_intent(&node.state(), true, "start-worker").unwrap();
    node.stop_daemon();
    assert_eq!(node.phase(first), Phase::Approved);
    let log = node.start_daemon();
    // No client asked for a worker: the restarted daemon claims the job by itself.
    wait_for("worker claim after restart", 30, || {
        node.phase(first) != Phase::Approved
    });
    let text = read(&log);
    assert!(
        text.contains("execution was enabled before this start") && text.contains("1 approved"),
        "{text}"
    );
    settle(&mut node, first);

    // An explicit stop sticks across the next restart.
    let out = node.tasks(&["stop-worker"]);
    let stop = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(stop.starts_with("tasks: execution stopped"), "{stop}");
    assert_eq!(stop.lines().count(), 1, "one line, not the ledger: {stop}");
    let second = node.approved_run_job();
    node.stop_daemon();
    let log = node.start_daemon();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        node.phase(second),
        Phase::Approved,
        "stop-worker must stick"
    );
    assert!(node.worker_free());
    assert!(
        read(&log).contains("execution stays stopped"),
        "{}",
        read(&log)
    );
    let r = request(&node.socket(), Action::Cancel(second)).unwrap();
    assert!(r.error.is_none(), "{:?}", r.error);
    assert_eq!(node.phase(second), Phase::Cancelled);

    // start-worker re-enables and prints one line (the full reply only with --json).
    let out = node.tasks(&["start-worker"]);
    let start = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(start.starts_with("tasks: execution enabled"), "{start}");
    assert_eq!(start.lines().count(), 1, "{start}");
    assert!(!start.contains("\"jobs\""), "{start}");
    assert!(!tinline_task_manager::worker::stopped(&node.state()));
    let out = node.tasks(&["start-worker", "--json"]);
    let reply: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(reply["jobs"].is_array(), "{reply}");
    wait_for("idle worker exit", 30, || node.worker_free());
}

#[test]
fn drain_then_restart_leaves_no_window_for_a_new_admission() {
    let mut node = Node::new("restart-drain");
    node.start_daemon();
    tinline_task_manager::worker::set_intent(&node.state(), true, "start-worker").unwrap();
    let out = node.tasks(&["drain", "--timeout", "30"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(text.starts_with("tasks: drained"), "{text}");
    // Between the drain and the restart, a `tasks run`-style start changes nothing.
    let id = node.approved_run_job();
    let out = node.tasks(&["start-worker"]);
    let start = String::from_utf8_lossy(&out.stdout);
    assert!(start.contains("draining for a daemon restart"), "{start}");
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(node.phase(id), Phase::Approved);
    assert!(node.worker_free());
    // The restart: the new daemon resumes by itself.
    node.stop_daemon();
    let log = node.start_daemon();
    wait_for("claim after the drained restart", 30, || {
        node.phase(id) != Phase::Approved
    });
    assert!(
        read(&log).contains("worker started for 1 approved"),
        "{}",
        read(&log)
    );
    assert!(!tinline_task_manager::worker::draining(&node.state()));
    settle(&mut node, id);
}

/// A process in its own group holding `lock` (as an admitted command group would).
fn hold(lock: &Path) -> Child {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    let mut child = Command::new("python3")
        .args([
            "-c",
            "import fcntl,sys,time; f=open(sys.argv[1],'a'); fcntl.flock(f, fcntl.LOCK_EX); \
             print('locked', flush=True); time.sleep(120)",
        ])
        .arg(lock)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "locked");
    child
}

fn note(lock: &Path, who: &str, pid: u32) {
    std::fs::write(
        lock.with_extension("holder"),
        serde_json::json!({"who": who, "cmd": "fixture", "since": 0.0, "pid": pid,
                           "start_ticks": null, "slot": "compile"})
        .to_string(),
    )
    .unwrap();
}

#[test]
fn restarted_daemon_reaps_an_ended_jobs_lease_and_names_untracked_ones() {
    let mut node = Node::new("restart-lease");
    let locks = node.dir.join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    std::fs::set_permissions(&locks, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    node.start_daemon();
    // A job the ledger ended long ago (interrupted/cancelled), whose command group survived.
    let ended = node.approved_run_job();
    request(&node.socket(), Action::Cancel(ended)).unwrap();
    assert_eq!(node.phase(ended), Phase::Cancelled);
    node.stop_daemon();
    let db = rusqlite::Connection::open(node.state().join("tasks.sqlite")).unwrap();
    db.execute(
        "UPDATE executions SET data=json_set(data, '$.finished_at', 1) WHERE job_id=?1",
        [ended],
    )
    .unwrap();
    drop(db);
    let build = locks.join("p2p-build.lock");
    let mut orphan = hold(&build);
    note(&build, &format!("fixture:job-{ended}"), orphan.id());
    let run = locks.join("p2p-run-0.lock");
    let mut foreign = hold(&run);
    note(&run, "legacy-helper", foreign.id());
    let log = node.start_daemon();
    let reaped = {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            if orphan.try_wait().unwrap().is_some() {
                break true;
            }
            if Instant::now() > end {
                break false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    // The audit names every holder; wait for both lines (the reap may finish first).
    let end = Instant::now() + Duration::from_secs(15);
    while !(read(&log).contains("ORPHAN") && read(&log).contains("UNTRACKED"))
        && Instant::now() < end
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    let text = read(&log);
    let still_foreign = foreign.try_wait().unwrap().is_none();
    let _ = orphan.kill();
    let _ = orphan.wait();
    let _ = foreign.kill();
    let _ = foreign.wait();
    assert!(reaped, "the ended job's lease was left held: {text}");
    assert!(text.contains(&format!("ORPHAN of job {ended}")), "{text}");
    assert!(still_foreign, "an untracked holder must never be signalled");
    assert!(
        text.contains("p2p-run-0.lock held by pid") && text.contains("UNTRACKED"),
        "{text}"
    );
}
