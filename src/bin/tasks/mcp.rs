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
//! Placement: every tool goes through [`Daemon`], today always the local
//! socket. The `placement` argument accepts only `"local"`; a later
//! cross-machine target plugs in there without changing the tool contracts.
use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tinline_task_manager::local_command::{Recipe, Resource};
use tinline_task_manager::{Action, Job, Phase, Response, Result, metrics::Snapshot};

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

const INSTRUCTIONS: &str = "Shared build/test queue of this machine (the `tasks` daemon). \
Use `submit` instead of running cargo, gradle or heavy tests directly. Always pass your own \
unique `who` and the absolute path of your OWN checkout as `cwd`. Keep Cargo at `-j 2` or \
less (the daemon limit). Check `cluster_status` to see why work is queued; queuing under \
pressure is intended, never work around it. Never resubmit a job that is still live: use \
`wait_job` / `job_logs` with its id. `cancel` only your own jobs. Never use the device \
ZD22257P6G.";

/// Where a tool's daemon requests go. Today always the local daemon's socket.
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
    fn ok(&self, action: Action) -> Result<Response> {
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

pub const TOOL_NAMES: [&str; 7] = [
    "cluster_status",
    "list_jobs",
    "job",
    "job_logs",
    "submit",
    "cancel",
    "wait_job",
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
        "description": "Which machine's daemon handles this. Only \"local\" exists today; \
                        cross-machine placement will be added here later."
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
            "description": "Live status of this machine's shared build/test queue (same data as \
                `tasks status --json`): CPU, memory, swap and pressure (PSI), the admission \
                pressure blockers holding work back per slot class (compile, run, offload), the compile/run/offload/device \
                slots with the job and agent holding each, and running and queued jobs with \
                owner, queue position, what they wait for, ETA, live CPU/RSS and build-cache \
                warmth. Call it before heavy work to see whether it will queue, and to explain \
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
                a timeout leaves the job running."),
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
                    "placement": placement_schema(),
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
    ]
}

/// A tool's successful outcome: text for the model, structured data for programs.
struct Outcome {
    text: String,
    structured: Value,
}

/// Runs one tool. Argument and daemon failures are tool errors (`isError`) so the
/// agent sees the message and can correct itself; the server keeps running.
pub fn call_tool(daemon: &Daemon, name: &str, args: &Map<String, Value>, ctx: &Ctx) -> Value {
    let outcome = Args::new(name, args).and_then(|args| {
        args.placement()?;
        match name {
            "cluster_status" => cluster_status(daemon, &args),
            "list_jobs" => list_jobs(daemon, &args),
            "job" => job(daemon, &args),
            "job_logs" => job_logs(daemon, &args),
            "submit" => submit(daemon, &args, ctx),
            "cancel" => cancel(daemon, &args),
            "wait_job" => wait_job(daemon, &args, ctx),
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
struct Args<'a>(&'a Map<String, Value>);

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
    fn get(&self, key: &str) -> Option<&'a Value> {
        self.0.get(key).filter(|v| !v.is_null())
    }
    fn str(&self, key: &str) -> Result<Option<&'a str>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(format!("{key} must be a string").into()),
        }
    }
    fn bool(&self, key: &str, default: bool) -> Result<bool> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(format!("{key} must be true or false").into()),
        }
    }
    fn int(&self, key: &str, min: i64, max: i64, default: i64) -> Result<i64> {
        match self.get(key) {
            None => Ok(default),
            Some(v) => match v.as_i64() {
                Some(n) if (min..=max).contains(&n) => Ok(n),
                _ => Err(format!("{key} must be an integer in {min}..={max}").into()),
            },
        }
    }
    fn id(&self) -> Result<i64> {
        self.int("id", 1, i64::MAX, 0)
    }
    fn placement(&self) -> Result<()> {
        match self.str("placement")? {
            None | Some("local") => Ok(()),
            Some(other) => Err(format!(
                "placement {other:?} is not available; only \"local\" (this machine's daemon) exists today"
            )
            .into()),
        }
    }
}

fn phase_name(phase: &Phase) -> String {
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

fn cluster_status(daemon: &Daemon, args: &Args) -> Result<Outcome> {
    let recent = args.int("recent_finished", 0, 50, 5)? as usize;
    let admission = args.bool("include_admission", false)?;
    let mut snapshot = daemon.status()?;
    let mut text = crate::status::render(&snapshot);
    // Keep every live job and the newest `recent` finished ones; drop the
    // sparkline rings, which only a chart needs.
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
    let mut structured = serde_json::to_value(&snapshot)?;
    if let Some(machine) = structured.get_mut("machine").and_then(Value::as_object_mut) {
        machine.remove("history");
    }
    for job in structured["jobs"].as_array_mut().into_iter().flatten() {
        if let Some(live) = job.get_mut("live").and_then(Value::as_object_mut) {
            live.remove("history");
        }
    }
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
    Ok(Outcome { text, structured })
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
    Ok(Outcome { text, structured })
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
pub fn run_spec(
    who: &str,
    cwd: &str,
    argv: Vec<String>,
    class: &str,
    serial: Option<&str>,
) -> Result<tinline_task_manager::JobSpec> {
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
    Recipe { argv, resource }.spec(who.to_owned(), cwd)
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
    let spec = run_spec(who, cwd, argv, class, serial)?;
    let response = daemon.ok(Action::Submit(spec))?;
    let id = response
        .submitted_id
        .ok_or("daemon did not return submitted_id; do not resubmit blindly: check list_jobs")?;
    eprintln!("tasks mcp: submitted job {id} for {who}");
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
    let mut outcome = if wait {
        let (job, timed_out) = wait_for(daemon, id, Duration::from_secs(timeout as u64), ctx)
            .map_err(|e| format!("{e}; {}", tinline_task_manager::live_job_hint(id)))?;
        waited(daemon, &job, timed_out, tail)
    } else {
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
    if let Some(warning) = warning {
        outcome.text.push_str(&format!("\nwarning: {warning}"));
        outcome.structured["warning"] = json!(warning);
    }
    Ok(outcome)
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
