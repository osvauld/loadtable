//! The receiving side: a bare mirror per (peer, repo), a working copy per (peer, checkout).
//!
//! Layout under the store root:
//!
//! ```text
//! <peer>/mirrors/<repo>.git        bare repository: every pack received for the repo
//! <peer>/<checkout>/tree/          the working copy a job runs in
//! <peer>/<checkout>/index          the working copy's tracked set (a git index)
//! <peer>/<checkout>/index.extras   the tracked set of the allowlisted extras
//! <peer>/<checkout>/state.json     what was applied, and when it was last used
//! <peer>/<checkout>/incoming/      the pack being received
//! ```
//!
//! One copy per (authenticated peer, origin host + checkout path): a peer can never touch
//! another peer's copy, whatever ids it sends. The working copy is updated with
//! `read-tree --reset -u`, so a file leaves it only when the previous tree listed it: build
//! outputs stay, and a file a job modified is restored.

use super::transfer::{ChangeKind, CheckoutRequest, PackTransfer};
use super::{NoObserver, OpenGate, SyncEvent, SyncGate, SyncLimits, SyncObserver, SyncPhase};
use crate::ids::{CheckoutId, GitId, PeerId};
use crate::proto::{MAX_CHUNK_BYTES, MAX_SYNC_TIPS, Refusal, SyncApply, SyncSummary, SyncTip};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Working copies kept per checkout in the mirror, newest first.
const KEEP_SNAPSHOTS: usize = 8;

/// What the store remembers about one working copy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct CopyState {
    pub repo: String,
    pub tree: Option<String>,
    pub extras_tree: Option<String>,
    pub extras: Vec<String>,
    /// True while an apply is in progress or failed: the copy is not usable.
    pub applying: bool,
    pub applied_ms: i64,
    pub last_used_ms: i64,
}

fn internal(what: &str, e: impl std::fmt::Display) -> Refusal {
    Refusal::Internal {
        detail: format!("{what}: {e}"),
    }
}

fn parse_id(s: &Option<String>) -> Option<GitId> {
    s.as_deref().and_then(|t| GitId::parse(t).ok())
}

/// The receiver-side store of synced checkouts.
pub struct SyncStore {
    pub(super) root: PathBuf,
    pub(super) limits: SyncLimits,
    pub(super) transfer: Arc<dyn PackTransfer>,
    pub(super) observer: Arc<dyn SyncObserver>,
    gate: Arc<dyn SyncGate>,
    pub(super) clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

fn system_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

impl SyncStore {
    /// A store under `root`.
    pub fn new(
        root: impl Into<PathBuf>,
        limits: SyncLimits,
        transfer: Arc<dyn PackTransfer>,
    ) -> SyncStore {
        SyncStore {
            root: root.into(),
            limits,
            transfer,
            observer: Arc::new(NoObserver),
            gate: Arc::new(OpenGate),
            clock: Arc::new(system_ms),
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Reports received syncs and retention deletions to `observer`.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn SyncObserver>) -> SyncStore {
        self.observer = observer;
        self
    }

    /// Takes `gate` around applying a sync (the step-4 I/O gate hook).
    #[must_use]
    pub fn with_gate(mut self, gate: Arc<dyn SyncGate>) -> SyncStore {
        self.gate = gate;
        self
    }

    /// Reads time (unix milliseconds) from `clock`; for tests.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> SyncStore {
        self.clock = clock;
        self
    }

    pub(super) fn peer_dir(&self, peer: &PeerId) -> PathBuf {
        self.root.join(peer.as_str())
    }

    pub(super) fn checkout_dir(&self, peer: &PeerId, checkout: &CheckoutId) -> PathBuf {
        self.peer_dir(peer).join(checkout.as_str())
    }

    pub(super) fn mirrors_dir(&self, peer: &PeerId) -> PathBuf {
        self.peer_dir(peer).join("mirrors")
    }

    pub(super) fn mirror_dir(&self, peer: &PeerId, repo: &CheckoutId) -> PathBuf {
        self.mirrors_dir(peer).join(format!("{repo}.git"))
    }

    /// The synced working copy for this peer and checkout.
    pub fn tree_root(&self, peer: &PeerId, checkout: &CheckoutId) -> PathBuf {
        self.checkout_dir(peer, checkout).join("tree")
    }

    pub(super) fn read_state(&self, dir: &Path) -> Option<CopyState> {
        serde_json::from_slice(&fs::read(dir.join("state.json")).ok()?).ok()
    }

    pub(super) fn write_state(&self, dir: &Path, st: &CopyState) -> Result<(), Refusal> {
        let json = serde_json::to_vec(st).map_err(|e| internal("encoding state", e))?;
        let tmp = dir.join("state.json.tmp");
        fs::write(&tmp, json)
            .and_then(|_| fs::rename(&tmp, dir.join("state.json")))
            .map_err(|e| internal("recording state", e))
    }

    /// True once a sync of this checkout has been applied whole.
    pub fn is_synced(&self, peer: &PeerId, checkout: &CheckoutId) -> bool {
        self.read_state(&self.checkout_dir(peer, checkout))
            .is_some_and(|s| !s.applying && s.tree.is_some())
    }

    /// The tree id applied by the last sync, if any.
    pub fn applied_tree(&self, peer: &PeerId, checkout: &CheckoutId) -> Option<GitId> {
        let st = self.read_state(&self.checkout_dir(peer, checkout))?;
        if st.applying {
            None
        } else {
            parse_id(&st.tree)
        }
    }

    /// Marks the copy as used now, so retention does not count it idle.
    pub fn touch(&self, peer: &PeerId, checkout: &CheckoutId) {
        let dir = self.checkout_dir(peer, checkout);
        if let Some(mut st) = self.read_state(&dir) {
            st.last_used_ms = (self.clock)();
            let _ = self.write_state(&dir, &st);
        }
    }

    fn lock_for(&self, peer: &PeerId, checkout: &CheckoutId) -> Arc<Mutex<()>> {
        let mut m = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(m.entry(format!("{peer}/{checkout}")).or_default())
    }

    /// The snapshots held in the mirror of `repo`, newest first.
    pub fn tips(&self, peer: &PeerId, repo: &CheckoutId) -> Result<Vec<SyncTip>, Refusal> {
        let mirror = self.mirror_dir(peer, repo);
        if !mirror.join("HEAD").is_file() {
            return Ok(Vec::new());
        }
        let refs = self
            .transfer
            .list_refs(&mirror, "refs/tasks/")
            .map_err(|e| internal("listing snapshots", e))?;
        let mut commits: BTreeMap<i64, (String, GitId)> = BTreeMap::new();
        let mut extras: HashMap<String, GitId> = HashMap::new();
        for (name, id) in refs {
            if let Some(stem) = name.strip_suffix(".extras") {
                extras.insert(stem.to_string(), id);
            } else if let Some(ms) = name.rsplit('/').next().and_then(|t| t.parse::<i64>().ok()) {
                commits.insert(ms, (name, id));
            }
        }
        let mut tips = Vec::new();
        for (_, (name, commit)) in commits.into_iter().rev().take(MAX_SYNC_TIPS) {
            let Some(tree) = self.transfer.commit_tree(&mirror, &commit) else {
                continue;
            };
            tips.push(SyncTip {
                commit,
                tree,
                extras: extras.get(&name).cloned(),
            });
        }
        Ok(tips)
    }

    /// Receives the next bytes of a pack. Returns the bytes held, which is the resume point.
    /// Offset 0 starts a new pack and drops any earlier one.
    pub fn put_chunk(
        &self,
        peer: &PeerId,
        checkout: &CheckoutId,
        session: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, Refusal> {
        if data.len() > MAX_CHUNK_BYTES {
            return Err(Refusal::LimitExceeded {
                detail: "chunk is over the chunk limit".to_string(),
            });
        }
        let dir = self.checkout_dir(peer, checkout).join("incoming");
        fs::create_dir_all(&dir).map_err(|e| internal("creating incoming", e))?;
        let part = dir.join(format!("{session}.pack"));
        if offset == 0 {
            // a new pack: anything left over from an earlier session is stale
            if let Ok(rd) = fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
        let held = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if offset != held {
            return Err(Refusal::OutOfOrder { expected: held });
        }
        let now = offset + data.len() as u64;
        if now > self.limits.max_pack_bytes {
            let _ = fs::remove_file(&part);
            return Err(Refusal::LimitExceeded {
                detail: format!("pack is over the {} byte limit", self.limits.max_pack_bytes),
            });
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&part)
            .map_err(|e| internal("opening pack", e))?;
        f.seek(SeekFrom::Start(offset))
            .and_then(|_| f.write_all(data))
            .map_err(|e| internal("writing pack", e))?;
        Ok(now)
    }

    /// Receives a whole pack from a raw stream into the session's spool file, replacing anything
    /// an earlier session left. At most `max_pack_bytes` are accepted; a longer stream is cut
    /// and refused, and the partial file is removed. Returns the bytes held. The length and
    /// SHA-256 are checked against `SyncApply` when it arrives, as for chunked packs.
    pub fn put_stream(
        &self,
        peer: &PeerId,
        checkout: &CheckoutId,
        session: u64,
        data: &mut dyn std::io::Read,
    ) -> Result<u64, Refusal> {
        let dir = self.checkout_dir(peer, checkout).join("incoming");
        fs::create_dir_all(&dir).map_err(|e| internal("creating incoming", e))?;
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                let _ = fs::remove_file(e.path());
            }
        }
        let part = dir.join(format!("{session}.pack"));
        let mut f = fs::File::create(&part).map_err(|e| internal("opening pack", e))?;
        let limit = self.limits.max_pack_bytes;
        // one byte over the limit is read so that "exactly the limit" is accepted
        let mut limited = std::io::Read::take(&mut *data, limit.saturating_add(1));
        let got = std::io::copy(&mut limited, &mut f).map_err(|e| {
            let _ = fs::remove_file(&part);
            internal("receiving pack", e)
        })?;
        if got > limit {
            let _ = fs::remove_file(&part);
            return Err(Refusal::LimitExceeded {
                detail: format!("pack is over the {limit} byte limit"),
            });
        }
        Ok(got)
    }

    /// Indexes the pack, checks the objects, and makes the working copy match the tree.
    pub fn apply(&self, peer: &PeerId, a: &SyncApply) -> Result<SyncSummary, Refusal> {
        let lock = self.lock_for(peer, &a.checkout);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let _gate = self.gate.enter(SyncPhase::Apply);
        let dir = self.checkout_dir(peer, &a.checkout);
        fs::create_dir_all(&dir).map_err(|e| internal("creating checkout", e))?;
        let mirror = self.mirror_dir(peer, &a.repo);
        let prev = self.read_state(&dir).unwrap_or_default();
        let prev_tree = if prev.applying {
            None
        } else {
            parse_id(&prev.tree)
        };

        // 1. the pack
        self.transfer
            .init_mirror(&mirror)
            .map_err(|e| internal("preparing mirror", e))?;
        // a previous tree the mirror no longer holds is a copy to rebuild from nothing
        let prev_tree = prev_tree.filter(|t| self.transfer.has_object(&mirror, t));
        let spool = dir.join("incoming").join(format!("{}.pack", a.session));
        if a.pack_bytes > 0 {
            let got = fs::metadata(&spool).map(|m| m.len()).unwrap_or(0);
            if got != a.pack_bytes {
                return Err(Refusal::BadPack {
                    detail: format!("received {got} of {} bytes", a.pack_bytes),
                });
            }
            let (hash, _) =
                super::paths::hash_file(&spool).map_err(|e| internal("reading pack", e))?;
            if Some(hash) != a.pack_sha256 {
                let _ = fs::remove_file(&spool);
                return Err(Refusal::HashMismatch);
            }
            self.transfer
                .receive_pack(&mirror, &spool)
                .map_err(|e| Refusal::BadPack {
                    detail: e.to_string(),
                })?;
            let _ = fs::remove_dir_all(dir.join("incoming"));
        }

        // 2. everything the tree needs is here
        for (what, id) in [
            ("tree", Some(&a.tree)),
            ("extras tree", a.extras_tree.as_ref()),
            ("commit", a.commit.as_ref()),
        ] {
            if let Some(id) = id
                && !self.transfer.has_object(&mirror, id)
            {
                return Err(Refusal::MissingObjects {
                    what: format!("{what} {}", id.short()),
                });
            }
        }

        // 3. keep the snapshot alive for later syncs and trim old ones
        let now = (self.clock)();
        self.retain_snapshot(peer, &mirror, a, now)?;

        // 4. extras must not overlap the tree
        let extra_files = match &a.extras_tree {
            Some(x) => {
                let files = self
                    .transfer
                    .tree_files(&mirror, x)
                    .map_err(|e| internal("listing extras", e))?;
                for f in &files {
                    if let Err(reason) = super::paths::check_rel_path(f) {
                        return Err(Refusal::BadPath {
                            path: f.clone(),
                            reason,
                        });
                    }
                }
                let clash = self
                    .transfer
                    .tree_contains(&mirror, &a.tree, &files)
                    .map_err(|e| internal("checking extras", e))?;
                if let Some(path) = clash.into_iter().next() {
                    return Err(Refusal::BadPath {
                        path,
                        reason: "an extra file is also part of the tree".to_string(),
                    });
                }
                files
            }
            None => Vec::new(),
        };

        // 5. the working copy. From here a failure leaves it marked unusable.
        let mut st = CopyState {
            repo: a.repo.to_string(),
            applying: true,
            applied_ms: prev.applied_ms,
            last_used_ms: now,
            ..prev.clone()
        };
        self.write_state(&dir, &st)?;
        let tree_dir = dir.join("tree");
        let changes = self
            .transfer
            .diff_tree(&mirror, prev_tree.as_ref(), &a.tree)
            .map_err(|e| internal("comparing trees", e))?;
        for c in &changes {
            if let Err(reason) = super::paths::check_rel_path(&c.path) {
                return Err(Refusal::BadPath {
                    path: c.path.clone(),
                    reason,
                });
            }
        }
        // untracked leftovers (a build output that is tracked now) would stop read-tree
        clear_collisions(
            &tree_dir,
            changes
                .iter()
                .filter(|c| c.kind == ChangeKind::Added)
                .map(|c| c.path.as_str()),
        );
        self.transfer
            .checkout_tree(&CheckoutRequest {
                git_dir: &mirror,
                worktree: &tree_dir,
                index: &dir.join("index"),
                tree: Some(&a.tree),
            })
            .map_err(|e| internal("updating the working copy", e))?;
        clear_collisions(
            &tree_dir,
            extra_files
                .iter()
                .map(String::as_str)
                .filter(|p| !prev.extras.contains(&(*p).to_string())),
        );
        self.transfer
            .checkout_tree(&CheckoutRequest {
                git_dir: &mirror,
                worktree: &tree_dir,
                index: &dir.join("index.extras"),
                tree: a.extras_tree.as_ref(),
            })
            .map_err(|e| internal("updating the extras", e))?;

        let removed = changes
            .iter()
            .filter(|c| c.kind == ChangeKind::Deleted)
            .count() as u64;
        let written = changes.len() as u64 - removed;
        st.tree = Some(a.tree.to_string());
        st.extras_tree = a.extras_tree.as_ref().map(ToString::to_string);
        st.extras = extra_files.clone();
        st.applying = false;
        st.applied_ms = now;
        self.write_state(&dir, &st)?;

        let summary = SyncSummary {
            tree: a.tree.clone(),
            files: a.files,
            written,
            unchanged: a.files.saturating_sub(written),
            removed,
            extras: extra_files.len() as u64,
            pack_bytes: a.pack_bytes,
        };
        self.observer.event(&SyncEvent::Received {
            peer: peer.clone(),
            checkout: a.checkout.clone(),
            tree: a.tree.clone(),
            bytes: a.pack_bytes,
            written,
            removed,
        });
        Ok(summary)
    }

    /// Points a ref of this checkout at the snapshot (a new commit, or the held commit whose
    /// tree is this one) and drops all but the newest few.
    fn retain_snapshot(
        &self,
        peer: &PeerId,
        mirror: &Path,
        a: &SyncApply,
        now: i64,
    ) -> Result<(), Refusal> {
        let commit = match &a.commit {
            Some(c) => Some(c.clone()),
            None => self
                .tips(peer, &a.repo)?
                .into_iter()
                .find(|t| t.tree == a.tree && t.extras == a.extras_tree)
                .map(|t| t.commit),
        };
        let prefix = format!("refs/tasks/{}/", a.checkout);
        let mut set = Vec::new();
        if let Some(c) = commit {
            let name = format!("{prefix}{now}");
            if let Some(x) = &a.extras_tree {
                set.push((format!("{name}.extras"), x.clone()));
            }
            set.push((name, c));
        }
        let mine = self
            .transfer
            .list_refs(mirror, &prefix)
            .map_err(|e| internal("listing snapshots", e))?;
        let stamp = |name: &str| -> Option<i64> {
            name.strip_suffix(".extras")
                .unwrap_or(name)
                .rsplit('/')
                .next()?
                .parse()
                .ok()
        };
        let mut stamps: Vec<i64> = mine.iter().filter_map(|(n, _)| stamp(n)).collect();
        stamps.sort_unstable_by(|x, y| y.cmp(x));
        stamps.dedup();
        // the snapshot being added takes one of the places
        let keep = KEEP_SNAPSHOTS.saturating_sub(1);
        let delete: Vec<String> = mine
            .iter()
            .filter(|(n, _)| {
                stamp(n).is_some_and(|ms| {
                    stamps
                        .iter()
                        .position(|s| *s == ms)
                        .is_some_and(|rank| rank >= keep)
                })
            })
            .map(|(n, _)| n.clone())
            .collect();
        self.transfer
            .update_refs(mirror, &set, &delete)
            .map_err(|e| internal("recording the snapshot", e))
    }
}

/// Removes untracked files and directories at paths the next tree is about to add, and files
/// or symlinks where it needs a directory, so `read-tree` can write them. Ancestors are looked
/// at without following symlinks, and unsafe paths are skipped (read-tree refuses them).
fn clear_collisions<'a>(tree: &Path, added: impl Iterator<Item = &'a str>) {
    'paths: for rel in added {
        if super::paths::check_rel_path(rel).is_err() {
            continue;
        }
        let mut cur = tree.to_path_buf();
        let comps: Vec<&str> = rel.split('/').collect();
        for c in comps.iter().take(comps.len().saturating_sub(1)) {
            cur.push(c);
            match fs::symlink_metadata(&cur) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => {
                    // a file or symlink where a directory is needed
                    let _ = fs::remove_file(&cur);
                    continue 'paths;
                }
                Err(_) => continue 'paths,
            }
        }
        let target = super::paths::join(tree, rel);
        match fs::symlink_metadata(&target) {
            Ok(m) if m.is_dir() => {
                let _ = fs::remove_dir_all(&target);
            }
            Ok(_) => {
                let _ = fs::remove_file(&target);
            }
            Err(_) => {}
        }
    }
}
