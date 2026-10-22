//! Content-addressed source transfer into a stable per-checkout directory.
//!
//! * **Manifest.** Same JSON schema as `scripts/worker_source.py` (SHA-256 receipt over path,
//!   exec bit, size and SHA-256), with each file's `iroh_blobs_hash` filled: lowercase hex of
//!   `BLAKE3(content)`, exactly the iroh-blobs raw (`BlobFormat::Raw`) blob hash.
//! * **Store.** An upstream iroh-blobs `FsStore`. Every received blob gets a named persistent tag
//!   `task-manager/source/<hash>`, so it survives restarts and repeated syncs send nothing.
//! * **Protocol** (ALPN [`SOURCE_ALPN`]): `Hello` -> allowlist admission (role host/both) ->
//!   `Challenge` -> signed `Request{checkout, manifest, mode: "sync", identity}` -> `Missing`
//!   (the hashes the receiver's store lacks) -> one `Blob{hash,size}` + bytes per missing hash,
//!   each BLAKE3-verified before import -> in-place sync ([`crate::sync`]) under the per-checkout
//!   lock -> signed `Response` with the sync counts. Any failure answers `Rejected{reason}`.
//! * **Identity.** A sync names the sender's host and Git top level; the receiver checks the
//!   checkout name is `checkout_id(host, toplevel)` (the `tasks-offload` id, so the same warm
//!   directory is reused) and records `<id>.identity` for the cluster snapshot. A checkout whose
//!   identity names another peer endpoint is refused.
//! * **Materializer** (immutable, used for fetched artifacts): path-safe, no-follow `openat`,
//!   `O_EXCL|O_NOFOLLOW`, exec bit honored, verified after write, atomic no-replace rename.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr};
use iroh_blobs::Hash;
use iroh_blobs::api::Store;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Accepted, Allowlist, CLOSE_GRACE, Capability, ClientError, Domains, Frame, accept_session,
    open_session, read_frame, read_frame_limited, write_frame,
};

/// Source-transfer protocol; distinct from iroh-blobs' own ALPN.
pub const SOURCE_ALPN: &[u8] = b"task-manager/source/0";

#[cfg(test)]
#[path = "source_portability_tests.rs"]
mod portability_tests;
/// Shared with `scripts/worker_source.py`.
pub const SCHEMA: &str = "task-manager-local-source-manifest-v1";
pub const SOURCE_DOMAINS: Domains = Domains {
    request: b"task-manager/source/request/v0\0",
    response: b"task-manager/source/response/v0\0",
};
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

impl Limits {
    /// Fetched build outputs (APKs, binaries) are larger than source files; they are streamed
    /// through the store and never held in memory whole.
    pub fn artifact() -> Self {
        Self {
            max_file_bytes: 2 * 1024 * 1024 * 1024,
            max_bytes: 4 * 1024 * 1024 * 1024,
            ..Self::default()
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
// No-follow filesystem primitives (Unix); only atomic no-replace rename is OS-specific.

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
        // Reset thread-local errno portably to distinguish EOF from a readdir error.
        nix::errno::Errno::clear();
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
fn rename_noreplace(dir: RawFd, from: &CStr, to: &CStr) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated and relative to the caller's open directory.
    // RENAME_EXCL is atomic: an existing destination (including a symlink) is never replaced.
    cvt(unsafe { libc::renameatx_np(dir, from.as_ptr(), dir, to.as_ptr(), libc::RENAME_EXCL) })
        .map(drop)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_noreplace(_dir: RawFd, _from: &CStr, _to: &CStr) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace directory rename is supported only on Linux and macOS",
    ))
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
    // Hardlinks are read like any regular file (as `tasks-offload` does); symlinks were
    // refused by the no-follow open above.
    if !meta.is_file() {
        return Err(SourceError::UnsafePath(format!(
            "{path:?}: special files/directories are unsupported"
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

/// Chunk size for streaming file and blob content.
pub(crate) const CHUNK: usize = 1024 * 1024;

/// One pass over `reader`: blake3 (the blob hash), SHA-256 and length, in `CHUNK`s.
pub(crate) fn digest_reader(reader: &mut impl Read) -> io::Result<(Hash, String, u64)> {
    let mut blake = blake3::Hasher::new();
    let mut sha = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut len = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        blake.update(&buf[..n]);
        sha.update(&buf[..n]);
        len += n as u64;
    }
    let hex: String = sha.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((Hash::from_bytes(*blake.finalize().as_bytes()), hex, len))
}

/// Receiving end of an mpsc channel as the stream `Blobs::add_stream` takes.
struct ChunkStream(tokio::sync::mpsc::Receiver<io::Result<bytes::Bytes>>);

impl n0_future::Stream for ChunkStream {
    type Item = io::Result<bytes::Bytes>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

/// Streams the chunks `pump` sends into the store without holding the whole blob in memory.
/// Returns the temp tag of what the store computed; the caller compares its hash with the
/// one it expects before tagging. A pump error fails the import.
pub(crate) async fn import_chunks<P, Fut>(
    store: &Store,
    pump: P,
) -> Result<iroh_blobs::api::TempTag, SourceError>
where
    P: FnOnce(tokio::sync::mpsc::Sender<io::Result<bytes::Bytes>>) -> Fut,
    Fut: std::future::Future<Output = io::Result<()>>,
{
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let error_tx = tx.clone();
    let add = store.blobs().add_stream(ChunkStream(rx)).await;
    let (pumped, added) = tokio::join!(
        async {
            let result = pump(tx).await;
            if let Err(e) = &result {
                let _ = error_tx
                    .send(Err(io::Error::new(e.kind(), e.to_string())))
                    .await;
            }
            drop(error_tx);
            result
        },
        add.temp_tag()
    );
    pumped.map_err(|e| SourceError::Io(format!("stream into the store: {e}")))?;
    added.map_err(|e| SourceError::Io(format!("store import: {e}")))
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

async fn write_tree(
    root: &OwnedFd,
    store: &Store,
    files: &[ManifestFile],
    limits: &Limits,
) -> Result<(), SourceError> {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; CHUNK];
    for f in files {
        let parts = check_path(&f.path, limits)?;
        let parent = descend(root, &parts, true)?;
        let name = cstr(parts[parts.len() - 1])?;
        let mut file = create_file_at(parent.as_raw_fd(), &name)
            .map_err(|e| SourceError::UnsafePath(format!("create {:?}: {e}", f.path)))?;
        if f.size > 0 {
            let hex = f.iroh_blobs_hash.clone().unwrap_or_default();
            let hash: Hash = hex
                .parse()
                .map_err(|_| SourceError::InvalidManifest(hex.clone()))?;
            let mut reader = store.blobs().reader(hash);
            let mut left = f.size;
            loop {
                let n = reader
                    .read(&mut buf)
                    .await
                    .map_err(|e| SourceError::MissingBlob(format!("{hex}: {e}")))?;
                if n == 0 {
                    break;
                }
                if n as u64 > left {
                    return Err(SourceError::HashMismatch(hex));
                }
                file.write_all(&buf[..n]).map_err(io_err(&f.path))?;
                left -= n as u64;
            }
            if left != 0 {
                return Err(SourceError::HashMismatch(hex));
            }
        }
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
        let (blake, sha, len) = digest_reader(&mut file).map_err(io_err(&f.path))?;
        if !meta.is_file()
            || meta.permissions().mode() & 0o777 != mode_of(f.executable)
            || len != f.size
            || Some(blake.to_hex().as_str()) != f.iroh_blobs_hash.as_deref()
            || sha != f.sha256
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

    // Every blob must be in the store before the filesystem is touched; the content is
    // streamed to disk below and verified against the manifest after writing.
    for f in &manifest.files {
        if f.size == 0 {
            continue;
        }
        let hex = f.iroh_blobs_hash.clone().unwrap_or_default();
        let hash: Hash = hex
            .parse()
            .map_err(|_| SourceError::InvalidManifest(hex.clone()))?;
        if !store.blobs().has(hash).await.unwrap_or(false) {
            return Err(SourceError::MissingBlob(hex));
        }
    }

    let mut nonce = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let tmp_name = format!(".tmp-{name}-{}", crate::hex(&nonce));
    let tmp = cstr(&tmp_name)?;
    mkdir_at(root.as_raw_fd(), &tmp, 0o700).map_err(io_err(&tmp_name))?;
    let result = async {
        let tmp_fd = open_dir_at(root.as_raw_fd(), &tmp).map_err(io_err(&tmp_name))?;
        write_tree(&tmp_fd, store, &manifest.files, limits).await?;
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
    }
    .await;
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

/// What the receiver does with a validated manifest. The daemon serves `Sync` only; the
/// field is kept so request bodies stay shaped like the `lan-peer` experiment's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceMode {
    #[default]
    Immutable,
    Sync,
}

impl SourceMode {
    fn is_immutable(&self) -> bool {
        *self == SourceMode::Immutable
    }
}

/// Where a synced checkout comes from (the sender's view), as in `tasks-offload`'s
/// `<id>.identity` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutIdentity {
    pub host: String,
    pub toplevel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRequest {
    pub checkout: String,
    pub manifest: Manifest,
    #[serde(default, skip_serializing_if = "SourceMode::is_immutable")]
    pub mode: SourceMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<CheckoutIdentity>,
}

/// Signed answer to a sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReply {
    pub checkout: PathBuf,
    pub created: bool,
    pub files: usize,
    pub unchanged: usize,
    pub written: usize,
    pub written_bytes: u64,
    pub mode_fixed: usize,
    pub deleted: usize,
    /// First 200 pruned paths.
    pub deleted_paths: Vec<String>,
    pub kept_non_source: usize,
    pub blobs_received: usize,
    pub bytes_received: u64,
}

/// Receiver configuration.
pub struct SourceService {
    pub store: Store,
    pub checkouts: PathBuf,
    pub limits: Limits,
}

#[derive(Debug)]
pub enum SourceOutcome {
    Synced {
        peer: String,
        reply: SyncReply,
    },
    Rejected {
        peer: Option<String>,
        reason: String,
    },
}

fn bounded_text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(|c| c.is_control())
}

/// Path of the identity record of checkout `name`.
pub fn identity_path(checkouts: &Path, name: &str) -> PathBuf {
    checkouts.join(format!("{name}.identity"))
}

/// Refuses an identity that does not hash to `name`, or an existing record naming another
/// peer endpoint. Python-written records (no `peer` key) are accepted.
pub fn check_identity(
    checkouts: &Path,
    name: &str,
    identity: &CheckoutIdentity,
    peer_endpoint: &str,
) -> Result<(), String> {
    if !bounded_text(&identity.host) || !bounded_text(&identity.toplevel) {
        return Err("InvalidIdentity".into());
    }
    if crate::select::checkout_id(&identity.host, &identity.toplevel) != name {
        return Err(format!(
            "IdentityMismatch(checkout {name} is not the id of {}:{})",
            identity.host, identity.toplevel
        ));
    }
    match std::fs::read(identity_path(checkouts, name)) {
        Ok(bytes) => {
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            match value.get("peer").and_then(|p| p.as_str()) {
                Some(owner) if owner != peer_endpoint => {
                    Err(format!("CheckoutOwnedByAnotherPeer({name})"))
                }
                _ => Ok(()),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Io(identity {name}: {e})")),
    }
}

/// Atomically (temp + rename) records the identity of checkout `name`.
pub fn write_identity(
    checkouts: &Path,
    name: &str,
    identity: &CheckoutIdentity,
    peer_endpoint: &str,
    peer_name: &str,
) -> Result<(), String> {
    let value = serde_json::json!({
        "host": identity.host,
        "toplevel": identity.toplevel,
        "peer": peer_endpoint,
        "peer_name": peer_name,
        "transport": "iroh",
    });
    let path = identity_path(checkouts, name);
    let tmp = checkouts.join(format!(".{name}.identity.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(&value).map_err(|e| e.to_string())?)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("identity {}: {e}", path.display()))
}

/// Receiver of one sync on `conn`.
pub async fn handle_source(
    ep: &Endpoint,
    conn: Connection,
    allow: &Allowlist,
    svc: &SourceService,
) -> Result<SourceOutcome, String> {
    let mut s = match accept_session(
        ep,
        &conn,
        allow,
        Capability::Source,
        SOURCE_DOMAINS,
        MAX_SOURCE_FRAME,
    )
    .await?
    {
        Accepted::Refused(reason) => {
            let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
            return Ok(SourceOutcome::Rejected { peer: None, reason });
        }
        Accepted::Session(s) => s,
    };
    let peer_name = s.peer.name.clone();
    let peer_endpoint = s.remote.to_string();
    let limits = &svc.limits;
    let outcome: Result<SyncReply, String> = async {
        let request: SourceRequest = serde_json::from_slice(&s.body)
            .map_err(|e| format!("{:?}", SourceError::InvalidManifest(e.to_string())))?;
        if request.mode != SourceMode::Sync {
            return Err("UnsupportedMode(only sync)".into());
        }
        let identity = request
            .identity
            .as_ref()
            .ok_or("MissingIdentity".to_string())?;
        check_identity(&svc.checkouts, &request.checkout, identity, &peer_endpoint)?;
        let sizes = crate::sync::precheck(&request.manifest, &request.checkout, limits)
            .map_err(|e| format!("{e:?}"))?;
        let wanted = missing(&svc.store, sizes.keys().copied())
            .await
            .map_err(|e| format!("{e:?}"))?;
        write_frame(
            &mut s.send,
            &Frame::Missing {
                hashes: wanted.clone(),
            },
        )
        .await?;
        let mut bytes_received = 0u64;
        for expected in &wanted {
            let (hash, size) = match read_frame(&mut s.recv).await? {
                Frame::Blob { hash, size } => (hash, size),
                other => return Err(format!("expected blob, got {other:?}")),
            };
            let expected_hash: Hash = expected.parse().map_err(|_| "bad hash".to_string())?;
            if &hash != expected {
                let _ = s.recv.stop(iroh::endpoint::VarInt::from_u32(1));
                return Err(format!("UnrequestedBlob({hash})"));
            }
            if Some(&size) != sizes.get(&expected_hash) {
                let _ = s.recv.stop(iroh::endpoint::VarInt::from_u32(1));
                return Err(format!("SizeMismatch({hash})"));
            }
            let mut buf = vec![0u8; size as usize];
            s.recv
                .read_exact(&mut buf)
                .await
                .map_err(|e| e.to_string())?;
            if let Err(e) = import_verified(&svc.store, expected_hash, buf).await {
                let _ = s.recv.stop(iroh::endpoint::VarInt::from_u32(1));
                return Err(format!("{e:?}"));
            }
            bytes_received += size;
        }
        let report = crate::sync::sync_checkout(
            &svc.store,
            &svc.checkouts,
            &request.checkout,
            &request.manifest,
            limits,
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
        write_identity(
            &svc.checkouts,
            &request.checkout,
            identity,
            &peer_endpoint,
            &peer_name,
        )?;
        Ok(SyncReply {
            checkout: report.checkout,
            created: report.created,
            files: report.files,
            unchanged: report.unchanged,
            written: report.written,
            written_bytes: report.written_bytes,
            mode_fixed: report.mode_fixed,
            deleted: report.deleted.len(),
            deleted_paths: report.deleted.into_iter().take(200).collect(),
            kept_non_source: report.kept_non_source,
            blobs_received: wanted.len(),
            bytes_received,
        })
    }
    .await;
    let outcome = match outcome {
        Ok(reply) => {
            let body = serde_json::to_vec(&reply).map_err(|e| e.to_string())?;
            s.respond(ep, &body).await?;
            let _ = s.send.finish();
            SourceOutcome::Synced {
                peer: peer_name,
                reply,
            }
        }
        Err(reason) => {
            // The client may already be gone (e.g. it stopped after a refused blob).
            let _ = s.reject(&reason).await;
            SourceOutcome::Rejected {
                peer: Some(peer_name),
                reason,
            }
        }
    };
    let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
    Ok(outcome)
}

#[derive(Debug, Clone)]
pub struct PushReport {
    pub missing: Vec<String>,
    pub blobs_sent: usize,
    pub bytes_sent: u64,
    pub reply: SyncReply,
}

/// Sender of one sync: offers `manifest` for stable checkout `checkout`, then sends exactly the
/// blobs the receiver reports missing, read no-follow from `source_root` and re-verified
/// against the manifest before sending.
pub async fn sync_push(
    ep: &Endpoint,
    to: EndpointAddr,
    source_root: &Path,
    manifest: &Manifest,
    checkout: &str,
    identity: &CheckoutIdentity,
    timeout: std::time::Duration,
) -> Result<PushReport, ClientError> {
    let limits = Limits::default();
    let root = open_root(source_root).map_err(|e| ClientError::Local(format!("source: {e}")))?;
    let body = serde_json::to_vec(&SourceRequest {
        checkout: checkout.to_owned(),
        manifest: manifest.clone(),
        mode: SourceMode::Sync,
        identity: Some(identity.clone()),
    })
    .map_err(|e| ClientError::Local(e.to_string()))?;
    let mut s = open_session(ep, to, SOURCE_ALPN, SOURCE_DOMAINS, &body, timeout).await?;
    let result = async {
        let wanted = match read_frame_limited(&mut s.recv, MAX_SOURCE_FRAME).await {
            Ok(Frame::Missing { hashes }) => hashes,
            Ok(Frame::Rejected { reason }) => return Err(ClientError::Refused(reason)),
            Ok(other) => return Err(ClientError::Protocol(format!("unexpected {other:?}"))),
            Err(e) => return Err(ClientError::Lost(e)),
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
            let f = by_hash.get(hash.as_str()).ok_or_else(|| {
                ClientError::Protocol(format!("receiver asked for unknown blob {hash}"))
            })?;
            let (content, _) = read_source_file(&root, &f.path, &limits)
                .map_err(|e| ClientError::Local(format!("source: {e}")))?;
            if Hash::new(&content).to_hex() != *hash || content.len() as u64 != f.size {
                return Err(ClientError::Local(format!(
                    "source: {:?} changed since it was hashed; keep the checkout stable and retry",
                    f.path
                )));
            }
            let sent = async {
                write_frame(
                    &mut s.send,
                    &Frame::Blob {
                        hash: hash.clone(),
                        size: f.size,
                    },
                )
                .await?;
                s.send.write_all(&content).await.map_err(|e| e.to_string())
            }
            .await;
            if let Err(e) = sent {
                write_failed = Some(e);
                break;
            }
            bytes_sent += f.size;
            blobs_sent += 1;
        }
        let response = s.response(MAX_SOURCE_FRAME).await;
        let body = match (response, write_failed) {
            (Ok(body), _) => body,
            (Err(ClientError::Lost(e)), Some(w)) => {
                return Err(ClientError::Lost(format!("{w}; {e}")));
            }
            (Err(e), _) => return Err(e),
        };
        let reply: SyncReply = serde_json::from_slice(&body)
            .map_err(|e| ClientError::Protocol(format!("bad sync reply: {e}")))?;
        if reply.checkout.file_name().and_then(|n| n.to_str()) != Some(checkout) {
            return Err(ClientError::Protocol(format!(
                "receiver reported an unexpected checkout {}",
                reply.checkout.display()
            )));
        }
        Ok(PushReport {
            missing: wanted,
            blobs_sent,
            bytes_sent,
            reply,
        })
    }
    .await;
    s.close();
    result
}
