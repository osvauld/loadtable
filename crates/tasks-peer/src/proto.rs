//! The wire protocol: versioned, size-limited messages (REQUIREMENTS 7.18, 7.19).
//!
//! Every frame is a JSON envelope `{"v": N, "body": {"type": ...}}`. A newer or
//! older version, an unknown message or an oversized frame is answered with a
//! named [`Refusal`]; decoding never panics and never waits for more input.

use crate::ids::{CheckoutId, Hash, RemoteJobId, SubmitKey, hexbytes};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use tasks_core::{JobSpec, Machine, Phase, SlotKind};
use tasks_runner::{End, Outcome};
use tasks_timing::history::{Cache, Outcome as TimingOutcome, Sample, SampleKey};

/// The version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;
/// The oldest version this build still understands.
pub const MIN_PROTOCOL_VERSION: u32 = 1;
/// ALPN for the iroh transport.
pub const ALPN: &[u8] = b"tasks-peer/1";

/// Largest data payload of one log, blob or fetch message (before hex).
pub const MAX_CHUNK_BYTES: usize = 128 * 1024;
/// Limit for small control messages.
pub const MAX_CONTROL_BYTES: usize = 64 * 1024;
/// Limit for one manifest page.
pub const MAX_MANIFEST_PAGE_BYTES: usize = 256 * 1024;
/// Limit for a message carrying one data chunk (hex doubles it, plus framing).
pub const MAX_DATA_FRAME_BYTES: usize = MAX_CHUNK_BYTES * 2 + 4 * 1024;
/// Absolute ceiling for any frame, checked before parsing.
pub const MAX_FRAME_BYTES: usize = MAX_DATA_FRAME_BYTES;
/// Most hashes in one have-request.
pub const MAX_HASHES_PER_REQUEST: usize = 512;
/// Most entries in one manifest page.
pub const MAX_ENTRIES_PER_PAGE: usize = 1024;
/// Most paths in one fetch listing request.
pub const MAX_FETCH_PATHS: usize = 256;
/// Longest relative path accepted on the wire.
pub const MAX_PATH_BYTES: usize = 1024;

/// A named refusal. It is both a message body and an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Refusal {
    /// The other side speaks a version this build does not.
    #[error("unsupported protocol version {theirs}; this peer speaks {min}..={max}")]
    UnsupportedVersion {
        /// The version in the frame.
        theirs: u32,
        /// Oldest version understood.
        min: u32,
        /// Newest version understood.
        max: u32,
    },
    /// The message type is not known to this build.
    #[error("unknown message type: {name}")]
    UnknownMessage {
        /// The type tag, truncated.
        name: String,
    },
    /// The frame is not a valid message.
    #[error("malformed message: {detail}")]
    Malformed {
        /// What was wrong.
        detail: String,
    },
    /// The frame is over its size limit.
    #[error("message of {size} bytes is over the {limit} byte limit")]
    TooLarge {
        /// Size seen.
        size: usize,
        /// Limit for this message.
        limit: usize,
    },
    /// The peer is not on the allowlist.
    #[error("peer {peer} is not on this daemon's allowlist")]
    NotAllowed {
        /// The refused identity.
        peer: String,
    },
    /// No such job here.
    #[error("{remote} is unknown to this peer")]
    UnknownJob {
        /// The id asked about.
        remote: RemoteJobId,
    },
    /// The checkout has not been synced.
    #[error("checkout {checkout} has not been synced to this peer")]
    NotSynced {
        /// The checkout.
        checkout: CheckoutId,
    },
    /// The receiver's own policy refuses the job.
    #[error("refused by the peer's policy: {reason}")]
    PolicyDenied {
        /// Why.
        reason: String,
    },
    /// A path is unsafe or outside the checkout.
    #[error("path {path:?} refused: {reason}")]
    BadPath {
        /// The path.
        path: String,
        /// Why.
        reason: String,
    },
    /// A chunk did not start where the receiver expects it.
    #[error("chunk out of order; the receiver expects offset {expected}")]
    OutOfOrder {
        /// The offset to resume from.
        expected: u64,
    },
    /// The manifest names blobs the receiver does not have.
    #[error("{} blobs are still missing", missing.len())]
    Incomplete {
        /// Hashes still needed (at most a page of them).
        missing: Vec<Hash>,
    },
    /// Received content does not match its hash.
    #[error("content does not match its hash")]
    HashMismatch,
    /// A sync or fetch limit was exceeded.
    #[error("limit exceeded: {detail}")]
    LimitExceeded {
        /// What.
        detail: String,
    },
    /// A job from this checkout is still live here, so its copy cannot be replaced yet. Retry
    /// when that job has ended (the origin's checkout lock normally prevents this).
    #[error("checkout {checkout} has a live job on this peer; its copy cannot be re-synced now")]
    Busy {
        /// The checkout.
        checkout: CheckoutId,
    },
    /// The peer failed internally.
    #[error("peer error: {detail}")]
    Internal {
        /// What.
        detail: String,
    },
    /// A refusal code from a newer peer that this build does not know.
    #[serde(other)]
    #[error("refused for a reason this version does not know")]
    Unrecognised,
}

/// A capability a peer advertises in its hello. Strings, so a newer peer's
/// extra capabilities are carried, not rejected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Capability(pub String);

impl Capability {
    /// Status stream.
    pub const STATUS: &'static str = "status";
    /// Forwarded jobs.
    pub const JOBS: &'static str = "jobs";
    /// Checkout sync.
    pub const SYNC: &'static str = "sync";
    /// Output fetch.
    pub const FETCH: &'static str = "fetch";
    /// Log resume from an offset.
    pub const RESUME: &'static str = "resume";
    /// The capabilities of this build.
    pub fn all() -> BTreeSet<Capability> {
        [
            Self::STATUS,
            Self::JOBS,
            Self::SYNC,
            Self::FETCH,
            Self::RESUME,
        ]
        .iter()
        .map(|s| Capability((*s).to_string()))
        .collect()
    }
}

/// Version and capability exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The sender's machine name.
    pub name: String,
    /// Oldest version the sender understands.
    pub min_version: u32,
    /// Newest version the sender understands.
    pub max_version: u32,
    /// What the sender offers.
    pub capabilities: BTreeSet<Capability>,
}

impl Hello {
    /// The hello of this build.
    pub fn local(name: &str) -> Hello {
        Hello {
            name: name.to_string(),
            min_version: MIN_PROTOCOL_VERSION,
            max_version: PROTOCOL_VERSION,
            capabilities: Capability::all(),
        }
    }

    /// The version both sides speak, or a named refusal.
    pub fn negotiate(&self, theirs: &Hello) -> Result<u32, Refusal> {
        let v = self.max_version.min(theirs.max_version);
        if v < self.min_version || v < theirs.min_version {
            return Err(Refusal::UnsupportedVersion {
                theirs: theirs.max_version,
                min: self.min_version,
                max: self.max_version,
            });
        }
        Ok(v)
    }
}

/// Slot usage for one slot kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotStatus {
    /// Which slots.
    pub kind: SlotKind,
    /// How many exist.
    pub total: u32,
    /// How many are held.
    pub used: u32,
}

/// Queue depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QueueSummary {
    /// Jobs waiting.
    pub waiting: u32,
    /// Jobs running.
    pub running: u32,
}

/// Pressure summary of a peer.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PressureSummary {
    /// CPU pressure percent, when known.
    pub cpu_pct: Option<u8>,
    /// Memory pressure percent, when known.
    pub memory_pct: Option<u8>,
    /// I/O pressure percent, when known.
    pub io_pct: Option<u8>,
    /// One line for humans.
    pub summary: String,
}

/// What a peer reports about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerStatus {
    /// Machine facts.
    pub machine: Machine,
    /// Slot usage.
    pub slots: Vec<SlotStatus>,
    /// Queue depth.
    pub queue: QueueSummary,
    /// Pressure.
    pub pressure: PressureSummary,
}

/// Run a command on the receiver, in its synced copy of a checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitJob {
    /// Idempotency key, scoped to the submitting peer (R-152).
    pub key: SubmitKey,
    /// Which synced copy to run in.
    pub checkout: CheckoutId,
    /// Working directory relative to the checkout root (`""` for the root).
    pub rel_cwd: String,
    /// The job. Its `cwd`, `placement` and `after` are rewritten by the receiver.
    pub spec: JobSpec,
}

/// The reply to a submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submitted {
    /// The receiver's id.
    pub remote: RemoteJobId,
    /// True when the key had already been submitted (nothing new was started).
    pub duplicate: bool,
}

/// How a run ended, in a form that crosses machines (no lease).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireOutcome {
    /// How it ended.
    pub end: WireEnd,
    /// Exit code, when known.
    pub exit_code: Option<i32>,
    /// Signal we sent that ended it.
    pub signal: Option<i32>,
    /// Wall time in ms.
    pub wall_ms: u64,
    /// Peak-ish resident memory seen, bytes.
    pub rss_bytes: Option<u64>,
    /// CPU seconds (user + system).
    pub cpu_s: Option<f64>,
    /// One line from the command's own status (R-086).
    pub description: String,
}

/// Mirror of [`End`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireEnd {
    /// Exited on its own.
    Exited,
    /// Cancelled.
    Cancelled,
    /// Cancelled because the client left.
    ClientLost,
    /// Wall-clock timeout.
    WallTimeout,
    /// Output-idle timeout.
    IdleTimeout,
    /// Result unknown.
    Lost,
}

impl From<End> for WireEnd {
    fn from(e: End) -> WireEnd {
        match e {
            End::Exited => WireEnd::Exited,
            End::Cancelled => WireEnd::Cancelled,
            End::ClientLost => WireEnd::ClientLost,
            End::WallTimeout => WireEnd::WallTimeout,
            End::IdleTimeout => WireEnd::IdleTimeout,
            End::Lost => WireEnd::Lost,
        }
    }
}

impl From<&Outcome> for WireOutcome {
    fn from(o: &Outcome) -> WireOutcome {
        WireOutcome {
            end: o.end.into(),
            exit_code: o.exit_code,
            signal: o.signal,
            wall_ms: u64::try_from(o.wall.as_millis()).unwrap_or(u64::MAX),
            rss_bytes: o.usage.map(|u| u.rss_bytes),
            cpu_s: o.usage.map(|u| u.cpu_user_s + u.cpu_sys_s),
            description: o.describe(),
        }
    }
}

impl WireOutcome {
    /// The terminal phase; same table as `Outcome::phase`. An unknown result is
    /// `Interrupted`, never a pass.
    pub fn phase(&self) -> Phase {
        match self.end {
            WireEnd::Cancelled | WireEnd::ClientLost => Phase::Cancelled,
            WireEnd::Lost => Phase::Interrupted,
            WireEnd::WallTimeout | WireEnd::IdleTimeout => Phase::TimedOut {
                exit: self.exit_code,
            },
            WireEnd::Exited => match self.exit_code {
                Some(0) => Phase::Passed,
                code => Phase::Failed { exit: code },
            },
        }
    }
}

/// A timing sample measured on the peer, shipped back with the result so the
/// origin's history learns what the peer's runs cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireSample {
    /// The recipe key display string.
    pub recipe: String,
    /// Target label; `None` for the whole job.
    pub target: Option<String>,
    /// Machine the run happened on (the peer's name).
    pub machine: String,
    /// Seconds.
    pub secs: f64,
    /// Build warmth: `warm`, `cold` or `unknown`.
    pub cache: String,
    /// `passed`, `failed` or `cancelled`.
    pub outcome: String,
    /// When it finished, ms.
    pub at_ms: u64,
}

impl WireSample {
    /// Wraps a stored sample.
    pub fn from_sample(key: &SampleKey, s: &Sample) -> WireSample {
        WireSample {
            recipe: key.recipe.clone(),
            target: key.target.clone(),
            machine: key.machine.clone(),
            secs: s.secs,
            cache: match s.cache {
                Cache::Warm => "warm",
                Cache::Cold => "cold",
                Cache::Unknown => "unknown",
            }
            .to_string(),
            outcome: match s.outcome {
                TimingOutcome::Passed => "passed",
                TimingOutcome::Failed => "failed",
                TimingOutcome::Cancelled => "cancelled",
            }
            .to_string(),
            at_ms: s.at_ms,
        }
    }

    /// Back to a key and a sample; `None` when a field is not understood
    /// (a newer peer's value), so the origin drops the sample, not the result.
    pub fn to_sample(&self) -> Option<(SampleKey, Sample)> {
        if !self.secs.is_finite() || self.secs < 0.0 {
            return None;
        }
        let cache = match self.cache.as_str() {
            "warm" => Cache::Warm,
            "cold" => Cache::Cold,
            "unknown" => Cache::Unknown,
            _ => return None,
        };
        let outcome = match self.outcome.as_str() {
            "passed" => TimingOutcome::Passed,
            "failed" => TimingOutcome::Failed,
            "cancelled" => TimingOutcome::Cancelled,
            _ => return None,
        };
        Some((
            SampleKey {
                recipe: self.recipe.clone(),
                target: self.target.clone(),
                machine: self.machine.clone(),
            },
            Sample {
                secs: self.secs,
                cache,
                outcome,
                at_ms: self.at_ms,
            },
        ))
    }
}

/// An event in a forwarded job's life.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum JobEvent {
    /// The phase changed (waiting reasons name the blocker and slot holder, R-3).
    Phase {
        /// The new phase.
        phase: Phase,
        /// Human detail such as `compile slot held by #154`.
        detail: String,
    },
    /// Log bytes at an absolute offset in the remote log.
    Log {
        /// Offset of the first byte.
        offset: u64,
        /// The bytes.
        #[serde(with = "hexbytes")]
        data: Vec<u8>,
    },
    /// The job is over. All log bytes up to `log_len` have been sent before this.
    Finished {
        /// The outcome.
        outcome: WireOutcome,
        /// Timing samples measured on the peer.
        samples: Vec<WireSample>,
        /// Total length of the remote log.
        log_len: u64,
    },
    /// Liveness only.
    Heartbeat,
}

/// Answer to a cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelState {
    /// The cancel was applied.
    Requested,
    /// The job had already ended.
    AlreadyFinished,
}

/// One file of a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to the checkout root, `/` separated.
    pub path: String,
    /// Content hash.
    pub hash: Hash,
    /// Size in bytes.
    pub size: u64,
    /// Executable bit.
    pub exec: bool,
}

/// Result of applying a sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SyncSummary {
    /// Files in the new manifest.
    pub files: u64,
    /// Files written or replaced.
    pub written: u64,
    /// Files left untouched (same content).
    pub unchanged: u64,
    /// Files removed.
    pub removed: u64,
}

/// One answer of a fetch listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FetchEntry {
    /// The file can be fetched by chunks.
    Found {
        /// Requested path.
        path: String,
        /// Content hash.
        hash: Hash,
        /// Size.
        size: u64,
        /// Executable bit.
        exec: bool,
    },
    /// No such file.
    Missing {
        /// Requested path.
        path: String,
    },
    /// Over the per-file limit; reported, the rest are still fetched.
    TooLarge {
        /// Requested path.
        path: String,
        /// Size.
        size: u64,
        /// The limit.
        limit: u64,
    },
    /// Refused (outside the checkout, symlink, not a file).
    Refused {
        /// Requested path.
        path: String,
        /// Why.
        reason: String,
    },
}

/// Every message. Requests and replies share the enum; a reply to any request
/// may be [`Body::Refuse`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Body {
    /// Open: versions and capabilities.
    Hello(Hello),
    /// Reply to hello.
    HelloAck(Hello),
    /// Ask for status.
    StatusRequest,
    /// Status reply.
    Status(PeerStatus),
    /// Submit a forwarded job.
    Submit(SubmitJob),
    /// Submission accepted.
    Submitted(Submitted),
    /// Attach to a job's events from a log offset.
    Subscribe {
        /// The job.
        remote: RemoteJobId,
        /// First log byte wanted.
        from_offset: u64,
    },
    /// One job event (streamed).
    Event(JobEvent),
    /// Cancel a job.
    Cancel {
        /// The job.
        remote: RemoteJobId,
    },
    /// Cancel reply.
    CancelAck {
        /// The job.
        remote: RemoteJobId,
        /// What happened.
        state: CancelState,
    },
    /// Which of these blobs do you have?
    SyncHaveRequest {
        /// The checkout.
        checkout: CheckoutId,
        /// At most [`MAX_HASHES_PER_REQUEST`] hashes.
        hashes: Vec<Hash>,
    },
    /// The ones the receiver already has.
    SyncHave {
        /// Present hashes.
        present: Vec<Hash>,
    },
    /// A chunk of a blob.
    SyncBlob {
        /// The checkout.
        checkout: CheckoutId,
        /// The blob hash.
        hash: Hash,
        /// Offset of this chunk.
        offset: u64,
        /// Total blob size.
        total: u64,
        /// The bytes.
        #[serde(with = "hexbytes")]
        data: Vec<u8>,
    },
    /// Bytes of the blob received so far (resume point).
    SyncBlobAck {
        /// Received length.
        received: u64,
    },
    /// One page of the full manifest; the last page commits the sync.
    SyncManifestPage {
        /// The checkout.
        checkout: CheckoutId,
        /// Sender-chosen session id.
        session: u64,
        /// Page number from 0.
        index: u32,
        /// True on the final page.
        last: bool,
        /// Entries.
        entries: Vec<FileEntry>,
    },
    /// Generic acknowledgement.
    Ok,
    /// Sync applied.
    SyncApplied(SyncSummary),
    /// List outputs to fetch.
    FetchList {
        /// The checkout.
        checkout: CheckoutId,
        /// Relative paths.
        paths: Vec<String>,
    },
    /// Fetch listing.
    FetchListing {
        /// One entry per requested path.
        entries: Vec<FetchEntry>,
    },
    /// Read a chunk of an output, content-addressed.
    FetchChunk {
        /// The checkout.
        checkout: CheckoutId,
        /// Relative path.
        path: String,
        /// Expected hash; the peer refuses if the file changed.
        hash: Hash,
        /// Offset.
        offset: u64,
        /// Bytes wanted, at most [`MAX_CHUNK_BYTES`].
        len: u32,
    },
    /// A chunk of an output.
    FetchData {
        /// Offset of the first byte.
        offset: u64,
        /// The bytes.
        #[serde(with = "hexbytes")]
        data: Vec<u8>,
        /// True when this reaches the end of the file.
        eof: bool,
    },
    /// A named refusal.
    Refuse(Refusal),
}

impl Body {
    /// The size limit of this message.
    pub fn limit(&self) -> usize {
        match self {
            Body::Event(JobEvent::Log { .. }) | Body::SyncBlob { .. } | Body::FetchData { .. } => {
                MAX_DATA_FRAME_BYTES
            }
            Body::SyncManifestPage { .. } => MAX_MANIFEST_PAGE_BYTES,
            _ => MAX_CONTROL_BYTES,
        }
    }
}

#[derive(Serialize)]
struct EnvelopeOut<'a> {
    v: u32,
    body: &'a Body,
}

/// Encodes a message; refuses one over its limit.
pub fn encode(body: &Body) -> Result<Vec<u8>, Refusal> {
    let bytes = serde_json::to_vec(&EnvelopeOut {
        v: PROTOCOL_VERSION,
        body,
    })
    .map_err(|e| Refusal::Malformed {
        detail: e.to_string(),
    })?;
    let limit = body.limit();
    if bytes.len() > limit {
        return Err(Refusal::TooLarge {
            size: bytes.len(),
            limit,
        });
    }
    Ok(bytes)
}

fn clip(s: &str) -> String {
    s.chars().take(64).collect()
}

/// Decodes a frame. Never panics; returns a named refusal for oversized,
/// malformed, newer/older-version and unknown-message frames.
pub fn decode(bytes: &[u8]) -> Result<Body, Refusal> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(Refusal::TooLarge {
            size: bytes.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| Refusal::Malformed {
            detail: clip(&e.to_string()),
        })?;
    let v = value
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| Refusal::Malformed {
            detail: "no version".to_string(),
        })?;
    let v = u32::try_from(v).unwrap_or(u32::MAX);
    if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&v) {
        return Err(Refusal::UnsupportedVersion {
            theirs: v,
            min: MIN_PROTOCOL_VERSION,
            max: PROTOCOL_VERSION,
        });
    }
    let body = value.get("body").ok_or_else(|| Refusal::Malformed {
        detail: "no body".to_string(),
    })?;
    let tag = body
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(clip);
    let parsed: Body = serde_json::from_value(body.clone()).map_err(|e| {
        let msg = e.to_string();
        match tag {
            Some(name) if msg.contains("unknown variant") => Refusal::UnknownMessage { name },
            _ => Refusal::Malformed { detail: clip(&msg) },
        }
    })?;
    let limit = parsed.limit();
    if bytes.len() > limit {
        return Err(Refusal::TooLarge {
            size: bytes.len(),
            limit,
        });
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_body() -> Vec<Body> {
        let h = Hash::of(b"x");
        let c = CheckoutId::derive("h", std::path::Path::new("/p"));
        vec![
            Body::Hello(Hello::local("a")),
            Body::StatusRequest,
            Body::Subscribe {
                remote: RemoteJobId(3),
                from_offset: 9,
            },
            Body::Event(JobEvent::Heartbeat),
            Body::Event(JobEvent::Log {
                offset: 4,
                data: b"hi\n".to_vec(),
            }),
            Body::Event(JobEvent::Phase {
                phase: Phase::Running,
                detail: String::new(),
            }),
            Body::Cancel {
                remote: RemoteJobId(3),
            },
            Body::SyncHaveRequest {
                checkout: c.clone(),
                hashes: vec![h],
            },
            Body::SyncBlob {
                checkout: c.clone(),
                hash: h,
                offset: 0,
                total: 1,
                data: vec![1],
            },
            Body::FetchList {
                checkout: c,
                paths: vec!["a".into()],
            },
            Body::Ok,
            Body::Refuse(Refusal::HashMismatch),
        ]
    }

    #[test]
    fn r152_wire_shapes_round_trip() {
        for b in every_body() {
            let bytes = encode(&b).unwrap();
            assert_eq!(decode(&bytes).unwrap(), b);
        }
    }

    #[test]
    fn r152_wire_shape_is_stable() {
        let b = Body::Cancel {
            remote: RemoteJobId(7),
        };
        assert_eq!(
            String::from_utf8(encode(&b).unwrap()).unwrap(),
            r#"{"v":1,"body":{"type":"cancel","remote":7}}"#
        );
    }

    #[test]
    fn r149_newer_version_gets_a_named_refusal() {
        let r = decode(br#"{"v":2,"body":{"type":"status_request"}}"#);
        assert_eq!(
            r,
            Err(Refusal::UnsupportedVersion {
                theirs: 2,
                min: 1,
                max: 1
            })
        );
        let r = decode(br#"{"v":0,"body":{"type":"ok"}}"#);
        assert!(matches!(
            r,
            Err(Refusal::UnsupportedVersion { theirs: 0, .. })
        ));
        let r = decode(br#"{"v":99999999999,"body":{"type":"ok"}}"#);
        assert!(matches!(r, Err(Refusal::UnsupportedVersion { .. })));
    }

    #[test]
    fn unknown_message_is_named_not_a_panic() {
        let r = decode(br#"{"v":1,"body":{"type":"teleport","x":1}}"#);
        assert_eq!(
            r,
            Err(Refusal::UnknownMessage {
                name: "teleport".into()
            })
        );
    }

    #[test]
    fn junk_is_malformed() {
        for junk in [
            &b""[..],
            b"{",
            b"[]",
            b"null",
            b"{\"v\":1}",
            b"{\"v\":\"1\",\"body\":{}}",
        ] {
            assert!(
                matches!(decode(junk), Err(Refusal::Malformed { .. })),
                "{junk:?}"
            );
        }
        // Right type, wrong field.
        let r = decode(br#"{"v":1,"body":{"type":"cancel","remote":"x"}}"#);
        assert!(matches!(r, Err(Refusal::Malformed { .. })));
        // Bad hash text is refused at the type, not later.
        let r = decode(
            br#"{"v":1,"body":{"type":"sync_have_request","checkout":"00000000000000000000000000000000","hashes":["nope"]}}"#,
        );
        assert!(matches!(r, Err(Refusal::Malformed { .. })));
    }

    #[test]
    fn every_message_has_a_size_limit() {
        // Oversized frames are refused before parsing.
        let big = vec![b' '; MAX_FRAME_BYTES + 1];
        assert!(matches!(decode(&big), Err(Refusal::TooLarge { .. })));
        // A control message over its own limit is refused on both sides.
        let hashes = vec![Hash::of(b"a"); 2000];
        let b = Body::SyncHaveRequest {
            checkout: CheckoutId::derive("h", std::path::Path::new("/p")),
            hashes,
        };
        assert!(
            matches!(encode(&b), Err(Refusal::TooLarge { limit, .. }) if limit == MAX_CONTROL_BYTES)
        );
        let mut json = String::from(r#"{"v":1,"body":{"type":"ok"}"#);
        json.push_str(&" ".repeat(MAX_CONTROL_BYTES));
        json.push('}');
        assert!(matches!(
            decode(json.as_bytes()),
            Err(Refusal::TooLarge { .. })
        ));
        // A data chunk of the maximum size still fits.
        let b = Body::Event(JobEvent::Log {
            offset: 0,
            data: vec![0xff; MAX_CHUNK_BYTES],
        });
        assert!(encode(&b).is_ok());
    }

    #[test]
    fn hello_negotiation_names_a_gap() {
        let a = Hello::local("a");
        let mut b = Hello::local("b");
        assert_eq!(a.negotiate(&b), Ok(1));
        b.min_version = 2;
        b.max_version = 3;
        assert!(matches!(
            a.negotiate(&b),
            Err(Refusal::UnsupportedVersion { theirs: 3, .. })
        ));
    }

    #[test]
    fn unrecognised_refusal_code_still_decodes() {
        let r = decode(br#"{"v":1,"body":{"type":"refuse","code":"from_the_future","x":1}}"#);
        assert_eq!(r, Ok(Body::Refuse(Refusal::Unrecognised)));
    }

    #[test]
    fn wire_outcome_phase_matches_runner_outcome() {
        use std::time::Duration;
        for end in [
            End::Exited,
            End::Cancelled,
            End::ClientLost,
            End::WallTimeout,
            End::IdleTimeout,
            End::Lost,
        ] {
            for code in [Some(0), Some(2), None] {
                let o = Outcome {
                    end,
                    exit_code: code,
                    signal: None,
                    usage: None,
                    wall: Duration::from_millis(1500),
                    lease: tasks_runner::Lease::new(tasks_platform::GroupId {
                        pgid: 1,
                        start_time: 1,
                    }),
                };
                let w = WireOutcome::from(&o);
                assert_eq!(w.phase(), o.phase(), "{end:?} {code:?}");
                assert_eq!(w.wall_ms, 1500);
                assert_eq!(w.description, o.describe());
            }
        }
    }

    #[test]
    fn samples_round_trip_and_bad_ones_are_dropped() {
        let key = SampleKey {
            recipe: "cargo:test".into(),
            target: Some("test:x".into()),
            machine: "nixos".into(),
        };
        let s = Sample {
            secs: 12.5,
            cache: Cache::Warm,
            outcome: TimingOutcome::Passed,
            at_ms: 99,
        };
        let w = WireSample::from_sample(&key, &s);
        let json = serde_json::to_string(&w).unwrap();
        let back: WireSample = serde_json::from_str(&json).unwrap();
        assert_eq!(back.to_sample(), Some((key, s)));
        let mut bad = w;
        bad.cache = "lukewarm".into();
        assert_eq!(bad.to_sample(), None);
    }
}
