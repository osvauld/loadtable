//! Checkout sync: git objects over the peer link.
//!
//! The origin snapshots the exact working tree into a tree id ([`Snapshot`]), asks the worker
//! which snapshots it already holds, streams a thin pack of what it lacks ([`PackTransfer`])
//! and tells the worker to make its copy match the tree. The worker keeps one bare mirror per
//! (peer, repo) and one working copy per (peer, checkout) in a [`SyncStore`].
//!
//! What it never does: write to the user's repository (index, refs, stash, objects), hash a
//! file whose stat did not change, or send an object the worker holds.
//!
//! I/O accounting: [`SyncGate`] is the hook point for design step 4 (transfers count against
//! the I/O gate). The origin takes it around snapshotting and pack building, the worker around
//! applying; the default gate admits at once.

mod extras;
mod git_cli;
mod gix_snapshot;
mod limits;
mod origin;
mod paths;
mod refuse;
mod retention;
mod secrets;
mod snapshot;
pub mod testrepo;
mod transfer;
mod worker;

pub use extras::{ExtraRules, MAX_PATTERNS};
pub use git_cli::{GixTransfer, empty_tree_id};
pub use gix_snapshot::GixSnapshot;
pub use limits::SyncLimits;
pub use origin::{ExtrasProvider, NoExtras, OriginSync, SyncDriver, SyncJob, SyncReport};
pub use paths::{check_rel_path, dir_size, hash_file, join};
pub use refuse::{Refusals, RefuseReason, Refused};
pub use retention::{OriginRetention, WorkerRetention};
pub use secrets::{secret_content, secret_name};
pub use snapshot::{
    PruneStats, Snapshot, SnapshotError, SnapshotIo, SnapshotRequest, TreeSnapshot,
};
pub use transfer::{
    ChangeKind, CheckoutRequest, PackRequest, PackTransfer, TransferError, TreeChange, repo_dir,
};
pub use worker::SyncStore;

use crate::ids::{CheckoutId, GitId, PeerId};

/// What kind of thing retention deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    /// A worker's working copy that sat idle.
    WorkingCopy,
    /// A worker's mirror nothing refers to any more.
    Mirror,
    /// Objects a worker's mirror garbage collection freed.
    MirrorGc,
    /// Origin snapshot objects older than the newest per checkout.
    OriginObjects,
}

impl Deleted {
    /// A stable name for audit rows.
    pub fn name(self) -> &'static str {
        match self {
            Deleted::WorkingCopy => "working_copy",
            Deleted::Mirror => "mirror",
            Deleted::MirrorGc => "mirror_gc",
            Deleted::OriginObjects => "origin_objects",
        }
    }
}

/// Something sync did that the audit log records (design §8, `data.*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEvent {
    /// The origin sent a snapshot to `peer`.
    Sent {
        /// The worker.
        peer: PeerId,
        /// The checkout.
        checkout: CheckoutId,
        /// The local job the sync was for.
        job: Option<i64>,
        /// Tree id sent.
        tree: GitId,
        /// HEAD it was based on.
        base: Option<GitId>,
        /// Files differing from HEAD.
        dirty: u32,
        /// Files in the tree.
        files: u64,
        /// Bytes of pack sent.
        bytes: u64,
    },
    /// The origin refused to send: files were refused by name.
    Refused {
        /// The worker.
        peer: PeerId,
        /// The checkout.
        checkout: CheckoutId,
        /// The local job.
        job: Option<i64>,
        /// `(path, reason code)` of each file, at most a few.
        files: Vec<(String, &'static str)>,
    },
    /// The worker applied a snapshot from `peer`.
    Received {
        /// The origin.
        peer: PeerId,
        /// The checkout.
        checkout: CheckoutId,
        /// Tree id applied.
        tree: GitId,
        /// Bytes of pack received.
        bytes: u64,
        /// Files written.
        written: u64,
        /// Files removed.
        removed: u64,
    },
    /// Retention deleted something.
    Deleted {
        /// What.
        kind: Deleted,
        /// The peer it belonged to, on a worker.
        peer: Option<PeerId>,
        /// Where (a checkout id, mirror name or repo id; never a full path).
        target: String,
        /// Bytes freed.
        bytes: u64,
    },
}

/// Told about sync events; the audit log plugs in here.
pub trait SyncObserver: Send + Sync {
    /// `event` happened.
    fn event(&self, event: &SyncEvent);
}

/// Ignores everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoObserver;

impl SyncObserver for NoObserver {
    fn event(&self, _event: &SyncEvent) {}
}

/// Which part of a sync is running, for the I/O gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPhase {
    /// Reading the working tree and writing objects (origin).
    Snapshot,
    /// Reading objects into a pack and streaming it (origin).
    Pack,
    /// Indexing a pack and writing the working copy (worker).
    Apply,
}

/// Held while a sync phase does local disk work. Step 4 implements this on top of admission so
/// that transfers wait for the I/O gate; dropping the guard releases the slot.
pub trait SyncGate: Send + Sync {
    /// Called before `phase` starts; returns a guard kept until it ends. May block.
    fn enter(&self, phase: SyncPhase) -> Box<dyn Send>;
}

/// Admits every phase at once.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenGate;

impl SyncGate for OpenGate {
    fn enter(&self, _phase: SyncPhase) -> Box<dyn Send> {
        Box::new(())
    }
}

/// Why a sync failed.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The link or the peer failed.
    #[error("sync failed: {0}")]
    Peer(#[from] crate::transport::PeerError),
    /// The working tree could not be snapshotted, or files were refused.
    #[error("{0}")]
    Snapshot(#[from] SnapshotError),
    /// Building or sending the pack failed.
    #[error("{0}")]
    Transfer(#[from] TransferError),
    /// The peer answered with the wrong kind of message.
    #[error("unexpected reply to {0}")]
    Unexpected(&'static str),
}

impl SyncError {
    /// True when retrying cannot help: the snapshot refused files by name, or the pack is over
    /// its limit. The forward ends at once instead of repeating the attempt.
    pub fn is_fatal(&self) -> bool {
        match self {
            SyncError::Snapshot(SnapshotError::Refused(_)) => true,
            SyncError::Snapshot(
                SnapshotError::TooManyFiles { .. } | SnapshotError::BadExtras(_),
            ) => true,
            SyncError::Transfer(TransferError::PackTooLarge { .. }) => true,
            SyncError::Peer(crate::transport::PeerError::Refused(r)) => matches!(
                r,
                crate::proto::Refusal::LimitExceeded { .. } | crate::proto::Refusal::BadPath { .. }
            ),
            _ => false,
        }
    }

    /// True when the worker still has a live job of this checkout.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            SyncError::Peer(crate::transport::PeerError::Refused(
                crate::proto::Refusal::Busy { .. }
            ))
        )
    }
}
