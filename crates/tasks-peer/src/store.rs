//! The receiver's copy of synced checkouts.
//!
//! Layout: `<root>/<peer>/<checkout>/{tree, incoming, stage, manifest.json}`.
//! One copy per (authenticated peer, origin host + checkout path): a peer can
//! never touch another peer's copy, whatever ids it sends. Sync is applied in
//! two steps: every changed file is first staged next to the tree and checked
//! complete, and only then moved into place, so a sync that is missing blobs
//! changes nothing. Files not in either manifest (build outputs) are left alone.

use crate::ids::{CheckoutId, Hash, PeerId};
use crate::manifest::{Manifest, check_rel_path, hash_file};
use crate::proto::{FileEntry, MAX_CHUNK_BYTES, MAX_HASHES_PER_REQUEST, Refusal, SyncSummary};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// Limits applied by the receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreLimits {
    /// Largest blob accepted.
    pub max_blob_bytes: u64,
    /// Most files in one manifest.
    pub max_files: usize,
}

impl Default for StoreLimits {
    fn default() -> Self {
        StoreLimits {
            max_blob_bytes: 256 * 1024 * 1024,
            max_files: 200_000,
        }
    }
}

type SessionKey = (PeerId, CheckoutId);

struct Session {
    id: u64,
    next_index: u32,
    entries: Vec<FileEntry>,
}

/// The receiver-side store of synced checkouts.
pub struct CheckoutStore {
    root: PathBuf,
    limits: StoreLimits,
    sessions: Mutex<HashMap<SessionKey, Session>>,
}

fn io_err(what: &str, e: std::io::Error) -> Refusal {
    Refusal::Internal {
        detail: format!("{what}: {e}"),
    }
}

fn bad_path(path: &str, reason: impl Into<String>) -> Refusal {
    Refusal::BadPath {
        path: path.to_string(),
        reason: reason.into(),
    }
}

impl CheckoutStore {
    /// A store under `root`.
    pub fn new(root: impl Into<PathBuf>, limits: StoreLimits) -> CheckoutStore {
        CheckoutStore {
            root: root.into(),
            limits,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionKey, Session>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn dir(&self, peer: &PeerId, checkout: &CheckoutId) -> PathBuf {
        self.root.join(peer.as_str()).join(checkout.as_str())
    }

    /// The synced working copy for this peer and checkout.
    pub fn tree_root(&self, peer: &PeerId, checkout: &CheckoutId) -> PathBuf {
        self.dir(peer, checkout).join("tree")
    }

    /// True once a sync of this checkout has been applied.
    pub fn is_synced(&self, peer: &PeerId, checkout: &CheckoutId) -> bool {
        self.dir(peer, checkout).join("manifest.json").is_file()
    }

    /// The manifest of the last applied sync.
    pub fn manifest(&self, peer: &PeerId, checkout: &CheckoutId) -> Manifest {
        let path = self.dir(peer, checkout).join("manifest.json");
        let Ok(raw) = fs::read(path) else {
            return Manifest::default();
        };
        let Ok(list) = serde_json::from_slice::<Vec<FileEntry>>(&raw) else {
            return Manifest::default();
        };
        Manifest {
            entries: list.into_iter().map(|e| (e.path.clone(), e)).collect(),
        }
    }

    fn incoming(&self, peer: &PeerId, checkout: &CheckoutId) -> PathBuf {
        self.dir(peer, checkout).join("incoming")
    }

    /// A readable file holding content `hash` that we already have, if any.
    fn source_of(
        &self,
        peer: &PeerId,
        checkout: &CheckoutId,
        by_hash: &BTreeMap<Hash, &FileEntry>,
        hash: &Hash,
    ) -> Option<PathBuf> {
        let blob = self.incoming(peer, checkout).join(hash.to_hex());
        if blob.is_file() {
            return Some(blob);
        }
        let e = by_hash.get(hash)?;
        let p = self.tree_root(peer, checkout).join(&e.path);
        let m = fs::symlink_metadata(&p).ok()?;
        (m.is_file() && m.len() == e.size).then_some(p)
    }

    /// Which of `hashes` can be produced here without being sent.
    pub fn have(
        &self,
        peer: &PeerId,
        checkout: &CheckoutId,
        hashes: &[Hash],
    ) -> Result<Vec<Hash>, Refusal> {
        if hashes.len() > MAX_HASHES_PER_REQUEST {
            return Err(Refusal::LimitExceeded {
                detail: format!("more than {MAX_HASHES_PER_REQUEST} hashes in one request"),
            });
        }
        let old = self.manifest(peer, checkout);
        let by_hash: BTreeMap<Hash, &FileEntry> =
            old.entries.values().map(|e| (e.hash, e)).collect();
        Ok(hashes
            .iter()
            .filter(|h| self.source_of(peer, checkout, &by_hash, h).is_some())
            .copied()
            .collect())
    }

    /// Receives one chunk of a blob. Returns the bytes held so far, which is
    /// the resume point. Offset 0 restarts; any other offset must continue.
    pub fn put_chunk(
        &self,
        peer: &PeerId,
        checkout: &CheckoutId,
        hash: &Hash,
        offset: u64,
        total: u64,
        data: &[u8],
    ) -> Result<u64, Refusal> {
        if total > self.limits.max_blob_bytes {
            return Err(Refusal::LimitExceeded {
                detail: format!(
                    "blob of {total} bytes is over the {} byte limit",
                    self.limits.max_blob_bytes
                ),
            });
        }
        if data.len() > MAX_CHUNK_BYTES {
            return Err(Refusal::LimitExceeded {
                detail: "chunk is over the chunk limit".to_string(),
            });
        }
        let dir = self.incoming(peer, checkout);
        fs::create_dir_all(&dir).map_err(|e| io_err("creating incoming", e))?;
        let done = dir.join(hash.to_hex());
        if done.is_file() {
            return Ok(total);
        }
        let part = dir.join(format!("{}.part", hash.to_hex()));
        let held = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if offset != 0 && offset != held {
            return Err(Refusal::OutOfOrder { expected: held });
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(offset == 0)
            .open(&part)
            .map_err(|e| io_err("opening blob", e))?;
        f.seek(SeekFrom::Start(offset))
            .and_then(|_| f.write_all(data))
            .map_err(|e| io_err("writing blob", e))?;
        let now = offset + data.len() as u64;
        if now > total {
            let _ = fs::remove_file(&part);
            return Err(Refusal::LimitExceeded {
                detail: "blob is longer than announced".to_string(),
            });
        }
        if now == total {
            drop(f);
            let ok = hash_file(&part).map(|(h, _)| h == *hash).unwrap_or(false);
            if !ok {
                let _ = fs::remove_file(&part);
                return Err(Refusal::HashMismatch);
            }
            fs::rename(&part, &done).map_err(|e| io_err("finishing blob", e))?;
        }
        Ok(now)
    }

    /// Takes one page of the manifest. The last page applies the sync and
    /// returns its summary; earlier pages return `None`.
    pub fn manifest_page(
        &self,
        peer: &PeerId,
        checkout: &CheckoutId,
        session: u64,
        index: u32,
        last: bool,
        entries: Vec<FileEntry>,
    ) -> Result<Option<SyncSummary>, Refusal> {
        let key = (peer.clone(), checkout.clone());
        let all = {
            let mut s = self.lock();
            if index == 0 {
                s.insert(
                    key.clone(),
                    Session {
                        id: session,
                        next_index: 0,
                        entries: Vec::new(),
                    },
                );
            }
            let cur = s.get_mut(&key).ok_or_else(|| Refusal::Malformed {
                detail: "manifest page without a first page".to_string(),
            })?;
            if cur.id != session || cur.next_index != index {
                s.remove(&key);
                return Err(Refusal::Malformed {
                    detail: "manifest pages out of order".to_string(),
                });
            }
            cur.next_index += 1;
            if cur.entries.len() + entries.len() > self.limits.max_files {
                s.remove(&key);
                return Err(Refusal::LimitExceeded {
                    detail: format!("more than {} files", self.limits.max_files),
                });
            }
            cur.entries.extend(entries);
            if !last {
                return Ok(None);
            }
            s.remove(&key).map(|c| c.entries).unwrap_or_default()
        };
        self.commit(peer, checkout, all).map(Some)
    }

    fn commit(
        &self,
        peer: &PeerId,
        checkout: &CheckoutId,
        entries: Vec<FileEntry>,
    ) -> Result<SyncSummary, Refusal> {
        let mut new = Manifest::default();
        for e in entries {
            check_rel_path(&e.path).map_err(|r| bad_path(&e.path, r))?;
            if new.entries.contains_key(&e.path) {
                return Err(bad_path(&e.path, "listed twice"));
            }
            new.entries.insert(e.path.clone(), e);
        }
        // A file and a directory of the same name cannot both exist.
        for p in new.entries.keys() {
            let mut anc = p.as_str();
            while let Some((parent, _)) = anc.rsplit_once('/') {
                if new.entries.contains_key(parent) {
                    return Err(bad_path(
                        p,
                        format!("{parent} is both a file and a directory"),
                    ));
                }
                anc = parent;
            }
        }
        let dir = self.dir(peer, checkout);
        let tree = dir.join("tree");
        let stage = dir.join("stage");
        let _ = fs::remove_dir_all(&stage);
        fs::create_dir_all(&tree).map_err(|e| io_err("creating tree", e))?;
        fs::create_dir_all(&stage).map_err(|e| io_err("creating stage", e))?;

        let old = self.manifest(peer, checkout);
        let by_hash: BTreeMap<Hash, &FileEntry> =
            old.entries.values().map(|e| (e.hash, e)).collect();

        // Step 1: stage every changed file; refuse if anything is missing.
        let mut summary = SyncSummary {
            files: new.entries.len() as u64,
            ..SyncSummary::default()
        };
        let mut staged: Vec<(usize, &FileEntry)> = Vec::new();
        let mut missing: Vec<Hash> = Vec::new();
        let mut chmod: Vec<&FileEntry> = Vec::new();
        for e in new.entries.values() {
            let target = tree.join(&e.path);
            let same = old.entries.get(&e.path).is_some_and(|o| o.hash == e.hash)
                && fs::symlink_metadata(&target)
                    .map(|m| m.is_file() && m.len() == e.size)
                    .unwrap_or(false);
            if same {
                summary.unchanged += 1;
                chmod.push(e);
                continue;
            }
            match self.source_of(peer, checkout, &by_hash, &e.hash) {
                Some(src) => {
                    let n = staged.len();
                    let out = stage.join(n.to_string());
                    fs::copy(&src, &out).map_err(|er| io_err("staging file", er))?;
                    let ok = hash_file(&out).map(|(h, _)| h == e.hash).unwrap_or(false);
                    if !ok {
                        let _ = fs::remove_dir_all(&stage);
                        return Err(Refusal::HashMismatch);
                    }
                    let mode = if e.exec { 0o755 } else { 0o644 };
                    fs::set_permissions(&out, fs::Permissions::from_mode(mode))
                        .map_err(|er| io_err("setting mode", er))?;
                    staged.push((n, e));
                }
                None => {
                    if missing.len() < MAX_HASHES_PER_REQUEST && !missing.contains(&e.hash) {
                        missing.push(e.hash);
                    }
                }
            }
        }
        if !missing.is_empty() {
            let _ = fs::remove_dir_all(&stage);
            return Err(Refusal::Incomplete { missing });
        }

        // Step 2: apply. From here a failure invalidates the recorded manifest
        // so the next sync re-checks everything instead of trusting it.
        let manifest_path = dir.join("manifest.json");
        let result = (|| -> Result<(), Refusal> {
            let removed = crate::manifest::deleted(&new, &old);
            for p in &removed {
                let target = tree.join(p);
                match fs::symlink_metadata(&target) {
                    Ok(m) if m.is_file() || m.file_type().is_symlink() => {
                        fs::remove_file(&target).map_err(|e| io_err("removing file", e))?;
                        summary.removed += 1;
                    }
                    _ => {}
                }
                prune_empty_parents(&tree, &target);
            }
            for (n, e) in &staged {
                let target = tree.join(&e.path);
                ensure_parents(&tree, &e.path)?;
                if let Ok(m) = fs::symlink_metadata(&target)
                    && m.is_dir()
                {
                    return Err(bad_path(&e.path, "would replace a directory"));
                }
                fs::rename(stage.join(n.to_string()), &target)
                    .map_err(|er| io_err("moving file into place", er))?;
                summary.written += 1;
            }
            for e in &chmod {
                let target = tree.join(&e.path);
                let want_exec = e.exec;
                if let Ok(m) = fs::metadata(&target) {
                    let is_exec = m.permissions().mode() & 0o111 != 0;
                    if is_exec != want_exec {
                        let mode = if want_exec { 0o755 } else { 0o644 };
                        fs::set_permissions(&target, fs::Permissions::from_mode(mode))
                            .map_err(|er| io_err("setting mode", er))?;
                    }
                }
            }
            Ok(())
        })();
        let _ = fs::remove_dir_all(&stage);
        if let Err(e) = result {
            let _ = fs::remove_file(&manifest_path);
            return Err(e);
        }
        let list: Vec<&FileEntry> = new.entries.values().collect();
        let json = serde_json::to_vec(&list).map_err(|e| Refusal::Internal {
            detail: e.to_string(),
        })?;
        let tmp = dir.join("manifest.json.tmp");
        fs::write(&tmp, json)
            .and_then(|_| fs::rename(&tmp, &manifest_path))
            .map_err(|e| io_err("recording manifest", e))?;
        let _ = fs::remove_dir_all(self.incoming(peer, checkout));
        Ok(summary)
    }
}

/// Creates the parent directories of `rel` under `tree`, refusing to walk
/// through a symlink or over a file.
fn ensure_parents(tree: &Path, rel: &str) -> Result<(), Refusal> {
    let mut cur = tree.to_path_buf();
    let comps: Vec<&str> = rel.split('/').collect();
    for c in comps.iter().take(comps.len().saturating_sub(1)) {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(m) if m.file_type().is_symlink() => {
                return Err(bad_path(rel, "a parent directory is a symlink"));
            }
            Ok(_) => return Err(bad_path(rel, "a parent is a file")),
            Err(_) => fs::create_dir(&cur).map_err(|e| io_err("creating directory", e))?,
        }
    }
    Ok(())
}

fn prune_empty_parents(tree: &Path, target: &Path) {
    let mut cur = target.parent();
    while let Some(d) = cur {
        if d == tree || !d.starts_with(tree) {
            break;
        }
        // Fails (and stops) as soon as a directory is not empty.
        if fs::remove_dir(d).is_err() {
            break;
        }
        cur = d.parent();
    }
}

/// Reads a whole small file; used by tests and by callers checking a copy.
pub fn read_to_string(path: &Path) -> std::io::Result<String> {
    let mut s = String::new();
    fs::File::open(path)?.read_to_string(&mut s)?;
    Ok(s)
}
