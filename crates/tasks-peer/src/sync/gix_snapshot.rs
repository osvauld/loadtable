//! [`GixSnapshot`]: the working tree as a git tree, read with gix.
//!
//! What it does, step by step:
//!
//! 1. Opens the user's repo read-only and reads its index `U` (the tracked set).
//! 2. Walks the working tree for untracked, non-ignored files (gix's ignore-aware directory
//!    walk against `U`). Together with `U` that is the exact file set `git add -A` would see.
//! 3. For each file it takes one `lstat` and compares it with the snapshot cache `P`, the index
//!    written by the previous snapshot of this checkout (and, failing that, with `U`). A stat
//!    that matches means the file is not opened; only the rest are read and hashed. Stat caches
//!    treat an entry as racy when its mtime is not older than the index file, as git does.
//! 4. New blobs and trees are written into a tasks-owned bare repository whose
//!    `objects/info/alternates` names the user's object directory, so existing objects are
//!    not written again and the user's index, refs, stash and objects are never touched.
//! 5. `P` is rewritten (only the tasks-owned copy).
//!
//! Content is hashed as the exact bytes on disk: no clean filters, no line-ending conversion.
//! Gaps in gix this works around are named in the comments where they apply.

use super::extras::symlink_refusal;
use super::paths::check_rel_path;
use super::refuse::{Refusals, RefuseReason, Refused};
use super::secrets::{secret_content, secret_name};
use super::snapshot::{
    PruneStats, Snapshot, SnapshotError, SnapshotIo, SnapshotRequest, TreeSnapshot,
};
use super::transfer::repo_dir;
use crate::ids::{CheckoutId, GitId};
use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};
use gix::index::entry::{Flags, Mode, Stat};
use gix::objs::tree::{Entry as TreeEntry, EntryKind, EntryMode};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

/// Most refusals collected before the snapshot stops looking.
const MAX_REFUSALS: usize = 200;
/// Files at most this big are checked for being an LFS pointer when they are not served from
/// the snapshot cache.
const LFS_POINTER_MAX: u64 = 1024;
const LFS_MAGIC: &[u8] = b"version https://git-lfs.github.com/spec/v1";

/// Snapshots with gix. State lives under `<state>/sync/<repo>/`.
#[derive(Debug, Clone)]
pub struct GixSnapshot {
    state: PathBuf,
}

fn store(e: impl std::fmt::Display) -> SnapshotError {
    SnapshotError::Store(e.to_string())
}

fn gid(id: ObjectId) -> Result<GitId, SnapshotError> {
    GitId::parse(&id.to_string()).map_err(store)
}

fn oid(id: &GitId) -> Result<ObjectId, SnapshotError> {
    ObjectId::from_hex(id.as_str().as_bytes()).map_err(store)
}

/// What an index entry knows about a file.
#[derive(Clone, Copy)]
struct Known {
    stat: Stat,
    id: ObjectId,
    mode: Mode,
}

/// One index read into lookup form.
struct Cache {
    known: HashMap<BString, Known>,
    /// Unmerged paths: always re-read.
    conflicted: HashSet<BString>,
    /// `(secs, nanos)` of the index file; entries not older than this are racy.
    stamp: (i64, u32),
    bytes: u64,
}

impl Cache {
    fn empty() -> Cache {
        Cache {
            known: HashMap::new(),
            conflicted: HashSet::new(),
            stamp: (0, 0),
            bytes: 0,
        }
    }

    fn racy(&self, stat: &Stat) -> bool {
        (self.stamp.0, self.stamp.1) <= (i64::from(stat.mtime.secs), stat.mtime.nsecs)
    }

    /// A usable object id for a file with this stat and mode, when the cache has one.
    fn lookup(&self, path: &BString, stat: &Stat, mode: Mode) -> Option<ObjectId> {
        if self.conflicted.contains(path) {
            return None;
        }
        let k = self.known.get(path)?;
        (k.mode == mode && k.stat == *stat && !self.racy(stat)).then_some(k.id)
    }
}

fn cache_of(file: &gix::index::File, bytes: u64) -> (Cache, Vec<BString>, Vec<BString>) {
    let mut c = Cache::empty();
    c.bytes = bytes;
    c.stamp = (
        file.timestamp().unix_seconds(),
        file.timestamp().nanoseconds(),
    );
    let mut tracked = Vec::new();
    let mut submodules = Vec::new();
    for e in file.entries() {
        let path = e.path(file).to_owned();
        if e.flags.contains(Flags::SKIP_WORKTREE) {
            continue;
        }
        if e.stage() != gix::index::entry::Stage::Unconflicted {
            c.conflicted.insert(path.clone());
            tracked.push(path);
            continue;
        }
        if e.mode == Mode::COMMIT {
            submodules.push(path);
            continue;
        }
        if e.flags.contains(Flags::INTENT_TO_ADD) {
            c.conflicted.insert(path.clone());
        } else {
            c.known.insert(
                path.clone(),
                Known {
                    stat: e.stat,
                    id: e.id,
                    mode: e.mode,
                },
            );
        }
        tracked.push(path);
    }
    tracked.sort();
    tracked.dedup();
    (c, tracked, submodules)
}

/// A file's entry in the new tree.
struct NewEntry {
    stat: Stat,
    id: ObjectId,
    mode: Mode,
}

enum Reading {
    Gone,
    Entry(NewEntry),
}

fn rel_of(path: &BString) -> Result<String, Refused> {
    String::from_utf8(path.to_vec()).map_err(|_| Refused {
        path: path.to_str_lossy().into_owned(),
        reason: RefuseReason::BadPath("not valid UTF-8".to_string()),
    })
}

fn tree_mode(mode: Mode) -> EntryMode {
    if mode == Mode::SYMLINK {
        EntryKind::Link.into()
    } else if mode == Mode::FILE_EXECUTABLE {
        EntryKind::BlobExecutable.into()
    } else {
        EntryKind::Blob.into()
    }
}

#[derive(Default)]
struct Dir {
    files: Vec<(String, Mode, ObjectId)>,
    dirs: BTreeMap<String, Dir>,
}

fn insert(dir: &mut Dir, comps: &[&str], mode: Mode, id: ObjectId) {
    match comps {
        [] => {}
        [name] => dir.files.push(((*name).to_string(), mode, id)),
        [first, rest @ ..] => insert(
            dir.dirs.entry((*first).to_string()).or_default(),
            rest,
            mode,
            id,
        ),
    }
}

fn write_dir(repo: &gix::Repository, dir: &Dir) -> Result<ObjectId, SnapshotError> {
    let mut entries: Vec<TreeEntry> = Vec::with_capacity(dir.files.len() + dir.dirs.len());
    for (name, mode, id) in &dir.files {
        entries.push(TreeEntry {
            mode: tree_mode(*mode),
            filename: BString::from(name.as_str()),
            oid: *id,
        });
    }
    for (name, sub) in &dir.dirs {
        entries.push(TreeEntry {
            mode: EntryKind::Tree.into(),
            filename: BString::from(name.as_str()),
            oid: write_dir(repo, sub)?,
        });
    }
    entries.sort();
    let tree = gix::objs::Tree { entries };
    Ok(repo.write_object(&tree).map_err(store)?.detach())
}

/// Writes `files` (path, mode, blob) as nested trees and returns the root tree id.
fn write_trees(
    repo: &gix::Repository,
    files: &BTreeMap<String, (Mode, ObjectId)>,
) -> Result<ObjectId, SnapshotError> {
    let mut root = Dir::default();
    for (path, (mode, id)) in files {
        let comps: Vec<&str> = path.split('/').collect();
        insert(&mut root, &comps, *mode, *id);
    }
    write_dir(repo, &root)
}

fn entry_map(
    repo: &gix::Repository,
    id: ObjectId,
) -> Result<BTreeMap<Vec<u8>, (EntryMode, ObjectId)>, SnapshotError> {
    let tree = repo.find_tree(id).map_err(store)?;
    let decoded = tree.decode().map_err(store)?;
    Ok(decoded
        .entries
        .iter()
        .map(|e| (e.filename.to_vec(), (e.mode, e.oid.to_owned())))
        .collect())
}

fn count_leaves(repo: &gix::Repository, id: ObjectId) -> Result<u32, SnapshotError> {
    let mut n = 0u32;
    for (_, (mode, child)) in entry_map(repo, id)? {
        n = n.saturating_add(if mode.is_tree() {
            count_leaves(repo, child)?
        } else {
            1
        });
    }
    Ok(n)
}

/// Files that differ between two trees (both may be absent).
fn count_diff(
    repo: &gix::Repository,
    a: Option<ObjectId>,
    b: Option<ObjectId>,
) -> Result<u32, SnapshotError> {
    let (ma, mb) = match (a, b) {
        (None, None) => return Ok(0),
        (Some(x), None) | (None, Some(x)) => return count_leaves(repo, x),
        (Some(x), Some(y)) if x == y => return Ok(0),
        (Some(x), Some(y)) => (entry_map(repo, x)?, entry_map(repo, y)?),
    };
    let names: std::collections::BTreeSet<&Vec<u8>> = ma.keys().chain(mb.keys()).collect();
    let mut n = 0u32;
    for name in names {
        let (l, r) = (ma.get(name), mb.get(name));
        n = n.saturating_add(match (l, r) {
            (Some((lm, li)), Some((rm, ri))) => {
                if lm == rm && li == ri {
                    0
                } else if lm.is_tree() && rm.is_tree() {
                    count_diff(repo, Some(*li), Some(*ri))?
                } else if lm.is_tree() {
                    count_leaves(repo, *li)?.saturating_add(1)
                } else if rm.is_tree() {
                    count_leaves(repo, *ri)?.saturating_add(1)
                } else {
                    1
                }
            }
            (Some((m, i)), None) | (None, Some((m, i))) => {
                if m.is_tree() {
                    count_leaves(repo, *i)?
                } else {
                    1
                }
            }
            (None, None) => 0,
        });
    }
    Ok(n)
}

fn index_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

impl GixSnapshot {
    /// A snapshotter keeping its object stores and stat caches under `state_dir`.
    pub fn new(state_dir: impl Into<PathBuf>) -> GixSnapshot {
        GixSnapshot {
            state: state_dir.into(),
        }
    }

    /// The tasks-owned bare repository of a repo, created on first use with the user's object
    /// directory as its alternate.
    fn tasks_repo(
        &self,
        dir: &Path,
        user_objects: &Path,
    ) -> Result<gix::Repository, SnapshotError> {
        let git = dir.join("git");
        if !git.join("HEAD").is_file() {
            std::fs::create_dir_all(dir)?;
            gix::init_bare(&git).map_err(store)?;
        }
        let alt = git.join("objects").join("info").join("alternates");
        let want = format!("{}\n", user_objects.display());
        if std::fs::read_to_string(&alt).ok().as_deref() != Some(want.as_str()) {
            if let Some(p) = alt.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::write(&alt, want)?;
        }
        gix::open(&git).map_err(store)
    }

    /// Opens the user's repo, returning it with its top directory, repo id and object directory.
    fn open_repo(
        &self,
        root: &Path,
        host: &str,
    ) -> Result<(gix::Repository, PathBuf, CheckoutId, PathBuf), SnapshotError> {
        let not_repo = |detail: String| SnapshotError::NotARepo {
            path: root.to_path_buf(),
            detail,
        };
        let canon = std::fs::canonicalize(root).map_err(|e| not_repo(e.to_string()))?;
        let repo = gix::open(&canon).map_err(|e| not_repo(e.to_string()))?;
        let workdir = repo
            .workdir()
            .ok_or_else(|| not_repo("the repository has no working tree".to_string()))?;
        let workdir = std::fs::canonicalize(workdir).map_err(|e| not_repo(e.to_string()))?;
        if workdir != canon {
            return Err(not_repo(format!(
                "the checkout root is not the top of its working tree ({})",
                workdir.display()
            )));
        }
        let common =
            std::fs::canonicalize(repo.common_dir()).map_err(|e| not_repo(e.to_string()))?;
        let id = CheckoutId::derive(host, &common);
        let objects = common.join("objects");
        Ok((repo, workdir, id, objects))
    }
}

/// Reads and hashes one path. `untracked` turns on the secret checks.
struct Ctx<'a> {
    repo: &'a gix::Repository,
    workdir: &'a Path,
    limits: &'a super::limits::SyncLimits,
    p: &'a Cache,
    u: &'a Cache,
    io: SnapshotIo,
    refused: Vec<Refused>,
    /// Entries whose mtime is this recent (seconds) are smudged in the cache.
    smudge_from: u32,
}

impl Ctx<'_> {
    fn refuse(&mut self, path: &str, reason: RefuseReason) {
        if self.refused.len() < MAX_REFUSALS {
            self.refused.push(Refused {
                path: path.to_string(),
                reason,
            });
        }
    }

    fn write_blob(&mut self, bytes: &[u8]) -> Result<ObjectId, SnapshotError> {
        let id = gix::objs::compute_hash(self.repo.object_hash(), gix::objs::Kind::Blob, bytes)
            .map_err(store)?;
        if !self.repo.has_object(id) {
            self.io.objects_written += 1;
        }
        Ok(self.repo.write_blob(bytes).map_err(store)?.detach())
    }

    /// One file of the tree. `Ok(None)` means it was refused (already recorded).
    fn read(&mut self, path: &BString, untracked: bool) -> Result<Option<Reading>, SnapshotError> {
        let rel = match rel_of(path) {
            Ok(r) => r,
            Err(r) => {
                self.refuse(&r.path, r.reason);
                return Ok(None);
            }
        };
        if let Err(why) = check_rel_path(&rel) {
            self.refuse(&rel, RefuseReason::BadPath(why));
            return Ok(None);
        }
        if untracked && let Some(kind) = secret_name(&rel) {
            self.refuse(&rel, RefuseReason::SecretName(kind));
            return Ok(None);
        }
        let abs = self
            .workdir
            .join(std::ffi::OsStr::from_bytes(path.as_bytes()));
        let meta = match gix::index::fs::Metadata::from_path_no_follow(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Some(Reading::Gone)),
            Err(e) => {
                self.refuse(&rel, RefuseReason::Unreadable(e.to_string()));
                return Ok(None);
            }
        };
        self.io.files_checked += 1;
        if meta.is_dir() {
            // a tracked file replaced by a directory: its files come from the walk
            return Ok(Some(Reading::Gone));
        }
        let mode = if meta.is_symlink() {
            Mode::SYMLINK
        } else if meta.is_file() {
            if meta.is_executable() {
                Mode::FILE_EXECUTABLE
            } else {
                Mode::FILE
            }
        } else {
            self.refuse(&rel, RefuseReason::NotRegular);
            return Ok(None);
        };
        if meta.len() > self.limits.max_file_bytes && mode != Mode::SYMLINK {
            self.refuse(
                &rel,
                RefuseReason::TooLarge {
                    size: meta.len(),
                    limit: self.limits.max_file_bytes,
                },
            );
            return Ok(None);
        }
        let mut stat = Stat::from_fs(&meta).map_err(store)?;
        // our own cache first (already vetted), then the user's index
        let cached_p = self.p.lookup(path, &stat, mode);
        let cached = cached_p.or_else(|| self.u.lookup(path, &stat, mode));
        let id = match cached {
            Some(id) if cached_p.is_some() || meta.len() > LFS_POINTER_MAX => id,
            // from the user's index: only tiny files could still be LFS pointers, so those
            // (a few hundred bytes at most) are read once
            Some(id) => {
                let bytes = self.read_bytes(&abs, &rel, mode)?;
                self.io.files_read += 1;
                self.io.bytes_read += bytes.len() as u64;
                if bytes.starts_with(LFS_MAGIC) {
                    self.refuse(&rel, RefuseReason::LfsPointer);
                    return Ok(None);
                }
                id
            }
            None => {
                let bytes = self.read_bytes(&abs, &rel, mode)?;
                self.io.files_read += 1;
                self.io.bytes_read += bytes.len() as u64;
                if bytes.len() as u64 > self.limits.max_file_bytes && mode != Mode::SYMLINK {
                    self.refuse(
                        &rel,
                        RefuseReason::TooLarge {
                            size: bytes.len() as u64,
                            limit: self.limits.max_file_bytes,
                        },
                    );
                    return Ok(None);
                }
                if mode != Mode::SYMLINK && bytes.starts_with(LFS_MAGIC) {
                    self.refuse(&rel, RefuseReason::LfsPointer);
                    return Ok(None);
                }
                if untracked && let Some(kind) = secret_content(&bytes) {
                    self.refuse(&rel, RefuseReason::SecretContent(kind));
                    return Ok(None);
                }
                self.write_blob(&bytes)?
            }
        };
        // git's rule for racily clean entries: a stat this new may hide a later write in the
        // same clock tick, so the cache records it as unknown and the next snapshot re-reads it
        if stat.mtime.secs >= self.smudge_from {
            stat.size = 0;
        }
        Ok(Some(Reading::Entry(NewEntry { stat, id, mode })))
    }

    fn read_bytes(&self, abs: &Path, rel: &str, mode: Mode) -> Result<Vec<u8>, SnapshotError> {
        let res = if mode == Mode::SYMLINK {
            std::fs::read_link(abs).map(|t| t.as_os_str().as_bytes().to_vec())
        } else {
            std::fs::read(abs)
        };
        res.map_err(|e| SnapshotError::Io(std::io::Error::new(e.kind(), format!("{rel}: {e}"))))
    }
}

impl Snapshot for GixSnapshot {
    fn snapshot(&self, req: &SnapshotRequest<'_>) -> Result<TreeSnapshot, SnapshotError> {
        let (user, workdir, repo_id, user_objects) = self.open_repo(req.root, req.origin_host)?;
        let dir = repo_dir(&self.state, &repo_id);
        let tasks = self.tasks_repo(&dir, &user_objects)?;
        let kind = user.object_hash();

        // the user's index: the tracked set, and a second stat cache
        let u_index = user.index_or_empty().map_err(store)?;
        let u_file: &gix::index::File = &u_index;
        let (u_cache, tracked, submodules) = cache_of(u_file, index_len(&user.index_path()));

        // our cache from the last snapshot of this checkout
        let p_path = dir.join(format!("{}.index", req.checkout));
        let p_cache =
            match gix::index::File::at(&p_path, kind, true, gix::index::decode::Options::default())
            {
                Ok(f) => cache_of(&f, index_len(&p_path)).0,
                Err(_) => Cache::empty(),
            };

        // untracked, non-ignored files: gix's walk wants the tracked entries flagged up to date
        let mut walk_index = u_file.clone();
        for e in walk_index.entries_mut() {
            e.flags.insert(Flags::UPTODATE);
        }
        let mut collect = gix::dir::walk::delegate::Collect::default();
        let opts = user
            .dirwalk_options()
            .map_err(store)?
            .emit_untracked(gix::dir::walk::EmissionMode::Matching);
        let stop = AtomicBool::new(false);
        user.dirwalk(
            &walk_index,
            Vec::<BString>::new(),
            &stop,
            opts,
            &mut collect,
        )
        .map_err(store)?;

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut cx = Ctx {
            repo: &tasks,
            workdir: &workdir,
            limits: req.limits,
            p: &p_cache,
            u: &u_cache,
            io: SnapshotIo {
                index_bytes_read: u_cache.bytes + p_cache.bytes,
                ..SnapshotIo::default()
            },
            refused: Vec::new(),
            smudge_from: u32::try_from(now.saturating_sub(1)).unwrap_or(u32::MAX),
        };
        for s in &submodules {
            let rel = s.to_str_lossy().into_owned();
            cx.refuse(&rel, RefuseReason::Submodule);
        }
        let mut untracked: Vec<BString> = Vec::new();
        for (entry, _) in collect.into_entries_by_path() {
            if !matches!(entry.status, gix::dir::entry::Status::Untracked) {
                continue;
            }
            match entry.disk_kind {
                Some(gix::dir::entry::Kind::File | gix::dir::entry::Kind::Symlink) => {
                    untracked.push(entry.rela_path)
                }
                Some(gix::dir::entry::Kind::Repository) => {
                    let rel = entry.rela_path.to_str_lossy().into_owned();
                    cx.refuse(&rel, RefuseReason::NestedRepository);
                }
                Some(gix::dir::entry::Kind::Untrackable) => {
                    let rel = entry.rela_path.to_str_lossy().into_owned();
                    cx.refuse(&rel, RefuseReason::NotRegular);
                }
                Some(gix::dir::entry::Kind::Directory) | None => {}
            }
        }
        let total = tracked.len() + untracked.len();
        if total > req.limits.max_files {
            return Err(SnapshotError::TooManyFiles {
                count: total,
                limit: req.limits.max_files,
            });
        }

        let mut new_entries: BTreeMap<String, NewEntry> = BTreeMap::new();
        for (list, is_untracked) in [(&tracked, false), (&untracked, true)] {
            for path in list {
                if let Some(Reading::Entry(e)) = cx.read(path, is_untracked)? {
                    new_entries.insert(path.to_str_lossy().into_owned(), e);
                }
            }
        }

        // allowlisted extras: ignored files that must travel
        let mut extra_blobs: BTreeMap<String, (Mode, ObjectId)> = BTreeMap::new();
        for rel in req.extras.candidates(&workdir) {
            if new_entries.contains_key(&rel) {
                continue; // already part of the tree
            }
            if extra_blobs.len() >= req.limits.max_extra_files {
                return Err(SnapshotError::BadExtras(format!(
                    "more than {} extra files match the allowlist",
                    req.limits.max_extra_files
                )));
            }
            let abs = super::paths::join(&workdir, &rel);
            let meta = match std::fs::symlink_metadata(&abs) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                let r = symlink_refusal(&rel);
                cx.refuse(&r.path, r.reason);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            if let Some(kind) = secret_name(&rel) {
                cx.refuse(&rel, RefuseReason::SecretName(kind));
                continue;
            }
            if meta.len() > req.limits.max_file_bytes {
                cx.refuse(
                    &rel,
                    RefuseReason::TooLarge {
                        size: meta.len(),
                        limit: req.limits.max_file_bytes,
                    },
                );
                continue;
            }
            let bytes = std::fs::read(&abs)?;
            cx.io.files_read += 1;
            cx.io.bytes_read += bytes.len() as u64;
            if let Some(kind) = secret_content(&bytes) {
                cx.refuse(&rel, RefuseReason::SecretContent(kind));
                continue;
            }
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 != 0 {
                    Mode::FILE_EXECUTABLE
                } else {
                    Mode::FILE
                }
            };
            let id = cx.write_blob(&bytes)?;
            extra_blobs.insert(rel, (mode, id));
        }

        if !cx.refused.is_empty() {
            return Err(SnapshotError::Refused(Refusals(cx.refused)));
        }
        let mut io = cx.io;

        let tree_files: BTreeMap<String, (Mode, ObjectId)> = new_entries
            .iter()
            .map(|(p, e)| (p.clone(), (e.mode, e.id)))
            .collect();
        let tree = write_trees(&tasks, &tree_files)?;
        let extras_tree = if extra_blobs.is_empty() {
            None
        } else {
            Some(write_trees(&tasks, &extra_blobs)?)
        };

        // The new stat cache; written only after every check above passed, and only when it
        // differs from the old one (an unchanged re-sync writes nothing).
        let unchanged = p_cache.known.len() == new_entries.len()
            && new_entries.iter().all(|(path, e)| {
                p_cache
                    .known
                    .get(&BString::from(path.as_str()))
                    .is_some_and(|k| k.id == e.id && k.mode == e.mode && k.stat == e.stat)
            });
        if !unchanged {
            let mut state = gix::index::State::new(kind);
            for (path, e) in &new_entries {
                state.dangerously_push_entry(
                    e.stat,
                    e.id,
                    Flags::empty(),
                    e.mode,
                    path.as_bytes().as_bstr(),
                );
            }
            state.sort_entries();
            let mut p_file = gix::index::File::from_state(state, &p_path);
            std::fs::create_dir_all(&dir)?;
            p_file
                .write(gix::index::write::Options {
                    extensions: gix::index::write::Extensions::None,
                    skip_hash: true,
                })
                .map_err(store)?;
            io.index_bytes_written = index_len(&p_path);
        }

        let head_tree = user
            .head_commit()
            .ok()
            .and_then(|c| c.tree_id().ok())
            .map(|t| t.detach());
        let base = user.head_id().ok().map(|h| h.detach());
        let dirty = count_diff(&tasks, head_tree, Some(tree))?;

        Ok(TreeSnapshot {
            repo: repo_id,
            git_dir: dir.join("git"),
            tree: gid(tree)?,
            extras_tree: extras_tree.map(gid).transpose()?,
            base: base.map(gid).transpose()?,
            dirty_files: dirty,
            files: new_entries.len() as u64,
            extra_files: extra_blobs.keys().cloned().collect(),
            io,
        })
    }

    fn seal(
        &self,
        snap: &TreeSnapshot,
        parent: Option<&GitId>,
        now_secs: i64,
    ) -> Result<GitId, SnapshotError> {
        let tasks = gix::open(&snap.git_dir).map_err(store)?;
        let mut raw = format!("tree {}\n", snap.tree);
        if let Some(p) = parent {
            raw.push_str(&format!("parent {p}\n"));
        }
        raw.push_str(&format!(
            "author tasks <tasks@localhost> {now_secs} +0000\ncommitter tasks <tasks@localhost> {now_secs} +0000\n\nsync snapshot\n"
        ));
        let commit = gix::objs::CommitRef::from_bytes(raw.as_bytes()).map_err(store)?;
        let id = tasks.write_object(&commit).map_err(store)?.detach();
        gid(id)
    }

    fn prune(
        &self,
        repo: &CheckoutId,
        keep: &[GitId],
        min_age: Duration,
    ) -> Result<PruneStats, SnapshotError> {
        let git = repo_dir(&self.state, repo).join("git");
        if !git.join("HEAD").is_file() {
            return Ok(PruneStats::default());
        }
        let tasks = gix::open(&git).map_err(store)?;
        let mut reachable: HashSet<ObjectId> = HashSet::new();
        let mut visited: HashSet<ObjectId> = HashSet::new();
        let mut stack: Vec<ObjectId> = Vec::new();
        for k in keep {
            let id = oid(k)?;
            let Ok(obj) = tasks.find_object(id) else {
                continue;
            };
            reachable.insert(id);
            match obj.kind {
                gix::objs::Kind::Commit => {
                    // the commit's tree only: its parents are history nobody needs
                    if let Ok(t) = obj.try_into_commit().map_err(store)?.tree_id() {
                        stack.push(t.detach());
                    }
                }
                gix::objs::Kind::Tree => stack.push(id),
                _ => {}
            }
        }
        while let Some(t) = stack.pop() {
            if !visited.insert(t) {
                continue;
            }
            reachable.insert(t);
            for (_, (mode, child)) in entry_map(&tasks, t)? {
                if mode.is_tree() {
                    stack.push(child);
                } else if !mode.is_commit() {
                    reachable.insert(child);
                }
            }
        }
        let mut stats = PruneStats::default();
        let objects = git.join("objects");
        let now = SystemTime::now();
        for d in std::fs::read_dir(&objects)?.flatten() {
            let name = d.file_name().to_string_lossy().into_owned();
            if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            for f in std::fs::read_dir(d.path())?.flatten() {
                let Ok(id) = ObjectId::from_hex(
                    format!("{name}{}", f.file_name().to_string_lossy()).as_bytes(),
                ) else {
                    continue;
                };
                if reachable.contains(&id) {
                    continue;
                }
                let Ok(meta) = f.metadata() else { continue };
                let old = meta
                    .modified()
                    .ok()
                    .and_then(|m| now.duration_since(m).ok())
                    .is_some_and(|age| age >= min_age);
                if old && std::fs::remove_file(f.path()).is_ok() {
                    stats.objects += 1;
                    stats.bytes += meta.len();
                }
            }
            let _ = std::fs::remove_dir(d.path()); // only succeeds when empty
        }
        Ok(stats)
    }
}
