//! Fetching named outputs back: content-addressed, bounded, and confined to
//! the checkout (R-159, REQUIREMENTS 7.13).

use crate::ids::{CheckoutId, Hash};
use crate::proto::{Body, FetchEntry, MAX_CHUNK_BYTES, MAX_FETCH_PATHS, Refusal};
use crate::sync::{check_rel_path, hash_file};
use crate::transport::{Connection, PeerError};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// What the serving side allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchPolicy {
    /// Largest file served (R-159: 2 GiB).
    pub max_file_bytes: u64,
    /// Top-level names that may be coordinator-made output links (`target`,
    /// `build`) pointing outside the tree. Every other symlink is refused.
    pub allowed_links: Vec<String>,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        FetchPolicy {
            max_file_bytes: 2 * 1024 * 1024 * 1024,
            allowed_links: Vec::new(),
        }
    }
}

fn resolve(tree: &Path, rel: &str, policy: &FetchPolicy) -> Result<PathBuf, String> {
    check_rel_path(rel)?;
    let mut cur = tree.to_path_buf();
    let comps: Vec<&str> = rel.split('/').collect();
    for (i, c) in comps.iter().enumerate() {
        cur.push(c);
        let Ok(m) = fs::symlink_metadata(&cur) else {
            // Not there: later components cannot exist either.
            return Ok(tree.join(rel));
        };
        if m.file_type().is_symlink() {
            let output_link =
                i == 0 && comps.len() > 1 && policy.allowed_links.iter().any(|l| l == c);
            if !output_link {
                return Err(format!("{c} is a symlink"));
            }
        }
    }
    Ok(tree.join(rel))
}

/// Lists what can be fetched for each requested path.
pub fn list(
    tree: &Path,
    paths: &[String],
    policy: &FetchPolicy,
) -> Result<Vec<FetchEntry>, Refusal> {
    if paths.len() > MAX_FETCH_PATHS {
        return Err(Refusal::LimitExceeded {
            detail: format!("more than {MAX_FETCH_PATHS} paths in one request"),
        });
    }
    Ok(paths.iter().map(|p| list_one(tree, p, policy)).collect())
}

fn list_one(tree: &Path, rel: &str, policy: &FetchPolicy) -> FetchEntry {
    let path = rel.to_string();
    let abs = match resolve(tree, rel, policy) {
        Ok(a) => a,
        Err(reason) => return FetchEntry::Refused { path, reason },
    };
    let Ok(m) = fs::metadata(&abs) else {
        return FetchEntry::Missing { path };
    };
    if !m.is_file() {
        return FetchEntry::Refused {
            path,
            reason: "not a regular file".to_string(),
        };
    }
    // The final component may itself be an output link target; metadata()
    // follows it, which is only reachable through an allowed link.
    if m.len() > policy.max_file_bytes {
        return FetchEntry::TooLarge {
            path,
            size: m.len(),
            limit: policy.max_file_bytes,
        };
    }
    match hash_file(&abs) {
        Ok((hash, size)) => FetchEntry::Found {
            path,
            hash,
            size,
            exec: m.permissions().mode() & 0o111 != 0,
        },
        Err(e) => FetchEntry::Refused {
            path,
            reason: format!("unreadable: {e}"),
        },
    }
}

/// Reads one chunk. At offset 0 the file is re-hashed against `hash`, so a
/// file that changed between listing and fetching is refused rather than
/// served as something else.
pub fn read_chunk(
    tree: &Path,
    rel: &str,
    hash: &Hash,
    offset: u64,
    len: u32,
    policy: &FetchPolicy,
) -> Result<(Vec<u8>, bool), Refusal> {
    let bad = |reason: String| Refusal::BadPath {
        path: rel.to_string(),
        reason,
    };
    let abs = resolve(tree, rel, policy).map_err(bad)?;
    if offset == 0 {
        let (h, _) = hash_file(&abs).map_err(|e| bad(format!("unreadable: {e}")))?;
        if h != *hash {
            return Err(Refusal::HashMismatch);
        }
    }
    let want = usize::try_from(len)
        .unwrap_or(MAX_CHUNK_BYTES)
        .min(MAX_CHUNK_BYTES);
    let mut f = fs::File::open(&abs).map_err(|e| bad(format!("unreadable: {e}")))?;
    let size = f.metadata().map_err(|e| bad(e.to_string()))?.len();
    f.seek(SeekFrom::Start(offset))
        .map_err(|e| bad(e.to_string()))?;
    let mut buf = vec![0u8; want];
    let mut got = 0;
    while got < want {
        let n = f
            .read(buf.get_mut(got..).unwrap_or(&mut []))
            .map_err(|e| bad(e.to_string()))?;
        if n == 0 {
            break;
        }
        got += n;
    }
    buf.truncate(got);
    let eof = offset + got as u64 >= size;
    Ok((buf, eof))
}

/// Why a fetch failed as a whole (single files are reported in the result).
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The link or the peer failed.
    #[error("fetch failed: {0}")]
    Peer(#[from] PeerError),
    /// The destination could not be written.
    #[error("writing {path}: {detail}")]
    Write {
        /// The path.
        path: String,
        /// Detail.
        detail: String,
    },
    /// The peer sent something unexpected.
    #[error("unexpected reply to {0}")]
    Unexpected(&'static str),
}

/// One fetched file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// Relative path.
    pub path: String,
    /// Content hash (verified).
    pub hash: Hash,
    /// Size.
    pub size: u64,
    /// True when the destination already had this content and nothing was transferred.
    pub unchanged: bool,
}

/// The result of a fetch: the rest still arrives when some files are refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchReport {
    /// Files now in place.
    pub fetched: Vec<Fetched>,
    /// Paths the peer does not have.
    pub missing: Vec<String>,
    /// Paths over the limit: `(path, size, limit)`.
    pub too_large: Vec<(String, u64, u64)>,
    /// Paths refused or failed: `(path, reason)`.
    pub refused: Vec<(String, String)>,
}

/// Fetches `paths` of the peer's copy of `checkout` into `dest`.
pub fn fetch(
    conn: &mut dyn Connection,
    checkout: &CheckoutId,
    paths: &[String],
    dest: &Path,
    max_file_bytes: u64,
) -> Result<FetchReport, FetchError> {
    let mut report = FetchReport::default();
    for batch in paths.chunks(MAX_FETCH_PATHS) {
        let reply = conn.request(Body::FetchList {
            checkout: checkout.clone(),
            paths: batch.to_vec(),
        })?;
        let Body::FetchListing { entries } = reply else {
            return Err(FetchError::Unexpected("fetch_list"));
        };
        for entry in entries {
            match entry {
                FetchEntry::Missing { path } => report.missing.push(path),
                FetchEntry::TooLarge { path, size, limit } => {
                    report.too_large.push((path, size, limit))
                }
                FetchEntry::Refused { path, reason } => report.refused.push((path, reason)),
                FetchEntry::Found {
                    path,
                    hash,
                    size,
                    exec,
                } => {
                    if let Err(e) = check_rel_path(&path) {
                        report
                            .refused
                            .push((path, format!("peer sent an unsafe path: {e}")));
                        continue;
                    }
                    if size > max_file_bytes {
                        report.too_large.push((path, size, max_file_bytes));
                        continue;
                    }
                    match fetch_one(conn, checkout, &path, hash, size, exec, dest)? {
                        Ok(unchanged) => report.fetched.push(Fetched {
                            path,
                            hash,
                            size,
                            unchanged,
                        }),
                        Err(reason) => report.refused.push((path, reason)),
                    }
                }
            }
        }
    }
    Ok(report)
}

/// `Ok(Ok(unchanged))` on success, `Ok(Err(reason))` for a per-file failure.
fn fetch_one(
    conn: &mut dyn Connection,
    checkout: &CheckoutId,
    path: &str,
    hash: Hash,
    size: u64,
    exec: bool,
    dest: &Path,
) -> Result<Result<bool, String>, FetchError> {
    let target = dest.join(path);
    let write_err = |e: std::io::Error| FetchError::Write {
        path: path.to_string(),
        detail: e.to_string(),
    };
    if fs::metadata(&target).is_ok_and(|m| m.is_file() && m.len() == size)
        && hash_file(&target).is_ok_and(|(h, _)| h == hash)
    {
        return Ok(Ok(true));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(write_err)?;
    }
    let tmp = target.with_file_name(format!(
        ".{}.tasks-fetch",
        target.file_name().and_then(|n| n.to_str()).unwrap_or("out")
    ));
    let mut out = fs::File::create(&tmp).map_err(write_err)?;
    let mut digest = Sha256::new();
    let mut offset = 0u64;
    loop {
        let reply = conn.request(Body::FetchChunk {
            checkout: checkout.clone(),
            path: path.to_string(),
            hash,
            offset,
            len: MAX_CHUNK_BYTES as u32,
        });
        let (data, eof) = match reply {
            Ok(Body::FetchData {
                offset: o,
                data,
                eof,
            }) if o == offset => (data, eof),
            Ok(_) => {
                let _ = fs::remove_file(&tmp);
                return Err(FetchError::Unexpected("fetch_chunk"));
            }
            Err(PeerError::Refused(r)) => {
                let _ = fs::remove_file(&tmp);
                return Ok(Err(r.to_string()));
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e.into());
            }
        };
        digest.update(&data);
        out.write_all(&data).map_err(write_err)?;
        offset += data.len() as u64;
        if offset > size {
            let _ = fs::remove_file(&tmp);
            return Ok(Err("peer sent more than the announced size".to_string()));
        }
        if eof || data.is_empty() {
            break;
        }
    }
    drop(out);
    if offset != size || Hash::from_digest(digest) != hash {
        let _ = fs::remove_file(&tmp);
        return Ok(Err("content does not match its hash".to_string()));
    }
    let mode = if exec { 0o755 } else { 0o644 };
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(write_err)?;
    fs::rename(&tmp, &target).map_err(write_err)?;
    Ok(Ok(false))
}
