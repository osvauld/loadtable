//! Phase 2b: artifact return over iroh-blobs, the reverse direction of [`crate::source`].
//!
//! **Protocol** (its own ALPN [`ARTIFACT_ALPN`] = `task-manager/artifact/0`, served only when the
//! source store/checkouts are configured; same admission, nonce and signature pattern):
//!
//! 1. `Hello{attestation}` → [`Policy::admit`] (attestation + allowlist) BEFORE any challenge.
//! 2. `Challenge{nonce}`.
//! 3. `Request{body, sig}`: body `{checkout, paths}`, signed over
//!    `task-manager/artifact/request/v0`, the nonce, both endpoint ids and `SHA-256(body)`.
//! 4. The worker validates (checkout name; every path structurally safe, inside the checkout,
//!    reached no-follow; a requested symlink is refused, symlinks/special files inside requested
//!    directories are skipped and listed; the source [`Limits`]), imports each file into its store
//!    under the named tag `task-manager/artifact/<hex>`, and answers `Response{body, sig}` with
//!    body `{checkout, manifest, skipped}` signed over the response domain, nonce, both ids,
//!    request and response hashes. The manifest has the source schema (SHA-256 receipt + BLAKE3).
//! 5. The client verifies the signature, validates the manifest itself (paths, receipt, limits,
//!    and every path inside one of the requested paths), and sends `Missing{hashes}`: only the
//!    hashes its OWN store lacks.
//! 6. The worker sends `Blob{hash,size}` + bytes for exactly those hashes (each must be in the
//!    manifest), read from its store and re-verified.
//! 7. The client checks order and size, BLAKE3-verifies each blob before importing it (tag
//!    `task-manager/artifact/<hex>`), then materializes into a NEW destination directory with the
//!    immutable materializer (refuses an existing name, no-follow, verify-after-write).
//!
//! A repeated fetch of unchanged artifacts therefore moves zero blob bytes: only the request and
//! the signed manifest cross. Whole blobs are buffered in memory (≤ `max_file_bytes` each).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use courier::token::Token;
use iroh::endpoint::{Connection, VarInt};
use iroh::{Endpoint, EndpointAddr, PublicKey};
use iroh_blobs::Hash;
use iroh_blobs::api::Store;
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::source::{
    Limits, MAX_SOURCE_FRAME, Manifest, ManifestFile, SourceError, check_checkout_name, check_path,
    cstr, descend, exists_nofollow, import_verified_tagged, materialize, missing, open_dir_at,
    open_file_at, open_root, read_dir_names, sha256_hex, stat_at, validate_manifest,
};
use crate::{CLOSE_GRACE, Frame, Policy, read_frame, read_frame_limited, verify_sig, write_frame};

pub const ARTIFACT_ALPN: &[u8] = b"task-manager/artifact/0";
/// Named-tag prefix protecting exported (worker) and received (client) artifact blobs.
pub const ARTIFACT_TAG_PREFIX: &str = "task-manager/artifact/";
const REQUEST_DOMAIN: &[u8] = b"task-manager/artifact/request/v0\0";
const RESPONSE_DOMAIN: &[u8] = b"task-manager/artifact/response/v0\0";
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

fn signed_message(
    domain: &[u8],
    challenge: &[u8],
    first: &PublicKey,
    second: &PublicKey,
    bodies: &[&[u8]],
) -> Vec<u8> {
    use sha2::{Digest, Sha256};
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

/// Worker behaviour; everything but `Honest` is a test-only adversary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeMode {
    Honest,
    /// Flips the first byte of every sent non-empty blob while keeping the claimed hash.
    TamperBlobs,
    /// Signs a manifest that also names a path outside the requested ones.
    ExtraPath,
}

#[derive(Debug)]
pub enum ArtifactOutcome {
    Served {
        checkout: String,
        files: usize,
        blobs_sent: usize,
        bytes_sent: u64,
    },
    Rejected(String),
}

pub async fn handle_artifact(
    ep: &Endpoint,
    conn: Connection,
    policy: &Policy,
    store: &Store,
    checkout_root: &Path,
    limits: &Limits,
) -> Result<ArtifactOutcome, String> {
    handle_artifact_as(
        ep,
        conn,
        policy,
        store,
        checkout_root,
        limits,
        ServeMode::Honest,
    )
    .await
}

pub async fn handle_artifact_as(
    ep: &Endpoint,
    conn: Connection,
    policy: &Policy,
    store: &Store,
    checkout_root: &Path,
    limits: &Limits,
    mode: ServeMode,
) -> Result<ArtifactOutcome, String> {
    if conn.alpn() != ARTIFACT_ALPN {
        return Err("unexpected ALPN".into());
    }
    let remote = conn.remote_id();
    let (mut send, mut recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
    let outcome = async {
        let Frame::Hello { attestation } = read_frame(&mut recv).await? else {
            return Err("expected hello".to_string());
        };
        if let Err(reject) = policy.admit(&remote, attestation.as_ref()) {
            return Ok(ArtifactOutcome::Rejected(format!("{reject:?}")));
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
            return Ok(ArtifactOutcome::Rejected("BadRequest".into()));
        }
        let request: ArtifactRequest = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ArtifactOutcome::Rejected(format!(
                    "{:?}",
                    SourceError::InvalidManifest(e.to_string())
                )));
            }
        };
        let exported = match export(
            store,
            checkout_root,
            &request.checkout,
            &request.paths,
            limits,
        )
        .await
        {
            Ok(e) => e,
            Err(e) => return Ok(ArtifactOutcome::Rejected(format!("{e:?}"))),
        };
        let mut manifest = exported.manifest;
        if mode == ServeMode::ExtraPath
            && let Some(first) = manifest.files.first().cloned()
        {
            let mut files = manifest.files.clone();
            files.push(ManifestFile {
                path: "zz-unrequested/copy".into(),
                ..first
            });
            manifest = Manifest::from_files_unchecked(files);
        }
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
        let wanted = match read_frame_limited(&mut recv, MAX_SOURCE_FRAME).await {
            Ok(Frame::Missing { hashes }) => hashes,
            // The client refused the manifest and closed: not a worker error.
            Ok(other) => return Err(format!("expected missing, got {other:?}")),
            Err(e) => return Err(format!("client closed after the manifest: {e}")),
        };
        let unique: BTreeSet<&String> = wanted.iter().collect();
        if unique.len() != wanted.len() || wanted.iter().any(|h| !sizes.contains_key(h)) {
            return Ok(ArtifactOutcome::Rejected("UnknownBlob".into()));
        }
        let mut bytes_sent = 0u64;
        for hex in &wanted {
            let hash: Hash = hex.parse().map_err(|_| "bad hash".to_string())?;
            let mut bytes = store
                .blobs()
                .get_bytes(hash)
                .await
                .map_err(|e| format!("store: {e}"))?
                .to_vec();
            if Hash::new(&bytes) != hash || bytes.len() as u64 != sizes[hex] {
                return Ok(ArtifactOutcome::Rejected(format!("HashMismatch({hex})")));
            }
            if mode == ServeMode::TamperBlobs && !bytes.is_empty() {
                bytes[0] ^= 0xff;
            }
            write_frame(
                &mut send,
                &Frame::Blob {
                    hash: hex.clone(),
                    size: bytes.len() as u64,
                },
            )
            .await?;
            if let Err(e) = send.write_all(&bytes).await {
                // The client stopped reading (it rejected a blob).
                return Err(format!("client stopped the transfer: {e}"));
            }
            bytes_sent += bytes.len() as u64;
        }
        Ok(ArtifactOutcome::Served {
            checkout: request.checkout,
            files: manifest.files.len(),
            blobs_sent: wanted.len(),
            bytes_sent,
        })
    }
    .await?;
    if let ArtifactOutcome::Rejected(reason) = &outcome {
        write_frame(
            &mut send,
            &Frame::Rejected {
                reason: reason.clone(),
            },
        )
        .await?;
    }
    send.finish().map_err(|e| e.to_string())?;
    let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
    Ok(outcome)
}

pub async fn serve_artifact_one(
    ep: &Endpoint,
    policy: &Policy,
    store: &Store,
    checkout_root: &Path,
    limits: &Limits,
) -> Result<ArtifactOutcome, String> {
    serve_artifact_one_as(ep, policy, store, checkout_root, limits, ServeMode::Honest).await
}

pub async fn serve_artifact_one_as(
    ep: &Endpoint,
    policy: &Policy,
    store: &Store,
    checkout_root: &Path,
    limits: &Limits,
    mode: ServeMode,
) -> Result<ArtifactOutcome, String> {
    let conn = crate::accept_one(ep).await?;
    handle_artifact_as(ep, conn, policy, store, checkout_root, limits, mode).await
}

#[derive(Debug, Clone)]
pub struct FetchReport {
    pub challenge: Vec<u8>,
    pub manifest: Manifest,
    pub skipped: Vec<String>,
    /// Hashes the local store lacked (requested from the worker).
    pub blobs_missing: Vec<String>,
    pub blobs_received: usize,
    pub bytes_received: u64,
    pub dest: PathBuf,
    pub response_fresh: bool,
}

fn under_requested(path: &str, requested: &[String]) -> bool {
    requested
        .iter()
        .any(|r| path == r || (path.starts_with(r.as_str()) && path[r.len()..].starts_with('/')))
}

/// Client side. Errors start with `local:` (destination/request problem, nothing sent),
/// `rejected:` (the worker's reason), `verify:` (the worker's answer failed verification; nothing
/// materialized) or are transport errors.
pub async fn fetch_artifacts(
    ep: &Endpoint,
    to: EndpointAddr,
    attestation: Option<Token>,
    checkout: &str,
    paths: &[String],
    store: &Store,
    dest: &Path,
) -> Result<FetchReport, String> {
    let limits = Limits::default();
    let (dest_parent, dest_name) = match (dest.parent(), dest.file_name().and_then(|n| n.to_str()))
    {
        (Some(parent), Some(name)) => (parent, name),
        _ => return Err(format!("local: bad destination {}", dest.display())),
    };
    check_checkout_name(dest_name).map_err(|e| format!("local: destination name: {e}"))?;
    let parent_fd = open_root(dest_parent).map_err(|e| format!("local: {e}"))?;
    if exists_nofollow(
        parent_fd.as_raw_fd(),
        &cstr(dest_name).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("local: {e}"))?
    {
        return Err(format!(
            "local: {:?}",
            SourceError::Exists(dest.display().to_string())
        ));
    }
    drop(parent_fd);
    check_checkout_name(checkout).map_err(|e| format!("local: {e}"))?;
    if paths.is_empty() || paths.len() > MAX_REQUEST_PATHS {
        return Err("local: between 1 and 256 artifact paths".into());
    }
    for p in paths {
        check_path(p, &limits).map_err(|e| format!("local: {e}"))?;
    }

    let receiver = to.id;
    let conn = ep
        .connect(to, ARTIFACT_ALPN)
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
        let body = serde_json::to_vec(&ArtifactRequest {
            checkout: checkout.to_owned(),
            paths: paths.to_vec(),
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
        let (response_body, sig) = match read_frame_limited(&mut recv, MAX_SOURCE_FRAME).await? {
            Frame::Response { body, sig } => (body, sig),
            Frame::Rejected { reason } => return Err(format!("rejected: {reason}")),
            other => return Err(format!("unexpected {other:?}")),
        };
        if !verify_sig(
            &receiver,
            &response_message(&challenge, &receiver, &ep.id(), &body, &response_body),
            &sig,
        ) {
            return Err("verify: response not bound to this session".into());
        }
        let response: ArtifactResponse = serde_json::from_slice(&response_body)
            .map_err(|e| format!("verify: bad response: {e}"))?;
        if response.checkout != checkout {
            return Err("verify: response names another checkout".into());
        }
        let sizes =
            validate_manifest(&response.manifest, &limits).map_err(|e| format!("verify: {e:?}"))?;
        if let Some(f) = response
            .manifest
            .files
            .iter()
            .find(|f| !under_requested(&f.path, paths))
        {
            return Err(format!("verify: UnrequestedPath({:?})", f.path));
        }
        let wanted = missing(store, sizes.keys().copied())
            .await
            .map_err(|e| format!("local: {e:?}"))?;
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
                Frame::Rejected { reason } => return Err(format!("rejected: {reason}")),
                other => return Err(format!("unexpected {other:?}")),
            };
            let expected_hash: Hash = expected.parse().map_err(|_| "bad hash".to_string())?;
            if &hash != expected {
                let _ = recv.stop(VarInt::from_u32(1));
                return Err(format!("verify: UnrequestedBlob({hash})"));
            }
            if Some(&size) != sizes.get(&expected_hash) {
                let _ = recv.stop(VarInt::from_u32(1));
                return Err(format!("verify: SizeMismatch({hash})"));
            }
            let mut buf = vec![0u8; size as usize];
            recv.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
            if let Err(e) =
                import_verified_tagged(store, expected_hash, buf, ARTIFACT_TAG_PREFIX).await
            {
                let _ = recv.stop(VarInt::from_u32(1));
                return Err(format!("verify: {e:?}"));
            }
            bytes_received += size;
        }
        let dest = materialize(store, dest_parent, dest_name, &response.manifest, &limits)
            .await
            .map_err(|e| format!("local: {e:?}"))?;
        Ok(FetchReport {
            challenge,
            manifest: response.manifest,
            skipped: response.skipped,
            blobs_received: wanted.len(),
            blobs_missing: wanted,
            bytes_received,
            dest,
            response_fresh: true,
        })
    }
    .await;
    conn.close(VarInt::from_u32(0), b"done");
    result
}
