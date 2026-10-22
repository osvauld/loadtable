//! `tasks mcp`: a Model Context Protocol server (JSON-RPC 2.0, one message per
//! line on stdin/stdout) that lets agents use the shared queue without shelling
//! out to `tasks run`. Every tool talks to the local daemon over its existing
//! Unix socket with the existing actions (List, Job, Submit, ApproveCommand,
//! StartWorker, Cancel, Logs, Status) plus the read-only admission report; no
//! daemon action is added. Submissions go through the same client policy as
//! `tasks run` (`Recipe::spec`: argv bounds, Cargo `-j` cap, helper nesting,
//! device denylist) and the daemon/worker re-check it at approval/execution.
//!
//! Stdout carries only JSON-RPC: it is moved to a private descriptor at startup
//! and fd 1 is pointed at stderr, so a stray print cannot corrupt the stream.
//! Logs go to stderr. Each `tools/call` runs on its own thread, so a long
//! `wait_job` does not block `ping` or other calls; `notifications/cancelled`
//! stops a wait (never the job itself).
//!
//! Placement: every tool talks to the LOCAL daemon. `submit` takes `placement`:
//! `"local"`, a peer name (forwarded over the iroh peer link with
//! `peer_link::forwarded_spec`, exactly like `tasks run --peer NAME`) or
//! `"anywhere"` (the default once a worker peer is configured; the library's
//! `placement::decide` picks the machine from this machine's snapshot and the
//! peers' latest snapshots, which the daemon's peer runtime keeps). A forwarded
//! job is a local job here (offload slot) whose log mirrors the peer's job.
//! The read tools accept only `"local"`; `cluster_status` lists every machine.
use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tinline_task_manager::health::{Severity, cluster};
use tinline_task_manager::local_command::{Recipe, Resource};
use tinline_task_manager::{Action, Job, Phase, Response, Result, metrics::Snapshot, placement};

use crate::display;

/// Newest protocol revision this server implements; older ones it also accepts.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const USAGE: &str =
    "usage: tasks mcp [--no-start-daemon]  # MCP server on stdio (newline-delimited JSON-RPC 2.0)";
/// Bound on one incoming JSON-RPC line.
const MAX_LINE: u64 = 1 << 20;
const MAX_IN_FLIGHT: usize = 32;
const POLL: Duration = Duration::from_millis(500);
const PROGRESS_EVERY: Duration = Duration::from_secs(15);
pub const DEFAULT_WAIT_S: i64 = 600;
pub const MAX_WAIT_S: i64 = 3600;
const MAX_TAIL: i64 = 5000;

// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const SERVER_BUSY: i64 = -32000;

const INSTRUCTIONS: &str = "Shared build/test queue of this machine (the `tasks` daemon) and \
of its peer machines (e.g. a LAN build worker). Use `submit` instead of running cargo, gradle \
or heavy tests directly. Always pass your own unique `who` and the absolute path of your OWN \
checkout as `cwd`. Placement: by default (`anywhere`, once a worker peer is configured) the \
task manager picks the machine that should finish first and sets Cargo's job count to that \
machine's cap; `local` keeps it here (Cargo `-j` at most this machine's limit); a peer name \
forwards it there. Check `cluster_status` to see every machine and why work is queued; queuing \
under pressure is intended, never work around it. Never resubmit a job that is still live: \
use `wait_job` / `job_logs` with its id. `cancel` only your own jobs. Never use the device \
ZD22257P6G. `who_is_compiling` shows who is compiling which crate right now on every machine, \
`search_logs` searches job logs on every machine, `job_cost` shows what a job cost.";

/// Where a tool's daemon requests go: the local daemon's socket (it forwards to peers).
pub struct Daemon {
    pub socket: PathBuf,
    /// Start `tasks daemon` when no socket answers, like every other subcommand.
    pub autostart: bool,
}

impl Daemon {
    fn unreachable(&self, error: impl std::fmt::Display) -> String {
        format!("tasks daemon ({}): {error}", self.socket.display())
    }

    /// One request with the CLI's retry/reconcile rules (`request_with_reconnect`):
    /// a lost submit reply is reconciled against the ledger, never resent.
    pub fn call(&self, action: Action) -> Result<Response> {
        let start = || -> Result<()> {
            if self.autostart {
                crate::ensure_daemon(&self.socket)
            } else {
                Err(
                    "the daemon is not running (tasks mcp --no-start-daemon does not start it)"
                        .into(),
                )
            }
        };
        tinline_task_manager::request_with_reconnect(&self.socket, action, start)
            .map_err(|e| self.unreachable(e).into())
    }

    /// A call whose daemon-side error is also an error.
    pub fn ok(&self, action: Action) -> Result<Response> {
        let response = self.call(action)?;
        match response.error {
            Some(error) => Err(error.into()),
            None => Ok(response),
        }
    }

    /// One job; falls back to the full listing on a daemon older than `Job`.
    pub fn job(&self, id: i64) -> Result<Job> {
        let mut response = self.call(Action::Job(id))?;
        if response
            .error
            .as_deref()
            .is_some_and(|e| e.contains("unknown variant"))
        {
            response = self.call(Action::List)?;
        }
        if let Some(error) = response.error {
            return Err(format!("job {id}: {error}").into());
        }
        response
            .jobs
            .into_iter()
            .find(|j| j.id == id)
            .ok_or_else(|| format!("job {id} is not in the ledger").into())
    }

    /// The daemon's state directory (forwarded jobs name it; placement records live there).
    pub fn state(&self) -> PathBuf {
        let dir = self.socket.parent().map(PathBuf::from).unwrap_or_default();
        std::fs::canonicalize(&dir).unwrap_or(dir)
    }

    /// Starts the daemon when allowed and needed (placement reads it before submitting).
    fn ensure(&self) -> Result<()> {
        if self.autostart {
            crate::ensure_daemon(&self.socket).map_err(|e| self.unreachable(e).into())
        } else {
            Ok(())
        }
    }

    pub fn status(&self) -> Result<Snapshot> {
        if self.autostart {
            crate::ensure_daemon(&self.socket).map_err(|e| self.unreachable(e))?;
        }
        let reply = tinline_task_manager::request_status(&self.socket, Duration::from_secs(60))
            .map_err(|e| self.unreachable(e))?;
        reply.status.ok_or_else(|| {
            reply
                .error
                .unwrap_or_else(|| "daemon returned no status".into())
                .into()
        })
    }
}

/// The JSON-RPC output stream, shared by the reader and tool threads.
#[derive(Clone)]
pub struct Out(Arc<Mutex<Box<dyn Write + Send>>>);

impl Out {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(writer)))
    }

    /// Moves the process's stdout to a private close-on-exec descriptor and
    /// points fd 1 at stderr, so only this writer can reach the client.
    fn take_stdout() -> Result<Self> {
        use std::os::fd::FromRawFd;
        std::io::stdout().flush()?;
        let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self::new(Box::new(unsafe {
            std::fs::File::from_raw_fd(fd)
        })))
    }

    pub fn send(&self, message: &Value) {
        let mut line = serde_json::to_vec(message).unwrap_or_default();
        line.push(b'\n');
        let mut out = self.0.lock().unwrap_or_else(|e| e.into_inner());
        // A closed client pipe is the client's choice; there is nobody to tell.
        let _ = out.write_all(&line).and_then(|()| out.flush());
    }
}

fn error_message(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

fn result_message(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn run(args: &[String]) -> Result<()> {
    let mut autostart = true;
    for arg in args {
        match arg.as_str() {
            "--no-start-daemon" => autostart = false,
            _ => return Err(USAGE.into()),
        }
    }
    let out = Out::take_stdout()?;
    let server = Arc::new(Server {
        daemon: Daemon {
            socket: tinline_task_manager::state_dir().join("taskd.sock"),
            autostart,
        },
        out,
        in_flight: Mutex::default(),
        shutdown: Arc::default(),
    });
    eprintln!(
        "tasks mcp: MCP {PROTOCOL_VERSION} on stdio; daemon socket {}{}",
        server.daemon.socket.display(),
        if autostart { "" } else { " (no autostart)" }
    );
    let mut stdin = std::io::stdin().lock();
    let mut threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    loop {
        let mut line = Vec::new();
        let read = (&mut stdin)
            .take(MAX_LINE + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if line.len() as u64 > MAX_LINE && !line.ends_with(b"\n") {
            // Skip the rest of the oversized line, then report it.
            let mut rest = Vec::new();
            stdin.read_until(b'\n', &mut rest)?;
            server.out.send(&error_message(
                Value::Null,
                INVALID_REQUEST,
                format!("message larger than {MAX_LINE} bytes"),
            ));
            continue;
        }
        if let Some(thread) = server.handle_line(&line) {
            threads.push(thread);
        }
        threads.retain(|t| !t.is_finished());
    }
    // No more requests: waits return early with the job's current state (jobs
    // are never touched), and every in-flight call still gets its response,
    // since a client may close stdin before reading them.
    server.shutdown.store(true, Ordering::SeqCst);
    for thread in threads {
        let _ = thread.join();
    }
    eprintln!("tasks mcp: stdin closed; exiting");
    Ok(())
}

pub struct Server {
    pub daemon: Daemon,
    pub out: Out,
    /// Cancellation flags of running `tools/call`s, keyed by the JSON of their id.
    pub in_flight: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Set at stdin EOF: waits stop early, responses are still sent.
    pub shutdown: Arc<AtomicBool>,
}

impl Server {
    /// Handles one input line. Returns the thread of a started tool call.
    pub fn handle_line(self: &Arc<Self>, line: &[u8]) -> Option<std::thread::JoinHandle<()>> {
        let text = String::from_utf8_lossy(line);
        if text.trim().is_empty() {
            return None;
        }
        let message: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                self.out.send(&error_message(
                    Value::Null,
                    PARSE_ERROR,
                    format!("parse error: {error}"),
                ));
                return None;
            }
        };
        let Some(object) = message.as_object() else {
            let why = if message.is_array() {
                "JSON-RPC batches are not supported (removed in MCP 2025-06-18)"
            } else {
                "a JSON-RPC message must be an object"
            };
            self.out
                .send(&error_message(Value::Null, INVALID_REQUEST, why));
            return None;
        };
        let id = match object.get("id") {
            None => None,
            Some(id @ (Value::String(_) | Value::Number(_))) => Some(id.clone()),
            Some(_) => {
                self.out.send(&error_message(
                    Value::Null,
                    INVALID_REQUEST,
                    "id must be a string or a number",
                ));
                return None;
            }
        };
        let reply_id = id.clone().unwrap_or(Value::Null);
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            self.out.send(&error_message(
                reply_id,
                INVALID_REQUEST,
                "jsonrpc must be \"2.0\"",
            ));
            return None;
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            if id.is_some() && (object.contains_key("result") || object.contains_key("error")) {
                // A response to a request this server never sends; ignore it.
                eprintln!("tasks mcp: ignoring unexpected response {reply_id}");
            } else {
                self.out.send(&error_message(
                    reply_id,
                    INVALID_REQUEST,
                    "method must be a string",
                ));
            }
            return None;
        };
        let params = object.get("params").cloned().unwrap_or(json!({}));
        let Some(id) = id else {
            self.notification(method, &params);
            return None;
        };
        if !params.is_object() {
            self.out.send(&error_message(
                id,
                INVALID_PARAMS,
                "params must be an object",
            ));
            return None;
        }
        match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                let version = requested
                    .filter(|v| SUPPORTED_VERSIONS.contains(v))
                    .unwrap_or(PROTOCOL_VERSION);
                self.out.send(&result_message(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {
                            "name": "tasks",
                            "title": "tasks: shared build/test queue",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "instructions": INSTRUCTIONS,
                    }),
                ));
            }
            "ping" => self.out.send(&result_message(id, json!({}))),
            "tools/list" => self
                .out
                .send(&result_message(id, json!({"tools": tools()}))),
            "tools/call" => return self.tools_call(id, &params),
            _ => self.out.send(&error_message(
                id,
                METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            )),
        }
        None
    }

    fn notification(&self, method: &str, params: &Value) {
        match method {
            "notifications/initialized" => eprintln!("tasks mcp: client initialized"),
            "notifications/cancelled" => {
                if let Some(id) = params.get("requestId") {
                    let key = id.to_string();
                    if let Some(flag) = self
                        .in_flight
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&key)
                    {
                        flag.store(true, Ordering::SeqCst);
                        eprintln!("tasks mcp: request {key} cancelled by the client");
                    }
                }
            }
            _ => {}
        }
    }

    fn tools_call(
        self: &Arc<Self>,
        id: Value,
        params: &Value,
    ) -> Option<std::thread::JoinHandle<()>> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            self.out.send(&error_message(
                id,
                INVALID_PARAMS,
                "tools/call needs a string name",
            ));
            return None;
        };
        if !TOOL_NAMES.contains(&name) {
            self.out.send(&error_message(
                id,
                INVALID_PARAMS,
                format!("unknown tool: {name}; tools/list names the available tools"),
            ));
            return None;
        }
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => {
                self.out.send(&error_message(
                    id,
                    INVALID_PARAMS,
                    "arguments must be an object",
                ));
                return None;
            }
        };
        let key = id.to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
            if in_flight.contains_key(&key) {
                drop(in_flight);
                self.out.send(&error_message(
                    id,
                    INVALID_REQUEST,
                    "a request with this id is still in progress",
                ));
                return None;
            }
            if in_flight.len() >= MAX_IN_FLIGHT {
                drop(in_flight);
                self.out.send(&error_message(
                    id,
                    SERVER_BUSY,
                    format!("too many concurrent tool calls (max {MAX_IN_FLIGHT})"),
                ));
                return None;
            }
            in_flight.insert(key.clone(), cancel.clone());
        }
        let server = self.clone();
        let name = name.to_owned();
        let progress = params
            .get("_meta")
            .and_then(|m| m.get("progressToken"))
            .filter(|t| t.is_string() || t.is_number())
            .cloned();
        Some(std::thread::spawn(move || {
            let ctx = Ctx {
                cancel: cancel.clone(),
                shutdown: server.shutdown.clone(),
                out: server.out.clone(),
                progress,
                sent: AtomicU64::new(0),
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                call_tool(&server.daemon, &name, &arguments, &ctx)
            }));
            server
                .in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            // A cancelled request gets no response (MCP cancellation rules).
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            server.out.send(&match outcome {
                Ok(result) => result_message(id, result),
                Err(_) => error_message(id, INTERNAL_ERROR, format!("tool {name} panicked")),
            });
        }))
    }
}

/// Per-call context: client cancellation, server shutdown and optional
/// progress notifications.
pub struct Ctx {
    pub cancel: Arc<AtomicBool>,
    pub shutdown: Arc<AtomicBool>,
    pub out: Out,
    pub progress: Option<Value>,
    pub sent: AtomicU64,
}

impl Ctx {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
    fn shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }
    fn progress(&self, message: String) {
        if let Some(token) = &self.progress {
            let n = self.sent.fetch_add(1, Ordering::SeqCst) + 1;
            self.out.send(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"progressToken": token, "progress": n, "message": message},
            }));
        }
    }
}

pub const TOOL_NAMES: [&str; 12] = [
    "cluster_status",
    "list_jobs",
    "job",
    "job_logs",
    "submit",
    "cancel",
    "wait_job",
    "who_is_compiling",
    "search_logs",
    "job_cost",
    "alerts",
    "submit_steps",
];

const PHASES: [&str; 11] = [
    "queued",
    "approved",
    "preparing",
    "waiting_build",
    "building",
    "waiting_run",
    "running",
    "passed",
    "failed",
    "interrupted",
    "cancelled",
];

fn placement_schema() -> Value {
    json!({
        "type": "string", "enum": ["local"], "default": "local",
        "description": "Which daemon answers: always this machine's (\"local\"). Jobs \
                        forwarded to a peer are local jobs here; `job`/`wait_job` show the \
                        peer and its remote job id, and cluster_status lists every machine."
    })
}

fn submit_placement_schema() -> Value {
    json!({
        "type": "string",
        "description": "Where the command runs. \"anywhere\" (the default when a worker peer is \
            configured; otherwise the same as local): the task manager compares the expected \
            wait (slot holder ETA, queue, pressure) plus run time (median of earlier passed runs \
            of this command per machine) of this machine and each online worker peer, picks the \
            sooner (ties go to the worker), and replaces a Cargo -j with CARGO_BUILD_JOBS = that \
            machine's cap. class device always stays local; class run stays local unless a peer \
            is named. \"local\": this machine, command unchanged. A peer name (see \
            cluster_status, e.g. \"worker\"): forward there over the peer link (the checkout is \
            synced; the job runs under the peer's admission; its log streams back; an offline \
            peer is an error and nothing is submitted)."
    })
}

/// Tool definitions for `tools/list`. The Cargo `-j` limit in `submit` is the one
/// this client would enforce right now (worker record, else environment/default).
pub fn tools() -> Vec<Value> {
    let max_jobs = tinline_task_manager::admission::submission_max_cargo_jobs()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "2".into());
    let id = |what: &str| json!({"type": "integer", "minimum": 1, "description": what});
    vec![
        json!({
            "name": "cluster_status",
            "title": "Machine and queue status",
            "description": "Live status of every machine: this machine's shared build/test queue \
                (same data as `tasks status --json`, at the top level) plus `machines`: this \
                machine and each peer (e.g. the LAN worker) with its latest snapshot, its age \
                and an online/stale flag. Per machine: CPU, memory, swap and pressure (PSI), the admission \
                pressure blockers holding work back per slot class (compile, run, offload), the compile/run/offload/device \
                slots with the job and agent holding each, and running and queued jobs with \
                owner, queue position, what they wait for, ETA, live CPU/RSS, build-cache \
                warmth and `compiling` (the crates/Gradle tasks each job's compiler processes \
                are working on right now). Call it before heavy work to see whether it will queue, and to explain \
                a waiting job. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "recent_finished": {"type": "integer", "minimum": 0, "maximum": 50, "default": 5,
                        "description": "How many recently finished jobs to include besides all live ones."},
                    "include_admission": {"type": "boolean", "default": false,
                        "description": "Also include the raw admission report (thresholds, lock holders, waiters, disk reserve). Per-class blockers (compile/run/offload) are always included."},
                    "placement": placement_schema(),
                },
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "list_jobs",
            "title": "List jobs",
            "description": "List jobs in the shared ledger, oldest first, keeping the newest \
                `limit` matches. Filter by `owner` (the exact `who` used to submit), by \
                `phases`, or `active_only` for unfinished jobs. Each job has id, owner, phase, \
                exit code, resource class, cwd and argv. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "owner": {"type": "string", "description": "Only jobs submitted with exactly this `who`."},
                    "phases": {"type": "array", "items": {"type": "string", "enum": PHASES},
                        "description": "Only jobs in one of these phases."},
                    "active_only": {"type": "boolean", "default": false,
                        "description": "Only unfinished jobs (not passed/failed/interrupted/cancelled)."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 20},
                    "placement": placement_schema(),
                },
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "job",
            "title": "One job",
            "description": "One job by id: phase, exit code, owner, cwd, argv, resource \
                class, admission wait and run time per stage, and the worker's execution \
                detail. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": id("Job id."), "placement": placement_schema()},
                "required": ["id"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "job_logs",
            "title": "Job log tail",
            "description": "The last `tail_lines` lines of a job's retained log (plain text; \
                default 100, max 5000). Works while the job runs. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id("Job id."),
                    "tail_lines": {"type": "integer", "minimum": 1, "maximum": MAX_TAIL, "default": 100},
                    "placement": placement_schema(),
                },
                "required": ["id"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "submit",
            "title": "Run a command through the queue",
            "description": format!("Run a command through the shared queue: exactly what \
                `tasks run --who WHO [--run | --device SERIAL] -- ARGV...` does from `cwd` \
                (submit, approve this command, start the worker, then admission). Use it \
                instead of running cargo, gradle or heavy tests directly. The command runs \
                with your user rights in the LIVE checkout at `cwd` (no sandbox, no \
                snapshot): keep that checkout stable while it runs, and use your own \
                checkout, never another agent's. Admission may queue the job behind other \
                agents or until memory/CPU/IO pressure falls; that is intended, never work \
                around it. Rules: Cargo `-j N` must be at most {max_jobs} (the daemon's \
                limit; omit it for the default); do not wrap buildlock.py/build_android.py; \
                argv is not run through a shell (use [\"sh\",\"-c\",\"...\"] if you need \
                one); never put secrets in argv. class `compile` (default) takes the compile \
                slot (a `cargo test` builds there, then runs in a run slot); `run` is for \
                already-built commands; `device` holds one device's lock (device_serial \
                required) and is NOT permission to install, wipe or publish. The device \
                ZD22257P6G is always refused. wait=false (default) returns the job id \
                immediately; then use wait_job / job_logs and never resubmit a live job. \
                wait=true blocks up to timeout_s and returns phase, exit code and a log tail; \
                a timeout leaves the job running. `placement` (default anywhere once a worker \
                peer is configured) may run it on another machine: the result names the \
                machine, why, and the remote job id; with anywhere or a peer, a Cargo -j is \
                replaced by the chosen machine's cap. Policy: a `cargo test` with a name \
                filter but no --lib/--test/--bin/--bench/--doc/--tests/--all-targets builds \
                every test target and is refused by default (a tool error naming the targets \
                your filter matches; nothing is queued): name those targets, or pass \
                broad_test=true to build everything deliberately (recorded on the job)."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "who": {"type": "string", "minLength": 1, "maxLength": 128,
                        "description": "Your unique agent name; recorded as the job owner (like --who)."},
                    "cwd": {"type": "string",
                        "description": "Absolute path of YOUR checkout; the command runs there."},
                    "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 256,
                        "description": "Program and arguments, e.g. [\"cargo\",\"test\",\"--offline\",\"--locked\",\"-j\",\"2\"]."},
                    "class": {"type": "string", "enum": ["compile", "run", "device"], "default": "compile",
                        "description": "Admission class: compile slot (default), run slot (already built), or a device lock."},
                    "device_serial": {"type": "string",
                        "description": "adb serial for class=device (e.g. emulator-5554 or host:port)."},
                    "wait": {"type": "boolean", "default": false,
                        "description": "Block until the job finishes or timeout_s passes."},
                    "timeout_s": {"type": "integer", "minimum": 1, "maximum": MAX_WAIT_S, "default": DEFAULT_WAIT_S,
                        "description": "Wait budget when wait=true. Keep it below your MCP client's tool timeout."},
                    "tail_lines": {"type": "integer", "minimum": 0, "maximum": MAX_TAIL, "default": 60,
                        "description": "Log lines returned when the job finished while waiting."},
                    "placement": submit_placement_schema(),
                    "broad_test": {"type": "boolean", "default": false,
                        "description": "Deliberately build every test target of a filtered test command (e.g. `cargo test -p X NAME` without --lib/--test T). This host's policy refuses such a command by default and the refusal names the targets your filter can be in: prefer naming them. true is recorded on the job."},
                    "pin": {"type": "boolean", "default": false,
                        "description": "Keep the job on the machine chosen at submit: a job still waiting is otherwise moved to an idle worker peer when that clearly finishes it sooner (same job id). placement \"local\" or a named peer also pins."},
                    "after": {"type": "array", "items": {"type": "integer", "minimum": 1}, "minItems": 1, "maxItems": 64,
                        "description": "Job ids (older jobs, yours or not) that must PASS before this job may start. It is approved now but not claimed until they all passed; if any ends failed, cancelled or interrupted this job is cancelled. Prefer submit_steps for a whole plan."},
                    "fetch": {"type": "array", "items": {"type": "string"}, "maxItems": 64,
                        "description": "Only for a job that runs on a peer: relative paths in the checkout to copy back after it passes (into ~/build-cache/offload/WHO/JOB_ID here)."},
                },
                "required": ["who", "cwd", "argv"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": true,
                            "idempotentHint": false, "openWorldHint": false}
        }),
        json!({
            "name": "cancel",
            "title": "Cancel your own job",
            "description": "Cancel one of YOUR unfinished jobs. Refused unless `who` equals \
                the job's owner exactly (the `who` you submitted with): never cancel another \
                agent's job. Cancellation is asynchronous (the worker stops the job's process \
                group); use wait_job to see it reach `cancelled`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id("Job id."),
                    "who": {"type": "string", "minLength": 1, "maxLength": 128,
                        "description": "Your agent name; must equal the job's owner."},
                    "placement": placement_schema(),
                },
                "required": ["id", "who"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": true,
                            "idempotentHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "wait_job",
            "title": "Wait for a job",
            "description": "Wait until a job finishes or `timeout_s` passes (default 600, max \
                3600), polling the daemon. Returns phase, exit code and, once finished, a log \
                tail. A timeout does NOT cancel the job: call again to keep waiting. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": id("Job id."),
                    "timeout_s": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_S, "default": DEFAULT_WAIT_S,
                        "description": "0 checks once without waiting. Keep it below your MCP client's tool timeout."},
                    "tail_lines": {"type": "integer", "minimum": 0, "maximum": MAX_TAIL, "default": 60,
                        "description": "Log lines returned once the job is finished."},
                    "placement": placement_schema(),
                },
                "required": ["id"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "who_is_compiling",
            "title": "Who is compiling what, on every machine",
            "description": "Answers \"who is compiling X right now?\" across this machine and each \
                peer (e.g. the LAN worker). Per machine: every executing job with its owner, \
                agent (owner up to @), checkout, command, phase, elapsed time and live CPU/RSS, \
                plus `compiling`: the compiler processes in that job's process tree at the last \
                2 s sample (crate name, kind lib/bin/test/build-script/build-script-run/doctest, \
                tool rustc/rustdoc/cargo/gradle/kotlin, pid, elapsed, CPU, RSS). With `crate`, \
                only jobs compiling that crate (Cargo package or crate name; - and _ are the \
                same) are listed. A job forwarded to a peer appears under the peer that runs \
                it. Limits: a crate compiled inside an sccache server (cache miss) shows as the \
                sccache client in the job; short compiles between samples are not seen. \
                Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "crate": {"type": "string", "maxLength": 128,
                        "description": "Only jobs compiling this crate/package (e.g. \"profile_sync\" or \"profile-sync\")."},
                    "placement": placement_schema(),
                },
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "search_logs",
            "title": "Search job logs on every machine",
            "description": "Search the retained logs of jobs (newest job first) for a substring \
                (default) or, with regex=true, a small regex subset: literals, ., [a-z], [^...], \
                \\d \\w \\s (and \\D \\W \\S), * + ?, ^ $ and top-level | (no groups or {n,m}). \
                Filter by owner (exact, or the agent part before @), job_ids, phases, since/until \
                (unix seconds or relative like \"2h\", \"30m\", \"3d\": jobs whose run overlaps \
                the window) and machine (\"all\" by default, \"local\", this machine's id, or a \
                peer name). Each match has machine, job id, owner, phase, line number, the line \
                and `context` lines around it. Jobs forwarded to a peer are searched in their \
                mirrored local log and named by the peer (with its remote job id); jobs that \
                exist only on a peer are searched there over the peer link. Bounded per machine \
                by max_jobs, max_matches, max_bytes (log bytes read) and max_output_bytes; \
                `truncated`/`truncated_reason` say when a bound stopped it: narrow the filters \
                rather than raising every bound. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "minLength": 1, "maxLength": 1000,
                        "description": "Substring to find (or regex subset with regex=true)."},
                    "regex": {"type": "boolean", "default": false},
                    "ignore_case": {"type": "boolean", "default": false},
                    "owner": {"type": "string", "description": "Exact owner, or the agent name before @host."},
                    "job_ids": {"type": "array", "items": {"type": "integer", "minimum": 1}, "maxItems": 500,
                        "description": "Only these job ids (ids of the machine that ran them)."},
                    "phases": {"type": "array", "items": {"type": "string", "enum": PHASES}},
                    "since": {"type": ["string", "integer"], "description": "Unix seconds or e.g. \"2h\" (ago)."},
                    "until": {"type": ["string", "integer"], "description": "Unix seconds or e.g. \"30m\" (ago)."},
                    "machine": {"type": "string", "default": "all",
                        "description": "\"all\", \"local\", this machine's id, or a peer name (see cluster_status)."},
                    "context": {"type": "integer", "minimum": 0, "maximum": tinline_task_manager::log_search::MAX_CONTEXT, "default": 0,
                        "description": "Lines of context before and after each match."},
                    "max_jobs": {"type": "integer", "minimum": 1, "maximum": tinline_task_manager::log_search::MAX_MAX_JOBS,
                        "default": tinline_task_manager::log_search::DEFAULT_MAX_JOBS},
                    "max_matches": {"type": "integer", "minimum": 1, "maximum": tinline_task_manager::log_search::MAX_MAX_MATCHES,
                        "default": tinline_task_manager::log_search::DEFAULT_MAX_MATCHES},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": tinline_task_manager::log_search::MAX_MAX_BYTES,
                        "default": tinline_task_manager::log_search::DEFAULT_MAX_BYTES},
                    "max_output_bytes": {"type": "integer", "minimum": 1024, "maximum": tinline_task_manager::log_search::MAX_MAX_OUTPUT_BYTES,
                        "default": tinline_task_manager::log_search::DEFAULT_MAX_OUTPUT_BYTES},
                    "placement": placement_schema(),
                },
                "required": ["pattern"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "job_cost",
            "title": "What a job cost",
            "description": "The compute a job used, to spot waste: wall time, CPU seconds and \
                average cores, peak RSS, build-cache warmth (and why), crates compiled, test \
                targets built, `broad_test_build` (a cargo test with a name filter but no \
                --lib/--test/--bin/--doc selector builds every test target of the package) and \
                a `hint` saying how to make the next run cheaper. Live jobs report the values \
                so far. A job forwarded to a peer is reported from the peer's snapshot when \
                available (else from the mirrored log). Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": id("Job id (of this machine's ledger)."), "placement": placement_schema()},
                "required": ["id"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "alerts",
            "title": "Health alerts",
            "description": "Active health alerts of this machine and every peer (most severe \
                first): disk below the reserve or filling fast, a build slot held with no live \
                job (naming the holder process), sustained memory pressure (naming the largest \
                processes), stale/offline peers, long-waiting jobs, work queued here while a \
                peer is idle, a machine missing a capability (e.g. libclang, from a failed \
                job's log; placement anywhere skips it for commands that need it), and very \
                high per-job CPU. Each alert has a stable id (KIND:NODE[:SUBJECT]), severity, \
                node, summary, evidence, a suggested fix and first/last seen; alerts clear \
                when the condition ends. Also lists each machine's disks (free, growth, \
                time to full, top consumers) and missing-capability marks. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "severity": {"type": "string", "enum": ["info", "warn", "critical"], "default": "info",
                        "description": "Minimum severity."},
                    "node": {"type": "string",
                        "description": "Only alerts about this machine (peer name, e.g. \"worker\", or hostname)."},
                    "include_recent": {"type": "boolean", "default": false,
                        "description": "Also return recently cleared alerts."},
                    "placement": placement_schema(),
                },
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "submit_steps",
            "title": "Run a plan of dependent steps",
            "description": "Submit a whole plan at once: each step is a `submit` (same rules, \
                same admission, placement, logs, ETA and cancel) and a step with `after` is \
                approved now but not started until the steps it names have passed; if one of \
                them fails, the steps that wait for it are cancelled and independent steps \
                still run. The plan is stored in the daemon, so it survives this call and \
                your session: use job_cost / wait_job / job_logs per step id. Use one step \
                per test target (`cargo test --test NAME`) with a first step that builds \
                (`cargo test --no-run`), instead of one `bash -c \"cargo test ... | grep ...\"`: \
                a shell hides the compile progress and the per-target time. Returns the job id \
                of every step, in dependency order. If a step cannot be submitted the steps \
                already submitted are cancelled.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "who": {"type": "string", "minLength": 1, "maxLength": 128,
                        "description": "Your unique agent name; recorded as the owner of every step."},
                    "cwd": {"type": "string",
                        "description": "Absolute path of YOUR checkout; every step runs there."},
                    "steps": {"type": "array", "minItems": 1, "maxItems": 64,
                        "items": {"type": "object", "additionalProperties": false,
                            "required": ["name", "argv"],
                            "properties": {
                                "name": {"type": "string", "maxLength": 64,
                                    "description": "Unique in the plan: letters, digits, _ - ."},
                                "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 256},
                                "after": {"type": "array", "items": {"type": "string"},
                                    "description": "Names of steps that must pass first."},
                                "run": {"type": "boolean", "default": false,
                                    "description": "Run admission class (already-built command) instead of compile."},
                                "broad_test": {"type": "boolean", "default": false,
                                    "description": "As for submit."}
                            }},
                        "description": "The steps."},
                    "pin": {"type": "boolean", "default": false,
                        "description": "As for submit: keep every step on the machine chosen at submit."},
                    "placement": submit_placement_schema(),
                },
                "required": ["who", "cwd", "steps"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": true,
                            "idempotentHint": false, "openWorldHint": false}
        }),
    ]
}

/// A tool's successful outcome: text for the model, structured data for programs.
pub struct Outcome {
    pub text: String,
    pub structured: Value,
}

/// Runs one tool. Argument and daemon failures are tool errors (`isError`) so the
/// agent sees the message and can correct itself; the server keeps running.
pub fn call_tool(daemon: &Daemon, name: &str, args: &Map<String, Value>, ctx: &Ctx) -> Value {
    let outcome = Args::new(name, args).and_then(|args| {
        if name != "submit" {
            args.placement()?;
        }
        match name {
            "cluster_status" => cluster_status(daemon, &args),
            "alerts" => alerts(daemon, &args),
            "list_jobs" => list_jobs(daemon, &args),
            "job" => job(daemon, &args),
            "job_logs" => job_logs(daemon, &args),
            "submit" => submit(daemon, &args, ctx),
            "submit_steps" => submit_steps(daemon, &args, ctx),
            "cancel" => cancel(daemon, &args),
            "wait_job" => wait_job(daemon, &args, ctx),
            "who_is_compiling" => crate::mcp_compute::who_is_compiling(daemon, &args),
            "search_logs" => crate::mcp_compute::search_logs(daemon, &args),
            "job_cost" => crate::mcp_compute::job_cost(daemon, &args),
            _ => Err(format!("unknown tool: {name}").into()),
        }
    });
    match outcome {
        Ok(Outcome { text, structured }) => json!({
            "content": [{"type": "text", "text": text}],
            "structuredContent": structured,
            "isError": false,
        }),
        Err(error) => {
            let message = error.to_string();
            json!({
                "content": [{"type": "text", "text": format!("error: {message}")}],
                "structuredContent": {"error": message},
                "isError": true,
            })
        }
    }
}

/// Typed access to a tool's arguments; rejects unknown keys and wrong types.
pub struct Args<'a>(&'a Map<String, Value>);

impl<'a> Args<'a> {
    fn new(tool: &str, map: &'a Map<String, Value>) -> Result<Self> {
        let schema = tools()
            .into_iter()
            .find(|t| t["name"] == tool)
            .ok_or_else(|| format!("unknown tool: {tool}"))?;
        let allowed = schema["inputSchema"]["properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        if let Some(key) = map.keys().find(|k| !allowed.contains_key(*k)) {
            let names: Vec<_> = allowed.keys().map(String::as_str).collect();
            return Err(format!(
                "unknown argument {key:?} for {tool}; allowed: {}",
                names.join(", ")
            )
            .into());
        }
        for key in schema["inputSchema"]["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if map.get(key).is_none_or(Value::is_null) {
                return Err(format!("missing required argument {key:?} for {tool}").into());
            }
        }
        Ok(Self(map))
    }
    pub fn get(&self, key: &str) -> Option<&'a Value> {
        self.0.get(key).filter(|v| !v.is_null())
    }
    pub fn str(&self, key: &str) -> Result<Option<&'a str>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(format!("{key} must be a string").into()),
        }
    }
    pub fn bool(&self, key: &str, default: bool) -> Result<bool> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(format!("{key} must be true or false").into()),
        }
    }
    pub fn int(&self, key: &str, min: i64, max: i64, default: i64) -> Result<i64> {
        match self.get(key) {
            None => Ok(default),
            Some(v) => match v.as_i64() {
                Some(n) if (min..=max).contains(&n) => Ok(n),
                _ => Err(format!("{key} must be an integer in {min}..={max}").into()),
            },
        }
    }
    pub fn id(&self) -> Result<i64> {
        self.int("id", 1, i64::MAX, 0)
    }
    fn placement(&self) -> Result<()> {
        match self.str("placement")? {
            None | Some("local") => Ok(()),
            Some(other) => Err(format!(
                "placement {other:?} is not available for this tool; only \"local\" (this \
                 machine's daemon, which also knows the jobs it forwarded to peers). Placement \
                 on another machine is chosen at submit"
            )
            .into()),
        }
    }
}

pub fn phase_name(phase: &Phase) -> String {
    phase.as_db().unwrap_or_else(|_| format!("{phase:?}"))
}

fn recipe(job: &Job) -> Option<Recipe> {
    (job.spec.suite == tinline_task_manager::local_command::SUITE)
        .then(|| serde_json::from_str(&job.spec.configuration).ok())
        .flatten()
}

/// Exit code of a finished job (`Phase::reported_exit_code`): 0 only when passed.
pub fn exit_code(job: &Job) -> Option<i32> {
    job.phase.reported_exit_code(
        job.execution
            .as_ref()
            .map_or(&[][..], |e| e.stages.as_slice()),
    )
}

/// The job as agents need it; the full ledger row stays available via `job`.
pub fn job_summary(job: &Job) -> Value {
    let execution = job.execution.as_ref();
    json!({
        "id": job.id,
        "owner": job.spec.owner,
        "phase": phase_name(&job.phase),
        "finished": job.phase.terminal(),
        "exit_code": exit_code(job),
        "resource": display::resource_label(job),
        "cwd": job.spec.source,
        "argv": recipe(job).map(|r| r.argv),
        "suite": job.spec.suite,
        "submitted_at": job.submitted_at,
        "started_at": execution.and_then(|e| e.started_at),
        "finished_at": execution.and_then(|e| e.finished_at),
        "stages": execution.map(|e| &e.stages),
        // Only `passed` is success; an interrupted job is never one, whatever its stages say.
        "passed": job.phase == Phase::Passed,
        "remote_result": execution.and_then(|e| e.remote_result.as_ref()),
    })
}

fn now() -> i64 {
    tinline_task_manager::execution::now()
}

fn job_line(job: &Job) -> String {
    let (age, wait, run) = display::timing(job, now());
    let exit = match exit_code(job) {
        Some(code) => format!(" (exit {code})"),
        None if job.phase.terminal() => format!(" (exit {})", display::exit_label(job)),
        None => String::new(),
    };
    format!(
        "job {} {}{exit} owner={} class={} cwd={}\n  age {age}, admission wait {wait}, run {run}\n  argv: {}{}",
        job.id,
        phase_name(&job.phase),
        job.spec.owner,
        display::resource_label(job),
        job.spec.source,
        display::command_summary(job),
        display::remote_note(job).replace("\ntasks: ", "\n  "),
    )
}

/// Keeps every live job and the newest `recent` finished ones.
fn keep_recent(snapshot: &mut Snapshot, recent: usize) {
    let finished: Vec<i64> = snapshot
        .jobs
        .iter()
        .filter(|j| j.phase.terminal())
        .map(|j| j.id)
        .collect();
    let keep_from = finished.len().saturating_sub(recent);
    let kept: std::collections::HashSet<i64> = finished[keep_from..].iter().copied().collect();
    snapshot
        .jobs
        .retain(|j| !j.phase.terminal() || kept.contains(&j.id));
}

/// A snapshot as JSON without the sparkline rings, which only a chart needs.
fn trimmed(snapshot: &Snapshot) -> Result<Value> {
    let mut value = serde_json::to_value(snapshot)?;
    if let Some(machine) = value.get_mut("machine").and_then(Value::as_object_mut) {
        machine.remove("history");
    }
    for job in value["jobs"].as_array_mut().into_iter().flatten() {
        if let Some(live) = job.get_mut("live").and_then(Value::as_object_mut) {
            live.remove("history");
        }
    }
    Ok(value)
}

/// Every machine: this one, then each worker/both peer's latest snapshot from the daemon's
/// peer runtime, with its age and an online/stale flag.
fn machines(
    daemon: &Daemon,
    local: &Snapshot,
    recent: usize,
    text: &mut String,
) -> Result<Vec<Value>> {
    let mut out = vec![json!({
        "name": local.machine.id, "local": true, "role": local.machine.role,
        "state": "online", "online": true, "stale": false, "age_s": 0.0,
        "machine_id": local.machine.id, "cargo_jobs_max": local.machine.cargo_jobs_max,
        "snapshot": "top level (machine, jobs)",
    })];
    if !placement::peer_config_present() {
        return Ok(out);
    }
    let report = match tinline_task_manager::peer_link::request_peers(
        &daemon.socket,
        Duration::from_secs(30),
    ) {
        Ok(r) => r,
        Err(e) => {
            text.push_str(&format!("\npeer machines unavailable: {e}\n"));
            return Ok(out);
        }
    };
    if !report.enabled {
        text.push_str(&format!(
            "\npeer link not enabled in the daemon: {}\n",
            report.error.as_deref().unwrap_or("still starting")
        ));
        return Ok(out);
    }
    for peer in &report.snapshots {
        let age = peer
            .age_s
            .map(|a| format!("{a:.1}s old"))
            .unwrap_or_else(|| "no snapshot".into());
        let flag = match (peer.online(), peer.stale) {
            (true, false) => "online",
            (true, true) => "online, STALE",
            (false, _) => "OFFLINE",
        };
        text.push_str(&format!(
            "\n=== peer {} ({}): {} [{flag}], snapshot {age}{}\n",
            peer.name,
            peer.role,
            peer.state,
            peer.error
                .as_deref()
                .map(|e| format!("; last error: {e}"))
                .unwrap_or_default()
        ));
        let parsed = peer
            .snapshot
            .as_ref()
            .map(|v| serde_json::from_value::<Snapshot>(v.clone()));
        let mut entry = json!({
            "name": peer.name, "local": false, "role": peer.role, "state": peer.state,
            "online": peer.online(), "stale": peer.stale, "age_s": peer.age_s,
            "received_at": peer.received_at, "error": peer.error,
            "machine_id": null, "cargo_jobs_max": null, "snapshot": null,
        });
        match parsed {
            Some(Ok(mut snap)) => {
                text.push_str(&crate::status::render(&snap));
                keep_recent(&mut snap, recent);
                entry["machine_id"] = json!(snap.machine.id);
                entry["cargo_jobs_max"] = json!(snap.machine.cargo_jobs_max);
                entry["snapshot"] = trimmed(&snap)?;
            }
            Some(Err(e)) => {
                text.push_str(&format!("(unreadable snapshot: {e})\n"));
                entry["error"] = json!(format!("unreadable snapshot: {e}"));
            }
            None => {}
        }
        out.push(entry);
    }
    Ok(out)
}

fn cluster_status(daemon: &Daemon, args: &Args) -> Result<Outcome> {
    let recent = args.int("recent_finished", 0, 50, 5)? as usize;
    let admission = args.bool("include_admission", false)?;
    let mut snapshot = daemon.status()?;
    let mut text = crate::status::render(&snapshot);
    keep_recent(&mut snapshot, recent);
    let mut structured = trimmed(&snapshot)?;
    // The snapshot's `blocker` is the compile gate's only; run and offload
    // slots have their own gates (read from the local admission report, not
    // the daemon), so an agent can tell whether run-class work would start.
    match tinline_task_manager::admission::status() {
        Ok(report) => {
            let none = |b: &Option<String>| b.clone().unwrap_or_else(|| "none".into());
            text.push_str(&format!(
                "run-slot blocker: {}; offload-slot blocker: {}\n",
                none(&report.run_pressure_blocker),
                none(&report.offload_pressure_blocker)
            ));
            structured["blockers"] = json!({
                "compile": report.pressure_blocker,
                "run": report.run_pressure_blocker,
                "offload": report.offload_pressure_blocker,
            });
            if admission {
                for note in &report.waiting {
                    text.push_str(&format!(
                        "admission waiter: {} for {} (pid {})\n",
                        note.who,
                        note.slot.as_deref().unwrap_or("-"),
                        note.pid
                    ));
                }
                structured["admission"] = serde_json::to_value(&report)?;
            }
        }
        Err(error) => {
            text.push_str(&format!("admission report unavailable: {error}\n"));
            if admission {
                structured["admission"] = json!({"error": error.to_string()});
            }
        }
    }
    structured["machines"] = json!(machines(daemon, &snapshot, recent, &mut text)?);
    // Active warn/critical health alerts of every machine (the `alerts` tool has all).
    let health = cluster::gather(&daemon.socket).filtered(&cluster::Filter {
        min_severity: Some(Severity::Warn),
        ..Default::default()
    });
    if !health.alerts.is_empty() {
        text.push_str(&format!(
            "\n{} active warn/critical alert(s):\n",
            health.alerts.len()
        ));
        for a in &health.alerts {
            text.push_str(&format!("{}\n", alert_line(a, health.generated_at)));
        }
    }
    structured["alerts"] = serde_json::to_value(&health.alerts)?;
    Ok(Outcome { text, structured })
}

fn alert_line(a: &cluster::ClusterAlert, now: f64) -> String {
    let mut line = a.alert.line(now);
    if a.reported_by != "local" {
        line.push_str(&format!(" [via {}]", a.reported_by));
    }
    if !a.alert.fix.is_empty() {
        line.push_str(&format!(" -- fix: {}", a.alert.fix));
    }
    line
}

fn alerts(daemon: &Daemon, args: &Args) -> Result<Outcome> {
    let filter = cluster::Filter {
        min_severity: args.str("severity")?.map(Severity::parse).transpose()?,
        node: args.str("node")?.map(str::to_owned),
        recent: args.bool("include_recent", false)?,
    };
    daemon.ensure()?;
    let view = cluster::gather(&daemon.socket).filtered(&filter);
    Ok(Outcome {
        text: view.render(view.generated_at),
        structured: serde_json::to_value(&view)?,
    })
}

/// Active warn/critical alerts a submission should know: about the machine it went to
/// (or this one), plus missing capabilities anywhere. Without peers (no decision) this reads
/// only this daemon's persisted alert book: no extra daemon request.
fn submit_alerts(daemon: &Daemon, decision: Option<&placement::Decision>) -> Vec<String> {
    let Some(_) = decision else {
        let path = daemon
            .state()
            .join(tinline_task_manager::health::monitor::ALERTS_FILE);
        let book: tinline_task_manager::health::AlertBook =
            tinline_task_manager::peer::records::read_json(&path).unwrap_or_default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        return book
            .sorted_active()
            .into_iter()
            .filter(|a| a.severity >= Severity::Warn && a.kind != "job_waiting")
            .map(|a| {
                let mut line = a.line(now);
                if !a.fix.is_empty() {
                    line.push_str(&format!(" -- fix: {}", a.fix));
                }
                line
            })
            .collect();
    };
    let health = cluster::gather(&daemon.socket);
    let mut machines: Vec<String> = vec!["local".into()];
    if let Some(id) = health.nodes.first().and_then(|n| n.machine_id.clone()) {
        machines.push(id);
    }
    if let Some(d) = decision {
        machines.push(d.machine.clone());
        machines.extend(d.machine_id.clone());
    }
    let names: Vec<&str> = machines.iter().map(String::as_str).collect();
    health
        .relevant(&names)
        .into_iter()
        .map(|a| alert_line(a, health.generated_at))
        .collect()
}

fn list_jobs(daemon: &Daemon, args: &Args) -> Result<Outcome> {
    let owner = args.str("owner")?;
    let active = args.bool("active_only", false)?;
    let limit = args.int("limit", 1, 500, 20)? as usize;
    let phases: Option<Vec<Phase>> = match args.get("phases") {
        None => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|p| {
                    serde_json::from_value::<Phase>(p.clone()).map_err(|_| {
                        format!("unknown phase {p}; one of: {}", PHASES.join(", ")).into()
                    })
                })
                .collect::<Result<_>>()?,
        ),
        Some(_) => return Err("phases must be an array of phase names".into()),
    };
    let response = daemon.ok(Action::List)?;
    let mut jobs = display::select(response.jobs, owner, active, None);
    if let Some(phases) = &phases {
        jobs.retain(|j| phases.contains(&j.phase));
    }
    let matched = jobs.len();
    jobs.drain(..matched.saturating_sub(limit));
    let home = std::env::var("HOME").ok();
    let mut text = if jobs.is_empty() {
        "no matching jobs\n".to_owned()
    } else {
        display::job_table(&jobs, now(), home.as_deref())
    };
    text.push_str(&format!(
        "{} of {matched} matching jobs shown (newest last)\n",
        jobs.len()
    ));
    Ok(Outcome {
        text,
        structured: json!({
            "jobs": jobs.iter().map(job_summary).collect::<Vec<_>>(),
            "matched": matched,
            "shown": jobs.len(),
        }),
    })
}

fn job(daemon: &Daemon, args: &Args) -> Result<Outcome> {
    let job = daemon.job(args.id()?)?;
    let mut structured = job_summary(&job);
    structured["execution"] = serde_json::to_value(&job.execution)?;
    let mut text = job_line(&job);
    if let Some(e) = &job.execution {
        text.push_str(&format!("\n  detail: {}", e.detail));
    }
    add_reroute(&job, &mut text, &mut structured);
    let mut outcome = Outcome { text, structured };
    add_placement(daemon, &job, &mut outcome);
    Ok(outcome)
}

/// The last `lines` lines of the daemon's retained log tail for `id`.
fn log_tail(daemon: &Daemon, id: i64, lines: usize) -> Result<(String, usize, bool)> {
    let response = daemon.ok(Action::Logs(id))?;
    let log = response.log.ok_or("daemon returned no log")?;
    let (shown, omitted) = crate::follow::tail(&log.text, lines);
    Ok((shown.to_owned(), omitted, log.truncated))
}

fn tail_header(id: i64, shown: &str, omitted: usize, truncated: bool) -> String {
    let count = shown.lines().count();
    let mut header = format!("--- job {id} log: last {count} lines");
    if omitted > 0 || truncated {
        header.push_str(" (earlier lines omitted; job_logs with a larger tail_lines shows more)");
    }
    header.push_str(" ---\n");
    header
}

fn job_logs(daemon: &Daemon, args: &Args) -> Result<Outcome> {
    let id = args.id()?;
    let lines = args.int("tail_lines", 1, MAX_TAIL, 100)? as usize;
    let (shown, omitted, truncated) = log_tail(daemon, id, lines)?;
    Ok(Outcome {
        text: format!("{}{shown}", tail_header(id, &shown, omitted, truncated)),
        structured: json!({
            "id": id,
            "text": shown,
            "lines": shown.lines().count(),
            "omitted_lines": omitted,
            "daemon_truncated": truncated,
        }),
    })
}

/// Polls `Job(id)` until it is terminal, the budget is spent, the server shuts
/// down (stdin closed) or the client cancels the request. Returns the job and
/// whether it stopped before the job finished.
fn wait_for(daemon: &Daemon, id: i64, budget: Duration, ctx: &Ctx) -> Result<(Job, bool)> {
    let began = Instant::now();
    let deadline = began + budget;
    let mut previous: Option<Phase> = None;
    let mut last_progress = began;
    loop {
        let job = daemon.job(id)?;
        let changed = previous.as_ref() != Some(&job.phase);
        if changed || last_progress.elapsed() >= PROGRESS_EVERY {
            ctx.progress(format!(
                "job {id} {} after {}s",
                phase_name(&job.phase),
                began.elapsed().as_secs()
            ));
            last_progress = Instant::now();
            previous = Some(job.phase.clone());
        }
        if job.phase.terminal() {
            return Ok((job, false));
        }
        let now = Instant::now();
        if now >= deadline || ctx.shutting_down() {
            return Ok((job, true));
        }
        if ctx.cancelled() {
            return Err(format!("wait cancelled by the client; job {id} keeps running").into());
        }
        std::thread::sleep(POLL.min(deadline - now));
    }
}

/// Text and structured result of a wait, with a log tail once finished.
fn waited(daemon: &Daemon, job: &Job, timed_out: bool, tail: usize) -> Outcome {
    let id = job.id;
    let mut text = job_line(job);
    let mut structured = job_summary(job);
    structured["timed_out"] = json!(timed_out);
    add_reroute(job, &mut text, &mut structured);
    if let Some(p) = placement_of(&daemon.state(), job) {
        text.push('\n');
        text.push_str(&placement_text(&p));
        structured["machine"] = p["machine"].clone();
        if p["forwarded"] == true {
            structured["remote_job_id"] = p["remote_job_id"].clone();
        }
        structured["placement"] = p;
    }
    if timed_out {
        text.push_str(&format!(
            "\nstill {} when the wait ended (budget spent or server shutting down); the job keeps running. Call wait_job {{\"id\": {id}}} again; do not resubmit.",
            phase_name(&job.phase)
        ));
    } else if tail > 0 {
        match log_tail(daemon, id, tail) {
            Ok((shown, omitted, truncated)) => {
                text.push('\n');
                text.push_str(&tail_header(id, &shown, omitted, truncated));
                text.push_str(&shown);
                structured["log_tail"] = json!(shown);
            }
            Err(error) => {
                text.push_str(&format!("\nlog unavailable: {error}"));
                structured["log_error"] = json!(error.to_string());
            }
        }
    }
    Outcome { text, structured }
}

fn wait_job(daemon: &Daemon, args: &Args, ctx: &Ctx) -> Result<Outcome> {
    let id = args.id()?;
    let timeout = args.int("timeout_s", 0, MAX_WAIT_S, DEFAULT_WAIT_S)?;
    let tail = args.int("tail_lines", 0, MAX_TAIL, 60)? as usize;
    let (job, timed_out) = wait_for(daemon, id, Duration::from_secs(timeout as u64), ctx)?;
    Ok(waited(daemon, &job, timed_out, tail))
}

/// The submission `tasks run` would make from `cwd`: same owner bound, same
/// device lock name and the same `Recipe::spec` checks (argv bounds, Cargo `-j`
/// cap, no nested admission helpers, offload client rule, device denylist).
#[cfg(test)]
pub fn run_spec(
    who: &str,
    cwd: &str,
    argv: Vec<String>,
    class: &str,
    serial: Option<&str>,
) -> Result<tinline_task_manager::JobSpec> {
    let (cwd, resource) = submission(who, cwd, class, serial)?;
    Recipe {
        broad_test: false,
        argv,
        resource,
    }
    .spec(who.to_owned(), cwd)
}

/// The resolved checkout and admission class of a submission (the `tasks run` checks).
fn submission(
    who: &str,
    cwd: &str,
    class: &str,
    serial: Option<&str>,
) -> Result<(PathBuf, Resource)> {
    if who.trim().is_empty() || who.len() > 128 {
        return Err("bounded owner required".into());
    }
    let path = std::path::Path::new(cwd);
    if !path.is_absolute() {
        return Err(format!("cwd must be an absolute path to your checkout, got {cwd:?}").into());
    }
    // `tasks run` records its getcwd(), which is the resolved path.
    let cwd = std::fs::canonicalize(path).map_err(|e| format!("cwd {cwd}: {e}"))?;
    if !cwd.is_dir() {
        return Err(format!("cwd {} is not a directory", cwd.display()).into());
    }
    let resource = match (class, serial) {
        ("compile", None) => Resource::Compile,
        ("run", None) => Resource::Run,
        ("device", Some(serial)) => {
            Resource::Device(tinline_task_manager::device::lock_serial(serial)?)
        }
        ("device", None) => return Err("class device requires device_serial".into()),
        (_, Some(_)) => return Err("device_serial is only valid with class device".into()),
        (other, None) => {
            return Err(format!("class must be compile, run or device, got {other:?}").into());
        }
    };
    Ok((cwd, resource))
}

/// How long placement waits for a just-started daemon's peer runtime to connect.
const PLACEMENT_SETTLE: Duration = Duration::from_secs(15);
/// How long a non-waiting submit waits for a forwarded job's remote id.
const REMOTE_ID_WAIT: Duration = Duration::from_secs(10);

/// The link record of a forwarded local job (`<state>/peer/links/<id>.json`).
fn forward_link(state: &std::path::Path, id: i64) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(state.join(format!("peer/links/{id}.json"))).ok()?).ok()
}

/// Where a job runs and why: the peer and remote job of a forwarded job (from its argv and
/// link record) and the recorded placement decision, if one was made. None for a plain local
/// job that had no placement decision.
pub fn placement_of(state: &std::path::Path, job: &Job) -> Option<Value> {
    let recorded = placement::recorded(state, job.id);
    let peer = recipe(job).and_then(|r| {
        tinline_task_manager::peer_link::ForwardArgs::from_recipe_argv(&r.argv)?
            .ok()
            .map(|a| a.peer)
    });
    if recorded.is_none() && peer.is_none() {
        return None;
    }
    let mut out = json!({
        "machine": peer.clone().or_else(|| recorded.as_ref()
            .and_then(|r| r["machine"].as_str().map(str::to_owned))),
        "forwarded": peer.is_some(),
        "peer": peer,
    });
    if let Some(r) = &recorded {
        out["requested"] = r["requested"].clone();
        out["reason"] = r["reason"].clone();
        out["machine_id"] = r["machine_id"].clone();
        out["cargo_jobs"] = r["cargo_jobs"].clone();
        out["notes"] = r["notes"].clone();
    }
    if out["forwarded"] == true {
        let link = forward_link(state, job.id);
        let field = |k: &str| link.as_ref().map_or(Value::Null, |l| l[k].clone());
        out["remote_job_id"] = field("remote_job_id");
        out["remote_phase"] = field("remote_phase");
        out["remote_state"] = field("remote_state");
        out["link_stage"] = field("stage");
    }
    Some(out)
}

fn placement_text(p: &Value) -> String {
    let machine = p["machine"].as_str().unwrap_or("?");
    let mut text = format!("placement: {machine}");
    if p["forwarded"] == true {
        match p["remote_job_id"].as_i64() {
            Some(id) => text.push_str(&format!(" (forwarded; remote job {id} on {machine}")),
            None => text.push_str(" (forwarded; remote job id not known yet"),
        }
        if let Some(phase) = p["remote_phase"].as_str() {
            text.push_str(&format!(", {phase}"));
        }
        text.push(')');
    }
    if let Some(reason) = p["reason"].as_str() {
        text.push_str(&format!(
            " — {}: {reason}",
            p["requested"].as_str().unwrap_or("?")
        ));
    }
    for note in p["notes"].as_array().into_iter().flatten() {
        if let Some(n) = note.as_str() {
            text.push_str(&format!("\n  note: {n}"));
        }
    }
    text
}

/// The move of a waiting job to a peer (reroute), if it was moved or tried.
pub fn reroute_of(job: &Job) -> Option<Value> {
    let r = job.execution.as_ref()?.reroute.as_ref()?;
    Some(json!({
        "state": r.state,
        "moved_from": r.moved_from,
        "moved_to": r.moved_to,
        "reason": r.reason,
        "at": r.at,
        "local_s": r.local_s,
        "peer_s": r.peer_s,
        "remote_job_id": r.remote_job_id,
        "note": r.note,
        "returned_at": r.returned_at,
        "summary": r.summary(),
    }))
}

/// Adds the reroute record of `job` (if any) to a tool result.
fn add_reroute(job: &Job, text: &mut String, structured: &mut Value) {
    if let Some(r) = reroute_of(job) {
        text.push_str(&format!("\n{}", r["summary"].as_str().unwrap_or_default()));
        structured["reroute"] = r;
    }
}

/// Adds the placement of `job` (if any) to a tool result.
fn add_placement(daemon: &Daemon, job: &Job, outcome: &mut Outcome) {
    if let Some(p) = placement_of(&daemon.state(), job) {
        outcome.text.push('\n');
        outcome.text.push_str(&placement_text(&p));
        outcome.structured["machine"] = p["machine"].clone();
        if p["forwarded"] == true {
            outcome.structured["remote_job_id"] = p["remote_job_id"].clone();
        }
        outcome.structured["placement"] = p;
    }
}

fn submit(daemon: &Daemon, args: &Args, ctx: &Ctx) -> Result<Outcome> {
    let who = args.str("who")?.unwrap_or_default();
    let cwd = args.str("cwd")?.unwrap_or_default();
    let argv: Vec<String> = match args.get("argv") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "argv must be an array of strings".into())
            })
            .collect::<Result<_>>()?,
        _ => return Err("argv must be an array of strings".into()),
    };
    let class = args.str("class")?.unwrap_or("compile");
    let serial = args.str("device_serial")?;
    let wait = args.bool("wait", false)?;
    let timeout = args.int("timeout_s", 1, MAX_WAIT_S, DEFAULT_WAIT_S)?;
    let tail = args.int("tail_lines", 0, MAX_TAIL, 60)? as usize;
    let fetch: Vec<String> = match args.get("fetch") {
        None => vec![],
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "fetch must be an array of relative paths".into())
            })
            .collect::<Result<_>>()?,
        Some(_) => return Err("fetch must be an array of relative paths".into()),
    };
    let after: Vec<i64> = match args.get("after") {
        None => vec![],
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_i64()
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| "after must be an array of job ids".into())
            })
            .collect::<Result<_>>()?,
        Some(_) => return Err("after must be an array of job ids".into()),
    };
    let request = match args.str("placement")? {
        None => placement::Request::Anywhere,
        Some(text) => placement::Request::parse(text)?,
    };
    let broad_test = args.bool("broad_test", false)?;
    // Submission policy (crate::policy): a refusal is a tool error and nothing is queued.
    let mut policy_warnings = vec![];
    {
        let (resolved, resource) = submission(who, cwd, class, serial)?;
        let recipe = Recipe {
            broad_test,
            argv: argv.clone(),
            resource,
        };
        match recipe.policy_verdict(&resolved, &tinline_task_manager::policy::effective()) {
            tinline_task_manager::policy::Verdict::Refused(message) => {
                return Err(format!("{message} Nothing was submitted.").into());
            }
            tinline_task_manager::policy::Verdict::Warn(lines) => policy_warnings = lines,
            tinline_task_manager::policy::Verdict::Allowed => {}
        }
    }
    let pin_why = if args.bool("pin", false)? {
        Some("MCP pin: true")
    } else if request == placement::Request::Local {
        Some("MCP placement \"local\"")
    } else {
        None
    };
    // `local` (and anywhere without peers) is exactly the previous submission.
    let (spec, decision) = if request == placement::Request::Local {
        if !fetch.is_empty() {
            return Err("fetch only applies when the job runs on a peer".into());
        }
        let (cwd, resource) = submission(who, cwd, class, serial)?;
        let recipe = Recipe {
            broad_test,
            argv,
            resource,
        };
        (recipe.spec(who.to_owned(), cwd)?, None)
    } else {
        let (cwd, resource) = submission(who, cwd, class, serial)?;
        daemon.ensure()?;
        let mut inputs = placement::gather(&daemon.socket, PLACEMENT_SETTLE)
            .map_err(|e| daemon.unreachable(e))?;
        placement::retain_needed(&mut inputs.peers, &argv, Some(&cwd));
        let decision =
            placement::decide_with(&argv, &crate::place_class(&resource), &request, &inputs)?;
        eprintln!("tasks mcp: {}", decision.line());
        let spec = placement::job_spec(
            &decision,
            resource,
            broad_test,
            who.to_owned(),
            cwd,
            &fetch,
            &daemon.state(),
            &std::env::current_exe()?,
        )?;
        // Without peers nothing was decided: keep the old, quiet result.
        let decided = decision.decided();
        (spec, decided.then_some(decision))
    };
    let response = daemon.ok(Action::Submit(spec))?;
    let id = response
        .submitted_id
        .ok_or("daemon did not return submitted_id; do not resubmit blindly: check list_jobs")?;
    eprintln!("tasks mcp: submitted job {id} for {who}");
    if let Some(decision) = &decision
        && let Err(e) = placement::record(&daemon.state(), id, decision)
    {
        eprintln!("tasks mcp: could not record the placement of job {id}: {e}");
    }
    if !after.is_empty()
        && let Err(e) = daemon.ok(Action::After(id, after.clone()))
    {
        return Err(format!(
            "job {id} was submitted but could not be made to wait for {after:?} ({e}); it is NOT approved. Cancel it with cancel {{\"id\": {id}, \"who\": \"{who}\"}}; do not resubmit blindly"
        )
        .into());
    }
    // Before approval: reroute never sees the job unpinned.
    if let Some(why) = pin_why
        && let Err(e) = tinline_task_manager::reroute::pin(&daemon.state(), id, why)
    {
        return Err(format!(
            "job {id} was submitted but could not be pinned ({e}); it is NOT approved. Cancel it with cancel {{\"id\": {id}, \"who\": \"{who}\"}}; do not resubmit blindly"
        )
        .into());
    }
    let warning = display::shared_cwd_warning(&response.jobs, id)
        .map(|w| w.trim_start_matches("tasks: ").to_owned());
    // Same approval sequence as `tasks run`: this call is the explicit approval.
    for action in [Action::ApproveCommand(id), Action::StartWorker] {
        if let Err(error) = daemon.ok(action) {
            return Err(format!(
                "job {id} was submitted but approving/starting it failed: {error}. Inspect it with job {{\"id\": {id}}} before retrying; do not resubmit"
            )
            .into());
        }
    }
    let forwarded = decision.as_ref().is_some_and(|d| d.peer().is_some());
    let mut outcome = if wait {
        let (job, timed_out) = wait_for(daemon, id, Duration::from_secs(timeout as u64), ctx)
            .map_err(|e| format!("{e}; {}", tinline_task_manager::live_job_hint(id)))?;
        waited(daemon, &job, timed_out, tail)
    } else {
        if forwarded {
            // The bridge needs a local offload slot and a sync first; wait briefly for the
            // remote id so the agent can see it, never for the job.
            let end = Instant::now() + REMOTE_ID_WAIT;
            while Instant::now() < end
                && !ctx.cancelled()
                && forward_link(&daemon.state(), id)
                    .is_none_or(|l| l["remote_job_id"].is_null() && l["stage"] != "done")
            {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        let job = response.jobs.iter().find(|j| j.id == id);
        let cwd = job.map_or("?", |j| j.spec.source.as_str());
        Outcome {
            text: format!(
                "submitted job {id} (owner {who}) in {cwd}; approved and handed to the worker. \
                 Next: wait_job {{\"id\": {id}}} or job_logs {{\"id\": {id}}}. Do not resubmit \
                 while it is live; cancel {{\"id\": {id}, \"who\": \"{who}\"}} cancels it."
            ),
            structured: match job {
                Some(job) => job_summary(job),
                None => json!({"id": id, "owner": who}),
            },
        }
    };
    outcome.structured["job_id"] = json!(id);
    if !wait && decision.is_some() {
        match daemon.job(id) {
            Ok(job) => add_placement(daemon, &job, &mut outcome),
            Err(_) => {
                if let Some(d) = &decision {
                    outcome.structured["machine"] = json!(d.machine);
                }
            }
        }
    }
    if let Some(d) = &decision {
        // The full decision (scores, skipped machines) as made at submit.
        outcome.text = format!("{}\n{}", d.line(), outcome.text);
        outcome.structured["decision"] = serde_json::to_value(d)?;
    }
    if let Some(warning) = warning {
        outcome.text.push_str(&format!("\nwarning: {warning}"));
        outcome.structured["warning"] = json!(warning);
    }
    for line in &policy_warnings {
        outcome.text.push_str(&format!("\nwarning: {line}"));
    }
    if !policy_warnings.is_empty() {
        outcome.structured["policy_warnings"] = json!(policy_warnings);
    }
    if broad_test {
        outcome.structured["broad_test"] = json!(true);
    }
    let alerts = submit_alerts(daemon, decision.as_ref());
    if !alerts.is_empty() {
        for a in &alerts {
            outcome.text.push_str(&format!("\nalert: {a}"));
        }
        outcome.structured["alerts"] = json!(alerts);
    }
    Ok(outcome)
}

/// `submit_steps`: every step of a validated plan becomes one `submit`, in dependency order, with
/// `after` set to the job ids of the steps it names.
fn submit_steps(daemon: &Daemon, args: &Args, ctx: &Ctx) -> Result<Outcome> {
    let who = args.str("who")?.unwrap_or_default();
    let cwd = args.str("cwd")?.unwrap_or_default();
    let steps = args.get("steps").cloned().unwrap_or(Value::Null);
    let plan = tinline_task_manager::steps::Plan::parse(&json!({ "steps": steps }).to_string())?;
    let mut ids: Vec<Option<i64>> = vec![None; plan.steps.len()];
    let fail = |daemon: &Daemon, ids: &[Option<i64>], why: String| -> Result<Outcome> {
        let started: Vec<i64> = ids.iter().flatten().copied().collect();
        for id in &started {
            let _ = daemon.ok(Action::Cancel(*id));
        }
        Err(format!(
            "{why}; cancelled the {} step(s) already submitted ({started:?}); nothing from this plan is left running",
            started.len()
        )
        .into())
    };
    for i in plan.order() {
        let step = &plan.steps[i];
        let after: Vec<i64> = step
            .after
            .iter()
            .filter_map(|name| plan.steps.iter().position(|s| &s.name == name))
            .filter_map(|j| ids[j])
            .collect();
        let mut map = Map::new();
        map.insert("who".into(), json!(who));
        map.insert("cwd".into(), json!(cwd));
        map.insert("argv".into(), json!(step.argv));
        map.insert(
            "class".into(),
            json!(if step.run { "run" } else { "compile" }),
        );
        map.insert("broad_test".into(), json!(step.broad_test));
        for key in ["placement", "pin"] {
            if let Some(v) = args.get(key) {
                map.insert(key.into(), v.clone());
            }
        }
        if !after.is_empty() {
            map.insert("after".into(), json!(after));
        }
        let one = Args::new("submit", &map)?;
        match submit(daemon, &one, ctx) {
            Ok(outcome) => ids[i] = outcome.structured["job_id"].as_i64(),
            Err(e) => {
                return fail(
                    daemon,
                    &ids,
                    format!("step {:?} was not submitted: {e}", step.name),
                );
            }
        }
        if ids[i].is_none() {
            return fail(
                daemon,
                &ids,
                format!("step {:?} returned no job id", step.name),
            );
        }
    }
    let rows: Vec<Value> = plan
        .steps
        .iter()
        .zip(&ids)
        .map(|(s, id)| json!({"name": s.name, "job_id": id, "after": s.after}))
        .collect();
    let mut text = format!("submitted {} step(s) for {who}:\n", rows.len());
    for (s, id) in plan.steps.iter().zip(&ids) {
        let waits = if s.after.is_empty() {
            "starts now".to_string()
        } else {
            format!("after {}", s.after.join(", "))
        };
        text.push_str(&format!(
            "  {:<14} job {:<6} {waits}\n",
            s.name,
            id.unwrap_or(0)
        ));
    }
    text.push_str("Next: wait_job / job_logs / job_cost per job id; cancel {\"id\", \"who\"} cancels a step and the steps that wait for it.");
    Ok(Outcome {
        text,
        structured: json!({"steps": rows}),
    })
}

fn cancel(daemon: &Daemon, args: &Args) -> Result<Outcome> {
    let id = args.id()?;
    let who = args.str("who")?.unwrap_or_default();
    let job = daemon.job(id)?;
    if job.spec.owner != who {
        return Err(format!(
            "refused: job {id} belongs to {:?}, not {who:?}; agents may cancel only their own jobs",
            job.spec.owner
        )
        .into());
    }
    if job.phase.terminal() {
        return Ok(Outcome {
            text: format!(
                "job {id} already finished ({}); nothing to cancel",
                phase_name(&job.phase)
            ),
            structured: job_summary(&job),
        });
    }
    daemon.ok(Action::Cancel(id))?;
    eprintln!("tasks mcp: cancel requested for job {id} by {who}");
    let mut structured = match daemon.job(id) {
        Ok(job) => job_summary(&job),
        Err(_) => json!({"id": id}),
    };
    structured["cancel_requested"] = json!(true);
    Ok(Outcome {
        text: format!(
            "cancel requested for job {id}; the worker stops its process group. \
             wait_job {{\"id\": {id}}} shows when it is cancelled."
        ),
        structured,
    })
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
