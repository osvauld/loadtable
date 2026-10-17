//! Phase 2b: stable-checkout sync. Updates `CHECKOUT_ROOT/<name>` IN PLACE from a validated
//! manifest whose blobs are already in the store, so a warm `target/` beside the sources stays
//! incremental:
//!
//! * **Unchanged** files (same size, exec bit and BLAKE3) are not rewritten: their mtime and inode
//!   are preserved. A drifted permission mode (e.g. 0664) is normalized with `fchmod`, which does
//!   not touch mtime.
//! * **Changed/new** files are written to a private `O_CREAT|O_EXCL|O_NOFOLLOW` temp file in the
//!   same directory, given mode 0644/0755, fsynced, then `renameat` over the old name (a symlink
//!   at that name is replaced, never followed).
//! * **Absent** files are pruned with exactly `scripts/offload_agent.py`'s rules ([`prune`]):
//!   `target/`, `android/build`, marker-adjacent outputs, excluded names and cache links are
//!   never deleted or descended.
//! * Every manifest path must itself be acceptable source (worker_source rules) and may not pass
//!   through a marker-adjacent output directory, so a sync never writes into build outputs.
//! * Descent is `openat(O_NOFOLLOW|O_DIRECTORY)` from the checkout root; the checkout itself may
//!   not be a symlink. After writing, every manifest file is re-read and verified (size, mode,
//!   BLAKE3, SHA-256), as the immutable materializer does.
//! * One `flock` per checkout (`CHECKOUT_ROOT/.lock-<name>`) serializes syncs of one checkout,
//!   also across server processes. Different checkouts proceed independently.
//!
//! Not atomic as a whole tree (a crash mid-sync leaves a mix of old and new files; the next sync
//! repairs it, and leftover `.tmp-sync-*` files are pruned as stale source).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iroh_blobs::Hash;
use iroh_blobs::api::Store;
use rand::RngCore;
use serde::Serialize;

use crate::prune::{self, source_path};
use crate::source::{
    Limits, Manifest, ManifestFile, SourceError, check_checkout_name, check_path, create_file_at,
    cstr, cvt, io_err, mkdir_at, mode_of, open_dir_at, open_file_at, open_root, validate_manifest,
    verify_tree,
};

/// How long a sync waits for another sync of the same checkout before answering `Busy`.
pub const LOCK_WAIT: Duration = Duration::from_secs(120);
const LOCK_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncReport {
    pub checkout: PathBuf,
    /// The checkout directory did not exist before this sync.
    pub created: bool,
    pub files: usize,
    pub unchanged: usize,
    pub written: usize,
    pub written_bytes: u64,
    /// Unchanged files whose permission mode was normalized (mtime kept).
    pub mode_fixed: usize,
    /// Pruned paths, sorted; directories end in `/`.
    pub deleted: Vec<String>,
    pub kept_non_source: usize,
}

/// Holds the per-checkout lock until dropped.
#[derive(Debug)]
pub struct CheckoutLock {
    _fd: OwnedFd,
}

fn try_lock(root: RawFd, name: &str) -> Result<Option<CheckoutLock>, SourceError> {
    check_checkout_name(name)?;
    let lock_name = cstr(&format!(".lock-{name}"))?;
    let flags = libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: valid NUL-terminated name; mode passed as the variadic third argument.
    let fd = cvt(unsafe { libc::openat(root, lock_name.as_ptr(), flags, 0o600 as libc::c_uint) })
        .map_err(io_err("checkout lock"))?;
    // SAFETY: fd is a fresh owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: valid fd.
    if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(CheckoutLock { _fd: fd }));
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(SourceError::Io(format!("checkout lock: {e}")))
    }
}

/// Blocking acquisition (tests and synchronous callers).
pub fn lock_checkout(
    checkout_root: &Path,
    name: &str,
    wait: Duration,
) -> Result<CheckoutLock, SourceError> {
    let root = open_root(checkout_root)?;
    let deadline = Instant::now() + wait;
    loop {
        if let Some(lock) = try_lock(root.as_raw_fd(), name)? {
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            return Err(SourceError::Busy(name.to_owned()));
        }
        std::thread::sleep(LOCK_POLL);
    }
}

async fn lock_checkout_async(
    root: RawFd,
    name: &str,
    wait: Duration,
) -> Result<CheckoutLock, SourceError> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(lock) = try_lock(root, name)? {
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            return Err(SourceError::Busy(name.to_owned()));
        }
        tokio::time::sleep(LOCK_POLL).await;
    }
}

/// Request-time validation for sync mode, before any blob is requested: checkout name, the
/// manifest itself, every path acceptable as source, and no path through an output directory
/// that the manifest's own markers make marker-adjacent. Returns `hash -> size`.
pub fn precheck(
    manifest: &Manifest,
    name: &str,
    limits: &Limits,
) -> Result<BTreeMap<Hash, u64>, SourceError> {
    check_checkout_name(name)?;
    let blobs = validate_manifest(manifest, limits)?;
    let paths: BTreeSet<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    for f in &manifest.files {
        source_path(&f.path)?;
        let parts: Vec<&str> = f.path.split('/').collect();
        for i in 0..parts.len() - 1 {
            let parent = parts[..i].join("/");
            let lower = parts[i].to_lowercase();
            if let Some((_, markers)) = prune::MARKED_OUTPUT_DIRS.iter().find(|(n, _)| *n == lower)
            {
                let beside = |m: &&str| {
                    let p = if parent.is_empty() {
                        (*m).to_owned()
                    } else {
                        format!("{parent}/{m}")
                    };
                    paths.contains(p.as_str())
                };
                if markers.iter().any(beside) {
                    return Err(SourceError::UnsafePath(format!(
                        "{:?} passes through a build-output directory",
                        f.path
                    )));
                }
            }
        }
    }
    Ok(blobs)
}

/// No-follow descent to the parent of `parts`, refusing marker-adjacent output directories
/// (worker_source.open_parent); optionally creating missing directories (0755).
fn descend_checked(root: &OwnedFd, parts: &[&str], create: bool) -> Result<OwnedFd, SourceError> {
    let mut cur = root.try_clone().map_err(io_err("dup"))?;
    for part in &parts[..parts.len() - 1] {
        let name = cstr(part)?;
        if prune::is_marked_output(cur.as_raw_fd(), part)? {
            return Err(SourceError::UnsafePath(format!(
                "{part:?} is a build-output directory beside a build marker"
            )));
        }
        if create
            && let Err(e) = mkdir_at(cur.as_raw_fd(), &name, 0o755)
            && e.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(SourceError::Io(format!("mkdir {part:?}: {e}")));
        }
        cur = open_dir_at(cur.as_raw_fd(), &name).map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) || e.raw_os_error() == Some(libc::ENOTDIR) {
                SourceError::UnsafePath(format!("{part:?} is not a plain directory: {e}"))
            } else {
                SourceError::Io(format!("open {part:?}: {e}"))
            }
        })?;
    }
    Ok(cur)
}

enum Plan {
    Unchanged { fix_mode: bool },
    Write,
}

/// Is the existing entry at `f.path` the same regular file (size, exec bit, BLAKE3)? Anything
/// unexpected (missing, symlink, directory, special, unreadable parent) means "write".
fn inspect(co: &OwnedFd, f: &ManifestFile, limits: &Limits) -> Plan {
    let Ok(parts) = check_path(&f.path, limits) else {
        return Plan::Write;
    };
    let mut cur = match co.try_clone() {
        Ok(fd) => fd,
        Err(_) => return Plan::Write,
    };
    for part in &parts[..parts.len() - 1] {
        let Ok(name) = cstr(part) else {
            return Plan::Write;
        };
        cur = match open_dir_at(cur.as_raw_fd(), &name) {
            Ok(fd) => fd,
            Err(_) => return Plan::Write,
        };
    }
    let Ok(name) = cstr(parts[parts.len() - 1]) else {
        return Plan::Write;
    };
    let Ok(mut file) = open_file_at(cur.as_raw_fd(), &name) else {
        return Plan::Write;
    };
    let Ok(meta) = file.metadata() else {
        return Plan::Write;
    };
    let mode = meta.permissions().mode();
    if !meta.is_file() || meta.len() != f.size || (mode & 0o111 != 0) != f.executable {
        return Plan::Write;
    }
    let mut content = Vec::with_capacity(f.size as usize);
    if (&mut file)
        .take(f.size + 1)
        .read_to_end(&mut content)
        .is_err()
        || content.len() as u64 != f.size
        || Some(Hash::new(&content).to_hex().as_str()) != f.iroh_blobs_hash.as_deref()
    {
        return Plan::Write;
    }
    Plan::Unchanged {
        fix_mode: mode & 0o7777 != mode_of(f.executable),
    }
}

fn rename_at(dir: RawFd, from: &CString, to: &CString) -> io::Result<()> {
    // SAFETY: both names NUL-terminated; plain renameat replaces `to` (never follows it).
    cvt(unsafe { libc::renameat(dir, from.as_ptr(), dir, to.as_ptr()) }).map(drop)
}

fn unlink_at(dir: RawFd, name: &CString) {
    // SAFETY: valid NUL-terminated name; best-effort cleanup of our own temp file.
    unsafe { libc::unlinkat(dir, name.as_ptr(), 0) };
}

/// Atomic replace of one file: private temp in the same dir, mode, fsync, rename, fsync dir.
fn replace_file(
    co: &OwnedFd,
    f: &ManifestFile,
    bytes: &[u8],
    limits: &Limits,
) -> Result<(), SourceError> {
    let parts = check_path(&f.path, limits)?;
    let parent = descend_checked(co, &parts, true)?;
    let name = cstr(parts[parts.len() - 1])?;
    let mut nonce = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let tmp = cstr(&format!(".tmp-sync-{}", crate::hex(&nonce)))?;
    let mut file = create_file_at(parent.as_raw_fd(), &tmp)
        .map_err(|e| SourceError::Io(format!("create temp for {:?}: {e}", f.path)))?;
    let written = (|| {
        file.write_all(bytes)?;
        file.set_permissions(std::fs::Permissions::from_mode(mode_of(f.executable)))?;
        file.sync_all()?;
        drop(file);
        rename_at(parent.as_raw_fd(), &tmp, &name)
    })();
    if let Err(e) = written {
        unlink_at(parent.as_raw_fd(), &tmp);
        return Err(SourceError::Io(format!("replace {:?}: {e}", f.path)));
    }
    // SAFETY: valid fd; makes the rename durable.
    unsafe { libc::fsync(parent.as_raw_fd()) };
    Ok(())
}

fn fix_mode(co: &OwnedFd, f: &ManifestFile, limits: &Limits) -> Result<(), SourceError> {
    let parts = check_path(&f.path, limits)?;
    let parent = descend_checked(co, &parts, false)?;
    let file = open_file_at(parent.as_raw_fd(), &cstr(parts[parts.len() - 1])?)
        .map_err(io_err(&f.path))?;
    if !file.metadata().map_err(io_err(&f.path))?.is_file() {
        return Err(SourceError::Io(format!("{:?} changed during sync", f.path)));
    }
    file.set_permissions(std::fs::Permissions::from_mode(mode_of(f.executable)))
        .map_err(io_err(&f.path))
}

/// [`sync_checkout_waiting`] with the default [`LOCK_WAIT`].
pub async fn sync_checkout(
    store: &Store,
    checkout_root: &Path,
    name: &str,
    manifest: &Manifest,
    limits: &Limits,
) -> Result<SyncReport, SourceError> {
    sync_checkout_waiting(store, checkout_root, name, manifest, limits, LOCK_WAIT).await
}

pub async fn sync_checkout_waiting(
    store: &Store,
    checkout_root: &Path,
    name: &str,
    manifest: &Manifest,
    limits: &Limits,
    lock_wait: Duration,
) -> Result<SyncReport, SourceError> {
    precheck(manifest, name, limits)?;
    let root = open_root(checkout_root)?;
    let _lock = lock_checkout_async(root.as_raw_fd(), name, lock_wait).await?;
    let cname = cstr(name)?;
    let (co, created) = match open_dir_at(root.as_raw_fd(), &cname) {
        Ok(fd) => (fd, false),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
            mkdir_at(root.as_raw_fd(), &cname, 0o700).map_err(io_err(name))?;
            (
                open_dir_at(root.as_raw_fd(), &cname).map_err(io_err(name))?,
                true,
            )
        }
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
            return Err(SourceError::UnsafePath(format!(
                "checkout {name:?} is not a plain directory"
            )));
        }
        Err(e) => return Err(SourceError::Io(format!("{name}: {e}"))),
    };

    // Plan, then fetch and verify every needed blob before touching the checkout.
    let plans: Vec<Plan> = manifest
        .files
        .iter()
        .map(|f| inspect(&co, f, limits))
        .collect();
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (f, plan) in manifest.files.iter().zip(&plans) {
        let hex = f.iroh_blobs_hash.clone().unwrap_or_default();
        if !matches!(plan, Plan::Write) || blobs.contains_key(&hex) {
            continue;
        }
        let hash: Hash = hex
            .parse()
            .map_err(|_| SourceError::InvalidManifest(hex.clone()))?;
        let bytes = if f.size == 0 {
            Vec::new()
        } else {
            if !store.blobs().has(hash).await.unwrap_or(false) {
                return Err(SourceError::MissingBlob(hex));
            }
            store
                .blobs()
                .get_bytes(hash)
                .await
                .map_err(|e| SourceError::MissingBlob(format!("{hex}: {e}")))?
                .to_vec()
        };
        if Hash::new(&bytes) != hash || bytes.len() as u64 != f.size {
            return Err(SourceError::HashMismatch(hex));
        }
        blobs.insert(hex, bytes);
    }

    let keep: BTreeSet<String> = manifest.files.iter().map(|f| f.path.clone()).collect();
    let pruned = prune::prune(&co, &keep)?;

    let mut report = SyncReport {
        checkout: checkout_root.join(name),
        created,
        files: manifest.files.len(),
        unchanged: 0,
        written: 0,
        written_bytes: 0,
        mode_fixed: 0,
        deleted: pruned.pruned,
        kept_non_source: pruned.kept_non_source,
    };
    for (f, plan) in manifest.files.iter().zip(&plans) {
        match plan {
            Plan::Unchanged { fix_mode: fix } => {
                if *fix {
                    fix_mode(&co, f, limits)?;
                    report.mode_fixed += 1;
                }
                report.unchanged += 1;
            }
            Plan::Write => {
                let bytes = &blobs[f.iroh_blobs_hash.as_deref().unwrap_or_default()];
                replace_file(&co, f, bytes, limits)?;
                report.written += 1;
                report.written_bytes += f.size;
            }
        }
    }
    verify_tree(&co, manifest, limits)?;
    // SAFETY: valid fd.
    unsafe { libc::fsync(co.as_raw_fd()) };
    Ok(report)
}
