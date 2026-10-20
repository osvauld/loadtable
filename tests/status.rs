#![cfg(target_os = "linux")]
//! `tasks status`: the daemon's machine snapshot over the real socket, with
//! isolated state AND an isolated TASKS_LOCK_DIR (no shared slot is touched).
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tinline_task_manager::metrics::{Snapshot, Warmth};
use tinline_task_manager::{Action, Phase, request, request_status};

struct Fixture {
    root: PathBuf,
    daemon: Child,
}
impl Fixture {
    fn start(label: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("tasks-status-{label}-{}", std::process::id()));
        assert!(!root.exists(), "refusing to reuse another fixture");
        std::fs::create_dir_all(root.join("locks")).unwrap();
        let daemon = Command::new(env!("CARGO_BIN_EXE_tasks"))
            .arg("daemon")
            .envs(Self::env_of(&root))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let fixture = Self { root, daemon };
        let end = Instant::now() + Duration::from_secs(10);
        while request(&fixture.socket(), Action::List).is_err() {
            assert!(Instant::now() < end, "daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        fixture
    }
    fn env_of(root: &std::path::Path) -> [(&'static str, PathBuf); 2] {
        [
            ("TASK_MANAGER_STATE_DIR", root.join("state")),
            ("TASKS_LOCK_DIR", root.join("locks")),
        ]
    }
    fn env(&self) -> [(&'static str, PathBuf); 2] {
        Self::env_of(&self.root)
    }
    fn socket(&self) -> PathBuf {
        self.root.join("state/taskd.sock")
    }
    fn status(&self) -> Snapshot {
        let reply = request_status(&self.socket(), Duration::from_secs(30)).unwrap();
        assert!(reply.error.is_none(), "{:?}", reply.error);
        reply.status.unwrap()
    }
    /// Polls until `ok` holds for a snapshot, or panics after `secs`.
    fn status_until(&self, secs: u64, ok: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let s = self.status();
            if ok(&s) {
                return s;
            }
            assert!(Instant::now() < end, "condition not reached: {s:#?}");
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn enqueue(f: &Fixture, args: &[&str]) -> i64 {
    let out = Command::new(env!("CARGO_BIN_EXE_tasks"))
        .arg("enqueue")
        .args(args)
        .envs(f.env())
        .current_dir(&f.root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    response["submitted_id"].as_i64().unwrap()
}

#[test]
fn status_reports_machine_slots_and_a_queued_proposal() {
    let f = Fixture::start("queued");
    let id = enqueue(
        &f,
        &[
            "--who",
            "synthetic-status",
            "--",
            "env",
            "SIM_FROM=1.5",
            "cargo",
            "check",
            "--offline",
            "-j",
            "2",
            "-p",
            "demo",
        ],
    );
    // Two sampler ticks give a CPU figure.
    let s = f.status_until(10, |s| s.machine.cpu_busy_cores.is_some());
    assert_eq!(s.version, 1);
    assert!(!s.machine.id.is_empty());
    assert!(s.machine.logical_cpus >= 1);
    assert!(s.machine.mem_total_bytes.unwrap() > 0);
    assert!(!s.machine.history.cpu_busy_cores.is_empty());
    let compile = s
        .machine
        .slots
        .iter()
        .find(|s| s.name == "p2p-build")
        .unwrap();
    assert!(!compile.busy, "isolated lock directory has no holder");
    assert_eq!(
        s.machine.slots.len(),
        13,
        "1 compile + 4 run + 4 offload + 4 service slots"
    );
    let job = s.jobs.iter().find(|j| j.id == id).unwrap();
    assert_eq!(job.phase, Phase::Queued);
    assert_eq!(job.command, "cargo check -p demo");
    assert_eq!(job.owner, "synthetic-status");
    assert!(job.live.is_none() && job.result.is_none() && job.estimate.is_none());
    assert_eq!(job.progress.as_ref().unwrap().crates_compiled, 0);
    let q = job.queue.as_ref().unwrap();
    assert!(
        q.waiting_for.as_ref().unwrap().starts_with("approval"),
        "{q:?}"
    );
    // The CLI prints the same document as one JSON line, and a text summary.
    let out = Command::new(env!("CARGO_BIN_EXE_tasks"))
        .args(["status", "--json"])
        .envs(f.env())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout.iter().filter(|b| **b == b'\n').count(), 1);
    let cli: Snapshot = serde_json::from_slice(&out.stdout).unwrap();
    assert!(cli.jobs.iter().any(|j| j.id == id));
    let text = Command::new(env!("CARGO_BIN_EXE_tasks"))
        .arg("status")
        .envs(f.env())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains(&format!("#{id} queued")), "{text}");
    // Existing requests are unchanged by the new action.
    let list = request(&f.socket(), Action::List).unwrap();
    assert!(list.error.is_none() && list.jobs.iter().any(|j| j.id == id));
}

/// Real worker: live sampling of a CPU-busy job, then the persisted result.
/// Subject to real PSI/memory gates, hence ignored. Run its prebuilt test
/// binary directly, NOT inside `tasks run` (its own worker takes admission):
///   target/debug/deps/status-* --ignored --nocapture
#[test]
#[ignore = "real worker E2E; needs host PSI/memory headroom; run outside tasks run"]
fn running_job_is_sampled_live_and_its_result_persisted() {
    let f = Fixture::start("e2e");
    // Two busy shell loops (a parent and a background child) for ~6 s.
    let script = "end=$(($(date +%s)+6)); (while [ $(date +%s) -lt $end ]; do :; done) & while [ $(date +%s) -lt $end ]; do :; done; wait";
    let id = enqueue(
        &f,
        &[
            "--who",
            "synthetic-status",
            "--run",
            "--",
            "/bin/sh",
            "-c",
            script,
        ],
    );
    for action in [Action::ApproveCommand(id), Action::StartWorker] {
        assert!(request(&f.socket(), action).unwrap().error.is_none());
    }
    let s = f.status_until(60, |s| {
        s.jobs.iter().any(|j| {
            j.id == id
                && j.live
                    .as_ref()
                    .is_some_and(|l| l.cpu_cores.unwrap_or(0.0) > 0.5)
        })
    });
    let job = s.jobs.iter().find(|j| j.id == id).unwrap();
    let live = job.live.as_ref().unwrap();
    eprintln!("live sample: {live:?}");
    assert!(live.processes >= 2, "{live:?}");
    assert!(live.rss_bytes > 0);
    assert_eq!(job.warmth, Warmth::Unknown);
    assert_eq!(job.warmth_reason, "not a Cargo command");
    let s = f.status_until(60, |s| {
        s.jobs
            .iter()
            .any(|j| j.id == id && j.result.as_ref().is_some_and(|r| r.cpu_user_s.is_some()))
    });
    let job = s.jobs.iter().find(|j| j.id == id).unwrap();
    let result = job.result.as_ref().unwrap();
    eprintln!("result: {result:?}");
    assert_eq!(job.phase, Phase::Passed);
    assert!(result.wall_s.unwrap() >= 5.0);
    let cpu = result.cpu_user_s.unwrap() + result.cpu_sys_s.unwrap();
    assert!(cpu > 6.0, "two busy loops for ~6 s: {cpu}");
    assert!(result.peak_rss_bytes.unwrap() > 0);
    // Persisted in the execution JSON, readable through the plain listing.
    let list = request(&f.socket(), Action::List).unwrap();
    let e = list
        .jobs
        .iter()
        .find(|j| j.id == id)
        .unwrap()
        .execution
        .clone()
        .unwrap();
    assert!(e.metrics.unwrap().cpu_user_s.is_some());
}
