//! The sending side of checkout sync: ask what the peer has, send what it
//! lacks in bounded chunks, then commit with the paged manifest.

use crate::ids::{CheckoutId, Hash};
use crate::manifest::{ManifestReport, Skipped, pages, want};
use crate::proto::{Body, MAX_CHUNK_BYTES, MAX_HASHES_PER_REQUEST, Refusal, SyncSummary};
use crate::transport::{Connection, PeerError};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// Why a sync failed.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The link or the peer failed.
    #[error("sync failed: {0}")]
    Peer(#[from] PeerError),
    /// A local file changed or vanished while it was being sent.
    #[error("reading {path} while syncing: {detail}")]
    Source {
        /// The path.
        path: String,
        /// Detail.
        detail: String,
    },
    /// The peer answered with the wrong kind of message.
    #[error("unexpected reply to {0}")]
    Unexpected(&'static str),
}

/// Where blob bytes come from.
pub trait SyncSource {
    /// Reads up to `len` bytes of `rel` at `offset`.
    fn read_chunk(&self, rel: &str, offset: u64, len: usize) -> std::io::Result<Vec<u8>>;
}

/// Reads from a checkout directory.
#[derive(Debug, Clone)]
pub struct DirSource(pub PathBuf);

impl SyncSource for DirSource {
    fn read_chunk(&self, rel: &str, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let mut f = std::fs::File::open(self.0.join(rel))?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len];
        let mut got = 0;
        while got < len {
            let n = f.read(buf.get_mut(got..).unwrap_or(&mut []))?;
            if n == 0 {
                break;
            }
            got += n;
        }
        buf.truncate(got);
        Ok(buf)
    }
}

/// What a sync did, for the summary line and JSON (R-7.20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// Files in the manifest.
    pub files: usize,
    /// Distinct blobs the peer lacked and received.
    pub blobs_sent: usize,
    /// Distinct blobs the peer already had.
    pub blobs_unchanged: usize,
    /// Bytes of blob data sent.
    pub bytes_sent: u64,
    /// What the peer applied.
    pub applied: SyncSummary,
    /// Files left out, with reasons.
    pub skipped: Vec<Skipped>,
}

impl SyncReport {
    /// `5 blobs sent (1.2 KiB), 1320 unchanged`.
    pub fn line(&self) -> String {
        format!(
            "{} blobs sent ({} bytes), {} unchanged, {} removed, {} files skipped",
            self.blobs_sent,
            self.bytes_sent,
            self.blobs_unchanged,
            self.applied.removed,
            self.skipped.len()
        )
    }
}

/// Pushes `report.manifest` to the peer so that its copy matches.
pub fn push_checkout(
    conn: &mut dyn Connection,
    checkout: &CheckoutId,
    report: &ManifestReport,
    source: &dyn SyncSource,
    session: u64,
) -> Result<SyncReport, SyncError> {
    let manifest = &report.manifest;
    // 1. What does the peer already have?
    let mut distinct: Vec<Hash> = Vec::new();
    let mut seen = BTreeSet::new();
    for e in manifest.entries.values() {
        if seen.insert(e.hash) {
            distinct.push(e.hash);
        }
    }
    let mut have: BTreeSet<Hash> = BTreeSet::new();
    for batch in distinct.chunks(MAX_HASHES_PER_REQUEST) {
        let reply = conn.request(Body::SyncHaveRequest {
            checkout: checkout.clone(),
            hashes: batch.to_vec(),
        })?;
        match reply {
            Body::SyncHave { present } => have.extend(present),
            _ => return Err(SyncError::Unexpected("sync_have_request")),
        }
    }
    // 2. Send what is missing.
    let wanted = want(manifest, &have);
    let path_of: BTreeMap<Hash, (&str, u64)> = manifest
        .entries
        .values()
        .map(|e| (e.hash, (e.path.as_str(), e.size)))
        .collect();
    let mut bytes_sent = 0u64;
    for h in &wanted {
        let Some((path, size)) = path_of.get(h).copied() else {
            continue;
        };
        bytes_sent += send_blob(conn, checkout, h, path, size, source)?;
    }
    // 3. Commit with the paged manifest.
    let ps = pages(manifest);
    let n = ps.len();
    let mut applied = None;
    for (i, entries) in ps.into_iter().enumerate() {
        let last = i + 1 == n;
        let reply = conn.request(Body::SyncManifestPage {
            checkout: checkout.clone(),
            session,
            index: u32::try_from(i).unwrap_or(u32::MAX),
            last,
            entries,
        })?;
        match (reply, last) {
            (Body::Ok, false) => {}
            (Body::SyncApplied(s), true) => applied = Some(s),
            _ => return Err(SyncError::Unexpected("sync_manifest_page")),
        }
    }
    Ok(SyncReport {
        files: manifest.len(),
        blobs_sent: wanted.len(),
        blobs_unchanged: distinct.len().saturating_sub(wanted.len()),
        bytes_sent,
        applied: applied.ok_or(SyncError::Unexpected("sync_manifest_page"))?,
        skipped: report.skipped.clone(),
    })
}

fn send_blob(
    conn: &mut dyn Connection,
    checkout: &CheckoutId,
    hash: &Hash,
    path: &str,
    size: u64,
    source: &dyn SyncSource,
) -> Result<u64, SyncError> {
    let mut offset = 0u64;
    let mut sent = 0u64;
    let mut resyncs = 0;
    loop {
        let data = source
            .read_chunk(path, offset, MAX_CHUNK_BYTES)
            .map_err(|e| SyncError::Source {
                path: path.to_string(),
                detail: e.to_string(),
            })?;
        let len = data.len() as u64;
        let reply = conn.request(Body::SyncBlob {
            checkout: checkout.clone(),
            hash: *hash,
            offset,
            total: size,
            data,
        });
        match reply {
            Ok(Body::SyncBlobAck { received }) => {
                sent += len;
                offset += len;
                if received >= size {
                    return Ok(sent);
                }
                if len == 0 {
                    // The file shrank since it was hashed.
                    return Err(SyncError::Source {
                        path: path.to_string(),
                        detail: "file changed while syncing".to_string(),
                    });
                }
            }
            Err(PeerError::Refused(Refusal::OutOfOrder { expected })) if resyncs < 3 => {
                resyncs += 1;
                offset = expected;
            }
            Ok(_) => return Err(SyncError::Unexpected("sync_blob")),
            Err(e) => return Err(e.into()),
        }
    }
}
