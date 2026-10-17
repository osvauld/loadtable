//! Artifact return over iroh-blobs, the reverse direction of [`crate::source`].
//!
//! ALPN [`ARTIFACT_ALPN`] = `task-manager/artifact/0`: `Hello` -> allowlist admission (role
//! host/both) -> `Challenge` -> signed `Request{checkout, paths}`. The receiver serves only a
//! checkout whose identity record names the requesting peer; it validates the paths (inside the
//! checkout, reached no-follow; a requested symlink is refused, symlinks/special files inside
//! requested directories are skipped and listed), imports the files into its store (tag
//! `task-manager/artifact/<hex>`) and answers a signed `{checkout, manifest, skipped}`. The client
//! validates the manifest itself, asks only for the hashes its own store lacks, BLAKE3-verifies
//! every blob before import and materializes into a NEW directory (immutable materializer).
//! Whole blobs are buffered in memory (at most `max_file_bytes` each).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use iroh::endpoint::{Connection, VarInt};
use iroh::{Endpoint, EndpointAddr};
use iroh_blobs::Hash;
use iroh_blobs::api::Store;
use serde::{Deserialize, Serialize};

use crate::source::{
    Limits, MAX_SOURCE_FRAME, Manifest, ManifestFile, SourceError, check_checkout_name, check_path,
    cstr, descend, exists_nofollow, import_verified_tagged, materialize, missing, open_dir_at,
    open_file_at, open_root, read_dir_names, sha256_hex, stat_at, validate_manifest,
};
use crate::source::{SourceService, identity_path};
use crate::{
    Accepted, Allowlist, CLOSE_GRACE, Capability, ClientError, Domains, Frame, accept_session,
    open_session, read_frame, read_frame_limited, write_frame,
};

pub const ARTIFACT_ALPN: &[u8] = b"task-manager/artifact/0";
/// Named-tag prefix protecting exported (worker) and received (client) artifact blobs.
pub const ARTIFACT_TAG_PREFIX: &str = "task-manager/artifact/";
pub const ARTIFACT_DOMAINS: Domains = Domains {
    request: b"task-manager/artifact/request/v0\0",
    response: b"task-manager/artifact/response/v0\0",
};
/// At most this many requested paths per request.
pub const MAX_REQUEST_PATHS: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRequest {
    pub checkout: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactResponse {
    pub checkout: String,
    pub manifest: Manifest,
    /// Symlinks, special files and non-UTF-8 names found inside requested directories; never
    /// followed or sent.
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Export {
    pub manifest: Manifest,
    pub skipped: Vec<String>,
}

fn is_dir(mode: libc::mode_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFDIR
}

fn is_reg(mode: libc::mode_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFREG
}

fn walk(
    dir: &OwnedFd,
    base: &str,
    limits: &Limits,
    files: &mut BTreeSet<String>,
    skipped: &mut BTreeSet<String>,
) -> Result<(), SourceError> {
    let names = read_dir_names(dir.as_raw_fd())
        .map_err(|e| SourceError::Io(format!("list {base:?}: {e}")))?;
    for cname in names {
        let Ok(name) = cname.to_str() else {
            skipped.insert(format!(
                "{base}/{}",
                String::from_utf8_lossy(cname.to_bytes())
            ));
            continue;
        };
        let rel = format!("{base}/{name}");
        check_path(&rel, limits)?;
        let st = stat_at(dir.as_raw_fd(), &cname)
            .map_err(|e| SourceError::Io(format!("stat {rel:?}: {e}")))?;
        if is_dir(st.st_mode) {
            let child = open_dir_at(dir.as_raw_fd(), &cname)
                .map_err(|e| SourceError::UnsafePath(format!("{rel:?}: {e}")))?;
            walk(&child, &rel, limits, files, skipped)?;
        } else if is_reg(st.st_mode) {
            files.insert(rel);
            if files.len() > limits.max_files {
                return Err(SourceError::Limit("too many artifact files".into()));
            }
        } else {
            skipped.insert(rel);
        }
    }
    Ok(())
}

/// Reads one artifact file no-follow (hardlinks allowed: Cargo hardlinks binaries into deps/).
fn read_artifact(
    co: &OwnedFd,
    path: &str,
    limits: &Limits,
) -> Result<(Vec<u8>, bool), SourceError> {
    let parts = check_path(path, limits)?;
    let parent = descend(co, &parts, false)?;
    let file = open_file_at(parent.as_raw_fd(), &cstr(parts[parts.len() - 1])?).map_err(|e| {
        if e.raw_os_error() == Some(libc::ELOOP) {
            SourceError::UnsafePath(format!("{path:?} is a symlink"))
        } else {
            SourceError::Io(format!("{path:?}: {e}"))
        }
    })?;
    let meta = file
        .metadata()
        .map_err(|e| SourceError::Io(format!("{path:?}: {e}")))?;
    if !meta.is_file() {
        return Err(SourceError::UnsafePath(format!(
            "{path:?} is not a regular file"
        )));
    }
    if meta.len() > limits.max_file_bytes {
        return Err(SourceError::Limit(format!("{path:?}: file too large")));
    }
    let mut content = Vec::with_capacity(meta.len() as usize);
    file.take(limits.max_file_bytes + 1)
        .read_to_end(&mut content)
        .map_err(|e| SourceError::Io(format!("{path:?}: {e}")))?;
    if content.len() as u64 != meta.len() {
        return Err(SourceError::Io(format!("{path:?} changed while reading")));
    }
    Ok((content, meta.permissions().mode() & 0o111 != 0))
}

/// Worker side: validates `paths` under `checkout_root/name`, imports every file into `store`
/// (tagged), and returns the manifest. Nothing outside the checkout is read; no symlink is
/// followed.
pub async fn export(
    store: &Store,
    checkout_root: &Path,
    name: &str,
    paths: &[String],
    limits: &Limits,
) -> Result<Export, SourceError> {
    check_checkout_name(name)?;
    if paths.is_empty() || paths.len() > MAX_REQUEST_PATHS {
        return Err(SourceError::Limit(format!(
            "between 1 and {MAX_REQUEST_PATHS} artifact paths per request"
        )));
    }
    for p in paths {
        check_path(p, limits)?;
    }
    let root = open_root(checkout_root)?;
    let co = open_dir_at(root.as_raw_fd(), &cstr(name)?).map_err(|e| {
        if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) {
            SourceError::UnsafePath(format!("checkout {name:?} is not a plain directory"))
        } else {
            SourceError::Io(format!("checkout {name:?}: {e}"))
        }
    })?;

    let mut files = BTreeSet::new();
    let mut skipped = BTreeSet::new();
    for p in paths {
        let parts = check_path(p, limits)?;
        let parent = descend(&co, &parts, false)?;
        let last = cstr(parts[parts.len() - 1])?;
        let st = stat_at(parent.as_raw_fd(), &last)
            .map_err(|e| SourceError::Io(format!("{p:?}: {e}")))?;
        if is_dir(st.st_mode) {
            let dir = open_dir_at(parent.as_raw_fd(), &last)
                .map_err(|e| SourceError::UnsafePath(format!("{p:?}: {e}")))?;
            walk(&dir, p, limits, &mut files, &mut skipped)?;
        } else if is_reg(st.st_mode) {
            files.insert(p.clone());
            if files.len() > limits.max_files {
                return Err(SourceError::Limit("too many artifact files".into()));
            }
        } else {
            return Err(SourceError::UnsafePath(format!(
                "{p:?} is a symlink or special file"
            )));
        }
    }

    let mut total = 0u64;
    let mut entries = Vec::with_capacity(files.len());
    for path in files {
        let (content, executable) = read_artifact(&co, &path, limits)?;
        total += content.len() as u64;
        if total > limits.max_bytes {
            return Err(SourceError::Limit("aggregate bytes exceed bound".into()));
        }
        let hash = Hash::new(&content);
        let entry = ManifestFile {
            path,
            size: content.len() as u64,
            executable,
            sha256: sha256_hex(&content),
            iroh_blobs_hash: Some(hash.to_hex()),
        };
        import_verified_tagged(store, hash, content, ARTIFACT_TAG_PREFIX).await?;
        entries.push(entry);
    }
    let manifest = Manifest::from_files_unchecked(entries);
    validate_manifest(&manifest, limits)?;
    Ok(Export {
        manifest,
        skipped: skipped.into_iter().collect(),
    })
}

#[derive(Debug)]
pub enum ArtifactOutcome {
    Served {
        peer: String,
        checkout: String,
        files: usize,
        blobs_sent: usize,
        bytes_sent: u64,
    },
    Rejected {
        peer: Option<String>,
        reason: String,
    },
}

/// The checkout must have been synced by this peer (its identity record names the endpoint).
fn owned_by(checkouts: &Path, name: &str, endpoint: &str) -> Result<(), String> {
    check_checkout_name(name).map_err(|e| format!("{e:?}"))?;
    let bytes = std::fs::read(identity_path(checkouts, name))
        .map_err(|_| format!("UnknownCheckout({name})"))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    if value.get("peer").and_then(|p| p.as_str()) == Some(endpoint) {
        Ok(())
    } else {
        Err(format!("CheckoutOwnedByAnotherPeer({name})"))
    }
}

pub async fn handle_artifact(
    ep: &Endpoint,
    conn: Connection,
    allow: &Allowlist,
    svc: &SourceService,
) -> Result<ArtifactOutcome, String> {
    let mut s = match accept_session(
        ep,
        &conn,
        allow,
        Capability::Artifact,
        ARTIFACT_DOMAINS,
        MAX_SOURCE_FRAME,
    )
    .await?
    {
        Accepted::Refused(reason) => {
            let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
            return Ok(ArtifactOutcome::Rejected { peer: None, reason });
        }
        Accepted::Session(s) => s,
    };
    let peer = s.peer.name.clone();
    let endpoint = s.remote.to_string();
    let result: Result<ArtifactOutcome, String> = async {
        let request: ArtifactRequest = serde_json::from_slice(&s.body)
            .map_err(|e| format!("{:?}", SourceError::InvalidManifest(e.to_string())))?;
        owned_by(&svc.checkouts, &request.checkout, &endpoint)?;
        let exported = export(
            &svc.store,
            &svc.checkouts,
            &request.checkout,
            &request.paths,
            &svc.limits,
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
        let manifest = exported.manifest;
        let sizes: BTreeMap<String, u64> = manifest
            .files
            .iter()
            .map(|f| (f.iroh_blobs_hash.clone().unwrap_or_default(), f.size))
            .collect();
        let response = serde_json::to_vec(&ArtifactResponse {
            checkout: request.checkout.clone(),
            manifest: manifest.clone(),
            skipped: exported.skipped,
        })
        .map_err(|e| e.to_string())?;
        s.respond(ep, &response).await?;
        let wanted = match read_frame_limited(&mut s.recv, MAX_SOURCE_FRAME).await {
            Ok(Frame::Missing { hashes }) => hashes,
            Ok(other) => return Err(format!("expected missing, got {other:?}")),
            Err(e) => return Err(format!("client closed after the manifest: {e}")),
        };
        let unique: BTreeSet<&String> = wanted.iter().collect();
        if unique.len() != wanted.len() || wanted.iter().any(|h| !sizes.contains_key(h)) {
            return Err("UnknownBlob".into());
        }
        let mut bytes_sent = 0u64;
        for hex in &wanted {
            let hash: Hash = hex.parse().map_err(|_| "bad hash".to_string())?;
            let bytes = svc
                .store
                .blobs()
                .get_bytes(hash)
                .await
                .map_err(|e| format!("store: {e}"))?
                .to_vec();
            if Hash::new(&bytes) != hash || bytes.len() as u64 != sizes[hex] {
                return Err(format!("HashMismatch({hex})"));
            }
            write_frame(
                &mut s.send,
                &Frame::Blob {
                    hash: hex.clone(),
                    size: bytes.len() as u64,
                },
            )
            .await?;
            s.send
                .write_all(&bytes)
                .await
                .map_err(|e| format!("client stopped the transfer: {e}"))?;
            bytes_sent += bytes.len() as u64;
        }
        let _ = s.send.finish();
        Ok(ArtifactOutcome::Served {
            peer: peer.clone(),
            checkout: request.checkout,
            files: manifest.files.len(),
            blobs_sent: wanted.len(),
            bytes_sent,
        })
    }
    .await;
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(reason) => {
            let _ = s.reject(&reason).await;
            ArtifactOutcome::Rejected {
                peer: Some(peer),
                reason,
            }
        }
    };
    let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
    Ok(outcome)
}

#[derive(Debug, Clone)]
pub struct FetchReport {
    pub manifest: Manifest,
    pub skipped: Vec<String>,
    /// Hashes the local store lacked (requested from the peer).
    pub blobs_missing: Vec<String>,
    pub blobs_received: usize,
    pub bytes_received: u64,
    pub dest: PathBuf,
}

fn under_requested(path: &str, requested: &[String]) -> bool {
    requested
        .iter()
        .any(|r| path == r || (path.starts_with(r.as_str()) && path[r.len()..].starts_with('/')))
}

/// Client side: fetches `paths` of the peer's checkout into the NEW directory `dest`.
pub async fn fetch_artifacts(
    ep: &Endpoint,
    to: EndpointAddr,
    checkout: &str,
    paths: &[String],
    store: &Store,
    dest: &Path,
    timeout: std::time::Duration,
) -> Result<FetchReport, ClientError> {
    let local = |e: String| ClientError::Local(e);
    let limits = Limits::default();
    let (dest_parent, dest_name) = match (dest.parent(), dest.file_name().and_then(|n| n.to_str()))
    {
        (Some(parent), Some(name)) => (parent, name),
        _ => return Err(local(format!("bad destination {}", dest.display()))),
    };
    check_checkout_name(dest_name).map_err(|e| local(format!("destination name: {e}")))?;
    let parent_fd = open_root(dest_parent).map_err(|e| local(e.to_string()))?;
    if exists_nofollow(
        parent_fd.as_raw_fd(),
        &cstr(dest_name).map_err(|e| local(e.to_string()))?,
    )
    .map_err(|e| local(e.to_string()))?
    {
        return Err(local(format!("{} already exists", dest.display())));
    }
    drop(parent_fd);
    check_checkout_name(checkout).map_err(|e| local(e.to_string()))?;
    if paths.is_empty() || paths.len() > MAX_REQUEST_PATHS {
        return Err(local(format!(
            "between 1 and {MAX_REQUEST_PATHS} artifact paths"
        )));
    }
    for p in paths {
        check_path(p, &limits).map_err(|e| local(e.to_string()))?;
    }
    let body = serde_json::to_vec(&ArtifactRequest {
        checkout: checkout.to_owned(),
        paths: paths.to_vec(),
    })
    .map_err(|e| local(e.to_string()))?;
    let mut s = open_session(ep, to, ARTIFACT_ALPN, ARTIFACT_DOMAINS, &body, timeout).await?;
    let result = async {
        let response_body = s.response(MAX_SOURCE_FRAME).await?;
        let response: ArtifactResponse = serde_json::from_slice(&response_body)
            .map_err(|e| ClientError::Protocol(format!("bad response: {e}")))?;
        if response.checkout != checkout {
            return Err(ClientError::Protocol(
                "response names another checkout".into(),
            ));
        }
        let sizes = validate_manifest(&response.manifest, &limits)
            .map_err(|e| ClientError::Protocol(format!("{e:?}")))?;
        if let Some(f) = response
            .manifest
            .files
            .iter()
            .find(|f| !under_requested(&f.path, paths))
        {
            return Err(ClientError::Protocol(format!(
                "UnrequestedPath({:?})",
                f.path
            )));
        }
        let wanted = missing(store, sizes.keys().copied())
            .await
            .map_err(|e| local(format!("{e:?}")))?;
        write_frame(
            &mut s.send,
            &Frame::Missing {
                hashes: wanted.clone(),
            },
        )
        .await
        .map_err(ClientError::Lost)?;
        let mut bytes_received = 0u64;
        for expected in &wanted {
            let (hash, size) = match read_frame(&mut s.recv).await {
                Ok(Frame::Blob { hash, size }) => (hash, size),
                Ok(Frame::Rejected { reason }) => return Err(ClientError::Refused(reason)),
                Ok(other) => return Err(ClientError::Protocol(format!("unexpected {other:?}"))),
                Err(e) => return Err(ClientError::Lost(e)),
            };
            let expected_hash: Hash = expected
                .parse()
                .map_err(|_| ClientError::Protocol("bad hash".into()))?;
            if &hash != expected || Some(&size) != sizes.get(&expected_hash) {
                let _ = s.recv.stop(VarInt::from_u32(1));
                return Err(ClientError::Protocol(format!(
                    "UnrequestedBlob/SizeMismatch({hash})"
                )));
            }
            let mut buf = vec![0u8; size as usize];
            s.recv
                .read_exact(&mut buf)
                .await
                .map_err(|e| ClientError::Lost(e.to_string()))?;
            if let Err(e) =
                import_verified_tagged(store, expected_hash, buf, ARTIFACT_TAG_PREFIX).await
            {
                let _ = s.recv.stop(VarInt::from_u32(1));
                return Err(ClientError::Protocol(format!("{e:?}")));
            }
            bytes_received += size;
        }
        let dest = materialize(store, dest_parent, dest_name, &response.manifest, &limits)
            .await
            .map_err(|e| local(format!("{e:?}")))?;
        Ok(FetchReport {
            manifest: response.manifest,
            skipped: response.skipped,
            blobs_received: wanted.len(),
            blobs_missing: wanted,
            bytes_received,
            dest,
        })
    }
    .await;
    s.close();
    result
}
