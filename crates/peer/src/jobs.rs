//! Forwarded jobs (ALPN [`JOBS_ALPN`] = `task-manager/jobs/0`).
//!
//! One signed request per stream, after the usual `Hello` -> allowlist admission (role
//! host/both) -> `Challenge` -> signed `Request`:
//!
//! * `submit` — run `argv` in the sender's synced checkout (`checkout`, `sub`) under the
//!   receiver's OWN job path: the embedder ([`JobHost`]) submits it to its ledger exactly like a
//!   local `tasks enqueue` (same validation, device denylist, Cargo `-j` cap), then approves
//!   and starts its worker only if this peer is configured `approve = "auto"`. Idempotent per
//!   `key`: the same key never creates a second job (no double run).
//! * `attach` — signed `Attached{phase}`, then a stream of `Log{offset,size}`+bytes (the job
//!   log from `offset`), `Phase{phase}` on changes and `Heartbeat` while idle, ending with
//!   `Done{phase, exit_code, detail}` once the job is terminal and its log drained. Re-attaching
//!   with the last received offset loses and repeats nothing.
//! * `cancel` — cancel a job this peer submitted (the receiver's normal cancel: the worker stops
//!   the job's own process group).
//! * `lookup` — the job id recorded for a `key`, if any (re-attach after a lost submit reply).
//! * `search_logs` — read-only, bounded search of the receiver's retained job logs. The query
//!   is opaque JSON (the embedder defines it); the reply is `LogMatches{size}` followed by
//!   exactly `size` raw bytes of JSON (at most [`MAX_SEARCH_REPLY`]). An older receiver
//!   refuses the unknown op as `BadRequest`.
//!
//! Only jobs submitted by the asking peer are visible to it through submit/attach/cancel/
//! lookup. `search_logs` searches every job's retained log on the receiver: a host peer may
//! already run any command there as the receiver's user, so this read widens nothing it could
//! not do; worker/observer peers (no `jobs/0` capability) cannot search.

use std::sync::Arc;
use std::time::{Duration, Instant};

use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr};
use serde::{Deserialize, Serialize};

use crate::{
    Accepted, Allowlist, CLOSE_GRACE, Capability, ClientError, Domains, Frame, MAX_FRAME,
    PeerEntry, accept_session, open_session, read_frame, write_frame,
};

pub const JOBS_ALPN: &[u8] = b"task-manager/jobs/0";
pub const JOBS_DOMAINS: Domains = Domains {
    request: b"task-manager/jobs/request/v0\0",
    response: b"task-manager/jobs/response/v0\0",
};
const LOG_CHUNK: usize = 64 * 1024;
/// Largest `search_logs` reply body (raw JSON after the signed `LogMatches` reply).
pub const MAX_SEARCH_REPLY: usize = 2 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteResource {
    Compile,
    Run,
    Device(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitRequest {
    /// Idempotency key chosen by the sender, `[A-Za-z0-9._:-]{1,128}`.
    pub key: String,
    pub origin_host: String,
    /// The sender's local job id (for the link record).
    pub origin_job_id: Option<i64>,
    /// `WHO@origin_host`.
    pub owner: String,
    /// Stable checkout name (the synced `checkout_id`).
    pub checkout: String,
    /// Working directory relative to the checkout ("" = its top).
    pub sub: String,
    pub argv: Vec<String>,
    pub resource: RemoteResource,
    /// The submitter deliberately allowed a broad test build (`--broad-test`); the receiver
    /// still applies its own policy. Additive: absent = false; older receivers ignore it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub broad_test: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum JobsRequest {
    Submit(SubmitRequest),
    Attach { remote_id: i64, offset: u64 },
    Cancel { remote_id: i64 },
    Lookup { key: String },
    SearchLogs { query: serde_json::Value },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum JobsReply {
    Submitted {
        remote_id: i64,
        existing: bool,
        approved: bool,
        phase: String,
    },
    Attached {
        phase: String,
    },
    Cancelled {
        phase: String,
    },
    Found {
        remote_id: Option<i64>,
    },
    /// `search_logs`: `size` raw bytes of JSON follow on the stream.
    LogMatches {
        size: u64,
    },
    /// The receiver's policy refused the request (validation, denylist, unknown job...).
    Refused {
        reason: String,
    },
}

pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobState {
    pub phase: String,
    pub terminal: bool,
    pub exit_code: Option<i32>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submitted {
    pub remote_id: i64,
    pub existing: bool,
    pub approved: bool,
    pub phase: String,
}

/// The receiving daemon's job path. Blocking calls (run on the blocking pool). `Err` is a
/// refusal reason sent back to the peer.
pub trait JobHost: Send + Sync + 'static {
    fn submit(&self, peer: &PeerEntry, request: &SubmitRequest) -> Result<Submitted, String>;
    fn state(&self, peer: &PeerEntry, id: i64) -> Result<JobState, String>;
    fn read_log(
        &self,
        peer: &PeerEntry,
        id: i64,
        offset: u64,
        max: usize,
    ) -> Result<Vec<u8>, String>;
    fn cancel(&self, peer: &PeerEntry, id: i64) -> Result<JobState, String>;
    fn lookup(&self, peer: &PeerEntry, key: &str) -> Result<Option<i64>, String>;
    /// Read-only search of the receiver's job logs (`search_logs`); the default refuses.
    fn search_logs(
        &self,
        _peer: &PeerEntry,
        _query: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Err("search_logs is not supported by this receiver".into())
    }
}

/// Server timing knobs (heartbeats keep a quiet attach stream distinguishable from a dead one).
#[derive(Debug, Clone, Copy)]
pub struct JobsTiming {
    pub heartbeat: Duration,
}

impl Default for JobsTiming {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(5),
        }
    }
}

#[derive(Debug)]
pub enum JobsOutcome {
    Handled {
        peer: String,
        what: String,
    },
    Rejected {
        peer: Option<String>,
        reason: String,
    },
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("job host task failed: {e}"))?
}

pub async fn handle_jobs(
    ep: &Endpoint,
    conn: Connection,
    allow: &Allowlist,
    host: Arc<dyn JobHost>,
    timing: JobsTiming,
) -> Result<JobsOutcome, String> {
    let mut s =
        match accept_session(ep, &conn, allow, Capability::Jobs, JOBS_DOMAINS, MAX_FRAME).await? {
            Accepted::Refused(reason) => {
                let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
                return Ok(JobsOutcome::Rejected { peer: None, reason });
            }
            Accepted::Session(s) => s,
        };
    let peer = s.peer.clone();
    let request: JobsRequest = match serde_json::from_slice(&s.body) {
        Ok(r) => r,
        Err(e) => {
            let reason = format!("BadRequest({e})");
            let _ = s.reject(&reason).await;
            return Ok(JobsOutcome::Rejected {
                peer: Some(peer.name),
                reason,
            });
        }
    };
    let what = match &request {
        JobsRequest::Submit(r) => format!("submit key={} argv0={:?}", r.key, r.argv.first()),
        JobsRequest::Attach { remote_id, offset } => format!("attach #{remote_id} @{offset}"),
        JobsRequest::Cancel { remote_id } => format!("cancel #{remote_id}"),
        JobsRequest::Lookup { key } => format!("lookup {key}"),
        JobsRequest::SearchLogs { .. } => "search_logs".to_string(),
    };
    let reply = |reply: JobsReply| serde_json::to_vec(&reply).map_err(|e| e.to_string());
    let (h, p) = (Arc::clone(&host), peer.clone());
    match request {
        JobsRequest::Submit(r) => {
            let answer = if !valid_key(&r.key) {
                JobsReply::Refused {
                    reason: "InvalidKey".into(),
                }
            } else {
                match blocking(move || h.submit(&p, &r)).await {
                    Ok(x) => JobsReply::Submitted {
                        remote_id: x.remote_id,
                        existing: x.existing,
                        approved: x.approved,
                        phase: x.phase,
                    },
                    Err(reason) => JobsReply::Refused { reason },
                }
            };
            s.respond(ep, &reply(answer)?).await?;
        }
        JobsRequest::Cancel { remote_id } => {
            let answer = match blocking(move || h.cancel(&p, remote_id)).await {
                Ok(state) => JobsReply::Cancelled { phase: state.phase },
                Err(reason) => JobsReply::Refused { reason },
            };
            s.respond(ep, &reply(answer)?).await?;
        }
        JobsRequest::Lookup { key } => {
            let answer = match blocking(move || h.lookup(&p, &key)).await {
                Ok(remote_id) => JobsReply::Found { remote_id },
                Err(reason) => JobsReply::Refused { reason },
            };
            s.respond(ep, &reply(answer)?).await?;
        }
        JobsRequest::SearchLogs { query } => {
            let result = blocking(move || {
                let value = h.search_logs(&p, &query)?;
                serde_json::to_vec(&value).map_err(|e| e.to_string())
            })
            .await;
            match result {
                Ok(bytes) if bytes.len() <= MAX_SEARCH_REPLY => {
                    s.respond(
                        ep,
                        &reply(JobsReply::LogMatches {
                            size: bytes.len() as u64,
                        })?,
                    )
                    .await?;
                    s.send.write_all(&bytes).await.map_err(|e| e.to_string())?;
                }
                Ok(bytes) => {
                    let reason = format!(
                        "search reply of {} bytes exceeds {MAX_SEARCH_REPLY}; lower max_output_bytes",
                        bytes.len()
                    );
                    s.respond(ep, &reply(JobsReply::Refused { reason })?)
                        .await?;
                }
                Err(reason) => {
                    s.respond(ep, &reply(JobsReply::Refused { reason })?)
                        .await?;
                }
            }
        }
        JobsRequest::Attach { remote_id, offset } => {
            let first = blocking(move || h.state(&p, remote_id)).await;
            let mut state = match first {
                Ok(state) => state,
                Err(reason) => {
                    s.respond(ep, &reply(JobsReply::Refused { reason })?)
                        .await?;
                    let _ = s.send.finish();
                    let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
                    return Ok(JobsOutcome::Handled {
                        peer: peer.name,
                        what,
                    });
                }
            };
            s.respond(
                ep,
                &reply(JobsReply::Attached {
                    phase: state.phase.clone(),
                })?,
            )
            .await?;
            let mut offset = offset;
            let mut last_frame = Instant::now();
            let stream = async {
                loop {
                    // Read the state BEFORE the log: once terminal, the log is complete, so the
                    // drain below cannot miss a final line.
                    let (h, p) = (Arc::clone(&host), peer.clone());
                    let now = blocking(move || h.state(&p, remote_id)).await?;
                    if now.phase != state.phase {
                        write_frame(
                            &mut s.send,
                            &Frame::Phase {
                                phase: now.phase.clone(),
                            },
                        )
                        .await?;
                        last_frame = Instant::now();
                    }
                    state = now;
                    loop {
                        let (h, p) = (Arc::clone(&host), peer.clone());
                        let chunk =
                            blocking(move || h.read_log(&p, remote_id, offset, LOG_CHUNK)).await?;
                        if chunk.is_empty() {
                            break;
                        }
                        write_frame(
                            &mut s.send,
                            &Frame::Log {
                                offset,
                                size: chunk.len() as u64,
                            },
                        )
                        .await?;
                        s.send.write_all(&chunk).await.map_err(|e| e.to_string())?;
                        offset += chunk.len() as u64;
                        last_frame = Instant::now();
                        if chunk.len() < LOG_CHUNK && !state.terminal {
                            break;
                        }
                    }
                    if state.terminal {
                        write_frame(
                            &mut s.send,
                            &Frame::Done {
                                phase: state.phase.clone(),
                                exit_code: state.exit_code,
                                detail: state.detail.clone(),
                            },
                        )
                        .await?;
                        let _ = s.send.finish();
                        return Ok::<(), String>(());
                    }
                    if last_frame.elapsed() >= timing.heartbeat {
                        write_frame(&mut s.send, &Frame::Heartbeat).await?;
                        last_frame = Instant::now();
                    }
                    tokio::time::sleep(POLL).await;
                }
            };
            tokio::select! {
                result = stream => result?,
                _ = conn.closed() => return Ok(JobsOutcome::Handled { peer: peer.name, what: format!("{what} (peer left)") }),
            }
        }
    }
    let _ = s.send.finish();
    let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
    Ok(JobsOutcome::Handled {
        peer: peer.name,
        what,
    })
}

/// One request/reply exchange (submit, cancel, lookup).
pub async fn call(
    ep: &Endpoint,
    to: EndpointAddr,
    request: &JobsRequest,
    timeout: Duration,
) -> Result<JobsReply, ClientError> {
    let body = serde_json::to_vec(request).map_err(|e| ClientError::Local(e.to_string()))?;
    let mut s = open_session(ep, to, JOBS_ALPN, JOBS_DOMAINS, &body, timeout).await?;
    let reply = match tokio::time::timeout(timeout, s.response(MAX_FRAME)).await {
        Ok(r) => r,
        Err(_) => Err(ClientError::Lost(format!("no reply within {timeout:?}"))),
    };
    s.close();
    let reply: JobsReply = serde_json::from_slice(&reply?)
        .map_err(|e| ClientError::Protocol(format!("bad reply: {e}")))?;
    Ok(reply)
}

/// `search_logs`: one signed request, the signed `LogMatches{size}` reply, then `size` raw bytes
/// of JSON. A refusal (including an older receiver's `BadRequest`) is `ClientError::Refused`.
pub async fn search_logs(
    ep: &Endpoint,
    to: EndpointAddr,
    query: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, ClientError> {
    let body = serde_json::to_vec(&JobsRequest::SearchLogs { query })
        .map_err(|e| ClientError::Local(e.to_string()))?;
    let mut s = open_session(ep, to, JOBS_ALPN, JOBS_DOMAINS, &body, timeout).await?;
    let result = async {
        let reply = match tokio::time::timeout(timeout, s.response(MAX_FRAME)).await {
            Ok(r) => r?,
            Err(_) => return Err(ClientError::Lost(format!("no reply within {timeout:?}"))),
        };
        let size = match serde_json::from_slice::<JobsReply>(&reply) {
            Ok(JobsReply::LogMatches { size }) => size,
            Ok(JobsReply::Refused { reason }) => return Err(ClientError::Refused(reason)),
            Ok(other) => return Err(ClientError::Protocol(format!("unexpected {other:?}"))),
            Err(e) => return Err(ClientError::Protocol(format!("bad reply: {e}"))),
        };
        if size as usize > MAX_SEARCH_REPLY {
            return Err(ClientError::Protocol(format!(
                "search reply of {size} bytes exceeds {MAX_SEARCH_REPLY}"
            )));
        }
        let mut buf = vec![0u8; size as usize];
        tokio::time::timeout(timeout, s.recv.read_exact(&mut buf))
            .await
            .map_err(|_| ClientError::Lost("stalled inside the search reply".into()))?
            .map_err(|e| ClientError::Lost(e.to_string()))?;
        serde_json::from_slice(&buf)
            .map_err(|e| ClientError::Protocol(format!("bad search reply: {e}")))
    }
    .await;
    s.close();
    result
}

#[derive(Debug)]
pub enum AttachEvent<'a> {
    Attached(&'a str),
    Log { offset: u64, bytes: &'a [u8] },
    Phase(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachDone {
    pub phase: String,
    pub exit_code: Option<i32>,
    pub detail: String,
}

/// Attaches to `remote_id` from log `offset` until `Done`. `stall`: no frame for this long
/// (heartbeats included) means the link is lost. `on_event` errors abort as `Local`.
pub async fn attach(
    ep: &Endpoint,
    to: EndpointAddr,
    remote_id: i64,
    offset: u64,
    timeout: Duration,
    stall: Duration,
    mut on_event: impl FnMut(AttachEvent<'_>) -> Result<(), String>,
) -> Result<AttachDone, ClientError> {
    let body = serde_json::to_vec(&JobsRequest::Attach { remote_id, offset })
        .map_err(|e| ClientError::Local(e.to_string()))?;
    let mut s = open_session(ep, to, JOBS_ALPN, JOBS_DOMAINS, &body, timeout).await?;
    let result = async {
        let reply = match tokio::time::timeout(timeout, s.response(MAX_FRAME)).await {
            Ok(r) => r?,
            Err(_) => return Err(ClientError::Lost(format!("no reply within {timeout:?}"))),
        };
        match serde_json::from_slice::<JobsReply>(&reply) {
            Ok(JobsReply::Attached { phase }) => {
                on_event(AttachEvent::Attached(&phase)).map_err(ClientError::Local)?
            }
            Ok(JobsReply::Refused { reason }) => return Err(ClientError::Refused(reason)),
            Ok(other) => return Err(ClientError::Protocol(format!("unexpected {other:?}"))),
            Err(e) => return Err(ClientError::Protocol(format!("bad reply: {e}"))),
        }
        let mut expected = offset;
        loop {
            let frame = match tokio::time::timeout(stall, read_frame(&mut s.recv)).await {
                Err(_) => {
                    return Err(ClientError::Lost(format!(
                        "no frame for {stall:?} (heartbeats stopped)"
                    )));
                }
                Ok(Err(e)) => return Err(ClientError::Lost(e)),
                Ok(Ok(frame)) => frame,
            };
            match frame {
                Frame::Log { offset, size } => {
                    if offset != expected || size > (4 * LOG_CHUNK) as u64 {
                        return Err(ClientError::Protocol(format!(
                            "log chunk at {offset} (+{size}), expected {expected}"
                        )));
                    }
                    let mut buf = vec![0u8; size as usize];
                    tokio::time::timeout(stall, s.recv.read_exact(&mut buf))
                        .await
                        .map_err(|_| ClientError::Lost("stalled inside a log chunk".into()))?
                        .map_err(|e| ClientError::Lost(e.to_string()))?;
                    on_event(AttachEvent::Log {
                        offset,
                        bytes: &buf,
                    })
                    .map_err(ClientError::Local)?;
                    expected += size;
                }
                Frame::Phase { phase } => {
                    on_event(AttachEvent::Phase(&phase)).map_err(ClientError::Local)?
                }
                Frame::Heartbeat => {}
                Frame::Done {
                    phase,
                    exit_code,
                    detail,
                } => {
                    return Ok(AttachDone {
                        phase,
                        exit_code,
                        detail,
                    });
                }
                Frame::Rejected { reason } => return Err(ClientError::Refused(reason)),
                other => return Err(ClientError::Protocol(format!("unexpected {other:?}"))),
            }
        }
    }
    .await;
    s.close();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_reply_wire_shapes_are_stable() {
        let r = JobsRequest::Cancel { remote_id: 7 };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"op":"cancel","remote_id":7}"#
        );
        let submit = JobsRequest::Submit(SubmitRequest {
            key: "k".into(),
            origin_host: "h".into(),
            origin_job_id: Some(1),
            owner: "a@h".into(),
            checkout: "0123456789abcdef".into(),
            sub: String::new(),
            argv: vec!["true".into()],
            resource: RemoteResource::Device("SERIAL".into()),
            broad_test: false,
        });
        let text = serde_json::to_string(&submit).unwrap();
        assert!(text.starts_with(r#"{"op":"submit","key":"k""#), "{text}");
        assert!(text.contains(r#""resource":{"device":"SERIAL"}"#), "{text}");
        let back: JobsRequest = serde_json::from_str(&text).unwrap();
        assert!(matches!(back, JobsRequest::Submit(s) if s.argv == ["true"]));
        assert_eq!(
            serde_json::to_string(&JobsReply::Found { remote_id: None }).unwrap(),
            r#"{"reply":"found","remote_id":null}"#
        );
        let search = JobsRequest::SearchLogs {
            query: serde_json::json!({"pattern": "x"}),
        };
        assert_eq!(
            serde_json::to_string(&search).unwrap(),
            r#"{"op":"search_logs","query":{"pattern":"x"}}"#
        );
        assert_eq!(
            serde_json::to_string(&JobsReply::LogMatches { size: 3 }).unwrap(),
            r#"{"reply":"log_matches","size":3}"#
        );
    }

    #[test]
    fn keys_are_bounded_and_plain() {
        assert!(valid_key("kurukkan:12:0123abcd"));
        assert!(!valid_key(""));
        assert!(!valid_key("a/b"));
        assert!(!valid_key(&"k".repeat(129)));
    }
}
