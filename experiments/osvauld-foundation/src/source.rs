//! Phase 2: content-addressed source transfer over the attested LAN-only transport.
//!
//! * **Manifest.** Same JSON schema as `scripts/worker_source.py` (SHA-256 receipt over path,
//!   exec bit, size and SHA-256), with each file's `iroh_blobs_hash` filled: lowercase hex of
//!   `BLAKE3(content)`, which is exactly the iroh-blobs raw (`BlobFormat::Raw`) blob hash.
//! * **Store.** An upstream iroh-blobs store (file-backed `FsStore` under a caller-given
//!   directory in `lan-peer`). Every received blob gets a named persistent tag
//!   `task-manager/source/<hash>`, so it survives restarts and is never garbage collected here.
//! * **Protocol** (ALPN [`SOURCE_ALPN`], same admission gate as the ping ALPN): `Hello` →
//!   [`Policy::admit`] → `Challenge` → signed `Request{checkout, manifest}` → `Missing{hashes}`
//!   (the "which of these do you have" answer) → one `Blob{hash,size}` header + raw bytes per
//!   missing hash, in order, each BLAKE3-verified before import → materialize → signed
//!   `Response`. Any failure answers `Rejected{reason}` and imports nothing further.
//! * **Materializer.** Path-safe (no `..`/absolute/empty/backslash/control components, no
//!   duplicates or file/dir conflicts), no-follow `openat` descent, `O_EXCL|O_NOFOLLOW` file
//!   creation, exec bit honored (0755/0644), every file re-read and hash-verified, then an
//!   atomic `renameat2(RENAME_NOREPLACE)` of a private temp dir. Checkouts are immutable.
//!
//! Not provided: execution envelopes, job submission, logs, artifacts, bao range streaming,
//! resumable transfers, quotas beyond [`Limits`], or a LAN-confinement proof. Linux only.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use courier::token::Token;
use iroh::endpoint::{Connection, VarInt};
use iroh::{Endpoint, EndpointAddr, PublicKey};
use iroh_blobs::Hash;
use iroh_blobs::api::Store;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::time::timeout;

use crate::{CLOSE_GRACE, Frame, Policy, read_frame, read_frame_limited, verify_sig, write_frame};

/// Source-transfer protocol; distinct from the ping ALPN and from iroh-blobs' own ALPN.
pub const SOURCE_ALPN: &[u8] = b"task-manager/source/0";
/// Shared with `scripts/worker_source.py`.
pub const SCHEMA: &str = "task-manager-local-source-manifest-v1";
const REQUEST_DOMAIN: &[u8] = b"task-manager/source/request/v0\0";
const RESPONSE_DOMAIN: &[u8] = b"task-manager/source/response/v0\0";
/// The request frame carries the whole manifest (JSON byte array, roughly 4x inflated).
pub const MAX_SOURCE_FRAME: usize = 32 * 1024 * 1024;
/// Named-tag prefix protecting received blobs in the store.
pub const TAG_PREFIX: &str = "task-manager/source/";

/// Bounds; defaults equal `worker_source.py`'s CEILING.
#[derive(Debug, Clone)]
pub struct Limits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_bytes: u64,
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 20_000,
            max_file_bytes: 64 * 1024 * 1024,
            max_bytes: 512 * 1024 * 1024,
            max_depth: 32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    InvalidManifest(String),
    UnsafePath(String),
    Limit(String),
    MissingBlob(String),
    HashMismatch(String),
    Exists(String),
    /// Another sync of the same stable checkout holds its lock.
    Busy(String),
    Io(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

pub(crate) fn io_err(context: &str) -> impl Fn(io::Error) -> SourceError + '_ {
    move |e| SourceError::Io(format!("{context}: {e}"))
}

// ---------------------------------------------------------------------------------------------
// Manifest

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub executable: bool,
    pub sha256: String,
    pub iroh_blobs_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: String,
    pub hash_algorithm: String,
    pub receipt: String,
    /// Collection hash; always null (files are transferred as individual raw blobs).
    pub iroh_blobs_hash: Option<String>,
    pub files: Vec<ManifestFile>,
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    crate::hex(&Sha256::digest(bytes))
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Same framing as `worker_source.Capture.receipt`.
pub fn receipt(files: &[ManifestFile]) -> Result<String, SourceError> {
    let mut digest = Sha256::new();
    digest.update(SCHEMA.as_bytes());
    digest.update([0u8]);
    for f in files {
        if !is_hex64(&f.sha256) {
            return Err(SourceError::InvalidManifest(format!(
                "{}: sha256 must be 64 lowercase hex",
                f.path
            )));
        }
        let mut sha = [0u8; 32];
        for (i, byte) in sha.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&f.sha256[2 * i..2 * i + 2], 16)
                .map_err(|e| SourceError::InvalidManifest(e.to_string()))?;
        }
        digest.update((f.path.len() as u64).to_le_bytes());
        digest.update(f.path.as_bytes());
        digest.update([f.executable as u8]);
        digest.update(f.size.to_le_bytes());
        digest.update(sha);
    }
    Ok(crate::hex(&digest.finalize()))
}

impl Manifest {
    /// Wraps files in their given order and computes the receipt; does NOT validate.
    pub fn from_files_unchecked(files: Vec<ManifestFile>) -> Self {
        let receipt = receipt(&files).unwrap_or_default();
        Self {
            schema: SCHEMA.to_owned(),
            hash_algorithm: "sha256".to_owned(),
            receipt,
            iroh_blobs_hash: None,
            files,
        }
    }
}

/// Structural path rule (shared with the Python capture's `_path`, minus its credential/output
/// denylist, which stays the capture's policy). Returns the components.
pub fn check_path<'a>(path: &'a str, limits: &Limits) -> Result<Vec<&'a str>, SourceError> {
    let parts: Vec<&str> = path.split('/').collect();
    if path.is_empty()
        || path.len() > 1024
        || parts.len() > limits.max_depth
        || path.contains('\\')
        || path.chars().any(|c| c.is_control())
        || parts.iter().any(|p| matches!(*p, "" | "." | ".."))
    {
        return Err(SourceError::UnsafePath(format!(
            "invalid, escaping or oversized path {path:?}"
        )));
    }
    Ok(parts)
}

/// A checkout is one plain directory name under the checkout root: `[A-Za-z0-9._-]{1,64}`,
/// not starting with `.` (temp directories use a leading dot).
pub fn check_checkout_name(name: &str) -> Result<(), SourceError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(SourceError::UnsafePath(format!(
            "checkout name {name:?} must match [A-Za-z0-9._-]{{1,64}} without a leading dot"
        )))
    }
}

/// Full manifest validation; returns `hash -> size` for the distinct blobs it references.
pub fn validate_manifest(
    manifest: &Manifest,
    limits: &Limits,
) -> Result<BTreeMap<Hash, u64>, SourceError> {
    if manifest.schema != SCHEMA || manifest.hash_algorithm != "sha256" {
        return Err(SourceError::InvalidManifest("unknown schema".into()));
    }
    if manifest.files.len() > limits.max_files {
        return Err(SourceError::Limit("too many files".into()));
    }
    let mut total = 0u64;
    let mut blobs = BTreeMap::new();
    let mut paths = HashSet::new();
    let mut previous: Option<&str> = None;
    for f in &manifest.files {
        check_path(&f.path, limits)?;
        if let Some(prev) = previous {
            if prev == f.path {
                return Err(SourceError::UnsafePath(format!(
                    "duplicate path {:?}",
                    f.path
                )));
            }
            if prev > f.path.as_str() {
                return Err(SourceError::InvalidManifest("files are not sorted".into()));
            }
        }
        previous = Some(&f.path);
        paths.insert(f.path.as_str());
        if f.size > limits.max_file_bytes {
            return Err(SourceError::Limit(format!("{}: file too large", f.path)));
        }
        total = total.saturating_add(f.size);
        if total > limits.max_bytes {
            return Err(SourceError::Limit("aggregate bytes exceed bound".into()));
        }
        let hex = f
            .iroh_blobs_hash
            .as_deref()
            .filter(|h| is_hex64(h))
            .ok_or_else(|| {
                SourceError::InvalidManifest(format!(
                    "{}: iroh_blobs_hash must be 64 lowercase hex",
                    f.path
                ))
            })?;
        let hash: Hash = hex
            .parse()
            .map_err(|_| SourceError::InvalidManifest(format!("{}: bad hash", f.path)))?;
        if *blobs.entry(hash).or_insert(f.size) != f.size {
            return Err(SourceError::InvalidManifest(format!(
                "{}: one hash with two sizes",
                f.path
            )));
        }
    }
    for f in &manifest.files {
        let mut prefix = String::new();
        for part in f
            .path
            .split('/')
            .collect::<Vec<_>>()
            .split_last()
            .unwrap()
            .1
        {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if paths.contains(prefix.as_str()) {
                return Err(SourceError::UnsafePath(format!(
                    "{prefix:?} is both a file and a directory"
                )));
            }
        }
    }
    if receipt(&manifest.files)? != manifest.receipt {
        return Err(SourceError::InvalidManifest("receipt mismatch".into()));
    }
    Ok(blobs)
}

/// Parses a NUL-separated list (`git ls-files -z` style). One trailing NUL is allowed; empty
/// entries, non-UTF-8 and duplicates are refused. Order is preserved.
pub fn parse_files0(bytes: &[u8]) -> Result<Vec<String>, SourceError> {
    let body = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    if body.is_empty() {
        return Err(SourceError::InvalidManifest("empty file list".into()));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in body.split(|b| *b == 0) {
        let path = std::str::from_utf8(raw)
            .map_err(|_| SourceError::UnsafePath("non-UTF-8 path".into()))?;
        if path.is_empty() {
            return Err(SourceError::UnsafePath("empty entry in file list".into()));
        }
        if !seen.insert(path) {
            return Err(SourceError::InvalidManifest(format!("duplicate {path:?}")));
        }
        out.push(path.to_owned());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// No-follow filesystem primitives (Linux)

pub(crate) fn cstr(name: &str) -> Result<CString, SourceError> {
    CString::new(name).map_err(|_| SourceError::UnsafePath(format!("NUL in {name:?}")))
}

pub(crate) fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

pub(crate) fn open_dir_at(dir: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: valid NUL-terminated name; the returned fd is owned exactly once.
    let fd = cvt(unsafe { libc::openat(dir, name.as_ptr(), flags) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(crate) fn open_file_at(dir: RawFd, name: &CStr) -> io::Result<File> {
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: as above.
    let fd = cvt(unsafe { libc::openat(dir, name.as_ptr(), flags) })?;
    Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

pub(crate) fn create_file_at(dir: RawFd, name: &CStr) -> io::Result<File> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: as above; mode is passed as the variadic third argument.
    let fd = cvt(unsafe { libc::openat(dir, name.as_ptr(), flags, 0o600 as libc::c_uint) })?;
    Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

pub(crate) fn mkdir_at(dir: RawFd, name: &CStr, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: valid NUL-terminated name.
    cvt(unsafe { libc::mkdirat(dir, name.as_ptr(), mode) }).map(drop)
}

pub(crate) fn exists_nofollow(dir: RawFd, name: &CStr) -> io::Result<bool> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: st is written by the kernel on success and not read otherwise.
    let ret = unsafe {
        libc::fstatat(
            dir,
            name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    match cvt(ret) {
        Ok(_) => Ok(true),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(false),
        Err(e) => Err(e),
    }
}

/// `fstatat(AT_SYMLINK_NOFOLLOW)`.
pub(crate) fn stat_at(dir: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: st is written by the kernel on success and only read then.
    cvt(unsafe {
        libc::fstatat(
            dir,
            name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    Ok(unsafe { st.assume_init() })
}

/// Names in an open directory (without `.`/`..`), read through a private duplicate fd.
pub(crate) fn read_dir_names(dir: RawFd) -> io::Result<Vec<CString>> {
    // SAFETY: F_DUPFD_CLOEXEC returns a new fd owned by the DIR stream below.
    let dup = cvt(unsafe { libc::fcntl(dir, libc::F_DUPFD_CLOEXEC, 0) })?;
    // SAFETY: dup is a valid directory fd; on success the stream owns it.
    let stream = unsafe { libc::fdopendir(dup) };
    if stream.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so dup is still ours to close.
        unsafe { libc::close(dup) };
        return Err(e);
    }
    // The duplicate shares the file offset with `dir`: start from the beginning.
    // SAFETY: stream is a valid DIR*.
    unsafe { libc::rewinddir(stream) };
    let mut out = Vec::new();
    let result = loop {
        // SAFETY: errno is thread-local; reset to tell end-of-stream from an error.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: stream is valid until closedir below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let e = io::Error::last_os_error();
            break if e.raw_os_error().unwrap_or(0) == 0 {
                Ok(())
            } else {
                Err(e)
            };
        }
        // SAFETY: d_name is NUL-terminated within the returned dirent.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            out.push(name.to_owned());
        }
    };
    // SAFETY: closes the stream and dup exactly once.
    unsafe { libc::closedir(stream) };
    result.map(|_| out)
}

fn rename_noreplace(dir: RawFd, from: &CStr, to: &CStr) -> io::Result<()> {
    // SAFETY: raw renameat2 syscall (works on glibc and musl); both names NUL-terminated.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            dir,
            from.as_ptr(),
            dir,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Opens a caller-given root directory (the root itself may be reached through symlinks; its
/// contents are always traversed no-follow).
pub(crate) fn open_root(path: &Path) -> Result<OwnedFd, SourceError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
        .map_err(io_err(&path.display().to_string()))?;
    Ok(OwnedFd::from(file))
}

/// No-follow descent to the parent directory of `parts`, optionally creating directories.
pub(crate) fn descend(
    root: &OwnedFd,
    parts: &[&str],
    create: bool,
) -> Result<OwnedFd, SourceError> {
    let mut cur = root.try_clone().map_err(io_err("dup"))?;
    for part in &parts[..parts.len() - 1] {
        let name = cstr(part)?;
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

/// Reads one source file no-follow: regular, single-link, bounded. Returns (bytes, executable).
pub(crate) fn read_source_file(
    root: &OwnedFd,
    path: &str,
    limits: &Limits,
) -> Result<(Vec<u8>, bool), SourceError> {
    let parts = check_path(path, limits)?;
    let parent = descend(root, &parts, false)?;
    let name = cstr(parts[parts.len() - 1])?;
    let file = open_file_at(parent.as_raw_fd(), &name).map_err(|e| {
        if e.raw_os_error() == Some(libc::ELOOP) {
            SourceError::UnsafePath(format!("{path:?} is a symlink"))
        } else {
            SourceError::Io(format!("{path:?}: {e}"))
        }
    })?;
    let meta = file.metadata().map_err(io_err(path))?;
    if !meta.is_file() || std::os::unix::fs::MetadataExt::nlink(&meta) != 1 {
        return Err(SourceError::UnsafePath(format!(
            "{path:?}: links/special files/directories are unsupported"
        )));
    }
    if meta.len() > limits.max_file_bytes {
        return Err(SourceError::Limit(format!("{path:?}: file too large")));
    }
    let mut content = Vec::with_capacity(meta.len() as usize);
    file.take(limits.max_file_bytes + 1)
        .read_to_end(&mut content)
        .map_err(io_err(path))?;
    if content.len() as u64 != meta.len() {
        return Err(SourceError::Io(format!("{path:?} changed while reading")));
    }
    Ok((content, meta.permissions().mode() & 0o111 != 0))
}

/// `lan-peer manifest`: hashes an explicit file list under `root` (no-follow, bounded).
pub fn manifest_from_files(
    root: &Path,
    paths: &[String],
    limits: &Limits,
) -> Result<Manifest, SourceError> {
    if paths.len() > limits.max_files {
        return Err(SourceError::Limit("too many files".into()));
    }
    let mut ordered: Vec<&String> = paths.iter().collect();
    ordered.sort();
    ordered.dedup();
    if ordered.len() != paths.len() {
        return Err(SourceError::InvalidManifest("duplicate path".into()));
    }
    let root = open_root(root)?;
    let mut total = 0u64;
    let mut files = Vec::with_capacity(ordered.len());
    for path in ordered {
        let (content, executable) = read_source_file(&root, path, limits)?;
        total += content.len() as u64;
        if total > limits.max_bytes {
            return Err(SourceError::Limit("aggregate bytes exceed bound".into()));
        }
        files.push(ManifestFile {
            path: path.clone(),
            size: content.len() as u64,
            executable,
            sha256: sha256_hex(&content),
            iroh_blobs_hash: Some(Hash::new(&content).to_hex()),
        });
    }
    let manifest = Manifest::from_files_unchecked(files);
    validate_manifest(&manifest, limits)?;
    Ok(manifest)
}

// ---------------------------------------------------------------------------------------------
// Store and materializer

/// The "which of these hashes do you have" query: sorted lowercase hex of absent blobs.
pub async fn missing(
    store: &Store,
    blobs: impl IntoIterator<Item = Hash>,
) -> Result<Vec<String>, SourceError> {
    let mut out = BTreeSet::new();
    for hash in blobs {
        let have = store
            .blobs()
            .has(hash)
            .await
            .map_err(|e| SourceError::Io(format!("store: {e}")))?;
        if !have {
            out.insert(hash.to_hex());
        }
    }
    Ok(out.into_iter().collect())
}

/// Imports verified bytes for `expected` under a persistent named tag.
pub async fn import_verified(
    store: &Store,
    expected: Hash,
    bytes: Vec<u8>,
) -> Result<(), SourceError> {
    import_verified_tagged(store, expected, bytes, TAG_PREFIX).await
}

/// [`import_verified`] under another named-tag prefix (e.g. artifacts).
pub async fn import_verified_tagged(
    store: &Store,
    expected: Hash,
    bytes: Vec<u8>,
    tag_prefix: &str,
) -> Result<(), SourceError> {
    if Hash::new(&bytes) != expected {
        return Err(SourceError::HashMismatch(expected.to_hex()));
    }
    let stored = store
        .blobs()
        .add_bytes(bytes)
        .with_named_tag(format!("{tag_prefix}{}", expected.to_hex()))
        .await
        .map_err(|e| SourceError::Io(format!("store import: {e}")))?;
    if stored.hash != expected {
        return Err(SourceError::HashMismatch(expected.to_hex()));
    }
    Ok(())
}

/// Fails with `Exists` if anything (including a dangling symlink) already has that name.
pub fn checkout_available(checkout_root: &Path, name: &str) -> Result<(), SourceError> {
    check_checkout_name(name)?;
    let root = open_root(checkout_root)?;
    if exists_nofollow(root.as_raw_fd(), &cstr(name)?).map_err(io_err(name))? {
        return Err(SourceError::Exists(name.to_owned()));
    }
    Ok(())
}

pub(crate) fn mode_of(executable: bool) -> u32 {
    if executable { 0o755 } else { 0o644 }
}

fn write_tree(
    root: &OwnedFd,
    files: &[(&ManifestFile, Vec<u8>)],
    limits: &Limits,
) -> Result<(), SourceError> {
    for (f, bytes) in files {
        let parts = check_path(&f.path, limits)?;
        let parent = descend(root, &parts, true)?;
        let name = cstr(parts[parts.len() - 1])?;
        let mut file = create_file_at(parent.as_raw_fd(), &name)
            .map_err(|e| SourceError::UnsafePath(format!("create {:?}: {e}", f.path)))?;
        file.write_all(bytes).map_err(io_err(&f.path))?;
        file.set_permissions(std::fs::Permissions::from_mode(mode_of(f.executable)))
            .map_err(io_err(&f.path))?;
        file.sync_all().map_err(io_err(&f.path))?;
    }
    Ok(())
}

pub(crate) fn verify_tree(
    root: &OwnedFd,
    manifest: &Manifest,
    limits: &Limits,
) -> Result<(), SourceError> {
    for f in &manifest.files {
        let parts = check_path(&f.path, limits)?;
        let parent = descend(root, &parts, false)?;
        let mut file = open_file_at(parent.as_raw_fd(), &cstr(parts[parts.len() - 1])?)
            .map_err(io_err(&f.path))?;
        let meta = file.metadata().map_err(io_err(&f.path))?;
        let mut content = Vec::new();
        file.read_to_end(&mut content).map_err(io_err(&f.path))?;
        let blake = Hash::new(&content).to_hex();
        if !meta.is_file()
            || meta.permissions().mode() & 0o777 != mode_of(f.executable)
            || content.len() as u64 != f.size
            || Some(blake.as_str()) != f.iroh_blobs_hash.as_deref()
            || sha256_hex(&content) != f.sha256
        {
            return Err(SourceError::HashMismatch(format!(
                "{:?} differs from the manifest after writing",
                f.path
            )));
        }
    }
    Ok(())
}

/// Turns a validated manifest plus store into `checkout_root/name`: private temp dir, write,
/// verify every file, then atomic no-replace rename. Returns the checkout path.
pub async fn materialize(
    store: &Store,
    checkout_root: &Path,
    name: &str,
    manifest: &Manifest,
    limits: &Limits,
) -> Result<PathBuf, SourceError> {
    check_checkout_name(name)?;
    validate_manifest(manifest, limits)?;
    let root = open_root(checkout_root)?;
    let target = cstr(name)?;
    if exists_nofollow(root.as_raw_fd(), &target).map_err(io_err(name))? {
        return Err(SourceError::Exists(name.to_owned()));
    }

    // Fetch and verify every blob before touching the filesystem.
    let mut contents = Vec::with_capacity(manifest.files.len());
    let mut cache: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for f in &manifest.files {
        let hex = f.iroh_blobs_hash.clone().unwrap_or_default();
        if !cache.contains_key(&hex) {
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
            cache.insert(hex.clone(), bytes);
        }
        contents.push((f, cache[&hex].clone()));
    }

    let mut nonce = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let tmp_name = format!(".tmp-{name}-{}", crate::hex(&nonce));
    let tmp = cstr(&tmp_name)?;
    mkdir_at(root.as_raw_fd(), &tmp, 0o700).map_err(io_err(&tmp_name))?;
    let result = (|| {
        let tmp_fd = open_dir_at(root.as_raw_fd(), &tmp).map_err(io_err(&tmp_name))?;
        write_tree(&tmp_fd, &contents, limits)?;
        verify_tree(&tmp_fd, manifest, limits)?;
        File::from(tmp_fd)
            .set_permissions(std::fs::Permissions::from_mode(0o755))
            .map_err(io_err(&tmp_name))?;
        rename_noreplace(root.as_raw_fd(), &tmp, &target).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                SourceError::Exists(name.to_owned())
            } else {
                SourceError::Io(format!("rename: {e}"))
            }
        })
    })();
    if let Err(e) = result {
        // Best effort: only our own private temp directory is removed.
        let _ = std::fs::remove_dir_all(checkout_root.join(&tmp_name));
        return Err(e);
    }
    let _ = File::from(root).sync_all();
    Ok(checkout_root.join(name))
}

// ---------------------------------------------------------------------------------------------
// Protocol

/// What the receiver does with a validated manifest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceMode {
    /// Materialize a NEW immutable checkout; an existing name is refused (phase 2).
    #[default]
    Immutable,
    /// Update the stable checkout of that name in place ([`crate::sync`], phase 2b).
    Sync,
}

impl SourceMode {
    fn is_immutable(&self) -> bool {
        *self == SourceMode::Immutable
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRequest {
    pub checkout: String,
    pub manifest: Manifest,
    /// Omitted on the wire for immutable requests, so phase-2 request bodies are unchanged.
    #[serde(default, skip_serializing_if = "SourceMode::is_immutable")]
    pub mode: SourceMode,
}

fn signed_message(
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

pub fn request_message(
    challenge: &[u8],
    requester: &PublicKey,
    receiver: &PublicKey,
    body: &[u8],
) -> Vec<u8> {
    signed_message(REQUEST_DOMAIN, challenge, requester, receiver, &[body])
}

pub fn response_message(
    challenge: &[u8],
    receiver: &PublicKey,
    requester: &PublicKey,
    request_body: &[u8],
    response_body: &[u8],
) -> Vec<u8> {
    signed_message(
        RESPONSE_DOMAIN,
        challenge,
        receiver,
        requester,
        &[request_body, response_body],
    )
}

#[derive(Debug)]
pub enum SourceOutcome {
    Materialized {
        checkout: PathBuf,
        files: usize,
        blobs_requested: usize,
        blobs_received: usize,
        bytes_received: u64,
    },
    Synced {
        report: crate::sync::SyncReport,
        blobs_received: usize,
        bytes_received: u64,
    },
    Rejected(String),
}

/// Receiver. Admission (attestation + allowlist) happens before the challenge, exactly as for
/// the ping ALPN; the manifest is validated and the destination checked before any blob is
/// requested; each blob is verified before import.
pub async fn handle_source(
    ep: &Endpoint,
    conn: Connection,
    policy: &Policy,
    store: &Store,
    checkout_root: &Path,
    limits: &Limits,
) -> Result<SourceOutcome, String> {
    if conn.alpn() != SOURCE_ALPN {
        return Err("unexpected ALPN".into());
    }
    let remote = conn.remote_id();
    let (mut send, mut recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
    let outcome = async {
        let Frame::Hello { attestation } = read_frame(&mut recv).await? else {
            return Err("expected hello".to_string());
        };
        if let Err(reject) = policy.admit(&remote, attestation.as_ref()) {
            return Ok(SourceOutcome::Rejected(format!("{reject:?}")));
        }
        let mut nonce = vec![0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        write_frame(
            &mut send,
            &Frame::Challenge {
                nonce: nonce.clone(),
            },
        )
        .await?;
        let Frame::Request { body, sig } = read_frame_limited(&mut recv, MAX_SOURCE_FRAME).await?
        else {
            return Err("expected request".to_string());
        };
        if !verify_sig(
            &remote,
            &request_message(&nonce, &remote, &ep.id(), &body),
            &sig,
        ) {
            return Ok(SourceOutcome::Rejected("BadRequest".into()));
        }
        let request: SourceRequest = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                return Ok(SourceOutcome::Rejected(format!(
                    "{:?}",
                    SourceError::InvalidManifest(e.to_string())
                )));
            }
        };
        let checked = match request.mode {
            SourceMode::Immutable => {
                validate_manifest(&request.manifest, limits).and_then(|blobs| {
                    checkout_available(checkout_root, &request.checkout).map(|_| blobs)
                })
            }
            SourceMode::Sync => crate::sync::precheck(&request.manifest, &request.checkout, limits),
        };
        let sizes = match checked {
            Ok(sizes) => sizes,
            Err(e) => return Ok(SourceOutcome::Rejected(format!("{e:?}"))),
        };
        let wanted = match missing(store, sizes.keys().copied()).await {
            Ok(w) => w,
            Err(e) => return Ok(SourceOutcome::Rejected(format!("{e:?}"))),
        };
        write_frame(
            &mut send,
            &Frame::Missing {
                hashes: wanted.clone(),
            },
        )
        .await?;
        let mut bytes_received = 0u64;
        for expected in &wanted {
            let (hash, size) = match read_frame(&mut recv).await? {
                Frame::Blob { hash, size } => (hash, size),
                other => return Err(format!("expected blob, got {other:?}")),
            };
            let expected_hash: Hash = expected.parse().map_err(|_| "bad hash".to_string())?;
            if &hash != expected {
                let _ = recv.stop(VarInt::from_u32(1));
                return Ok(SourceOutcome::Rejected(format!("UnrequestedBlob({hash})")));
            }
            if Some(&size) != sizes.get(&expected_hash) {
                let _ = recv.stop(VarInt::from_u32(1));
                return Ok(SourceOutcome::Rejected(format!("SizeMismatch({hash})")));
            }
            let mut buf = vec![0u8; size as usize];
            recv.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
            if let Err(e) = import_verified(store, expected_hash, buf).await {
                let _ = recv.stop(VarInt::from_u32(1));
                return Ok(SourceOutcome::Rejected(format!("{e:?}")));
            }
            bytes_received += size;
        }
        let (response, outcome) = match request.mode {
            SourceMode::Immutable => {
                let checkout = match materialize(
                    store,
                    checkout_root,
                    &request.checkout,
                    &request.manifest,
                    limits,
                )
                .await
                {
                    Ok(path) => path,
                    Err(e) => return Ok(SourceOutcome::Rejected(format!("{e:?}"))),
                };
                let response = serde_json::json!({
                    "checkout": checkout,
                    "files": request.manifest.files.len(),
                    "blobs_received": wanted.len(),
                    "bytes_received": bytes_received,
                });
                let outcome = SourceOutcome::Materialized {
                    checkout,
                    files: request.manifest.files.len(),
                    blobs_requested: wanted.len(),
                    blobs_received: wanted.len(),
                    bytes_received,
                };
                (response, outcome)
            }
            SourceMode::Sync => {
                let report = match crate::sync::sync_checkout(
                    store,
                    checkout_root,
                    &request.checkout,
                    &request.manifest,
                    limits,
                )
                .await
                {
                    Ok(report) => report,
                    Err(e) => return Ok(SourceOutcome::Rejected(format!("{e:?}"))),
                };
                let response = serde_json::json!({
                    "mode": "sync",
                    "checkout": report.checkout,
                    "created": report.created,
                    "files": report.files,
                    "unchanged": report.unchanged,
                    "written": report.written,
                    "written_bytes": report.written_bytes,
                    "mode_fixed": report.mode_fixed,
                    "deleted": report.deleted.len(),
                    "deleted_paths": report.deleted.iter().take(200).collect::<Vec<_>>(),
                    "kept_non_source": report.kept_non_source,
                    "blobs_received": wanted.len(),
                    "bytes_received": bytes_received,
                });
                let outcome = SourceOutcome::Synced {
                    report,
                    blobs_received: wanted.len(),
                    bytes_received,
                };
                (response, outcome)
            }
        };
        let response = serde_json::to_vec(&response).map_err(|e| e.to_string())?;
        let sig = ep
            .secret_key()
            .sign(&response_message(
                &nonce,
                &ep.id(),
                &remote,
                &body,
                &response,
            ))
            .to_bytes()
            .to_vec();
        write_frame(
            &mut send,
            &Frame::Response {
                body: response,
                sig,
            },
        )
        .await?;
        Ok(outcome)
    }
    .await?;
    if let SourceOutcome::Rejected(reason) = &outcome {
        write_frame(
            &mut send,
            &Frame::Rejected {
                reason: reason.clone(),
            },
        )
        .await?;
    }
    send.finish().map_err(|e| e.to_string())?;
    let _ = timeout(CLOSE_GRACE, conn.closed()).await;
    Ok(outcome)
}

pub async fn serve_source_one(
    ep: &Endpoint,
    policy: &Policy,
    store: &Store,
    checkout_root: &Path,
    limits: &Limits,
) -> Result<SourceOutcome, String> {
    let conn = crate::accept_one(ep).await?;
    handle_source(ep, conn, policy, store, checkout_root, limits).await
}

pub enum PushMode {
    Honest,
    /// Test-only adversary: flips the first byte of every sent non-empty blob while keeping the
    /// claimed hash.
    TamperBlobs,
}

#[derive(Debug, Clone)]
pub struct PushReport {
    pub challenge: Vec<u8>,
    pub missing: Vec<String>,
    pub blobs_sent: usize,
    pub bytes_sent: u64,
    pub response_body: Vec<u8>,
    pub response_fresh: bool,
}

/// Requester: offers `manifest`, then sends exactly the blobs the receiver reports missing, read
/// no-follow from `source_root` and re-verified against the manifest before sending. Errors
/// starting with `rejected:` carry the receiver's reason; `source:` is a local source problem.
pub async fn push_source(
    ep: &Endpoint,
    to: EndpointAddr,
    attestation: Option<Token>,
    source_root: &Path,
    manifest: &Manifest,
    checkout: &str,
    mode: PushMode,
) -> Result<PushReport, String> {
    push_source_as(
        ep,
        to,
        attestation,
        source_root,
        manifest,
        checkout,
        mode,
        SourceMode::Immutable,
    )
    .await
}

/// [`push_source`] with an explicit receiver mode (`Sync` updates a stable checkout in place).
#[allow(clippy::too_many_arguments)]
pub async fn push_source_as(
    ep: &Endpoint,
    to: EndpointAddr,
    attestation: Option<Token>,
    source_root: &Path,
    manifest: &Manifest,
    checkout: &str,
    mode: PushMode,
    kind: SourceMode,
) -> Result<PushReport, String> {
    let limits = Limits::default();
    let root = open_root(source_root).map_err(|e| format!("source: {e}"))?;
    let receiver = to.id;
    let conn = ep
        .connect(to, SOURCE_ALPN)
        .await
        .map_err(|e| e.to_string())?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    let result = async {
        write_frame(&mut send, &Frame::Hello { attestation }).await?;
        let challenge = match read_frame(&mut recv).await? {
            Frame::Challenge { nonce } => nonce,
            Frame::Rejected { reason } => return Err(format!("rejected: {reason}")),
            other => return Err(format!("unexpected {other:?}")),
        };
        let body = serde_json::to_vec(&SourceRequest {
            checkout: checkout.to_owned(),
            manifest: manifest.clone(),
            mode: kind,
        })
        .map_err(|e| e.to_string())?;
        let sig = ep
            .secret_key()
            .sign(&request_message(&challenge, &ep.id(), &receiver, &body))
            .to_bytes()
            .to_vec();
        write_frame(
            &mut send,
            &Frame::Request {
                body: body.clone(),
                sig,
            },
        )
        .await?;
        let wanted = match read_frame(&mut recv).await? {
            Frame::Missing { hashes } => hashes,
            Frame::Rejected { reason } => return Err(format!("rejected: {reason}")),
            other => return Err(format!("unexpected {other:?}")),
        };
        let by_hash: BTreeMap<&str, &ManifestFile> = manifest
            .files
            .iter()
            .filter_map(|f| f.iroh_blobs_hash.as_deref().map(|h| (h, f)))
            .collect();
        let mut bytes_sent = 0u64;
        let mut blobs_sent = 0usize;
        let mut write_failed = None;
        for hash in &wanted {
            let f = by_hash
                .get(hash.as_str())
                .ok_or_else(|| format!("receiver asked for unknown blob {hash}"))?;
            let (mut content, _) =
                read_source_file(&root, &f.path, &limits).map_err(|e| format!("source: {e}"))?;
            if Hash::new(&content).to_hex() != *hash || content.len() as u64 != f.size {
                return Err(format!("source: {:?} changed since the manifest", f.path));
            }
            if matches!(mode, PushMode::TamperBlobs) && !content.is_empty() {
                content[0] ^= 0xff;
            }
            let header = Frame::Blob {
                hash: hash.clone(),
                size: f.size,
            };
            let sent = async {
                write_frame(&mut send, &header).await?;
                send.write_all(&content).await.map_err(|e| e.to_string())
            }
            .await;
            if let Err(e) = sent {
                // The receiver stopped reading (e.g. it rejected a blob): read its reason.
                write_failed = Some(e);
                break;
            }
            bytes_sent += f.size;
            blobs_sent += 1;
        }
        match read_frame(&mut recv).await {
            Ok(Frame::Response {
                body: response_body,
                sig,
            }) => {
                let response_fresh = verify_sig(
                    &receiver,
                    &response_message(&challenge, &receiver, &ep.id(), &body, &response_body),
                    &sig,
                );
                Ok(PushReport {
                    challenge,
                    missing: wanted,
                    blobs_sent,
                    bytes_sent,
                    response_body,
                    response_fresh,
                })
            }
            Ok(Frame::Rejected { reason }) => Err(format!("rejected: {reason}")),
            Ok(other) => Err(format!("unexpected {other:?}")),
            Err(e) => Err(write_failed.unwrap_or(e)),
        }
    }
    .await;
    conn.close(VarInt::from_u32(0), b"done");
    result
}
