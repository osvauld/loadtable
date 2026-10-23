#![allow(dead_code, missing_docs)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Helpers shared by the integration tests.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tasks_core::{JobId, Kind, Phase, Placement};
use tasks_daemon::api::{JobView, Request, Response};
use tasks_mcp::cancel::CancelToken;
use tasks_mcp::tools::{Outcome, ToolResult};
use tasks_mcp::{Config, DaemonError, FakeDaemon, Server, Tools};

pub const NOW: i64 = 1_000_000_000;

pub fn config() -> Config {
    let mut c = Config::from_env(PathBuf::from("/w"), &|k| {
        (k == "TASKS_WHO").then(|| "me".to_string())
    });
    c.poll = Duration::from_millis(1);
    c.now_ms = Arc::new(|| NOW);
    c
}

pub fn argv(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

pub fn job(id: i64, owner: &str, phase: Phase, cmd: &str) -> JobView {
    let done = phase.is_terminal();
    JobView {
        id: JobId(id),
        owner: owner.into(),
        argv: argv(cmd),
        cwd: PathBuf::from("/w"),
        kind: Kind::Compile,
        placement: Placement::Here,
        after: vec![],
        exit: match &phase {
            Phase::Passed => Some(0),
            Phase::Failed { exit } => *exit,
            _ => None,
        },
        phase,
        wait_text: None,
        summary: None,
        submitted_at_ms: NOW - 100_000,
        started_at_ms: Some(NOW - 90_000),
        finished_at_ms: done.then_some(NOW - 30_000),
        machine: Some("laptop".into()),
        forwarded_to: None,
        cancel_requested_by: None,
        plan: None,
        log_path: None,
        figures: None,
        priority: Default::default(),
        priority_reason: None,
        remote_wait: None,
        fetch_paths: Vec::new(),
        fetch: Vec::new(),
        cancel_reason: None,
        timeout_secs: None,
        remote_phase: None,
    }
}

pub fn tools_with(d: &Arc<FakeDaemon>) -> Tools {
    Tools::new(Arc::clone(d) as Arc<dyn tasks_mcp::Daemon>, config())
}

pub fn fake(
    f: impl Fn(&Request) -> Result<Response, DaemonError> + Send + Sync + 'static,
) -> Arc<FakeDaemon> {
    Arc::new(FakeDaemon::from_fn(f))
}

/// A daemon that must never be called.
pub fn never_called() -> Arc<FakeDaemon> {
    fake(|r| Err(DaemonError::Failed(format!("unexpected request {r:?}"))))
}

/// Calls a tool and expects a result.
pub fn call(tools: &Tools, name: &str, args: Value) -> ToolResult {
    match tools.call(name, args, &CancelToken::new()) {
        Some(Outcome::Done(r)) => r,
        other => panic!("expected a result from {name}, got {other:?}"),
    }
}

pub fn error_kind(r: &ToolResult) -> String {
    assert!(r.is_error, "expected an error result, got {}", r.text);
    r.structured["error"]["kind"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// -- sessions -------------------------------------------------------------------------------

/// An output sink the test can read while the server still runs.
#[derive(Clone, Default)]
pub struct SharedBuf(pub Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::ErrorKind::Other)?
            .extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedBuf {
    pub fn lines(&self) -> Vec<String> {
        let bytes = self.0.lock().map(|b| b.clone()).unwrap_or_default();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn messages(&self) -> Vec<Value> {
        self.lines()
            .iter()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON {l:?}: {e}")))
            .collect()
    }

    /// Waits until a response with this id is out.
    pub fn wait_for_id(&self, id: i64) -> Value {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(m) = self.messages().into_iter().find(|m| m["id"] == json!(id)) {
                return m;
            }
            assert!(Instant::now() < end, "no response for id {id}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// An input the test feeds line by line; closing the sender is end of input.
pub struct ChanReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

pub fn input_channel() -> (Sender<Vec<u8>>, std::io::BufReader<ChanReader>) {
    let (tx, rx) = channel();
    (
        tx,
        std::io::BufReader::new(ChanReader {
            rx,
            buf: vec![],
            pos: 0,
        }),
    )
}

impl Read for ChanReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(b) => {
                    self.buf = b;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

pub fn send(tx: &Sender<Vec<u8>>, v: &Value) {
    let mut line = v.to_string().into_bytes();
    line.push(b'\n');
    tx.send(line).unwrap();
}

pub fn rpc(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub fn tool_call(id: i64, name: &str, args: Value) -> Value {
    rpc(id, "tools/call", json!({"name": name, "arguments": args}))
}

/// Runs a whole session from `input` text and returns every output message.
pub fn session(d: &Arc<FakeDaemon>, input: &str) -> Vec<Value> {
    let out = SharedBuf::default();
    Server::new(tools_with(d))
        .serve(input.as_bytes(), out.clone())
        .unwrap();
    out.messages()
}
