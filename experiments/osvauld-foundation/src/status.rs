//! Read-only cluster status stream (ALPN [`STATUS_ALPN`] = `task-manager/status/0`).
//!
//! The worker publishes the output of ONE fixed, server-configured command (default
//! `~/.local/bin/tasks status --json`, the cluster snapshot of `docs/design/cluster-snapshot.md`)
//! to attested, allowlisted peers. Nothing a client sends can change what runs:
//!
//! 1. `Hello{attestation}` → [`Policy::admit`] (account attestation + local allowlist) BEFORE any
//!    challenge, exactly as the source/artifact ALPNs.
//! 2. `Challenge{nonce}`.
//! 3. `Request{body, sig}`: signed over `task-manager/status/request/v0`, the nonce, both endpoint
//!    ids and `SHA-256(body)`. The body is bounded by the 64 KiB frame limit and otherwise
//!    IGNORED: the command's argv, interval and size bound come from `lan-peer serve` flags only.
//! 4. `Response{body, sig}`: body `{"interval_s", "max_bytes"}`, signed over the response domain,
//!    the nonce, both ids, request and response hashes (session freshness for the client).
//! 5. Then, for every sample: `Snapshot{seq, size}` followed by exactly `size` bytes of compact
//!    JSON (one object with a `version` field, at most `max_bytes`), or `StatusError{seq, error}`
//!    when the command failed. The stream stays open until either side closes it.
//!
//! **Sampling.** One sampler per server, shared by all subscribers through a `watch` channel: it
//! runs only while at least one subscriber is connected (it exits at the first tick that finds
//! none), at most once per `interval` (minimum 1 s), each run bounded by a timeout and its stdout
//! by `max_bytes` (at most [`MAX_SNAPSHOT_BYTES`]). A slow subscriber only ever sees the latest
//! sample; nothing queues per subscriber. At most [`MAX_SUBSCRIBERS`] streams at once.
//!
//! **Security argument.** There is no remote-execution surface: the only process ever started is
//! the argv fixed at `serve` start, with no client-controlled arguments, environment, stdin or
//! timing beyond "someone is subscribed" (which can at most make it run once per interval).
//! Information goes only to peers that pass the same admission as the other ALPNs (TLS-proven
//! endpoint id + valid account attestation + local allowlist); a rejected peer gets no challenge
//! and no data. What the attested peer learns is whatever the status command prints (job owners,
//! argv, checkout paths, machine load) — that is the intended disclosure and why the allowlist,
//! not the account, gates it.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use courier::token::Token;
use iroh::endpoint::{Connection, VarInt};
use iroh::{Endpoint, EndpointAddr, PublicKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::{CLOSE_GRACE, Frame, Policy, read_frame, verify_sig, write_frame};

pub const STATUS_ALPN: &[u8] = b"task-manager/status/0";
const REQUEST_DOMAIN: &[u8] = b"task-manager/status/request/v0\0";
const RESPONSE_DOMAIN: &[u8] = b"task-manager/status/response/v0\0";
/// Hard upper bound of one snapshot (server output and client read).
pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(2);
/// Concurrent status streams per server.
pub const MAX_SUBSCRIBERS: usize = 8;
pub const DEFAULT_STATUS_CMD: &str = "~/.local/bin/tasks status --json";
/// A status command that has not exited by then is killed (its whole process group).
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
/// A subscriber that does not accept a snapshot within this time is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_CHARS: usize = 1000;

/// Server-side configuration. Fixed at `serve` start; never derived from a request.
#[derive(Debug, Clone)]
pub struct StatusConfig {
    pub argv: Vec<String>,
    pub interval: Duration,
    pub max_bytes: usize,
    pub command_timeout: Duration,
}

impl StatusConfig {
    pub fn new(argv: Vec<String>, interval: Duration, max_bytes: usize) -> Result<Self, String> {
        if argv.is_empty() || argv[0].is_empty() {
            return Err("status command is empty".into());
        }
        if interval < MIN_INTERVAL {
            return Err(format!(
                "status interval {interval:?} is below the {MIN_INTERVAL:?} minimum"
            ));
        }
        if max_bytes == 0 || max_bytes > MAX_SNAPSHOT_BYTES {
            return Err(format!(
                "status max bytes must be 1..={MAX_SNAPSHOT_BYTES}, got {max_bytes}"
            ));
        }
        Ok(Self {
            argv,
            interval,
            max_bytes,
            command_timeout: COMMAND_TIMEOUT,
        })
    }
}

/// Splits a `--status-cmd` value on whitespace (NO shell: no quoting, globbing, variables or
/// pipes) and expands a leading `~/` of any word to `$HOME/`.
pub fn parse_status_cmd(text: &str, home: Option<&str>) -> Result<Vec<String>, String> {
    let argv: Vec<String> = text
        .split_whitespace()
        .map(|word| match (word.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => Ok(format!("{}/{rest}", home.trim_end_matches('/'))),
            (Some(_), None) => Err(format!("{word}: HOME is not set")),
            (None, _) => Ok(word.to_owned()),
        })
        .collect::<Result<_, _>>()?;
    if argv.is_empty() {
        return Err("--status-cmd is empty".into());
    }
    Ok(argv)
}

/// One sample of the status command.
#[derive(Debug, Clone)]
pub enum Sample {
    /// Compact JSON of one snapshot object.
    Snapshot(Arc<Vec<u8>>),
    Error(String),
}

fn truncate(mut text: String) -> String {
    if text.chars().count() > MAX_ERROR_CHARS {
        text = text.chars().take(MAX_ERROR_CHARS).collect::<String>() + "...";
    }
    text
}

/// Validates command output: one JSON object with a numeric `version`, re-serialized compactly
/// (so it is a single line) and checked against `max_bytes` again.
pub fn validate_snapshot(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>, String> {
    if bytes.len() > max_bytes {
        return Err(format!("snapshot too large (> {max_bytes} bytes)"));
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("status output is not JSON: {e}"))?;
    if !value.get("version").is_some_and(|v| v.is_u64()) {
        return Err("status output is not a snapshot object with a numeric version".into());
    }
    let compact = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
    if compact.len() > max_bytes {
        return Err(format!("snapshot too large (> {max_bytes} bytes)"));
    }
    Ok(compact)
}

fn read_capped(mut from: impl Read, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    (&mut from).take(cap as u64).read_to_end(&mut buf)?;
    // Drain (and discard) the rest so the child never blocks on a full pipe.
    std::io::copy(&mut from, &mut std::io::sink())?;
    Ok(buf)
}

/// Runs the configured command once (blocking). Its own process group, stdin from /dev/null,
/// stdout capped at `max_bytes + 1`, stderr capped for the error message, killed (whole group)
/// after `command_timeout`.
pub fn run_status_command(config: &StatusConfig) -> Sample {
    let mut child = match Command::new(&config.argv[0])
        .args(&config.argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Sample::Error(format!("cannot start {}: {e}", config.argv[0])),
    };
    let pid = child.id() as libc::pid_t;
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().expect("piped stdout");
    let cap = config.max_bytes + 1;
    std::thread::spawn(move || {
        let _ = out_tx.send(read_capped(stdout, cap));
    });
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    let stderr = child.stderr.take().expect("piped stderr");
    std::thread::spawn(move || {
        let _ = err_tx.send(read_capped(stderr, 4096));
    });

    let deadline = Instant::now() + config.command_timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Sample::Error(format!("wait for status command: {e}")),
        }
    };
    let Some(status) = status else {
        // SAFETY: plain kill(2) of the child's own process group.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let _ = child.wait();
        return Sample::Error(format!(
            "status command timed out after {:?}",
            config.command_timeout
        ));
    };
    // A descendant that kept the pipe open must not hang the sampler.
    let grace = Duration::from_secs(2);
    let stdout = match out_rx.recv_timeout(grace) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => return Sample::Error(format!("read status output: {e}")),
        Err(_) => return Sample::Error("status output pipe stayed open after exit".into()),
    };
    if !status.success() {
        let stderr = err_rx
            .recv_timeout(grace)
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        return Sample::Error(truncate(format!(
            "status command failed ({status}): {}",
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    match validate_snapshot(&stdout, config.max_bytes) {
        Ok(compact) => Sample::Snapshot(Arc::new(compact)),
        Err(e) => Sample::Error(truncate(e)),
    }
}

struct HubState {
    subscribers: usize,
    sampler_running: bool,
}

/// Shared sampler + latest sample for all status subscribers of one server.
pub struct StatusHub {
    config: StatusConfig,
    state: Mutex<HubState>,
    tx: watch::Sender<(u64, Option<Sample>)>,
    samples: AtomicU64,
}

/// Holding one keeps the sampler alive; dropping the last one stops it at its next tick.
pub struct Subscription {
    hub: Arc<StatusHub>,
    pub rx: watch::Receiver<(u64, Option<Sample>)>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut state = self.hub.state.lock().expect("hub state");
        state.subscribers -= 1;
    }
}

impl StatusHub {
    pub fn new(config: StatusConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            state: Mutex::new(HubState {
                subscribers: 0,
                sampler_running: false,
            }),
            tx: watch::channel((0, None)).0,
            samples: AtomicU64::new(0),
        })
    }

    pub fn config(&self) -> &StatusConfig {
        &self.config
    }

    pub fn subscribers(&self) -> usize {
        self.state.lock().expect("hub state").subscribers
    }

    pub fn sampler_running(&self) -> bool {
        self.state.lock().expect("hub state").sampler_running
    }

    /// Number of times the status command has been run.
    pub fn samples_taken(&self) -> u64 {
        self.samples.load(Ordering::SeqCst)
    }

    /// Registers a subscriber and starts the sampler if it is not running. A subscriber that
    /// starts the sampler waits for the first fresh sample; one that joins a running sampler
    /// gets the current (at most one interval old) sample immediately.
    pub fn subscribe(self: &Arc<Self>) -> Result<Subscription, String> {
        let mut rx = self.tx.subscribe();
        let mut state = self.state.lock().expect("hub state");
        if state.subscribers >= MAX_SUBSCRIBERS {
            return Err("TooManySubscribers".into());
        }
        state.subscribers += 1;
        if state.sampler_running {
            if rx.borrow().1.is_some() {
                rx.mark_changed();
            }
        } else {
            rx.mark_unchanged();
            state.sampler_running = true;
            tokio::spawn(Arc::clone(self).sample_loop());
        }
        Ok(Subscription {
            hub: Arc::clone(self),
            rx,
        })
    }

    async fn sample_loop(self: Arc<Self>) {
        loop {
            let config = self.config.clone();
            let sample = tokio::task::spawn_blocking(move || run_status_command(&config))
                .await
                .unwrap_or_else(|e| Sample::Error(format!("sampler task failed: {e}")));
            self.samples.fetch_add(1, Ordering::SeqCst);
            self.tx.send_modify(|(seq, latest)| {
                *seq += 1;
                *latest = Some(sample);
            });
            tokio::time::sleep(self.config.interval).await;
            let mut state = self.state.lock().expect("hub state");
            if state.subscribers == 0 {
                state.sampler_running = false;
                return;
            }
        }
    }
}

fn signed_message(
    domain: &[u8],
    challenge: &[u8],
    first: &PublicKey,
    second: &PublicKey,
    bodies: &[&[u8]],
) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut m = domain.to_vec();
    m.extend_from_slice(challenge);
    m.extend_from_slice(first.as_bytes());
    m.extend_from_slice(second.as_bytes());
    for body in bodies {
        m.extend(Sha256::digest(body));
    }
    m
}

pub fn request_message(
    challenge: &[u8],
    requester: &PublicKey,
    receiver: &PublicKey,
    body: &[u8],
) -> Vec<u8> {
    signed_message(REQUEST_DOMAIN, challenge, requester, receiver, &[body])
}

pub fn response_message(
    challenge: &[u8],
    receiver: &PublicKey,
    requester: &PublicKey,
    request_body: &[u8],
    response_body: &[u8],
) -> Vec<u8> {
    signed_message(
        RESPONSE_DOMAIN,
        challenge,
        receiver,
        requester,
        &[request_body, response_body],
    )
}

/// The signed session parameters the server announces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusHello {
    pub interval_s: f64,
    pub max_bytes: usize,
}

#[derive(Debug)]
pub enum StatusOutcome {
    /// The stream ended (client closed, endpoint closed or a write failed).
    Ended {
        snapshots_sent: u64,
        errors_sent: u64,
        reason: String,
    },
    Rejected(String),
}

/// Server side of one status stream. Runs until the subscriber goes away.
pub async fn handle_status(
    ep: &Endpoint,
    conn: Connection,
    policy: &Policy,
    hub: &Arc<StatusHub>,
) -> Result<StatusOutcome, String> {
    if conn.alpn() != STATUS_ALPN {
        return Err("unexpected ALPN".into());
    }
    let remote = conn.remote_id();
    let (mut send, mut recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
    let handshake = async {
        let Frame::Hello { attestation } = read_frame(&mut recv).await? else {
            return Err("expected hello".to_string());
        };
        if let Err(reject) = policy.admit(&remote, attestation.as_ref()) {
            return Ok(Err(format!("{reject:?}")));
        }
        let mut nonce = vec![0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        write_frame(
            &mut send,
            &Frame::Challenge {
                nonce: nonce.clone(),
            },
        )
        .await?;
        // The body is authenticated but deliberately ignored: no request field reaches the
        // command, its interval or its bounds.
        let Frame::Request { body, sig } = read_frame(&mut recv).await? else {
            return Err("expected request".to_string());
        };
        if !verify_sig(
            &remote,
            &request_message(&nonce, &remote, &ep.id(), &body),
            &sig,
        ) {
            return Ok(Err("BadRequest".to_string()));
        }
        let subscription = match hub.subscribe() {
            Ok(s) => s,
            Err(reason) => return Ok(Err(reason)),
        };
        let response = serde_json::to_vec(&StatusHello {
            interval_s: hub.config.interval.as_secs_f64(),
            max_bytes: hub.config.max_bytes,
        })
        .map_err(|e| e.to_string())?;
        let sig = ep
            .secret_key()
            .sign(&response_message(
                &nonce,
                &ep.id(),
                &remote,
                &body,
                &response,
            ))
            .to_bytes()
            .to_vec();
        write_frame(
            &mut send,
            &Frame::Response {
                body: response,
                sig,
            },
        )
        .await?;
        Ok(Ok(subscription))
    };
    let mut subscription = match tokio::time::timeout(CLOSE_GRACE * 3, handshake).await {
        Err(_) => return Err("handshake timed out".into()),
        Ok(Err(e)) => return Err(e),
        Ok(Ok(Err(reason))) => {
            write_frame(
                &mut send,
                &Frame::Rejected {
                    reason: reason.clone(),
                },
            )
            .await?;
            send.finish().map_err(|e| e.to_string())?;
            let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
            return Ok(StatusOutcome::Rejected(reason));
        }
        Ok(Ok(Ok(subscription))) => subscription,
    };

    let (mut snapshots_sent, mut errors_sent) = (0u64, 0u64);
    let reason = loop {
        tokio::select! {
            changed = subscription.rx.changed() => {
                if changed.is_err() {
                    break "hub closed".to_string();
                }
            }
            closed = conn.closed() => break format!("closed: {closed}"),
        }
        let (seq, sample) = subscription.rx.borrow_and_update().clone();
        let write = async {
            match sample {
                Some(Sample::Snapshot(bytes)) => {
                    write_frame(
                        &mut send,
                        &Frame::Snapshot {
                            seq,
                            size: bytes.len() as u64,
                        },
                    )
                    .await?;
                    send.write_all(&bytes).await.map_err(|e| e.to_string())?;
                    Ok(true)
                }
                Some(Sample::Error(error)) => {
                    write_frame(&mut send, &Frame::StatusError { seq, error }).await?;
                    Ok(false)
                }
                None => Err("no sample".to_string()),
            }
        };
        match tokio::time::timeout(WRITE_TIMEOUT, write).await {
            Ok(Ok(true)) => snapshots_sent += 1,
            Ok(Ok(false)) => errors_sent += 1,
            Ok(Err(e)) => break format!("write: {e}"),
            Err(_) => break "subscriber stopped reading".to_string(),
        }
    };
    drop(subscription);
    let _ = send.finish();
    conn.close(VarInt::from_u32(0), b"status stream ended");
    Ok(StatusOutcome::Ended {
        snapshots_sent,
        errors_sent,
        reason,
    })
}

/// What a subscriber receives.
#[derive(Debug, Clone)]
pub enum StatusEvent {
    Snapshot { seq: u64, value: serde_json::Value },
    Error { seq: u64, error: String },
}

/// Client limits for one status session.
#[derive(Debug, Clone, Copy)]
pub struct WatchOptions {
    /// Largest snapshot accepted (at most [`MAX_SNAPSHOT_BYTES`]).
    pub max_bytes: usize,
    /// Connect + admission + signed hello must finish within this.
    pub handshake_timeout: Duration,
    /// No frame for this long ends the session. `None`: 3 × the announced interval + 10 s.
    pub stall: Option<Duration>,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            max_bytes: MAX_SNAPSHOT_BYTES,
            handshake_timeout: Duration::from_secs(30),
            stall: None,
        }
    }
}

/// Client side: handshake, then call `on_event` for every frame until it returns `false`
/// (`Ok`), the stream ends or no frame arrives within the stall timeout. Errors start with
/// `rejected:` (the server's admission), `verify:` (bad response signature, oversized or
/// malformed snapshot), `stalled`, `timeout` (handshake) or are transport errors.
pub async fn watch_status(
    ep: &Endpoint,
    to: EndpointAddr,
    attestation: Option<Token>,
    options: WatchOptions,
    mut on_event: impl FnMut(StatusEvent) -> bool,
) -> Result<StatusHello, String> {
    let receiver = to.id;
    let max_bytes = options.max_bytes.min(MAX_SNAPSHOT_BYTES);
    let conn = tokio::time::timeout(options.handshake_timeout, ep.connect(to, STATUS_ALPN))
        .await
        .map_err(|_| "timeout: connect".to_string())?
        .map_err(|e| e.to_string())?;
    let result = async {
        let handshake = async {
            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
            write_frame(&mut send, &Frame::Hello { attestation }).await?;
            let challenge = match read_frame(&mut recv).await? {
                Frame::Challenge { nonce } => nonce,
                Frame::Rejected { reason } => return Err(format!("rejected: {reason}")),
                other => return Err(format!("unexpected {other:?}")),
            };
            let body = br#"{"subscribe":"status"}"#.to_vec();
            let sig = ep
                .secret_key()
                .sign(&request_message(&challenge, &ep.id(), &receiver, &body))
                .to_bytes()
                .to_vec();
            write_frame(
                &mut send,
                &Frame::Request {
                    body: body.clone(),
                    sig,
                },
            )
            .await?;
            let (response, sig) = match read_frame(&mut recv).await? {
                Frame::Response { body, sig } => (body, sig),
                Frame::Rejected { reason } => return Err(format!("rejected: {reason}")),
                other => return Err(format!("unexpected {other:?}")),
            };
            if !verify_sig(
                &receiver,
                &response_message(&challenge, &receiver, &ep.id(), &body, &response),
                &sig,
            ) {
                return Err("verify: response not bound to this session".into());
            }
            let hello: StatusHello =
                serde_json::from_slice(&response).map_err(|e| format!("verify: bad hello: {e}"))?;
            Ok((send, recv, hello))
        };
        let (_send, mut recv, hello) = tokio::time::timeout(options.handshake_timeout, handshake)
            .await
            .map_err(|_| "timeout: handshake".to_string())??;
        let stall = options.stall.unwrap_or_else(|| {
            Duration::from_secs_f64(hello.interval_s.clamp(1.0, 3600.0) * 3.0 + 10.0)
        });
        loop {
            let frame = match tokio::time::timeout(stall, read_frame(&mut recv)).await {
                Err(_) => return Err(format!("stalled: no snapshot for {stall:?}")),
                Ok(frame) => frame?,
            };
            let event = match frame {
                Frame::Snapshot { seq, size } => {
                    if size > max_bytes as u64 {
                        let _ = recv.stop(VarInt::from_u32(1));
                        return Err(format!(
                            "verify: snapshot too large ({size} > {max_bytes} bytes)"
                        ));
                    }
                    let mut buf = vec![0u8; size as usize];
                    tokio::time::timeout(stall, recv.read_exact(&mut buf))
                        .await
                        .map_err(|_| format!("stalled: snapshot body for {stall:?}"))?
                        .map_err(|e| e.to_string())?;
                    let value: serde_json::Value = serde_json::from_slice(&buf)
                        .map_err(|e| format!("verify: snapshot is not JSON: {e}"))?;
                    if !value.is_object() {
                        return Err("verify: snapshot is not a JSON object".into());
                    }
                    StatusEvent::Snapshot { seq, value }
                }
                Frame::StatusError { seq, error } => StatusEvent::Error { seq, error },
                Frame::Rejected { reason } => return Err(format!("rejected: {reason}")),
                other => return Err(format!("unexpected {other:?}")),
            };
            if !on_event(event) {
                return Ok(hello);
            }
        }
    }
    .await;
    conn.close(VarInt::from_u32(0), b"done");
    result
}

/// What `status-watch` prints for a live snapshot: the server's document, with
/// `machine.online = true` and a `stream` object added (contract: unknown fields are ignored).
pub fn annotate_live(
    mut value: serde_json::Value,
    transport: &str,
    peer: &str,
    seq: u64,
    received_at: f64,
) -> serde_json::Value {
    if !value.get("machine").is_some_and(|m| m.is_object()) {
        value["machine"] = serde_json::json!({});
    }
    value["machine"]["online"] = serde_json::json!(true);
    value["stream"] = serde_json::json!({
        "transport": transport,
        "peer": peer,
        "seq": seq,
        "received_at": received_at,
    });
    value
}

/// The synthetic line printed when the stream is down (`marker: "offline"`) or the worker's
/// status command failed (`marker: "status_error"`, machine still online). `jobs` is empty
/// because nothing is known; readers must keep their last view (greyed), not clear it.
pub fn marker(
    kind: &str,
    machine_id: &str,
    transport: &str,
    peer: &str,
    error: &str,
    retry_in_s: Option<f64>,
    now: f64,
) -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "taken_at": now,
        "machine": {"id": machine_id, "online": kind != "offline"},
        "jobs": [],
        "stream": {
            "transport": transport,
            "peer": peer,
            "marker": kind,
            "error": error,
            "retry_in_s": retry_in_s,
        },
    })
}

pub fn now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
