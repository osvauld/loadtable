#![cfg(target_os = "linux")]
//! Socket compatibility of the new `peers` action and the `peer` ledger link.
//!
//! * Old client -> new daemon: requests and reply parsing use FROZEN copies of the protocol
//!   types as they were before peer forwarding (no `Peers` action, no `Execution::peer`).
//! * New client -> old daemon: a fake daemon that deserializes with the frozen old `Action`
//!   and answers exactly like the old `serve` (`error_response(serde error)`).
use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod old {
    //! The socket protocol of the release before peer forwarding (commit d5a7502).
    use serde::{Deserialize, Serialize};
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct JobSpec {
        pub suite: String,
        pub owner: String,
        pub source: String,
        pub revision: String,
        pub configuration: String,
        pub cost: String,
    }
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct RuntimeSample {
        pub suite: String,
        pub configuration: String,
        pub stage: String,
        pub seconds: f64,
    }
    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(tag = "type", content = "data", rename_all = "snake_case")]
    pub enum Action {
        List,
        Job(i64),
        Submit(JobSpec),
        Cancel(i64),
        Approve(i64),
        ApproveAndroidTrial(i64),
        ApproveCommand(i64),
        StartWorker,
        Logs(i64),
        Record(RuntimeSample),
        Status,
    }
    #[derive(Debug, Serialize, Deserialize)]
    pub struct Request {
        pub version: u32,
        pub action: Action,
    }
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct StageTiming {
        pub stage: String,
        pub wait_seconds: f64,
        pub seconds: Option<f64>,
        pub exit_code: Option<i32>,
        pub signal: Option<i32>,
    }
    /// Pre-peer `Execution`: no `peer` field, and (like the original) no deny_unknown_fields.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct Execution {
        pub approved_at: i64,
        pub started_at: Option<i64>,
        pub finished_at: Option<i64>,
        pub cancel_requested: bool,
        pub stages: Vec<StageTiming>,
        #[serde(default)]
        pub local_command: bool,
        pub detail: String,
        #[serde(default)]
        pub metrics: Option<serde_json::Value>,
    }
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct Job {
        pub id: i64,
        pub spec: JobSpec,
        pub phase: String,
        pub submitted_at: i64,
        #[serde(default)]
        pub execution: Option<Execution>,
    }
    #[derive(Debug, Serialize, Deserialize)]
    pub struct Response {
        pub version: u32,
        pub execution_enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub submitted_id: Option<i64>,
        pub jobs: Vec<Job>,
        #[serde(default)]
        pub estimates: Vec<serde_json::Value>,
        pub error: Option<String>,
        #[serde(default)]
        pub log: Option<serde_json::Value>,
    }
}

struct Dir(PathBuf);
impl Dir {
    fn new(label: &str) -> Self {
        let p = std::env::temp_dir().join(format!("tasks-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn exchange(socket: &Path, request: &impl Serialize) -> String {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut line = serde_json::to_vec(request).unwrap();
    line.push(b'\n');
    stream.write_all(&line).unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    reply
}

fn start_daemon(dir: &Path) -> Child {
    let state = dir.join("state");
    let child = Command::new(env!("CARGO_BIN_EXE_tasks"))
        .arg("daemon")
        .env("TASK_MANAGER_STATE_DIR", &state)
        .env("TASKS_LOCK_DIR", dir.join("locks"))
        // No peer config: the peer runtime stays disabled.
        .env("TASKS_PEER_CONFIG", dir.join("absent-peer.toml"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(state.join("taskd.sock")).is_err() {
        assert!(Instant::now() < end, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

#[test]
fn old_clients_work_against_a_daemon_with_peer_support() {
    let dir = Dir::new("peer-proto-old-client");
    let state = dir.0.join("state");
    std::fs::create_dir_all(&state).unwrap();
    // A finished job whose execution JSON carries the new `peer` link.
    {
        use tinline_task_manager::local_command::{Recipe, Resource};
        let mut store = tinline_task_manager::Store::open(&state.join("tasks.sqlite")).unwrap();
        let spec = Recipe {
            argv: vec!["true".into()],
            resource: Resource::Run,
        }
        .spec("synthetic-old-client".into(), dir.0.clone())
        .unwrap();
        let job = store.submit(&spec).unwrap();
        store.approve_command(job.id).unwrap();
        // A synthetic (dead) worker claims it, then the job fails: terminal with an execution.
        store
            .claim(tinline_task_manager::execution::ProcessIdentity {
                pid: 1,
                start_ticks: 1,
            })
            .unwrap()
            .unwrap();
        store
            .finish(job.id, tinline_task_manager::Phase::Failed, "synthetic")
            .unwrap();
        let link = tinline_task_manager::peer_link::PeerLink {
            direction: "outbound".into(),
            peer: "beta".into(),
            peer_endpoint: "00".repeat(32),
            peer_job_id: Some(7),
            owner: "synthetic@host".into(),
            key: "host:1:abc".into(),
            transport: "iroh".into(),
            remote_phase: Some("failed".into()),
            remote_state: Some("terminal".into()),
            sync_s: Some(0.5),
            fetch_s: None,
        };
        assert!(
            tinline_task_manager::peer_link::write_peer_link(&mut store, job.id, &link).unwrap()
        );
        // Never overwritten.
        assert!(
            !tinline_task_manager::peer_link::write_peer_link(&mut store, job.id, &link).unwrap()
        );
    }
    let mut daemon = start_daemon(&dir.0);
    let socket = state.join("taskd.sock");
    let ask = |action: old::Action| -> old::Response {
        let reply = exchange(&socket, &old::Request { version: 2, action });
        serde_json::from_str(&reply)
            .unwrap_or_else(|e| panic!("old client cannot parse: {e}\n{reply}"))
    };
    let listed = ask(old::Action::List);
    assert_eq!(listed.error, None);
    assert_eq!(listed.jobs.len(), 1);
    assert_eq!(listed.jobs[0].phase, "failed");
    assert!(
        listed.jobs[0].execution.is_some(),
        "old Execution ignores `peer`"
    );
    let one = ask(old::Action::Job(listed.jobs[0].id));
    assert_eq!(one.jobs.len(), 1);
    let submitted = ask(old::Action::Submit(old::JobSpec {
        suite: "sim.groups.smoke".into(),
        owner: "synthetic-old-client".into(),
        source: "/fixture".into(),
        revision: "fixture-revision".into(),
        configuration: "release".into(),
        cost: "cpu_simulation".into(),
    }));
    assert_eq!(submitted.error, None);
    let id = submitted.submitted_id.unwrap();
    let cancelled = ask(old::Action::Cancel(id));
    assert_eq!(cancelled.error, None);
    // `status` keeps its own reply shape.
    let status: serde_json::Value = serde_json::from_str(&exchange(
        &socket,
        &old::Request {
            version: 2,
            action: old::Action::Status,
        },
    ))
    .unwrap();
    assert_eq!(status["status"]["version"], 1);
    // The new action on the new daemon: peer link disabled without a config, said plainly.
    let report =
        tinline_task_manager::peer_link::request_peers(&socket, Duration::from_secs(10)).unwrap();
    assert!(!report.enabled);
    assert!(report.error.unwrap().contains("not configured"));
    let _ = daemon.kill();
    let _ = daemon.wait();
}

/// The old daemon's `serve`: deserialize with the old Action; on failure answer
/// `error_response(error.to_string())`.
fn fake_old_daemon(socket: PathBuf, seen: Arc<Mutex<Vec<String>>>) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(60);
        while Instant::now() < end {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            let mut line = String::new();
            if BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap_or(0)
                == 0
            {
                continue; // liveness probe
            }
            seen.lock().unwrap().push(line.clone());
            let response = match serde_json::from_str::<old::Request>(&line) {
                Ok(r) if matches!(r.action, old::Action::List) => old::Response {
                    version: 2,
                    execution_enabled: false,
                    submitted_id: None,
                    jobs: vec![],
                    estimates: vec![],
                    error: None,
                    log: None,
                },
                Ok(_) => old::Response {
                    version: 2,
                    execution_enabled: false,
                    submitted_id: None,
                    jobs: vec![],
                    estimates: vec![],
                    error: Some("fixture: not modelled".into()),
                    log: None,
                },
                Err(e) => old::Response {
                    version: 2,
                    execution_enabled: false,
                    submitted_id: None,
                    jobs: vec![],
                    estimates: vec![],
                    error: Some(e.to_string()),
                    log: None,
                },
            };
            let mut out = serde_json::to_vec(&response).unwrap();
            out.push(b'\n');
            let _ = stream.write_all(&out);
        }
    })
}

#[test]
fn new_clients_get_the_unknown_variant_fallback_from_an_old_daemon() {
    let dir = Dir::new("peer-proto-old-daemon");
    let state = dir.0.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let _server = fake_old_daemon(state.join("taskd.sock"), Arc::clone(&seen));
    let socket = state.join("taskd.sock");
    let error = tinline_task_manager::peer_link::request_peers(&socket, Duration::from_secs(10))
        .unwrap_err()
        .to_string();
    assert!(error.contains("predates peer forwarding"), "{error}");
    assert!(error.contains("unknown variant `peers`"), "{error}");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tasks"))
            .args(args)
            .env("TASK_MANAGER_STATE_DIR", &state)
            .current_dir(&dir.0)
            .output()
            .unwrap()
    };
    let out = run(&["peer", "list"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("predates peer forwarding"));
    let out = run(&[
        "run",
        "--who",
        "synthetic",
        "--peer",
        "beta",
        "--run",
        "--",
        "true",
    ]);
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("predates peer forwarding") && text.contains("nothing was submitted"),
        "{text}"
    );
    // Existing actions still reach the old daemon unchanged; no submit was sent for --peer.
    let out = run(&["list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|l| !l.contains("\"submit\"")), "{seen:?}");
    assert!(seen.iter().any(|l| l.contains("\"peers\"")));
    // The wire form of the new action.
    assert_eq!(
        serde_json::to_string(&tinline_task_manager::Action::Peers).unwrap(),
        r#"{"type":"peers"}"#
    );
}
