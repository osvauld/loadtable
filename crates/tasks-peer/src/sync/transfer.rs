//! Moving git objects: pack generation, receipt, refs and tree checkout.

use crate::ids::GitId;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Why a transfer step failed.
#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    /// A git command or the object store failed.
    #[error("{what}: {detail}")]
    Git {
        /// The step.
        what: &'static str,
        /// What git said.
        detail: String,
    },
    /// The pack is over its byte limit.
    #[error("pack is over the {limit} byte limit")]
    PackTooLarge {
        /// The limit.
        limit: u64,
    },
    /// The sink refused bytes.
    #[error("writing the pack failed: {0}")]
    Sink(String),
    /// I/O failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// What to pack.
#[derive(Debug, Clone, Copy)]
pub struct PackRequest<'a> {
    /// The repository to read objects from (the tasks store, its alternates included).
    pub git_dir: &'a Path,
    /// Commits and trees to send.
    pub want: &'a [GitId],
    /// Commits the receiver has; everything reachable from them is left out, and their trees
    /// are the bases a thin pack deltas against.
    pub have: &'a [GitId],
    /// Stop with [`TransferError::PackTooLarge`] after this many bytes.
    pub max_bytes: u64,
}

/// Where a working copy lives.
#[derive(Debug, Clone, Copy)]
pub struct CheckoutRequest<'a> {
    /// The repository holding the objects (the worker's bare mirror).
    pub git_dir: &'a Path,
    /// The working copy directory.
    pub worktree: &'a Path,
    /// The index of this working copy (its tracked set).
    pub index: &'a Path,
    /// The tree to make the working copy match; `None` empties it of tracked files.
    pub tree: Option<&'a GitId>,
}

/// How a file differs between two trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Only in the new tree.
    Added,
    /// In both, different.
    Modified,
    /// Only in the old tree.
    Deleted,
}

/// One differing file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeChange {
    /// How it differs.
    pub kind: ChangeKind,
    /// Path relative to the tree root.
    pub path: String,
}

/// Everything sync does with git objects besides snapshotting.
pub trait PackTransfer: Send + Sync {
    /// Creates a bare repository at `dir` if there is none.
    fn init_mirror(&self, dir: &Path) -> Result<(), TransferError>;
    /// True when the object exists in `git_dir` (alternates included).
    fn has_object(&self, git_dir: &Path, id: &GitId) -> bool;
    /// The tree of a commit.
    fn commit_tree(&self, git_dir: &Path, commit: &GitId) -> Option<GitId>;
    /// Refs under `prefix` with the ids they point at.
    fn list_refs(
        &self,
        git_dir: &Path,
        prefix: &str,
    ) -> Result<Vec<(String, GitId)>, TransferError>;
    /// Sets and deletes refs in one transaction.
    fn update_refs(
        &self,
        git_dir: &Path,
        set: &[(String, GitId)],
        delete: &[String],
    ) -> Result<(), TransferError>;
    /// Streams a thin pack of `want` minus `have` into `out`; returns the bytes written.
    fn build_pack(&self, req: &PackRequest<'_>, out: &mut dyn Write) -> Result<u64, TransferError>;
    /// Indexes the pack file into `git_dir`, completing a thin pack from objects already there.
    fn receive_pack(&self, git_dir: &Path, pack: &Path) -> Result<(), TransferError>;
    /// Makes the working copy match the tree. Files that left the tree are removed only when
    /// the index lists them; untracked files (build outputs) are left alone.
    fn checkout_tree(&self, req: &CheckoutRequest<'_>) -> Result<(), TransferError>;
    /// Which of `paths` exist in `tree`.
    fn tree_contains(
        &self,
        git_dir: &Path,
        tree: &GitId,
        paths: &[String],
    ) -> Result<Vec<String>, TransferError>;
    /// Every file path in `tree`.
    fn tree_files(&self, git_dir: &Path, tree: &GitId) -> Result<Vec<String>, TransferError>;
    /// The files that differ going from `old` (or an empty tree) to `new`, without rename
    /// detection.
    fn diff_tree(
        &self,
        git_dir: &Path,
        old: Option<&GitId>,
        new: &GitId,
    ) -> Result<Vec<TreeChange>, TransferError>;
    /// Repacks and prunes `git_dir`; objects younger than `prune_age` (`1.day.ago`) stay.
    fn gc(&self, git_dir: &Path, prune_age: &str) -> Result<(), TransferError>;
}

/// The path of a tasks-owned directory for `repo` under `state`.
pub fn repo_dir(state: &Path, repo: &crate::ids::CheckoutId) -> PathBuf {
    state.join("sync").join(repo.as_str())
}
