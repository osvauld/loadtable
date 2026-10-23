//! The origin side of the object model: a snapshot of the exact working tree.

use super::extras::ExtraRules;
use super::limits::SyncLimits;
use super::refuse::Refusals;
use crate::ids::{CheckoutId, GitId};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What to snapshot.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotRequest<'a> {
    /// The checkout root (the top of the working tree).
    pub root: &'a Path,
    /// The checkout's id; names its stat cache.
    pub checkout: &'a CheckoutId,
    /// This machine's name, part of the repo id.
    pub origin_host: &'a str,
    /// Size limits.
    pub limits: &'a SyncLimits,
    /// Gitignored files that must travel.
    pub extras: &'a ExtraRules,
}

/// What a snapshot read and wrote, so a re-sync can be shown to be cheap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotIo {
    /// Files whose stat was compared with the cache.
    pub files_checked: u64,
    /// Files whose content was read to hash them.
    pub files_read: u64,
    /// Bytes of working-tree content read.
    pub bytes_read: u64,
    /// Bytes of index files read (the user's, the snapshot cache).
    pub index_bytes_read: u64,
    /// Bytes of index files written (the snapshot cache only).
    pub index_bytes_written: u64,
    /// New objects written to the tasks object directory.
    pub objects_written: u64,
}

/// The tree id of a working tree, plus where its objects are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSnapshot {
    /// The repo this checkout belongs to (a worktree of the same repo shares it).
    pub repo: CheckoutId,
    /// The tasks-owned bare repository holding the snapshot's objects; the user's repo is an
    /// alternate and is never written.
    pub git_dir: PathBuf,
    /// Tree id of the working tree, uncommitted changes included, ignored files excluded.
    pub tree: GitId,
    /// Tree id of the allowlisted extras, when there are any.
    pub extras_tree: Option<GitId>,
    /// HEAD of the repo at snapshot time (`None` when unborn).
    pub base: Option<GitId>,
    /// Files that differ from HEAD's tree.
    pub dirty_files: u32,
    /// Files in the tree.
    pub files: u64,
    /// Paths of the extras that travel.
    pub extra_files: Vec<String>,
    /// What it cost.
    pub io: SnapshotIo,
}

/// Why a snapshot failed.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The checkout is not a git working tree this backend can read.
    #[error("{path} is not a usable git working tree: {detail}")]
    NotARepo {
        /// The root.
        path: PathBuf,
        /// What was wrong.
        detail: String,
    },
    /// Files were refused, by name.
    #[error("{0}")]
    Refused(Refusals),
    /// Too many files.
    #[error("{count} files is over the limit of {limit}")]
    TooManyFiles {
        /// Seen.
        count: usize,
        /// Limit.
        limit: usize,
    },
    /// The allowlist is invalid.
    #[error("invalid sync allowlist: {0}")]
    BadExtras(String),
    /// The tasks object store or a git command failed.
    #[error("snapshot store: {0}")]
    Store(String),
    /// File I/O failed.
    #[error("snapshot I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// What pruning the origin's object store freed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Loose objects deleted.
    pub objects: u64,
    /// Bytes freed.
    pub bytes: u64,
}

/// Snapshots a working tree without ever writing to the user's repository.
pub trait Snapshot: Send + Sync {
    /// Writes the working tree as a tree object into the tasks object directory and returns
    /// its id. Only files whose stat changed since the last snapshot are read.
    fn snapshot(&self, req: &SnapshotRequest<'_>) -> Result<TreeSnapshot, SnapshotError>;

    /// Wraps `snap.tree` in a parentless-or-chained commit (so a thin pack has a base) and
    /// returns the commit id. `now_secs` is the commit time.
    fn seal(
        &self,
        snap: &TreeSnapshot,
        parent: Option<&GitId>,
        now_secs: i64,
    ) -> Result<GitId, SnapshotError>;

    /// Deletes loose objects in the store of `repo` that are not reachable from `keep` (commits
    /// are read for their tree only, never their parents) and are older than `min_age`.
    fn prune(
        &self,
        repo: &CheckoutId,
        keep: &[GitId],
        min_age: Duration,
    ) -> Result<PruneStats, SnapshotError>;
}
