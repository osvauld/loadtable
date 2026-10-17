//! Daemon-to-daemon link of the task manager over iroh (LAN only).
//!
//! Moved here from `experiments/osvauld-foundation` (the `lan-peer` experiment) as a proper
//! component of the coordinator. What changed on the way:
//!
//! * **Admission is the allowlist, not an account attestation.** The experiment admitted a peer
//!   when it presented a courier attestation from the (synthetic, publicly derivable) test
//!   account AND appeared in a local allowlist. The attestation was membership only and pulled
//!   machine-local path dependencies on the osvauld parent. Here the TLS-proven endpoint id
//!   (`conn.remote_id()`, authenticated by the iroh handshake) must be listed in the daemon's
//!   peer config with a role that permits the ALPN; anything else is refused and logged before a
//!   challenge is issued. The `Hello` frame keeps its (now ignored) `attestation` field so the
//!   wire shape of `status/0`, `source/0` and `artifact/0` is unchanged.
//! * **Freshness** is unchanged: the receiver issues a random challenge per stream; requests and
//!   responses are signed by the endpoint keys over the challenge, both ids and body hashes.
//! * New ALPN [`jobs::JOBS_ALPN`] for forwarded job submission, attach (log/phase stream), cancel
//!   and lookup. Executing anything is the job host's business (the receiving daemon's normal
//!   submit/approve/worker path); this crate never starts a process except the fixed status
//!   source configured by its embedder.
//!
//! Linux only (no-follow `openat` materializer, flock).

pub mod artifact;
pub mod config;
pub mod jobs;
pub mod prune;
pub mod select;
pub mod server;
pub mod source;
pub mod status;
pub mod sync;

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iroh::endpoint::{
    Connection, NetReportConfig, PortmapperConfig, RecvStream, SendStream, VarInt, presets,
};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMode, SecretKey, Signature, Watcher};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use config::{Allowlist, Capability, PeerConfig, PeerEntry, Role};

/// Default small frame bound (handshakes, replies, log/blob headers).
pub const MAX_FRAME: usize = 64 * 1024;
/// How long a receiver waits for the requester to close after its final frame.
pub const CLOSE_GRACE: Duration = Duration::from_secs(10);
/// Connect + admission + signed request/response must finish within this.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Length-prefixed JSON frames shared by every ALPN. `Missing`/`Blob` are used by source and
/// artifact transfer; `Snapshot`/`StatusError` by the status stream; `Log`/`Phase`/`Done`/
/// `Heartbeat` by a jobs attach stream. A `Blob`, `Snapshot` or `Log` header is followed by
/// exactly `size` raw bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Frame {
    /// The attestation is accepted for wire compatibility with `lan-peer` and ignored.
    Hello {
        attestation: Option<serde_json::Value>,
    },
    Challenge {
        nonce: Vec<u8>,
    },
    Request {
        body: Vec<u8>,
        sig: Vec<u8>,
    },
    Response {
        body: Vec<u8>,
        sig: Vec<u8>,
    },
    Rejected {
        reason: String,
    },
    Missing {
        hashes: Vec<String>,
    },
    Blob {
        hash: String,
        size: u64,
    },
    Snapshot {
        seq: u64,
        size: u64,
    },
    StatusError {
        seq: u64,
        error: String,
    },
    Log {
        offset: u64,
        size: u64,
    },
    Phase {
        phase: String,
    },
    Done {
        phase: String,
        exit_code: Option<i32>,
        detail: String,
    },
    Heartbeat,
}

pub async fn write_frame(send: &mut SendStream, frame: &Frame) -> Result<(), String> {
    let bytes = serde_json::to_vec(frame).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&bytes);
    send.write_all(&out).await.map_err(|e| e.to_string())
}

pub async fn read_frame(recv: &mut RecvStream) -> Result<Frame, String> {
    read_frame_limited(recv, MAX_FRAME).await
}

/// [`read_frame`] with an explicit bound (a source request carries a whole manifest).
pub async fn read_frame_limited(recv: &mut RecvStream, max: usize) -> Result<Frame, String> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.map_err(|e| e.to_string())?;
    let len = u32::from_be_bytes(len) as usize;
    if len > max {
        return Err("frame too large".into());
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&buf).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------
// Signed session handshake (receiver challenge, no timestamps).

/// Request/response signing domains of one ALPN.
#[derive(Debug, Clone, Copy)]
pub struct Domains {
    pub request: &'static [u8],
    pub response: &'static [u8],
}

pub fn signed_message(
    domain: &[u8],
    challenge: &[u8],
    first: &PublicKey,
    second: &PublicKey,
    bodies: &[&[u8]],
) -> Vec<u8> {
    let mut m = domain.to_vec();
    m.extend_from_slice(challenge);
    m.extend_from_slice(first.as_bytes());
    m.extend_from_slice(second.as_bytes());
    for body in bodies {
        m.extend(Sha256::digest(body));
    }
    m
}

pub fn verify_sig(key: &PublicKey, message: &[u8], sig: &[u8]) -> bool {
    let Ok(sig) = <[u8; 64]>::try_from(sig) else {
        return false;
    };
    key.verify(message, &Signature::from_bytes(&sig)).is_ok()
}

/// An admitted, verified request on the server side of one stream.
pub struct ServerSession {
    pub send: SendStream,
    pub recv: RecvStream,
    pub remote: PublicKey,
    pub peer: PeerEntry,
    pub body: Vec<u8>,
    nonce: Vec<u8>,
    domains: Domains,
}

pub enum Accepted {
    Session(Box<ServerSession>),
    /// Refused before or after the challenge; the reason was sent and the stream finished.
    Refused(String),
}

impl ServerSession {
    /// Signs and sends the response body (bound to this session's challenge and request).
    pub async fn respond(&mut self, ep: &Endpoint, body: &[u8]) -> Result<(), String> {
        let sig = ep
            .secret_key()
            .sign(&signed_message(
                self.domains.response,
                &self.nonce,
                &ep.id(),
                &self.remote,
                &[&self.body, body],
            ))
            .to_bytes()
            .to_vec();
        write_frame(
            &mut self.send,
            &Frame::Response {
                body: body.to_vec(),
                sig,
            },
        )
        .await
    }

    pub async fn reject(&mut self, reason: &str) -> Result<(), String> {
        write_frame(
            &mut self.send,
            &Frame::Rejected {
                reason: reason.to_owned(),
            },
        )
        .await?;
        self.send.finish().map_err(|e| e.to_string())
    }
}

/// Server side of the handshake on the first bi-stream of `conn`: `Hello` → admission of the
/// TLS-proven remote id against `allow` for `need` (refused BEFORE any challenge) →
/// `Challenge` → signed `Request` (bounded by `max_request`).
pub async fn accept_session(
    ep: &Endpoint,
    conn: &Connection,
    allow: &Allowlist,
    need: Capability,
    domains: Domains,
    max_request: usize,
) -> Result<Accepted, String> {
    let remote = conn.remote_id();
    let (mut send, mut recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
    let Frame::Hello { .. } = read_frame(&mut recv).await? else {
        return Err("expected hello".into());
    };
    let peer = match allow.admit(&remote, need) {
        Ok(peer) => peer.clone(),
        Err(reason) => {
            write_frame(
                &mut send,
                &Frame::Rejected {
                    reason: reason.clone(),
                },
            )
            .await?;
            let _ = send.finish();
            return Ok(Accepted::Refused(reason));
        }
    };
    let mut nonce = vec![0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    write_frame(
        &mut send,
        &Frame::Challenge {
            nonce: nonce.clone(),
        },
    )
    .await?;
    let Frame::Request { body, sig } = read_frame_limited(&mut recv, max_request).await? else {
        return Err("expected request".into());
    };
    if !verify_sig(
        &remote,
        &signed_message(domains.request, &nonce, &remote, &ep.id(), &[&body]),
        &sig,
    ) {
        let reason = "BadRequest".to_string();
        write_frame(
            &mut send,
            &Frame::Rejected {
                reason: reason.clone(),
            },
        )
        .await?;
        let _ = send.finish();
        return Ok(Accepted::Refused(reason));
    }
    Ok(Accepted::Session(Box::new(ServerSession {
        send,
        recv,
        remote,
        peer,
        body,
        nonce,
        domains,
    })))
}

/// Why a client call failed. `Offline`: nothing reached the peer (connect/handshake failed);
/// `Refused`: the peer's policy refused the request (admission or a signed refusal);
/// `Lost`: the link broke after the request may have been delivered; `Protocol`: a malformed or
/// unverifiable answer; `Local`: a problem on this side (e.g. the source changed while sending).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    Offline(String),
    Refused(String),
    Lost(String),
    Protocol(String),
    Local(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Offline(e) => write!(f, "peer unreachable: {e}"),
            Self::Refused(e) => write!(f, "refused by peer: {e}"),
            Self::Lost(e) => write!(f, "link lost: {e}"),
            Self::Protocol(e) => write!(f, "protocol error: {e}"),
            Self::Local(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Client side of one session: connection plus the stream after the signed request was sent.
pub struct ClientSession {
    pub conn: Connection,
    pub send: SendStream,
    pub recv: RecvStream,
    pub challenge: Vec<u8>,
    pub receiver: PublicKey,
    request_body: Vec<u8>,
    requester: PublicKey,
    domains: Domains,
}

impl ClientSession {
    /// Reads the signed response and verifies it is bound to this session.
    pub async fn response(&mut self, max: usize) -> Result<Vec<u8>, ClientError> {
        match read_frame_limited(&mut self.recv, max).await {
            Ok(Frame::Response { body, sig }) => {
                if verify_sig(
                    &self.receiver,
                    &signed_message(
                        self.domains.response,
                        &self.challenge,
                        &self.receiver,
                        &self.requester,
                        &[&self.request_body, &body],
                    ),
                    &sig,
                ) {
                    Ok(body)
                } else {
                    Err(ClientError::Protocol(
                        "response not bound to this session".into(),
                    ))
                }
            }
            Ok(Frame::Rejected { reason }) => Err(ClientError::Refused(reason)),
            Ok(other) => Err(ClientError::Protocol(format!("unexpected {other:?}"))),
            Err(e) => Err(ClientError::Lost(e)),
        }
    }

    pub fn close(&self) {
        self.conn.close(VarInt::from_u32(0), b"done");
    }
}

/// Connects, says hello, answers the challenge with the signed `body`. Connect/handshake
/// failures are `Offline` (the request was not delivered), except an admission refusal.
pub async fn open_session(
    ep: &Endpoint,
    to: EndpointAddr,
    alpn: &[u8],
    domains: Domains,
    body: &[u8],
    timeout: Duration,
) -> Result<ClientSession, ClientError> {
    let receiver = to.id;
    let conn = tokio::time::timeout(timeout, ep.connect(to, alpn))
        .await
        .map_err(|_| ClientError::Offline(format!("connect timed out after {timeout:?}")))?
        .map_err(|e| ClientError::Offline(e.to_string()))?;
    let handshake = async {
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| ClientError::Offline(e.to_string()))?;
        write_frame(&mut send, &Frame::Hello { attestation: None })
            .await
            .map_err(ClientError::Offline)?;
        let challenge = match read_frame(&mut recv).await {
            Ok(Frame::Challenge { nonce }) => nonce,
            Ok(Frame::Rejected { reason }) => return Err(ClientError::Refused(reason)),
            Ok(other) => return Err(ClientError::Protocol(format!("unexpected {other:?}"))),
            Err(e) => return Err(ClientError::Offline(e)),
        };
        let sig = ep
            .secret_key()
            .sign(&signed_message(
                domains.request,
                &challenge,
                &ep.id(),
                &receiver,
                &[body],
            ))
            .to_bytes()
            .to_vec();
        // From here on the request may have been delivered: failures are `Lost`.
        write_frame(
            &mut send,
            &Frame::Request {
                body: body.to_vec(),
                sig,
            },
        )
        .await
        .map_err(ClientError::Lost)?;
        Ok((send, recv, challenge))
    };
    let (send, recv, challenge) = match tokio::time::timeout(timeout, handshake).await {
        Ok(Ok(parts)) => parts,
        Ok(Err(e)) => {
            conn.close(VarInt::from_u32(0), b"handshake failed");
            return Err(e);
        }
        Err(_) => {
            conn.close(VarInt::from_u32(0), b"handshake timeout");
            return Err(ClientError::Offline(format!(
                "handshake timed out after {timeout:?}"
            )));
        }
    };
    Ok(ClientSession {
        conn,
        send,
        recv,
        challenge,
        receiver,
        request_body: body.to_vec(),
        requester: ep.id(),
        domains,
    })
}

// ---------------------------------------------------------------------------------------------
// Endpoints: no presets::N0. Minimal = crypto provider only; then explicitly no relays, no
// address lookup, no portmapper, minimal net report, and exactly ONE caller-chosen socket.

/// Refuses unspecified (0.0.0.0 / ::) binds: the caller must name the interface address.
pub async fn lan_endpoint(
    secret: SecretKey,
    bind: SocketAddr,
    alpns: Vec<Vec<u8>>,
) -> Result<Endpoint, String> {
    if bind.ip().is_unspecified() {
        return Err(format!(
            "refusing unspecified bind {bind}; name the LAN interface address"
        ));
    }
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .clear_address_lookup()
        .portmapper_config(PortmapperConfig::Disabled)
        .net_report_config(NetReportConfig::minimal())
        .clear_ip_transports()
        .bind_addr(bind)
        .map_err(|e| e.to_string())?
        .secret_key(secret)
        .alpns(alpns)
        .bind()
        .await
        .map_err(|e| e.to_string())
}

/// Observable LAN-only configuration of a bound endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct LanReport {
    pub address_lookup_services: usize,
    pub relay_urls: usize,
    pub home_relays: usize,
    pub bound_sockets: Vec<SocketAddr>,
}

impl LanReport {
    pub fn of(ep: &Endpoint) -> Result<Self, String> {
        let addr = ep.addr();
        Ok(Self {
            address_lookup_services: ep.address_lookup().map_err(|e| e.to_string())?.len(),
            relay_urls: addr.relay_urls().count(),
            home_relays: ep.home_relay_status().get().len(),
            bound_sockets: ep.bound_sockets(),
        })
    }

    pub fn is_relay_and_lookup_free(&self) -> bool {
        self.address_lookup_services == 0 && self.relay_urls == 0 && self.home_relays == 0
    }
}

/// The dialable address of a bound endpoint (its single socket).
pub fn direct_addr(ep: &Endpoint) -> Result<EndpointAddr, String> {
    let socket = *ep
        .bound_sockets()
        .first()
        .ok_or("endpoint has no bound socket")?;
    Ok(EndpointAddr::new(ep.id()).with_ip_addr(socket))
}

pub fn peer_addr(id: PublicKey, addr: SocketAddr) -> EndpointAddr {
    EndpointAddr::new(id).with_ip_addr(addr)
}

// Re-exported so embedders use exactly this crate's versions.
pub use iroh;
pub use iroh_blobs;
