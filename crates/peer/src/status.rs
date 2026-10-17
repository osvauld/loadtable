//! Read-only cluster status stream (ALPN [`STATUS_ALPN`] = `task-manager/status/0`).
//!
//! The daemon publishes its own `tasks status --json` snapshot (cluster snapshot v1,
//! `docs/design/cluster-snapshot.md`) to allowlisted peers of any role:
//!
//! 1. `Hello` -> allowlist admission BEFORE any challenge.
//! 2. `Challenge{nonce}`; signed `Request{body, sig}` (the body is authenticated and ignored).
//! 3. Signed `Response{body}`: `{"interval_s", "max_bytes"}`.
//! 4. Then per sample: `Snapshot{seq, size}` + `size` bytes of compact JSON, or
//!    `StatusError{seq, error}`. The stream stays open until either side closes it.
//!
//! **Sampling.** One sampler per daemon, shared by all subscribers through a `watch` channel,
//! running only while someone is subscribed, at most once per interval. The sample source is a
//! fixed function supplied by the embedder (the daemon's in-process snapshot); nothing a client
//! sends reaches it. A slow subscriber only ever sees the latest sample.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::endpoint::{Connection, VarInt};
use iroh::{Endpoint, EndpointAddr};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::{
    Accepted, Allowlist, CLOSE_GRACE, Capability, ClientError, Domains, Frame, MAX_FRAME,
    accept_session, open_session, read_frame, write_frame,
};

pub const STATUS_ALPN: &[u8] = b"task-manager/status/0";
pub const STATUS_DOMAINS: Domains = Domains {
    request: b"task-manager/status/request/v0\0",
    response: b"task-manager/status/response/v0\0",
};
/// Hard upper bound of one snapshot (server output and client read).
pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
/// Concurrent status streams per daemon.
pub const MAX_SUBSCRIBERS: usize = 8;
/// A subscriber that does not accept a snapshot within this time is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// One sample of the status source.
#[derive(Debug, Clone)]
pub enum Sample {
    /// Compact JSON of one snapshot object.
    Snapshot(Arc<Vec<u8>>),
    Error(String),
}

pub type SampleSource = Arc<dyn Fn() -> Sample + Send + Sync>;

/// Validates a snapshot: one JSON object with a numeric `version`, re-serialized compactly.
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

struct HubState {
    subscribers: usize,
    sampler_running: bool,
}

/// Shared sampler + latest sample for all status subscribers of one daemon.
pub struct StatusHub {
    source: SampleSource,
    interval: Duration,
    max_bytes: usize,
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
    pub fn new(source: SampleSource, interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            source,
            interval: interval.max(Duration::from_secs(1)),
            max_bytes: MAX_SNAPSHOT_BYTES,
            state: Mutex::new(HubState {
                subscribers: 0,
                sampler_running: false,
            }),
            tx: watch::channel((0, None)).0,
            samples: AtomicU64::new(0),
        })
    }

    pub fn samples_taken(&self) -> u64 {
        self.samples.load(Ordering::SeqCst)
    }

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
            let source = Arc::clone(&self.source);
            let max = self.max_bytes;
            let sample = tokio::task::spawn_blocking(move || match source() {
                Sample::Snapshot(bytes) => match validate_snapshot(&bytes, max) {
                    Ok(compact) => Sample::Snapshot(Arc::new(compact)),
                    Err(e) => Sample::Error(e),
                },
                error => error,
            })
            .await
            .unwrap_or_else(|e| Sample::Error(format!("sampler task failed: {e}")));
            self.samples.fetch_add(1, Ordering::SeqCst);
            self.tx.send_modify(|(seq, latest)| {
                *seq += 1;
                *latest = Some(sample);
            });
            tokio::time::sleep(self.interval).await;
            let mut state = self.state.lock().expect("hub state");
            if state.subscribers == 0 {
                state.sampler_running = false;
                return;
            }
        }
    }
}

/// The signed session parameters the server announces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusHello {
    pub interval_s: f64,
    pub max_bytes: usize,
}

#[derive(Debug)]
pub enum StatusOutcome {
    Ended {
        peer: String,
        snapshots_sent: u64,
        errors_sent: u64,
        reason: String,
    },
    Rejected {
        peer: Option<String>,
        reason: String,
    },
}

/// Server side of one status stream. Runs until the subscriber goes away.
pub async fn handle_status(
    ep: &Endpoint,
    conn: Connection,
    allow: &Allowlist,
    hub: &Arc<StatusHub>,
) -> Result<StatusOutcome, String> {
    let mut s = match accept_session(
        ep,
        &conn,
        allow,
        Capability::Status,
        STATUS_DOMAINS,
        MAX_FRAME,
    )
    .await?
    {
        Accepted::Refused(reason) => {
            let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
            return Ok(StatusOutcome::Rejected { peer: None, reason });
        }
        Accepted::Session(s) => s,
    };
    let peer = s.peer.name.clone();
    let mut subscription = match hub.subscribe() {
        Ok(sub) => sub,
        Err(reason) => {
            let _ = s.reject(&reason).await;
            return Ok(StatusOutcome::Rejected {
                peer: Some(peer),
                reason,
            });
        }
    };
    let hello = serde_json::to_vec(&StatusHello {
        interval_s: hub.interval.as_secs_f64(),
        max_bytes: hub.max_bytes,
    })
    .map_err(|e| e.to_string())?;
    s.respond(ep, &hello).await?;
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
        let send = &mut s.send;
        let write = async {
            match sample {
                Some(Sample::Snapshot(bytes)) => {
                    write_frame(
                        send,
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
                    write_frame(send, &Frame::StatusError { seq, error }).await?;
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
    let _ = s.send.finish();
    conn.close(VarInt::from_u32(0), b"status stream ended");
    Ok(StatusOutcome::Ended {
        peer,
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

/// Client side: handshake, then `on_event` for every frame until it returns `false` (`Ok`),
/// the stream ends or no frame arrives within 3 x interval + 10 s.
pub async fn watch_status(
    ep: &Endpoint,
    to: EndpointAddr,
    timeout: Duration,
    mut on_event: impl FnMut(StatusEvent) -> bool,
) -> Result<StatusHello, ClientError> {
    let body = br#"{"subscribe":"status"}"#;
    let mut s = open_session(ep, to, STATUS_ALPN, STATUS_DOMAINS, body, timeout).await?;
    let result = async {
        let hello: StatusHello = serde_json::from_slice(&s.response(MAX_FRAME).await?)
            .map_err(|e| ClientError::Protocol(format!("bad hello: {e}")))?;
        let stall = Duration::from_secs_f64(hello.interval_s.clamp(1.0, 3600.0) * 3.0 + 10.0);
        loop {
            let frame = match tokio::time::timeout(stall, read_frame(&mut s.recv)).await {
                Err(_) => {
                    return Err(ClientError::Lost(format!(
                        "stalled: no snapshot for {stall:?}"
                    )));
                }
                Ok(Err(e)) => return Err(ClientError::Lost(e)),
                Ok(Ok(frame)) => frame,
            };
            let event = match frame {
                Frame::Snapshot { seq, size } => {
                    if size > MAX_SNAPSHOT_BYTES as u64 {
                        return Err(ClientError::Protocol(format!(
                            "snapshot too large ({size} bytes)"
                        )));
                    }
                    let mut buf = vec![0u8; size as usize];
                    tokio::time::timeout(stall, s.recv.read_exact(&mut buf))
                        .await
                        .map_err(|_| ClientError::Lost("stalled in a snapshot body".into()))?
                        .map_err(|e| ClientError::Lost(e.to_string()))?;
                    let value: serde_json::Value = serde_json::from_slice(&buf)
                        .map_err(|e| ClientError::Protocol(format!("snapshot is not JSON: {e}")))?;
                    StatusEvent::Snapshot { seq, value }
                }
                Frame::StatusError { seq, error } => StatusEvent::Error { seq, error },
                Frame::Rejected { reason } => return Err(ClientError::Refused(reason)),
                other => return Err(ClientError::Protocol(format!("unexpected {other:?}"))),
            };
            if !on_event(event) {
                return Ok(hello.clone());
            }
        }
    }
    .await;
    s.close();
    result
}

/// What a watcher prints for a live snapshot: the peer's document with `machine.online = true`
/// and a `stream` object (contract: unknown fields are ignored by readers).
pub fn annotate_live(
    mut value: serde_json::Value,
    peer: &str,
    seq: u64,
    received_at: f64,
) -> serde_json::Value {
    if !value.get("machine").is_some_and(|m| m.is_object()) {
        value["machine"] = serde_json::json!({});
    }
    value["machine"]["online"] = serde_json::json!(true);
    value["stream"] = serde_json::json!({
        "transport": "iroh",
        "peer": peer,
        "seq": seq,
        "received_at": received_at,
    });
    value
}

/// Synthetic line for a down stream (`offline`) or a failed status sample (`status_error`).
pub fn marker(
    kind: &str,
    machine_id: &str,
    peer: &str,
    error: &str,
    retry_in_s: Option<f64>,
) -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "taken_at": crate::now_f64(),
        "machine": {"id": machine_id, "online": kind != "offline"},
        "jobs": [],
        "stream": {
            "transport": "iroh",
            "peer": peer,
            "marker": kind,
            "error": error,
            "retry_in_s": retry_in_s,
        },
    })
}
