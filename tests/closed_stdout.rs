#![cfg(target_os = "linux")]
//! Feedback #22: does `tasks run ... | head -1` (client stdout closed early) affect
//! the job or its recorded result? The client writes to stdout only AFTER the job
//! is terminal, and afterwards sends no mutation, so it cannot. These tests pin that.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::{fs::PermissionsExt, net::UnixListener};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tinline_task_manager::{Action, PROTOCOL_VERSION, Request};

struct Dir(PathBuf);
impl Dir {
    fn new(label: &str) -> Self {
        let p = std::env::temp_dir().join(format!("tasks-{label}-{}", std::process::id()));
        assert!(!p.exists(), "refusing to reuse another fixture");
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn wait_exit(child: &mut std::process::Child, secs: u64) -> std::process::ExitStatus {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > end {
            let _ = child.kill();
            panic!("client did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Deterministic: a fake daemon records every request of a real `tasks run`
/// client whose stdout reader closes after one line.
#[test]
fn closed_client_stdout_sends_no_mutation_after_the_job_is_terminal() {
    let dir = Dir::new("closed-stdout");
    let listener = UnixListener::bind(dir.0.join("taskd.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log: String = (0..20_000).map(|i| format!("line-{i}\n")).collect();
    let server = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(30);
            while Instant::now() < end {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).unwrap();
                let request: Request = serde_json::from_str(&line).unwrap();
                let mut response = serde_json::json!({
                    "version": PROTOCOL_VERSION, "execution_enabled": true,
                    "jobs": [], "estimates": [], "error": null, "log": null,
                });
                let name: String = match request.action {
                    Action::Submit(_) => {
                        response["submitted_id"] = 42.into();
                        "submit".into()
                    }
                    Action::ApproveCommand(42) => "approve_command".into(),
                    Action::StartWorker => "start_worker".into(),
                    action @ (Action::List | Action::Job(42)) => {
                        response["jobs"] = serde_json::json!([{
                            "id": 42, "phase": "passed", "submitted_at": 0,
                            "spec": {"suite": "local.command", "owner": "synthetic-sigpipe",
                                     "source": "/", "revision": "live-workspace",
                                     "configuration": "{}", "cost": "cpu_simulation"}
                        }]);
                        if matches!(action, Action::List) {
                            "list"
                        } else {
                            "job"
                        }
                        .into()
                    }
                    Action::Logs(42) => {
                        response["log"] =
                            serde_json::json!({"job_id": 42, "text": log, "truncated": false});
                        "logs".into()
                    }
                    other => {
                        response["error"] = "unexpected request in fixture".into();
                        format!("{other:?}")
                    }
                };
                seen.lock().unwrap().push(name);
                let _ = serde_json::to_writer(&mut stream, &response);
                let _ = stream.write_all(b"\n");
            }
        })
    };
    let mut client = Command::new(env!("CARGO_BIN_EXE_tasks"))
        .args([
            "run",
            "--who",
            "synthetic-sigpipe",
            "--run",
            "--",
            "/bin/true",
        ])
        .env("TASK_MANAGER_STATE_DIR", &dir.0)
        .current_dir(&dir.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        // Like `| head -1`: read one line, then close the pipe.
        let mut first = String::new();
        BufReader::new(client.stdout.take().unwrap())
            .read_line(&mut first)
            .unwrap();
        assert_eq!(first, "line-0\n");
    }
    let status = wait_exit(&mut client, 30);
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut client.stderr.take().unwrap(), &mut stderr).unwrap();
    // Writing the rest of the log hits EPIPE after the job is already terminal and
    // recorded; it does not change the job's result.
    eprintln!("client exit after closed stdout: {status:?}; stderr tail: {stderr:?}");
    // pi-whitepaper 745-747: a closed reader is a quiet 141 (128 + SIGPIPE), not a panic.
    assert!(!stderr.contains("panicked"), "{stderr:?}");
    assert_eq!(status.code(), Some(141), "{stderr:?}");
    std::thread::sleep(Duration::from_millis(200));
    let seen = seen.lock().unwrap().clone();
    let logs = seen.iter().position(|s| s == "logs").expect("logs fetched");
    assert_eq!(&seen[..3], ["submit", "approve_command", "start_worker"]);
    // A waiting client polls its one job, not the whole listing.
    assert!(seen[3..logs].iter().all(|s| s == "job"), "{seen:?}");
    assert_eq!(
        logs,
        seen.len() - 1,
        "no request after the terminal log: {seen:?}"
    );
    drop(server);
}

/// Real daemon/worker with isolated state AND an isolated TASKS_LOCK_DIR, so it
/// shares no slot with the host queue. Still subject to real PSI/memory gates,
/// hence ignored by default. Run its prebuilt test binary directly, NOT inside
/// `tasks run` (its own worker acquires admission):
///   target/debug/deps/closed_stdout-* --ignored --nocapture
#[test]
#[ignore = "real worker E2E; needs host PSI/memory headroom; run outside tasks run"]
fn real_job_result_is_unaffected_by_a_closed_client_pipe() {
    let dir = Dir::new("closed-stdout-e2e");
    let (state, locks) = (dir.0.join("state"), dir.0.join("locks"));
    let bin = env!("CARGO_BIN_EXE_tasks");
    let env = [
        ("TASK_MANAGER_STATE_DIR", state.as_os_str()),
        ("TASKS_LOCK_DIR", locks.as_os_str()),
    ];
    let mut daemon = Command::new(bin)
        .arg("daemon")
        .envs(env)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while !state.join("taskd.sock").exists() {
        assert!(Instant::now() < end, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let script = r#"i=0; while [ $i -lt 20000 ]; do echo line-$i; i=$((i+1)); done; exit 7"#;
    let pipeline = format!(
        "set -o pipefail 2>/dev/null; '{bin}' run --who synthetic-sigpipe --run -- /bin/sh -c '{script}' | head -1"
    );
    let out = Command::new("/bin/bash")
        .args(["-c", &pipeline])
        .envs(env)
        .current_dir(&dir.0)
        .output()
        .unwrap();
    let list = tinline_task_manager::request(&state.join("taskd.sock"), Action::List).unwrap();
    let _ = daemon.kill();
    let _ = daemon.wait();
    // `tasks run` prints the bounded log TAIL, so head sees some single line.
    let first = String::from_utf8_lossy(&out.stdout);
    assert!(
        first.ends_with('\n') && first.lines().count() == 1,
        "{first:?}"
    );
    let job = list.jobs.last().expect("job recorded");
    assert_eq!(job.phase, tinline_task_manager::Phase::Failed);
    let exit = job
        .execution
        .as_ref()
        .unwrap()
        .stages
        .last()
        .unwrap()
        .exit_code;
    assert_eq!(exit, Some(7), "recorded result must be the command's own");
    assert_eq!(
        std::fs::metadata(&locks).unwrap().permissions().mode() & 0o7777,
        0o700
    );
    eprintln!(
        "pipeline status {:?}; client stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}
