//! The stdio server: newline-delimited JSON-RPC 2.0 with MCP framing.
//!
//! * R-194: only JSON-RPC goes to the output; diagnostics go to stderr. A malformed message
//!   gets a JSON-RPC error. A cancelled request gets no response. Every `tools/call` runs on
//!   its own thread, so one slow daemon call blocks nothing else, and requests already in
//!   flight are answered after the input closes.
//! * Cancelling a wait stops the wait; it never touches the job (R-104, R-105).

use crate::cancel::CancelToken;
use crate::defs::tool_defs;
use crate::rpc::{
    INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, Incoming, METHOD_NOT_FOUND, SERVER_BUSY,
    error_response, parse_line, result_response,
};
use crate::tools::{Outcome, ToolResult, Tools};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex, PoisonError};

/// MCP protocol versions this server speaks, newest first.
pub const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
/// Longest accepted input line, in bytes.
pub const MAX_LINE_BYTES: usize = 4 << 20;
/// Most tool calls in flight at once.
pub const MAX_IN_FLIGHT: usize = 64;

const INSTRUCTIONS: &str = "tasks coordinates builds and tests on this machine (and its peers). \
Submit work with submit, submit_steps or run_suite; each returns job ids at once. Then wait for a \
job (cancelling a wait never touches the job), read logs, or check status. Pass your own agent \
name as who. Heavy cargo/gradle commands belong in the queue, not in a shell.";

/// The version to answer an `initialize` that asked for `requested`: the same when supported,
/// else our newest.
pub fn negotiate_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|r| SUPPORTED_VERSIONS.iter().find(|v| **v == r))
        .copied()
        .unwrap_or(SUPPORTED_VERSIONS[0])
}

/// The server: a tool set and its in-flight requests.
pub struct Server {
    tools: Tools,
    in_flight: Mutex<HashMap<String, Arc<CancelToken>>>,
}

struct Out<W> {
    w: Mutex<W>,
}

impl<W: Write> Out<W> {
    fn send(&self, line: &str) {
        let mut w = self.w.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(e) = w
            .write_all(line.as_bytes())
            .and_then(|()| w.write_all(b"\n"))
            .and_then(|()| w.flush())
        {
            // the client went away; there is nobody to tell
            eprintln!("tasks mcp: cannot write to the client: {e}");
        }
    }
}

enum LineRead {
    Line,
    TooLong,
    Eof,
}

/// Reads one line of at most `max` bytes into `buf`. A longer line is consumed to its end and
/// reported as [`LineRead::TooLong`].
fn read_line_bounded(r: &mut dyn BufRead, max: usize, buf: &mut Vec<u8>) -> io::Result<LineRead> {
    buf.clear();
    let mut too_long = false;
    let mut any = false;
    loop {
        let chunk = match r.fill_buf() {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            return Ok(if !any {
                LineRead::Eof
            } else if too_long {
                LineRead::TooLong
            } else {
                LineRead::Line
            });
        }
        any = true;
        let (take, found) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if !too_long {
            if buf.len() + take > max + 1 {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk.get(..take).unwrap_or(&[]));
            }
        }
        r.consume(take);
        if found {
            if !too_long {
                while matches!(buf.last(), Some(b'\n' | b'\r')) {
                    buf.pop();
                }
            }
            return Ok(if too_long {
                LineRead::TooLong
            } else {
                LineRead::Line
            });
        }
    }
}

impl Server {
    /// A server over `tools`.
    pub fn new(tools: Tools) -> Server {
        Server {
            tools,
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    fn flights(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<CancelToken>>> {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Serves `input` until it closes, then finishes the requests still in flight and returns.
    pub fn serve<R: BufRead, W: Write + Send>(&self, mut input: R, output: W) -> io::Result<()> {
        let out = Out {
            w: Mutex::new(output),
        };
        std::thread::scope(|scope| {
            let mut buf = Vec::new();
            loop {
                match read_line_bounded(&mut input, MAX_LINE_BYTES, &mut buf)? {
                    LineRead::Eof => return Ok(()),
                    LineRead::TooLong => {
                        out.send(&error_response(
                            &Value::Null,
                            INVALID_REQUEST,
                            &format!("message is longer than {MAX_LINE_BYTES} bytes"),
                        ));
                        continue;
                    }
                    LineRead::Line => {}
                }
                if buf.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match parse_line(&buf) {
                    Incoming::Invalid { id, code, message } => {
                        eprintln!("tasks mcp: bad message: {message}");
                        out.send(&error_response(&id, code, &message));
                    }
                    Incoming::Notification { method, params } => {
                        self.notification(&method, &params)
                    }
                    Incoming::Request { id, method, params } => match method.as_str() {
                        "tools/call" => self.spawn_call(scope, &out, id, params),
                        _ => out.send(&self.simple_request(&id, &method, &params)),
                    },
                }
            }
        })
    }

    fn notification(&self, method: &str, params: &Value) {
        if method == "notifications/cancelled" {
            // the id of a request that is not in flight any more is not an error
            if let Some(rid) = params.get("requestId")
                && let Some(tok) = self.flights().get(&rid.to_string())
            {
                tok.cancel();
            }
        }
        // notifications/initialized and unknown notifications need no action
    }

    fn simple_request(&self, id: &Value, method: &str, params: &Value) -> String {
        match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                result_response(
                    id,
                    json!({
                        "protocolVersion": negotiate_version(requested),
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": "tasks", "version": env!("CARGO_PKG_VERSION")},
                        "instructions": INSTRUCTIONS,
                    }),
                )
            }
            "ping" => result_response(id, json!({})),
            "tools/list" => result_response(
                id,
                json!({"tools": tool_defs().iter().map(|d| d.to_json()).collect::<Vec<_>>()}),
            ),
            other => error_response(id, METHOD_NOT_FOUND, &format!("method not found: {other}")),
        }
    }

    fn spawn_call<'scope, 'env, W: Write + Send>(
        &'scope self,
        scope: &'scope std::thread::Scope<'scope, 'env>,
        out: &'scope Out<W>,
        id: Value,
        params: Value,
    ) {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(name) = name else {
            out.send(&error_response(
                &id,
                INVALID_PARAMS,
                "tools/call needs a tool name",
            ));
            return;
        };
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        if !tool_defs().iter().any(|d| d.name == name) {
            out.send(&error_response(
                &id,
                INVALID_PARAMS,
                &format!("unknown tool: {name}"),
            ));
            return;
        }
        let key = id.to_string();
        let tok = CancelToken::new();
        {
            let mut flights = self.flights();
            if flights.len() >= MAX_IN_FLIGHT {
                drop(flights);
                out.send(&error_response(
                    &id,
                    SERVER_BUSY,
                    "too many tool calls in flight; wait for one to finish",
                ));
                return;
            }
            flights.insert(key.clone(), Arc::clone(&tok));
        }
        let worker = {
            let tok = Arc::clone(&tok);
            let id = id.clone();
            let key = key.clone();
            move || {
                let outcome = self.tools.call(&name, args, &tok);
                self.flights().remove(&key);
                match outcome {
                    // cancelled: no response, even if the work finished meanwhile
                    _ if tok.is_cancelled() => {}
                    Some(Outcome::Done(r)) => {
                        out.send(&result_response(&id, ToolResult::to_json(&r)))
                    }
                    Some(Outcome::Cancelled) => {}
                    None => out.send(&error_response(&id, INTERNAL_ERROR, "tool vanished")),
                }
            }
        };
        if std::thread::Builder::new()
            .name("tasks-mcp-call".into())
            .spawn_scoped(scope, worker)
            .is_err()
        {
            self.flights().remove(&key);
            out.send(&error_response(
                &id,
                INTERNAL_ERROR,
                "cannot start a thread for this call",
            ));
        }
    }
}
