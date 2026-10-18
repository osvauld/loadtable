#![cfg(all(target_os = "linux", feature = "mcp"))]
//! `tasks mcp` as a subprocess, speaking newline-delimited JSON-RPC on stdio to a
//! fake daemon on a temp socket (TASK_MANAGER_STATE_DIR). The fake records every
//! socket action, so the tests see exactly what the server asked the daemon for.
//! Every stdout line the server prints is checked to be a JSON-RPC 2.0 message.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tinline_task_manager::{Action, PROTOCOL_VERSION, Request};

struct Dir(PathBuf);
impl Dir {
    fn new(label: &str) -> Self {
        let p = std::env::temp_dir().join(format!("tasks-mcp-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn canonical(&self) -> String {
        std::fs::canonicalize(&self.0)
            .unwrap()
            .to_str()
            .unwrap()
            .into()
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A ledger row as the daemon serializes it.
fn job(
    id: i64,
    owner: &str,
    phase: &str,
    argv: &[&str],
    resource: Value,
    exit: Option<i32>,
) -> Value {
    let configuration = json!({"argv": argv, "resource": resource}).to_string();
    let execution = if phase == "queued" {
        Value::Null
    } else {
        let stages = match exit {
            Some(code) => json!([{"stage": "run", "wait_seconds": 1.0, "seconds": 2.0,
                                  "exit_code": code, "signal": null}]),
            None => json!([]),
        };
        json!({"approved_at": 100, "started_at": 101, "finished_at": exit.map(|_| 103),
               "cancel_requested": false, "worker": null, "supervisor": null,
               "snapshot": null, "stages": stages, "local_command": true,
               "detail": "Explicitly approved trusted local argv/live cwd"})
    };
    json!({"id": id, "phase": phase, "submitted_at": 100, "execution": execution,
           "spec": {"suite": "local.command", "owner": owner, "source": "/synthetic/checkout",
                    "revision": "live-workspace", "configuration": configuration,
                    "cost": "cpu_simulation"}})
}

type Handler = dyn Fn(&Action, &mut Value) + Send + Sync;

/// Fake daemon: answers each request with `handler` filling a default reply,
/// and records every action (serialized) in order.
struct Fake {
    seen: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
}
impl Fake {
    fn start(socket: &Path, handler: Box<Handler>) -> Self {
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let stop: Arc<AtomicBool> = Arc::default();
        let (seen2, stop2) = (seen.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let mut line = String::new();
                if BufReader::new(&mut stream).read_line(&mut line).is_err() {
                    continue;
                }
                let request: Request = serde_json::from_str(&line).unwrap();
                seen2
                    .lock()
                    .unwrap()
                    .push(serde_json::to_value(&request.action).unwrap());
                let mut reply = json!({
                    "version": PROTOCOL_VERSION, "execution_enabled": true,
                    "jobs": [], "estimates": [], "error": null, "log": null,
                });
                handler(&request.action, &mut reply);
                let _ = serde_json::to_writer(&mut stream, &reply);
                let _ = stream.write_all(b"\n");
            }
        });
        Self { seen, stop }
    }
    fn seen(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
    fn types(&self) -> Vec<String> {
        self.seen()
            .iter()
            .map(|a| a["type"].as_str().unwrap().to_owned())
            .collect()
    }
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// `tasks mcp --no-start-daemon` with isolated state, plus its stdout lines.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    all: Vec<Value>,
    next: i64,
}
impl Client {
    fn start(state: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tasks"))
            .args(["mcp", "--no-start-daemon"])
            .env("TASK_MANAGER_STATE_DIR", state)
            .env("TASKS_LOCK_DIR", state.join("locks"))
            // Hermetic: never this user's peer config (no peers = placement is local).
            .env("TASKS_PEER_CONFIG", state.join("no-peer-config.toml"))
            .env_remove("TASKS_MAX_CARGO_JOBS")
            .env_remove("TASKS_DENIED_DEVICES")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            lines: rx,
            all: Vec::new(),
            next: 1,
        }
    }
    fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }
    /// Next stdout message; asserts it is JSON-RPC 2.0.
    fn recv(&mut self, timeout: Duration) -> Option<Value> {
        match self.lines.recv_timeout(timeout) {
            Ok(line) => {
                let value: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("non-JSON on stdout ({e}): {line:?}"));
                assert_eq!(value["jsonrpc"], "2.0", "{line}");
                assert!(
                    value.get("result").is_some()
                        || value.get("error").is_some()
                        || value.get("method").is_some(),
                    "{line}"
                );
                self.all.push(value.clone());
                Some(value)
            }
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }
    /// Reply to request `id`, skipping notifications.
    fn reply(&mut self, id: &Value) -> Value {
        let end = Instant::now() + Duration::from_secs(60);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let message = self.recv(left).expect("no reply from tasks mcp");
            if &message["id"] == id {
                return message;
            }
            assert!(message.get("id").is_none(), "unexpected reply {message}");
        }
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = json!(self.next);
        self.next += 1;
        self.send_raw(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        self.reply(&id)
    }
    fn initialize(&mut self) -> Value {
        let reply = self.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "synthetic-client", "version": "0"}}),
        );
        self.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        reply
    }
    /// tools/call result (asserts it is a result, not a JSON-RPC error).
    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert!(reply.get("error").is_none(), "{reply}");
        let result = reply["result"].clone();
        assert!(result["content"][0]["text"].is_string(), "{result}");
        result
    }
    fn close(self) -> std::process::ExitStatus {
        self.finish().0
    }
    /// Closes stdin, waits for exit, returns the status and every message seen.
    fn finish(mut self) -> (std::process::ExitStatus, Vec<Value>) {
        drop(self.stdin.take());
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // Drain what is left; it must be JSON-RPC too.
                while self.recv(Duration::from_millis(200)).is_some() {}
                return (status, std::mem::take(&mut self.all));
            }
            assert!(Instant::now() < end, "tasks mcp did not exit at EOF");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

#[test]
fn initialize_handshake_and_ping() {
    let dir = Dir::new("init");
    let mut client = Client::start(&dir.0);
    let reply = client.initialize();
    let result = &reply["result"];
    assert_eq!(result["protocolVersion"], "2025-06-18");
    assert_eq!(result["serverInfo"]["name"], "tasks");
    assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
    assert!(result["instructions"].as_str().unwrap().contains("submit"));
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
    assert!(client.close().success());
}

#[test]
fn tools_list_matches_the_schemas() {
    let dir = Dir::new("list");
    let mut client = Client::start(&dir.0);
    client.initialize();
    let tools = client.request("tools/list", json!({}))["result"]["tools"].clone();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "cluster_status",
            "list_jobs",
            "job",
            "job_logs",
            "submit",
            "cancel",
            "wait_job",
            "who_is_compiling",
            "search_logs",
            "job_cost"
        ]
    );
    for tool in tools.as_array().unwrap() {
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert!(tool["description"].as_str().unwrap().len() > 40);
        let properties = schema["properties"].as_object().unwrap();
        for key in schema["required"].as_array().into_iter().flatten() {
            assert!(properties.contains_key(key.as_str().unwrap()));
        }
    }
    let submit = &tools[4]["inputSchema"];
    assert_eq!(submit["required"], json!(["who", "cwd", "argv"]));
    assert_eq!(
        submit["properties"]["class"]["enum"],
        json!(["compile", "run", "device"])
    );
    // The documented -j limit is the default daemon limit in this isolated state.
    assert!(
        tools[4]["description"]
            .as_str()
            .unwrap()
            .contains("at most 2")
    );
    assert_eq!(tools[5]["inputSchema"]["required"], json!(["id", "who"]));
    assert!(client.close().success());
}

/// Read-only tools against a fake ledger with three jobs and a status fixture.
#[test]
fn read_only_tools_happy_paths() {
    let dir = Dir::new("read");
    let snapshot: Value = serde_json::from_slice(
        &std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/snapshot/metrics-laptop-busy.json"
        ))
        .unwrap(),
    )
    .unwrap();
    let machine = snapshot["machine"]["id"].clone();
    let jobs = json!([
        job(
            40,
            "synthetic-a",
            "passed",
            &["cargo", "build"],
            json!("compile"),
            Some(0)
        ),
        job(
            41,
            "synthetic-b",
            "failed",
            &["cargo", "test"],
            json!("compile"),
            Some(101)
        ),
        job(
            42,
            "synthetic-a",
            "running",
            &["/bin/sleep", "9"],
            json!("run"),
            None
        ),
    ]);
    let log: String = (1..=10).map(|i| format!("line-{i}\n")).collect();
    let fake = {
        let snapshot = snapshot.clone();
        Fake::start(
            &dir.0.join("taskd.sock"),
            Box::new(move |action, reply| match action {
                Action::Status => {
                    *reply =
                        json!({"version": PROTOCOL_VERSION, "error": null, "status": snapshot});
                }
                Action::List => reply["jobs"] = jobs.clone(),
                Action::Job(id) => {
                    reply["jobs"] = json!(
                        jobs.as_array()
                            .unwrap()
                            .iter()
                            .filter(|j| j["id"] == *id)
                            .collect::<Vec<_>>()
                    )
                }
                Action::Logs(id) => {
                    reply["log"] = json!({"job_id": id, "text": log, "truncated": false})
                }
                _ => reply["error"] = "unexpected".into(),
            }),
        )
    };
    let mut client = Client::start(&dir.0);
    client.initialize();

    let status = client.tool("cluster_status", json!({}));
    assert_eq!(status["isError"], false, "{status}");
    assert_eq!(status["structuredContent"]["machine"]["id"], machine);
    assert!(
        status["structuredContent"]["machine"]
            .get("history")
            .is_none()
    );
    assert!(text(&status).contains("blocker:"), "{}", text(&status));
    // Per-class blockers come from the (isolated) local admission report.
    assert!(
        text(&status).contains("run-slot blocker:")
            || text(&status).contains("admission report unavailable"),
        "{}",
        text(&status)
    );

    let listed = client.tool("list_jobs", json!({"owner": "synthetic-a", "limit": 1}));
    let s = &listed["structuredContent"];
    assert_eq!(
        (s["matched"].clone(), s["shown"].clone()),
        (json!(2), json!(1))
    );
    assert_eq!(s["jobs"][0]["id"], 42);
    assert_eq!(s["jobs"][0]["argv"], json!(["/bin/sleep", "9"]));
    let failed = client.tool("list_jobs", json!({"phases": ["failed"]}));
    assert_eq!(failed["structuredContent"]["jobs"][0]["exit_code"], 101);
    let active = client.tool("list_jobs", json!({"active_only": true}));
    assert_eq!(active["structuredContent"]["matched"], 1);

    let one = client.tool("job", json!({"id": 41}));
    assert_eq!(one["structuredContent"]["phase"], "failed");
    assert_eq!(one["structuredContent"]["owner"], "synthetic-b");
    assert!(
        text(&one).contains("job 41 failed (exit 101)"),
        "{}",
        text(&one)
    );
    let missing = client.tool("job", json!({"id": 99}));
    assert_eq!(missing["isError"], true);

    let logs = client.tool("job_logs", json!({"id": 42, "tail_lines": 2}));
    assert_eq!(logs["structuredContent"]["text"], "line-9\nline-10\n");
    assert_eq!(logs["structuredContent"]["omitted_lines"], 8);

    let waited = client.tool(
        "wait_job",
        json!({"id": 40, "timeout_s": 5, "tail_lines": 1}),
    );
    assert_eq!(waited["structuredContent"]["timed_out"], false);
    assert_eq!(waited["structuredContent"]["exit_code"], 0);
    assert_eq!(waited["structuredContent"]["log_tail"], "line-10\n");
    let pending = client.tool("wait_job", json!({"id": 42, "timeout_s": 0}));
    assert_eq!(pending["structuredContent"]["timed_out"], true);
    assert!(text(&pending).contains("keeps running"));

    let types = fake.types();
    assert!(
        types
            .iter()
            .all(|t| ["status", "list", "job", "logs"].contains(&t.as_str())),
        "{types:?}"
    );
    assert!(client.close().success());
}

/// Submit goes through Submit -> ApproveCommand -> StartWorker with the spec
/// `tasks run` would send, then (wait=true) polls Job and fetches the log.
#[test]
fn submit_routes_through_the_submit_action() {
    let dir = Dir::new("submit");
    let checkout = Dir::new("submit-checkout");
    let polls = Arc::new(Mutex::new(0));
    let fake = {
        let polls = polls.clone();
        Fake::start(
            &dir.0.join("taskd.sock"),
            Box::new(move |action, reply| match action {
                Action::Submit(spec) => {
                    reply["submitted_id"] = 7.into();
                    let mut row = job(7, &spec.owner, "queued", &[], json!("run"), None);
                    row["spec"] = serde_json::to_value(spec).unwrap();
                    // Another owner is live in the same checkout: expect a warning.
                    let mut other = job(
                        6,
                        "synthetic-other",
                        "running",
                        &["true"],
                        json!("run"),
                        None,
                    );
                    other["spec"]["source"] = row["spec"]["source"].clone();
                    reply["jobs"] = json!([other, row]);
                }
                Action::ApproveCommand(7) | Action::StartWorker => {}
                Action::Job(7) => {
                    let mut n = polls.lock().unwrap();
                    *n += 1;
                    let phase = if *n < 3 { "running" } else { "passed" };
                    let exit = (*n >= 3).then_some(0);
                    reply["jobs"] = json!([job(
                        7,
                        "synthetic-mcp",
                        phase,
                        &["true"],
                        json!("run"),
                        exit
                    )]);
                }
                Action::Logs(7) => {
                    reply["log"] = json!({"job_id": 7, "text": "built\nok\n", "truncated": false})
                }
                _ => reply["error"] = "unexpected".into(),
            }),
        )
    };
    let mut client = Client::start(&dir.0);
    client.initialize();
    let argv = json!(["cargo", "test", "--offline", "--locked", "-j", "2"]);
    let result = client.tool(
        "submit",
        json!({"who": "synthetic-mcp", "cwd": checkout.0, "argv": argv}),
    );
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["structuredContent"]["job_id"], 7);
    assert!(
        result["structuredContent"]["warning"]
            .as_str()
            .unwrap()
            .contains("synthetic-other")
    );
    assert_eq!(fake.types(), ["submit", "approve_command", "start_worker"]);
    let seen = fake.seen();
    let spec = &seen[0]["data"];
    assert_eq!(spec["suite"], "local.command");
    assert_eq!(spec["owner"], "synthetic-mcp");
    assert_eq!(spec["source"], checkout.canonical());
    assert_eq!(spec["revision"], "live-workspace");
    assert_eq!(spec["cost"], "cpu_simulation");
    let recipe: Value = serde_json::from_str(spec["configuration"].as_str().unwrap()).unwrap();
    assert_eq!(recipe, json!({"argv": argv, "resource": "compile"}));
    assert_eq!(seen[1]["data"], 7);

    // Device class, waiting, with progress notifications.
    let id = json!(100);
    client.send_raw(
        &json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": "submit", "_meta": {"progressToken": "tok"},
            "arguments": {"who": "synthetic-mcp", "cwd": checkout.0, "argv": ["adb", "devices"],
                          "class": "device", "device_serial": "emulator-5554",
                          "wait": true, "timeout_s": 30, "tail_lines": 1}}})
        .to_string(),
    );
    let reply = client.reply(&id);
    let result = &reply["result"];
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["structuredContent"]["phase"], "passed");
    assert_eq!(result["structuredContent"]["exit_code"], 0);
    assert_eq!(result["structuredContent"]["log_tail"], "ok\n");
    assert!(text(result).contains("passed (exit 0)"));
    let progress: Vec<&Value> = client
        .all
        .iter()
        .filter(|m| m["method"] == "notifications/progress")
        .collect();
    assert!(
        !progress.is_empty()
            && progress
                .iter()
                .all(|m| m["params"]["progressToken"] == "tok")
    );
    let seen = fake.seen();
    let recipe: Value =
        serde_json::from_str(seen[3]["data"]["configuration"].as_str().unwrap()).unwrap();
    assert_eq!(recipe["resource"], json!({"device": "emulator-5554"}));
    let types = fake.types();
    assert_eq!(&types[3..6], ["submit", "approve_command", "start_worker"]);
    assert!(
        types[6..types.len() - 1].iter().all(|t| t == "job"),
        "{types:?}"
    );
    assert_eq!(types.last().unwrap(), "logs");
    assert!(client.close().success());
}

/// Policy refusals happen before anything reaches the daemon.
#[test]
fn submit_refuses_what_tasks_run_refuses() {
    let dir = Dir::new("refuse");
    let checkout = Dir::new("refuse-checkout");
    let fake = Fake::start(
        &dir.0.join("taskd.sock"),
        Box::new(|_, reply| reply["error"] = "must not be called".into()),
    );
    let mut client = Client::start(&dir.0);
    client.initialize();
    let cwd = checkout.0.to_str().unwrap();
    for (arguments, needle) in [
        (
            json!({"who": "w", "cwd": cwd, "argv": ["adb", "install", "x.apk"],
                "class": "device", "device_serial": "ZD22257P6G"}),
            "denylisted device ZD22257P6G",
        ),
        (
            json!({"who": "w", "cwd": cwd, "argv": ["adb", "-s", "ZD22257P6G", "shell"], "class": "run"}),
            "names denylisted device ZD22257P6G",
        ),
        (
            json!({"who": "w", "cwd": cwd, "argv": ["cargo", "build", "-j", "8"]}),
            "Cargo jobs",
        ),
        (
            json!({"who": "w", "cwd": "relative", "argv": ["true"]}),
            "absolute",
        ),
        (
            json!({"who": "w", "cwd": "/nonexistent/checkout", "argv": ["true"]}),
            "/nonexistent/checkout",
        ),
        (
            json!({"who": "", "cwd": cwd, "argv": ["true"]}),
            "bounded owner",
        ),
        (
            json!({"who": "w", "cwd": cwd, "argv": ["tasks-offload"], "class": "offload"}),
            "class must be",
        ),
    ] {
        let result = client.tool("submit", arguments.clone());
        assert_eq!(result["isError"], true, "{arguments}: {result}");
        assert!(
            text(&result).contains(needle),
            "{arguments}: {}",
            text(&result)
        );
    }
    assert!(fake.seen().is_empty(), "{:?}", fake.seen());
    assert!(client.close().success());
}

#[test]
fn cancel_requires_the_owner() {
    let dir = Dir::new("cancel");
    let fake = Fake::start(
        &dir.0.join("taskd.sock"),
        Box::new(|action, reply| match action {
            Action::Job(id @ 5..=7) => {
                let (owner, phase) = match id {
                    5 => ("synthetic-other", "running"),
                    6 => ("synthetic-me", "running"),
                    _ => ("synthetic-me", "passed"),
                };
                let exit = (phase == "passed").then_some(0);
                reply["jobs"] = json!([job(*id, owner, phase, &["true"], json!("run"), exit)]);
            }
            Action::Cancel(6) => {}
            _ => reply["error"] = "unexpected".into(),
        }),
    );
    let mut client = Client::start(&dir.0);
    client.initialize();
    let refused = client.tool("cancel", json!({"id": 5, "who": "synthetic-me"}));
    assert_eq!(refused["isError"], true);
    assert!(
        text(&refused).contains("belongs to \"synthetic-other\""),
        "{}",
        text(&refused)
    );
    assert_eq!(fake.types(), ["job"], "no cancel was sent");
    let done = client.tool("cancel", json!({"id": 7, "who": "synthetic-me"}));
    assert_eq!(done["isError"], false);
    assert!(text(&done).contains("already finished"));
    let ok = client.tool("cancel", json!({"id": 6, "who": "synthetic-me"}));
    assert_eq!(ok["isError"], false, "{ok}");
    assert_eq!(ok["structuredContent"]["cancel_requested"], true);
    let seen = fake.seen();
    let cancels: Vec<&Value> = seen.iter().filter(|a| a["type"] == "cancel").collect();
    assert_eq!(cancels, [&json!({"type": "cancel", "data": 6})]);
    assert!(client.close().success());
}

/// No daemon socket at all: every tool fails as a tool error, the server stays up.
#[test]
fn daemon_down_is_a_clean_tool_error() {
    let dir = Dir::new("down");
    let checkout = Dir::new("down-checkout");
    let mut client = Client::start(&dir.0);
    client.initialize();
    for (name, arguments) in [
        ("cluster_status", json!({})),
        ("list_jobs", json!({})),
        ("job", json!({"id": 1})),
        ("job_logs", json!({"id": 1})),
        ("wait_job", json!({"id": 1, "timeout_s": 1})),
        ("cancel", json!({"id": 1, "who": "synthetic-me"})),
        (
            "submit",
            json!({"who": "synthetic-me", "cwd": checkout.0, "argv": ["true"], "class": "run"}),
        ),
    ] {
        let result = client.tool(name, arguments);
        assert_eq!(result["isError"], true, "{name}: {result}");
        assert!(
            text(&result).contains("tasks daemon"),
            "{name}: {}",
            text(&result)
        );
    }
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
    assert!(
        !dir.0.join("taskd.sock").exists(),
        "--no-start-daemon started nothing"
    );
    assert!(client.close().success());
}

#[test]
fn malformed_json_rpc_unknown_tool_and_method() {
    let dir = Dir::new("malformed");
    let mut client = Client::start(&dir.0);
    client.initialize();
    for (line, id, code) in [
        ("this is not json", Value::Null, -32700),
        (
            "[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}]",
            Value::Null,
            -32600,
        ),
        ("{\"jsonrpc\":\"2.0\",\"id\":11}", json!(11), -32600),
        ("{\"id\":12,\"method\":\"ping\"}", json!(12), -32600),
        (
            "{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"no/such\"}",
            json!(13),
            -32601,
        ),
        (
            "{\"jsonrpc\":\"2.0\",\"id\":14,\"method\":\"tools/call\",\"params\":{\"name\":\"rm_rf\"}}",
            json!(14),
            -32602,
        ),
    ] {
        client.send_raw(line);
        let reply = client.reply(&id);
        assert_eq!(reply["error"]["code"], code, "{line}: {reply}");
        assert!(reply["error"]["message"].is_string());
    }
    // A notification for an unknown method is ignored; the server still works.
    client.send_raw("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/whatever\"}");
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
    assert!(client.close().success());
}

/// A long wait runs on its own thread: ping is answered meanwhile, and a client
/// cancellation stops the wait without a response and without cancelling the job.
#[test]
fn waits_are_concurrent_and_client_cancellable() {
    let dir = Dir::new("concurrent");
    let fake = Fake::start(
        &dir.0.join("taskd.sock"),
        Box::new(|action, reply| match action {
            Action::Job(3) => {
                reply["jobs"] = json!([job(
                    3,
                    "synthetic-me",
                    "running",
                    &["true"],
                    json!("run"),
                    None
                )])
            }
            _ => reply["error"] = "unexpected".into(),
        }),
    );
    let mut client = Client::start(&dir.0);
    client.initialize();
    client.send_raw(
        r#"{"jsonrpc":"2.0","id":"w","method":"tools/call","params":{"name":"wait_job","arguments":{"id":3,"timeout_s":60}}}"#,
    );
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
    assert!(started.elapsed() < Duration::from_secs(5));
    client.send_raw(
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"w","reason":"test"}}"#,
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
    assert!(
        client.all.iter().all(|m| m["id"] != "w"),
        "cancelled request got a reply"
    );
    assert!(
        fake.types().iter().all(|t| t == "job"),
        "{:?}",
        fake.types()
    );
    let started = Instant::now();
    assert!(client.close().success());
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// A client that writes its requests and closes stdin at once (a pipe) still
/// gets every response; a pending wait returns early instead of blocking exit.
#[test]
fn responses_are_sent_after_stdin_closes() {
    let dir = Dir::new("eof");
    let _fake = Fake::start(
        &dir.0.join("taskd.sock"),
        Box::new(|action, reply| match action {
            Action::Job(3) => {
                reply["jobs"] = json!([job(
                    3,
                    "synthetic-me",
                    "running",
                    &["true"],
                    json!("run"),
                    None
                )])
            }
            _ => reply["error"] = "unexpected".into(),
        }),
    );
    let mut client = Client::start(&dir.0);
    for (id, name, arguments) in [
        (1, "wait_job", json!({"id": 3, "timeout_s": 600})),
        (2, "job", json!({"id": 3})),
    ] {
        client.send_raw(
            &json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": {"name": name, "arguments": arguments}})
            .to_string(),
        );
    }
    let started = Instant::now();
    let (status, messages) = client.finish();
    assert!(status.success());
    assert!(started.elapsed() < Duration::from_secs(10));
    let reply = |id: i64| {
        messages
            .iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("no reply {id}: {messages:?}"))
    };
    assert_eq!(reply(1)["result"]["structuredContent"]["timed_out"], true);
    assert_eq!(reply(2)["result"]["structuredContent"]["phase"], "running");
}

/// Real daemon and worker with isolated state and TASKS_LOCK_DIR: submit a
/// run-class command through MCP and wait for it. Subject to the host's real
/// PSI/memory admission gates, so ignored by default. Run its prebuilt test
/// binary directly (its own worker acquires admission), not inside `tasks run`:
///   target/debug/deps/mcp-* --ignored --nocapture real_daemon
#[test]
#[ignore = "real daemon/worker E2E; needs host PSI/memory headroom; run outside tasks run"]
fn real_daemon_submit_and_wait() {
    let dir = Dir::new("e2e");
    let checkout = Dir::new("e2e-checkout");
    let state = dir.0.join("state");
    let bin = env!("CARGO_BIN_EXE_tasks");
    let mut daemon = Command::new(bin)
        .arg("daemon")
        .env("TASK_MANAGER_STATE_DIR", &state)
        .env("TASKS_LOCK_DIR", dir.0.join("locks"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while !state.join("taskd.sock").exists() {
        assert!(Instant::now() < end, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut client = Client::start(&state);
    client.initialize();
    let result = client.tool(
        "submit",
        json!({"who": "synthetic-mcp-e2e", "cwd": checkout.0, "class": "run",
               "argv": ["/bin/sh", "-c", "echo hello-from-mcp"],
               "wait": true, "timeout_s": 300, "tail_lines": 5}),
    );
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["structuredContent"]["phase"], "passed", "{result}");
    assert!(
        result["structuredContent"]["log_tail"]
            .as_str()
            .unwrap()
            .contains("hello-from-mcp")
    );
}
