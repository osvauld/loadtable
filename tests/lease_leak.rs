#![cfg(target_os = "linux")]
//! Feedback lease-leak, end to end through a real daemon and worker with
//! isolated state AND an isolated TASKS_LOCK_DIR (no shared slot is touched,
//! no real compiler-cache server is contacted: RUSTC_WRAPPER is synthetic or
//! removed). Subject to real PSI/memory gates, hence ignored. Run its prebuilt
//! test binary directly or `cargo test --test lease_leak -- --ignored`.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tinline_task_manager::metrics::Snapshot;
use tinline_task_manager::{Action, Phase, request, request_status};

struct Fixture {
    root: PathBuf,
    daemon: Child,
    pids: Vec<i32>,
}
impl Fixture {
    fn start(label: &str, wrapper: Option<&Path>) -> Self {
        let root = std::env::temp_dir().join(format!("tasks-lease-{label}-{}", std::process::id()));
        assert!(!root.exists(), "refusing to reuse another fixture");
        std::fs::create_dir_all(root.join("locks")).unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tasks"));
        cmd.arg("daemon")
            .envs(Self::env_of(&root))
            .env_remove("RUSTC_WRAPPER")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(w) = wrapper {
            cmd.env("RUSTC_WRAPPER", w);
        }
        // This test may itself run inside an admitted job (e.g. an offload
        // `cargo test`): its isolated instance must not keep that job's real
        // slot leases if a worker it started outlives the test.
        // SAFETY: only an async-signal-safe syscall between fork and exec.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::syscall(
                    libc::SYS_close_range,
                    3u32,
                    u32::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                );
                Ok(())
            });
        }
        let daemon = cmd.spawn().unwrap();
        let f = Self {
            root,
            daemon,
            pids: vec![],
        };
        let end = Instant::now() + Duration::from_secs(10);
        while request(&f.socket(), Action::List).is_err() {
            assert!(Instant::now() < end, "daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        f
    }
    fn env_of(root: &Path) -> [(&'static str, PathBuf); 2] {
        [
            ("TASK_MANAGER_STATE_DIR", root.join("state")),
            ("TASKS_LOCK_DIR", root.join("locks")),
        ]
    }
    fn socket(&self) -> PathBuf {
        self.root.join("state/taskd.sock")
    }
    fn tasks(&self, args: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_tasks"))
            .args(args)
            .envs(Self::env_of(&self.root))
            .current_dir(self.root.join("work"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    /// Enqueues, approves and starts a compile-slot command.
    fn submit(&self, script: &str) -> i64 {
        let out = self.tasks(&[
            "enqueue",
            "--who",
            "synthetic-lease",
            "--",
            "/bin/sh",
            "-c",
            script,
        ]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let id = v["submitted_id"].as_i64().unwrap();
        for action in [Action::ApproveCommand(id), Action::StartWorker] {
            assert!(request(&self.socket(), action).unwrap().error.is_none());
        }
        id
    }
    fn status_until(&self, secs: u64, ok: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let reply = request_status(&self.socket(), Duration::from_secs(30)).unwrap();
            let s = reply.status.unwrap();
            if ok(&s) {
                return s;
            }
            assert!(Instant::now() < end, "condition not reached: {s:#?}");
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    fn phase(s: &Snapshot, id: i64) -> Option<Phase> {
        s.jobs.iter().find(|j| j.id == id).map(|j| j.phase.clone())
    }
    fn read_pid(&mut self, file: &Path) -> i32 {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(pid) = std::fs::read_to_string(file)
                .ok()
                .and_then(|t| t.trim().parse().ok())
            {
                self.pids.push(pid);
                return pid;
            }
            assert!(Instant::now() < end, "{} never written", file.display());
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for pid in &self.pids {
            unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        // The worker (and its stage/admission processes) run detached from
        // the daemon: stop every process of THIS isolated instance, found by
        // its unique state directory, so a failed assertion leaves nothing.
        kill_instance(&self.root.join("state"));
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn kill_instance(state: &Path) {
    let want = [
        b"TASK_MANAGER_STATE_DIR=".as_slice(),
        state.as_os_str().as_encoded_bytes(),
    ]
    .concat();
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        if std::fs::read(entry.path().join("environ"))
            .is_ok_and(|env| env.split(|b| *b == 0).any(|v| v == want.as_slice()))
        {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
}

/// A finished job's daemonized child keeps the compile lease (deliberate:
/// tasks never steals a flock). The next job's queue text, `tasks status` and
/// its own log name that process, command and originating job instead of
/// "an untracked holder"; it is admitted once the process exits.
#[test]
#[ignore = "real worker E2E; needs host PSI/memory headroom"]
fn escaped_daemon_is_named_while_it_holds_the_compile_slot() {
    let mut f = Fixture::start("named", None);
    let pidfile = f.root.join("daemon.pid");
    let a = f.submit(&format!(
        "setsid sh -c 'echo $$ > {}; exec sleep 300' </dev/null >/dev/null 2>&1 &",
        pidfile.display()
    ));
    f.status_until(120, |s| Fixture::phase(s, a) == Some(Phase::Passed));
    let daemon = f.read_pid(&pidfile);
    let b = f.submit("true");
    let want = format!("pid {daemon} (sleep 300) from job #{a}, which has ended");
    let s = f.status_until(60, |s| {
        s.jobs.iter().any(|j| {
            j.id == b
                && j.queue
                    .as_ref()
                    .and_then(|q| q.waiting_for.as_deref())
                    .is_some_and(|w| w.contains(&want))
        })
    });
    let slot = s
        .machine
        .slots
        .iter()
        .find(|s| s.name == "p2p-build")
        .unwrap();
    assert!(slot.busy && slot.holder_job.is_none());
    assert_eq!(slot.holder_processes[0].pid, daemon as u32);
    assert_eq!(slot.holder_processes[0].tasks_job, Some(a));
    let text = f.tasks(&["status"]);
    assert!(
        text.contains(&format!("p2p-build held by pid {daemon}")),
        "{text}"
    );
    // The waiting admission says the same in the job's own log (10 s cadence).
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let log = f.tasks(&["logs", &b.to_string()]);
        if log.contains(&format!(
            "compile slot held by pid {daemon} (sleep 300) from job #{a}"
        )) {
            break;
        }
        assert!(Instant::now() < end, "no holder line in the log:\n{log}");
        std::thread::sleep(Duration::from_millis(500));
    }
    assert_eq!(Fixture::phase(&s, b), Some(Phase::WaitingBuild));
    unsafe { libc::kill(daemon, libc::SIGKILL) };
    f.status_until(60, |s| Fixture::phase(s, b) == Some(Phase::Passed));
}

const FAKE_SCCACHE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
alive() { [ -s "$dir/server.pid" ] && kill -0 "$(cat "$dir/server.pid")" 2>/dev/null; }
start() {
  setsid sh -c 'echo $$ > "$1/server.pid.tmp"; mv "$1/server.pid.tmp" "$1/server.pid"; exec sleep 300' sh "$dir" </dev/null >/dev/null 2>&1 &
  i=0; while ! alive && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done
}
case "$1" in
  --start-server) if alive; then echo "Server startup failed: Address in use" >&2; exit 2; fi; start;;
  --show-stats) alive || exit 1;;
  *) alive || start;;
esac
"#;

/// With RUSTC_WRAPPER a (synthetic, sccache-like) server-based wrapper, the
/// worker starts the server outside the job, so the build's wrapper calls do
/// not autostart one that inherits the lease: the next build is admitted
/// while the server is still running.
#[test]
#[ignore = "real worker E2E; needs host PSI/memory headroom"]
fn compiler_cache_server_started_by_the_worker_does_not_hold_the_slot() {
    use std::os::unix::fs::PermissionsExt;
    let bin = std::env::temp_dir().join(format!("tasks-lease-bin-{}", std::process::id()));
    std::fs::create_dir_all(&bin).unwrap();
    let wrapper = bin.join("sccache");
    std::fs::write(&wrapper, FAKE_SCCACHE).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut f = Fixture::start("cache", Some(&wrapper));
    let a = f.submit("\"$RUSTC_WRAPPER\" rustc -vV");
    f.status_until(120, |s| Fixture::phase(s, a) == Some(Phase::Passed));
    let server = f.read_pid(&bin.join("server.pid"));
    let log = f.tasks(&["logs", &a.to_string()]);
    assert!(
        log.contains("started the compiler-cache server outside the job"),
        "{log}"
    );
    let b = f.submit("true");
    f.status_until(60, |s| Fixture::phase(s, b) == Some(Phase::Passed));
    assert!(
        unsafe { libc::kill(server, 0) } == 0,
        "the server is still running for later jobs"
    );
    let log = f.tasks(&["logs", &b.to_string()]);
    assert!(
        log.contains("compiler-cache server already running"),
        "{log}"
    );
    drop(f);
    let _ = std::fs::remove_dir_all(&bin);
}
