//! The wire protocol: versioned, size-limited messages (REQUIREMENTS 7.18, 7.19).
//!
//! Every frame is a JSON envelope `{"v": N, "body": {"type": ...}}`. A newer or
//! older version, an unknown message or an oversized frame is answered with a
//! named [`Refusal`]; decoding never panics and never waits for more input.

use crate::ids::{CheckoutId, GitId, Hash, RemoteJobId, SubmitKey, hexbytes};
use crate::trust::wire::{PairDone, PairRedeem, PresentAck};
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
pub const ALPN: &[u8] = b"tasks/1";

/// Largest data payload of one log, blob or fetch message (before hex).
pub const MAX_CHUNK_BYTES: usize = 128 * 1024;
/// Limit for small control messages.
pub const MAX_CONTROL_BYTES: usize = 64 * 1024;
/// Limit for a message carrying one data chunk (hex doubles it, plus framing).
pub const MAX_DATA_FRAME_BYTES: usize = MAX_CHUNK_BYTES * 2 + 4 * 1024;
/// Absolute ceiling for any frame, checked before parsing.
pub const MAX_FRAME_BYTES: usize = MAX_DATA_FRAME_BYTES;
/// Most snapshots a worker reports in one [`Body::SyncHave`].
pub const MAX_SYNC_TIPS: usize = 32;
/// Most paths in one fetch listing request.
pub const MAX_FETCH_PATHS: usize = tasks_core::MAX_FETCH_PATHS;
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
    /// The pack did not arrive whole, or git rejected it.
    #[error("pack refused: {detail}")]
    BadPack {
        /// What was wrong.
        detail: String,
    },
    /// The sync names objects the receiver does not have (and was sent no pack for).
    #[error("objects missing on the receiver: {what}")]
    MissingObjects {
        /// Which.
        what: String,
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
    /// No valid grant covers this connection: none presented, or it is unknown, expired,
    /// revoked or bound to another device. The reason names which.
    #[error("not authorised: {reason}")]
    Unauthorized {
        /// Why.
        reason: String,
    },
    /// The peer's grants do not allow this request.
    #[error("role denied: this request needs {need}; the peer holds {}", .have.join(", "))]
    RoleDenied {
        /// The role the request needs.
        need: String,
        /// The roles the peer holds.
        have: Vec<String>,
    },
    /// A pairing step was refused.
    #[error("pairing refused: {reason}")]
    PairingRefused {
        /// Why.
        reason: String,
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
    /// Checkout sync as git objects (tree ids, thin packs).
    pub const SYNC: &'static str = "sync-git";
    /// Output fetch.
    pub const FETCH: &'static str = "fetch";
    /// Log resume from an offset.
    pub const RESUME: &'static str = "resume";
    /// Reports and drops what it keeps for the asker's checkouts (R-839).
    pub const CACHE: &'static str = "cache";
    /// The capabilities of this build.
    pub fn all() -> BTreeSet<Capability> {
        [
            Self::STATUS,
            Self::JOBS,
            Self::SYNC,
            Self::FETCH,
            Self::RESUME,
            Self::CACHE,
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
    /// Memory pressure level (`normal`, `warn`, `critical`) for hosts without PSI percentages,
    /// such as macOS.
    #[serde(default)]
    pub memory_level: Option<String>,
    /// I/O pressure percent, when known.
    pub io_pct: Option<u8>,
    /// One line for humans.
    pub summary: String,
    /// CPU busy percent (not pressure), when the peer measures it.
    #[serde(default)]
    pub cpu_busy_pct: Option<u8>,
    /// Memory available to new work now, bytes, when the peer reports it.
    #[serde(default)]
    pub memory_available_bytes: Option<u64>,
    /// Swap size in bytes (0 without swap), when the peer reports it.
    #[serde(default)]
    pub swap_total_bytes: Option<u64>,
    /// Swap in use in bytes.
    #[serde(default)]
    pub swap_used_bytes: Option<u64>,
    /// Bytes per second read back from swap, when two readings exist.
    #[serde(default)]
    pub swap_in_per_s: Option<u64>,
    /// Bytes per second written out to swap.
    #[serde(default)]
    pub swap_out_per_s: Option<u64>,
}

/// One running job on a peer and what it uses now, for the origin's "CPU by job" rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerJobUse {
    /// The job's id on the peer.
    pub id: u64,
    /// Who owns it there.
    pub owner: String,
    /// CPU in thousandths of a core over the last sample interval, when two readings exist.
    #[serde(default)]
    pub cores_milli: Option<u32>,
    /// Memory in use now, bytes, when sampled.
    #[serde(default)]
    pub mem_bytes: Option<u64>,
}

/// Most running jobs a status reply lists.
pub const MAX_STATUS_JOBS: usize = 32;

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
    /// The running jobs and their use now. Absent from an older peer.
    #[serde(default)]
    pub jobs: Vec<PeerJobUse>,
    /// The whole machine's CPU in use, thousandths of a core. Absent from an older peer.
    #[serde(default)]
    pub cores_in_use_milli: Option<u32>,
    /// Where its power comes from; absent from older peers.
    #[serde(default)]
    pub power: Option<PowerSummary>,
    /// Its daemon's version and build, `0.1.0 (41ac455 2026-10-27)`; absent from older peers
    /// (R-807).
    #[serde(default)]
    pub version: Option<String>,
}

/// A peer's power state, shown next to its link state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerSummary {
    /// Running on a battery.
    pub on_battery: bool,
    /// Battery charge, when there is a battery.
    pub percent: Option<u8>,
    /// Jobs from peers wait there now, on battery (R-834); placement passes it over when
    /// another machine can start the job (R-836). False from older peers.
    #[serde(default)]
    pub holds_peer_jobs: bool,
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
    /// The job only reads the synced copy, so the receiver does not queue it behind the other
    /// jobs of the copy (R-700). Beside the spec, not in it, so a peer that does not know the
    /// field ignores it instead of refusing the job.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared_checkout: bool,
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

/// A snapshot the worker already holds: the commit the origin sent, its tree and the extras
/// tree it was sent with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncTip {
    /// The snapshot commit.
    pub commit: GitId,
    /// Its tree.
    pub tree: GitId,
    /// The extras tree synced with it.
    pub extras: Option<GitId>,
}

/// Finish a sync: the snapshot to apply and the pack that carried it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncApply {
    /// The checkout.
    pub checkout: CheckoutId,
    /// The repo the checkout belongs to (the worker keeps one mirror per repo).
    pub repo: CheckoutId,
    /// Session of the pack chunks sent before.
    pub session: u64,
    /// Tree id of the working tree.
    pub tree: GitId,
    /// Tree id of the allowlisted extras.
    pub extras_tree: Option<GitId>,
    /// The snapshot commit the pack was built around (absent when the worker already held the
    /// tree and no pack was sent).
    pub commit: Option<GitId>,
    /// HEAD of the origin's repo at snapshot time.
    pub base: Option<GitId>,
    /// Files that differ from HEAD.
    pub dirty: u32,
    /// Files in the tree.
    pub files: u64,
    /// Bytes of pack sent.
    pub pack_bytes: u64,
    /// SHA-256 of the pack bytes; required when `pack_bytes` is not 0.
    pub pack_sha256: Option<Hash>,
}

/// Result of applying a sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSummary {
    /// The tree now in the working copy.
    pub tree: GitId,
    /// Files in the tree.
    pub files: u64,
    /// Files written or replaced.
    pub written: u64,
    /// Files left untouched (same content).
    pub unchanged: u64,
    /// Files removed.
    pub removed: u64,
    /// Allowlisted extra files in place.
    pub extras: u64,
    /// Bytes of pack received.
    pub pack_bytes: u64,
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
        /// The file's BLAKE3 hash (hex) when the peer also offers it over iroh-blobs; absent from
        /// older peers, which serve chunks only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blob: Option<String>,
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

/// A checkout of the asker that the receiver keeps something of (R-839).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldCheckout {
    /// The checkout.
    pub checkout: CheckoutId,
    /// Bytes of the synced working copy; 0 once retention deleted it.
    pub copy_bytes: u64,
    /// Bytes of its build output in the receiver's cache.
    pub output_bytes: u64,
    /// When a job last used the copy, ms since the epoch; none without a copy.
    #[serde(default)]
    pub last_used_ms: Option<i64>,
    /// A job of the asker runs or waits in it: a drop keeps it.
    #[serde(default)]
    pub busy: bool,
}

/// The result of dropping one checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedCheckout {
    /// The checkout.
    pub checkout: CheckoutId,
    /// Bytes freed.
    pub freed_bytes: u64,
    /// Why it was kept, when it was.
    #[serde(default)]
    pub kept: Option<String>,
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
    /// Start a sync: which snapshots of this checkout do you hold?
    SyncOffer {
        /// The checkout.
        checkout: CheckoutId,
        /// The repo it belongs to.
        repo: CheckoutId,
    },
    /// The snapshots the receiver holds for the repo, newest first (at most [`MAX_SYNC_TIPS`]).
    SyncHave {
        /// The tips.
        tips: Vec<SyncTip>,
    },
    /// The next bytes of the thin pack.
    SyncPackChunk {
        /// The checkout.
        checkout: CheckoutId,
        /// Sender-chosen session id.
        session: u64,
        /// Offset of this chunk; must continue where the last one ended.
        offset: u64,
        /// The bytes.
        #[serde(with = "hexbytes")]
        data: Vec<u8>,
    },
    /// Opens a raw pack stream: after the receiver's `Ok` the sender writes the thin pack as plain
    /// bytes on the same stream and finishes it; the receiver answers `SyncPackAck` with the
    /// length it holds. An older receiver answers `Refuse` at the first frame, and the sender
    /// falls back to `SyncPackChunk` frames.
    SyncPackStream {
        /// The checkout.
        checkout: CheckoutId,
        /// Sender-chosen session id (names the spool file, as for chunks).
        session: u64,
    },
    /// Bytes of the pack received so far.
    SyncPackAck {
        /// Received length.
        received: u64,
    },
    /// The pack is complete: index it and make the working copy match the tree.
    SyncApply(SyncApply),
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
    /// First frame of a connection: the grants this machine holds from the peer.
    Present {
        /// Grant tokens (`g1.…`).
        grants: Vec<String>,
    },
    /// The presented grants were accepted.
    PresentAck(PresentAck),
    /// Pairing: ask for a challenge to sign.
    PairBegin,
    /// Pairing: the challenge.
    PairChallenge {
        /// 32 random bytes, lowercase hex.
        challenge: String,
    },
    /// Pairing: redeem an invite.
    PairRedeem(PairRedeem),
    /// Pairing: redeemed; the grants minted for the redeemer.
    PairDone(PairDone),
    /// Renew a grant over the live connection.
    Renew {
        /// The grant being renewed.
        grant: String,
    },
    /// The renewed grant.
    Renewed {
        /// The new token.
        grant: String,
    },
    /// Ask for the live metrics of one job this peer submitted. An older peer refuses it as an
    /// unknown message.
    JobMetricsRequest {
        /// The job on the receiver.
        remote: RemoteJobId,
    },
    /// The metrics of the job.
    JobMetrics(tasks_core::metrics::JobMetricsView),
    /// What do you keep for my checkouts? Answered with [`Body::CacheHeld`]; an older peer
    /// refuses it as an unknown message (R-839).
    CacheRequest,
    /// The checkouts the receiver keeps for the asker.
    CacheHeld {
        /// One per checkout with a copy or build output here.
        checkouts: Vec<HeldCheckout>,
    },
    /// Delete the synced copy and build output of these checkouts of mine (R-839).
    CacheDrop {
        /// The checkouts.
        checkouts: Vec<CheckoutId>,
    },
    /// What a drop did, one entry per checkout asked.
    CacheDropped {
        /// The results.
        checkouts: Vec<DroppedCheckout>,
    },
    /// A named refusal.
    Refuse(Refusal),
}

/// Samples kept when a metrics reply would be over its frame limit.
pub fn fit_metrics(
    mut v: tasks_core::metrics::JobMetricsView,
) -> tasks_core::metrics::JobMetricsView {
    while encode(&Body::JobMetrics(v.clone())).is_err() && !v.samples.is_empty() {
        // the oldest quarter goes first: the graph's right edge is what matters
        let drop = (v.samples.len() / 4).max(1);
        v.samples.drain(..drop);
    }
    v
}

impl Body {
    /// The size limit of this message.
    pub fn limit(&self) -> usize {
        match self {
            Body::Event(JobEvent::Log { .. })
            | Body::SyncPackChunk { .. }
            | Body::FetchData { .. }
            | Body::JobMetrics(_) => MAX_DATA_FRAME_BYTES,
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

    // R-372: a message this build does not know is a named refusal, which is what a newer sender
    // (a stream head to an older receiver) relies on to fall back
    #[test]
    fn r372_an_unknown_message_kind_is_refused_as_malformed_not_dropped() {
        let junk = format!(r#"{{"v":{PROTOCOL_VERSION},"body":{{"from_the_future":{{"x":1}}}}}}"#);
        assert!(matches!(
            decode(junk.as_bytes()),
            Err(Refusal::Malformed { .. })
        ));
        let b = Body::SyncPackStream {
            checkout: CheckoutId::derive("h", std::path::Path::new("/p")),
            session: 4,
        };
        assert_eq!(decode(&encode(&b).unwrap()).unwrap(), b);
    }

    // R-627
    #[test]
    fn r627_status_json_from_an_older_peer_parses_and_a_big_metrics_reply_is_trimmed() {
        let now = crate::fake::FakeStatus::nixos().0;
        let mut json = serde_json::to_value(&now).unwrap();
        // what a peer that predates the fields sends
        let Some(obj) = json.as_object_mut() else {
            panic!("not an object")
        };
        obj.remove("jobs");
        obj.remove("cores_in_use_milli");
        let old = json.to_string();
        let s: PeerStatus = serde_json::from_str(&old).unwrap();
        assert!(s.jobs.is_empty() && s.cores_in_use_milli.is_none());
        let sample = |t: i64| tasks_core::metrics::MetricSample {
            t_ms: t,
            cpu_usec: Some(u64::MAX),
            mem_bytes: Some(u64::MAX),
            io_read_bytes: Some(u64::MAX),
            io_write_bytes: Some(u64::MAX),
            io_some_us: Some(u64::MAX),
        };
        let big = tasks_core::metrics::JobMetricsView {
            quality: None,
            samples: (0..2000).map(|t| sample(1_700_000_000_000 + t)).collect(),
            procs: Vec::new(),
            usual_peak_mem_bytes: None,
            note: None,
            source: None,
        };
        assert!(encode(&Body::JobMetrics(big.clone())).is_err());
        let fit = fit_metrics(big.clone());
        assert!(encode(&Body::JobMetrics(fit.clone())).is_ok());
        // the newest samples survive
        assert_eq!(fit.samples.last(), big.samples.last());
        assert!(!fit.samples.is_empty());
        let small = tasks_core::metrics::JobMetricsView {
            samples: vec![sample(1)],
            ..big
        };
        assert_eq!(fit_metrics(small.clone()), small);
    }

    fn every_body() -> Vec<Body> {
        let h = Hash::of(b"x");
        let c = CheckoutId::derive("h", std::path::Path::new("/p"));
        vec![
            Body::Hello(Hello::local("a")),
            Body::StatusRequest,
            Body::JobMetricsRequest {
                remote: RemoteJobId(3),
            },
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
            Body::SyncOffer {
                checkout: c.clone(),
                repo: c.clone(),
            },
            Body::SyncHave {
                tips: vec![SyncTip {
                    commit: GitId::parse(&"a".repeat(40)).unwrap(),
                    tree: GitId::parse(&"b".repeat(40)).unwrap(),
                    extras: None,
                }],
            },
            Body::SyncPackChunk {
                checkout: c.clone(),
                session: 1,
                offset: 0,
                data: vec![1],
            },
            Body::SyncPackStream {
                checkout: c.clone(),
                session: 1,
            },
            Body::SyncApply(SyncApply {
                checkout: c.clone(),
                repo: c.clone(),
                session: 1,
                tree: GitId::parse(&"b".repeat(40)).unwrap(),
                extras_tree: None,
                commit: None,
                base: None,
                dirty: 0,
                files: 1,
                pack_bytes: 1,
                pack_sha256: Some(h),
            }),
            Body::FetchList {
                checkout: c,
                paths: vec!["a".into()],
            },
            Body::Ok,
            Body::Refuse(Refusal::HashMismatch),
            Body::Present {
                grants: vec!["g1.a.b".into()],
            },
            Body::PresentAck(PresentAck {
                did: "did:key:zA".into(),
                name: "nixos".into(),
                roles: vec![crate::trust::Role::Submit],
                expires: 9,
            }),
            Body::PairBegin,
            Body::PairChallenge {
                challenge: "00".repeat(32),
            },
            Body::PairRedeem(PairRedeem {
                invite: "i1.a.b".into(),
                did: "did:key:zB".into(),
                name: "kurukkan".into(),
                challenge_sig: "00".repeat(64),
                relay: None,
                direct_addrs: vec!["192.168.1.5:1".into()],
                reverse: vec![],
            }),
            Body::PairDone(PairDone {
                did: "did:key:zA".into(),
                name: "nixos".into(),
                node_id: "ab".repeat(32),
                grants: vec!["g1.c.d".into()],
            }),
            Body::Renew {
                grant: "g1.a.b".into(),
            },
            Body::Renewed {
                grant: "g1.e.f".into(),
            },
            Body::Refuse(Refusal::Unauthorized {
                reason: "expired".into(),
            }),
            Body::Refuse(Refusal::RoleDenied {
                need: "submit".into(),
                have: vec!["observe".into()],
            }),
            Body::Refuse(Refusal::PairingRefused {
                reason: "spent".into(),
            }),
        ]
    }

    // R-239
    #[test]
    fn r239_trust_wire_shapes_are_stable() {
        let b = Body::Refuse(Refusal::Unauthorized {
            reason: "grant revoked".into(),
        });
        assert_eq!(
            String::from_utf8(encode(&b).unwrap()).unwrap(),
            r#"{"v":1,"body":{"type":"refuse","code":"unauthorized","reason":"grant revoked"}}"#
        );
        let b = Body::Present {
            grants: vec!["g1.x.y".into()],
        };
        assert_eq!(
            String::from_utf8(encode(&b).unwrap()).unwrap(),
            r#"{"v":1,"body":{"type":"present","grants":["g1.x.y"]}}"#
        );
        assert_eq!(ALPN, b"tasks/1");
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
        // A bad id is refused at the type, not later.
        let r = decode(
            br#"{"v":1,"body":{"type":"sync_offer","checkout":"nope","repo":"00000000000000000000000000000000"}}"#,
        );
        assert!(matches!(r, Err(Refusal::Malformed { .. })));
    }

    #[test]
    fn every_message_has_a_size_limit() {
        // Oversized frames are refused before parsing.
        let big = vec![b' '; MAX_FRAME_BYTES + 1];
        assert!(matches!(decode(&big), Err(Refusal::TooLarge { .. })));
        // A control message over its own limit is refused on both sides.
        let tip = SyncTip {
            commit: GitId::parse(&"a".repeat(40)).unwrap(),
            tree: GitId::parse(&"b".repeat(40)).unwrap(),
            extras: None,
        };
        let b = Body::SyncHave {
            tips: vec![tip; 2000],
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
                    figures: None,
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
