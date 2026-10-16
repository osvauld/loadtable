#![cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tinline_task_manager::{
    Action, Cost, JobSpec, Phase, Response, request, request_with_reconnect,
};

struct Daemon {
    child: Child,
    state: PathBuf,
}
impl Daemon {
    fn start(state: PathBuf) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_tasks"))
            .arg("daemon")
            .env("TASK_MANAGER_STATE_DIR", &state)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut daemon = Self { child, state };
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if request(&daemon.socket(), Action::List).is_ok() {
                return daemon;
            }
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            assert!(Instant::now() < end, "daemon startup timeout");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn socket(&self) -> PathBuf {
        self.state.join("taskd.sock")
    }
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

#[test]
fn socket_queue_rejects_bad_requests_and_survives_daemon_restart() {
    let state = std::env::temp_dir().join(format!("task-manager-socket-{}", std::process::id()));
    assert!(!state.exists(), "refusing to overwrite another fixture");
    let mut daemon = Daemon::start(state.clone());
    let spec = JobSpec {
        suite: "sim.groups.smoke".into(),
        owner: "fixture-agent".into(),
        source: "/fixture".into(),
        revision: "fixture-revision".into(),
        configuration: "release".into(),
        cost: Cost::CpuSimulation,
    };
    let response = request(&daemon.socket(), Action::Submit(spec)).unwrap();
    assert!(response.error.is_none());
    assert!(!response.execution_enabled);
    let id = response.jobs[0].id;
    let mut stream = UnixStream::connect(daemon.socket()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .write_all(b"{\"version\":999,\"action\":{\"type\":\"cancel\",\"data\":1}}\n")
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let response: Response = serde_json::from_str(&line).unwrap();
    assert!(response.error.unwrap().contains("version"));
    assert_eq!(
        request(&daemon.socket(), Action::List).unwrap().jobs[0].phase,
        Phase::Queued
    );
    let second = Command::new(env!("CARGO_BIN_EXE_tasks"))
        .arg("daemon")
        .env("TASK_MANAGER_STATE_DIR", &state)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!second.success(), "second daemon must not share the ledger");
    assert!(request(&daemon.socket(), Action::List).is_ok());
    daemon.stop();
    // Leave the killed fixture's socket in place, just like an unexpected crash.
    let mut restarted = None;
    let response = request_with_reconnect(&daemon.socket(), Action::List, || {
        restarted = Some(Daemon::start(state.clone()));
        Ok(())
    })
    .unwrap();
    let restarted = restarted.expect("stale socket must trigger recovery");
    assert_eq!(response.jobs.len(), 1);
    assert_eq!(response.jobs[0].id, id);
    assert_eq!(response.jobs[0].phase, Phase::Queued);
    let response = request(&restarted.socket(), Action::Cancel(id)).unwrap();
    assert!(response.error.is_none());
    assert_eq!(response.jobs[0].phase, Phase::Cancelled);
}
