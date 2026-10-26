//! The sending side: snapshot, negotiate, stream the thin pack, apply.

use super::extras::ExtraRules;
use super::limits::SyncLimits;
use super::snapshot::{Snapshot, SnapshotError, SnapshotIo, SnapshotRequest};
use super::transfer::{PackRequest, PackTransfer};
use super::{NoObserver, OpenGate, SyncError, SyncEvent, SyncGate, SyncObserver, SyncPhase};
use crate::ids::{CheckoutId, GitId, Hash, PeerId};
use crate::locks::CopyTree;
use crate::proto::{Body, MAX_CHUNK_BYTES, Refusal, SyncApply, SyncSummary};
use crate::transport::{Connection, PeerError, Upload};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Which checkout a sync is for.
#[derive(Debug, Clone, Copy)]
pub struct SyncJob<'a> {
    /// The checkout root on this machine.
    pub root: &'a Path,
    /// The checkout's id.
    pub checkout: &'a CheckoutId,
    /// This machine's name, part of the repo id.
    pub origin_host: &'a str,
    /// The worker.
    pub peer: &'a PeerId,
    /// The local job the sync is for.
    pub local_job: i64,
    /// A pack session id, unique per attempt.
    pub session: u64,
}

/// What a sync did, for the summary line and JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// Tree id of the working tree.
    pub tree: GitId,
    /// Tree id of the extras.
    pub extras_tree: Option<GitId>,
    /// HEAD the snapshot was based on.
    pub base: Option<GitId>,
    /// Files differing from HEAD.
    pub dirty_files: u32,
    /// Files in the tree.
    pub files: u64,
    /// Bytes of pack sent (0 when the worker already held the tree).
    pub pack_bytes_sent: u64,
    /// True when the worker already held this tree and nothing was packed.
    pub already_there: bool,
    /// What the worker applied.
    pub applied: SyncSummary,
    /// What snapshotting read and wrote.
    pub io: SnapshotIo,
    /// The extras that travelled.
    pub extra_files: Vec<String>,
}

impl SyncReport {
    /// The key of the tree this sync left on the worker.
    pub fn copy_tree(&self) -> CopyTree {
        CopyTree::new(
            &self.tree.to_string(),
            self.extras_tree
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
        )
    }

    /// `tree 1a2b3c4d5e6f on 9f8e7d6c5b4a (120 files, 3 differ from HEAD): 1234 bytes sent, ...`.
    pub fn line(&self) -> String {
        format!(
            "tree {} on {} ({} files, {} differ from HEAD): {} bytes sent{}, {} written, {} removed, {} extras; read {} files ({} bytes)",
            self.tree.short(),
            self.base.as_ref().map_or("an unborn HEAD", |b| b.short()),
            self.files,
            self.dirty_files,
            self.pack_bytes_sent,
            if self.already_there {
                " (worker had it)"
            } else {
                ""
            },
            self.applied.written,
            self.applied.removed,
            self.extra_files.len(),
            self.io.files_read,
            self.io.bytes_read,
        )
    }
}

/// Runs one sync over an open connection.
pub trait SyncDriver: Send + Sync {
    /// Makes the worker's copy of the checkout match its working tree now.
    fn push(&self, conn: &mut dyn Connection, job: &SyncJob<'_>) -> Result<SyncReport, SyncError>;

    /// The tree the working tree has now, as the copy key a [`push`](Self::push) of it would
    /// leave on the worker. Reads this machine only: nothing is sent. A driver that cannot say
    /// returns an error, and the caller then syncs.
    fn tree_of(&self, job: &SyncJob<'_>) -> Result<CopyTree, SyncError> {
        let _ = job;
        Err(SyncError::Unexpected("tree_of"))
    }
}

/// The extras allowlist of a checkout, read from its repo run policy.
pub trait ExtrasProvider: Send + Sync {
    /// Patterns of gitignored files that must travel with the checkout at `root`. An error
    /// (an unreadable or invalid policy) stops the sync with that text instead of sending
    /// less than the repo asked for.
    fn extras(&self, root: &Path) -> Result<Vec<String>, String>;
}

/// No extras.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoExtras;

impl ExtrasProvider for NoExtras {
    fn extras(&self, _root: &Path) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

/// The newest snapshot of one checkout, for retention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Latest {
    pub commit: Option<String>,
    pub tree: String,
    pub extras: Option<String>,
    pub at_ms: i64,
}

pub(super) type LatestMap = BTreeMap<String, Latest>;

pub(super) fn latest_path(state: &Path, repo: &CheckoutId) -> PathBuf {
    super::transfer::repo_dir(state, repo).join("latest.json")
}

pub(super) fn read_latest(state: &Path, repo: &CheckoutId) -> LatestMap {
    std::fs::read(latest_path(state, repo))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub(super) fn write_latest(
    state: &Path,
    repo: &CheckoutId,
    map: &LatestMap,
) -> std::io::Result<()> {
    let path = latest_path(state, repo);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(map)?)?;
    std::fs::rename(tmp, path)
}

/// The origin side of sync, wired from a [`Snapshot`] and a [`PackTransfer`].
pub struct OriginSync {
    pub(super) snapshot: Arc<dyn Snapshot>,
    transfer: Arc<dyn PackTransfer>,
    limits: SyncLimits,
    extras: Arc<dyn ExtrasProvider>,
    pub(super) observer: Arc<dyn SyncObserver>,
    gate: Arc<dyn SyncGate>,
    pub(super) state: PathBuf,
    pub(super) clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

fn system_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

impl OriginSync {
    /// Snapshots through `snapshot`, packs through `transfer`; `state` is where the snapshot
    /// store lives (the same directory the snapshotter was given).
    pub fn new(
        snapshot: Arc<dyn Snapshot>,
        transfer: Arc<dyn PackTransfer>,
        limits: SyncLimits,
        state: impl Into<PathBuf>,
    ) -> OriginSync {
        OriginSync {
            snapshot,
            transfer,
            limits,
            extras: Arc::new(NoExtras),
            observer: Arc::new(NoObserver),
            gate: Arc::new(OpenGate),
            state: state.into(),
            clock: Arc::new(system_ms),
        }
    }

    /// Reads the extras allowlist of each checkout from `extras`.
    #[must_use]
    pub fn with_extras(mut self, extras: Arc<dyn ExtrasProvider>) -> OriginSync {
        self.extras = extras;
        self
    }

    /// Reports sent syncs and refusals to `observer`.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn SyncObserver>) -> OriginSync {
        self.observer = observer;
        self
    }

    /// Takes `gate` around snapshotting and pack building (the step-4 I/O gate hook).
    #[must_use]
    pub fn with_gate(mut self, gate: Arc<dyn SyncGate>) -> OriginSync {
        self.gate = gate;
        self
    }

    /// Reads time (unix milliseconds) from `clock`; for tests.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> OriginSync {
        self.clock = clock;
        self
    }

    fn record_latest(&self, repo: &CheckoutId, checkout: &CheckoutId, l: Latest) {
        let mut map = read_latest(&self.state, repo);
        map.insert(checkout.to_string(), l);
        // best effort: losing this only delays pruning
        let _ = write_latest(&self.state, repo, &map);
    }
}

/// Sends the pack to the worker in `MAX_CHUNK_BYTES` pieces as pack-objects produces it, hashing
/// as it goes.
struct ChunkSink<'a> {
    conn: &'a mut dyn Connection,
    checkout: &'a CheckoutId,
    session: u64,
    sent: u64,
    /// The raw stream the pack goes through; `None` sends it as chunked requests (the fallback
    /// for a peer that does not know streams).
    upload: Option<Box<dyn Upload>>,
    buf: Vec<u8>,
    sha: Sha256,
    err: Option<SyncError>,
    /// Told the bytes sent so far each time another MiB has gone out (R-801).
    progress: &'a dyn Fn(u64),
}

/// How often the pack's progress is reported, in bytes sent.
const PROGRESS_STEP: u64 = 1 << 20;

impl ChunkSink<'_> {
    fn sent_more(&mut self, len: u64) {
        let before = self.sent;
        self.sent += len;
        if before / PROGRESS_STEP != self.sent / PROGRESS_STEP {
            (self.progress)(self.sent);
        }
    }

    fn send(&mut self, data: Vec<u8>) -> Result<(), SyncError> {
        let len = data.len() as u64;
        self.sha.update(&data);
        if let Some(up) = self.upload.as_mut() {
            up.write(&data)?;
            self.sent_more(len);
            return Ok(());
        }
        let reply = self.conn.request(Body::SyncPackChunk {
            checkout: self.checkout.clone(),
            session: self.session,
            offset: self.sent,
            data,
        })?;
        match reply {
            Body::SyncPackAck { received } if received == self.sent + len => {
                self.sent_more(len);
                Ok(())
            }
            Body::SyncPackAck { .. } => Err(SyncError::Peer(PeerError::Protocol(
                "the worker acknowledged a different pack length".to_string(),
            ))),
            _ => Err(SyncError::Unexpected("sync_pack_chunk")),
        }
    }

    fn flush(&mut self) -> Result<(), SyncError> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.buf);
        self.send(data)
    }

    /// Ends a raw stream and checks the receiver holds every byte sent.
    fn finish_stream(&mut self) -> Result<(), SyncError> {
        let Some(up) = self.upload.take() else {
            return Ok(());
        };
        match up.finish()? {
            Body::SyncPackAck { received } if received == self.sent => Ok(()),
            Body::SyncPackAck { .. } => Err(SyncError::Peer(PeerError::Protocol(
                "the worker acknowledged a different pack length".to_string(),
            ))),
            _ => Err(SyncError::Unexpected("sync_pack_stream")),
        }
    }
}

impl Write for ChunkSink<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        while self.buf.len() >= MAX_CHUNK_BYTES {
            let rest = self.buf.split_off(MAX_CHUNK_BYTES);
            let chunk = std::mem::replace(&mut self.buf, rest);
            if let Err(e) = self.send(chunk) {
                self.err = Some(e);
                return Err(std::io::Error::other("sending the pack failed"));
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SyncDriver for OriginSync {
    fn tree_of(&self, job: &SyncJob<'_>) -> Result<CopyTree, SyncError> {
        let patterns = self
            .extras
            .extras(job.root)
            .map_err(SnapshotError::BadExtras)?;
        let rules = ExtraRules::new(&patterns).map_err(SnapshotError::BadExtras)?;
        let snap = {
            let _g = self.gate.enter(SyncPhase::Snapshot);
            self.snapshot.snapshot(&SnapshotRequest {
                root: job.root,
                checkout: job.checkout,
                origin_host: job.origin_host,
                limits: &self.limits,
                extras: &rules,
            })?
        };
        Ok(CopyTree::new(
            &snap.tree.to_string(),
            snap.extras_tree
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
        ))
    }

    fn push(&self, conn: &mut dyn Connection, job: &SyncJob<'_>) -> Result<SyncReport, SyncError> {
        let patterns = self
            .extras
            .extras(job.root)
            .map_err(SnapshotError::BadExtras)?;
        let rules = ExtraRules::new(&patterns).map_err(SnapshotError::BadExtras)?;
        let snap = {
            let _g = self.gate.enter(SyncPhase::Snapshot);
            self.snapshot.snapshot(&SnapshotRequest {
                root: job.root,
                checkout: job.checkout,
                origin_host: job.origin_host,
                limits: &self.limits,
                extras: &rules,
            })
        };
        let snap = match snap {
            Ok(s) => s,
            Err(SnapshotError::Refused(r)) => {
                self.observer.event(&SyncEvent::Refused {
                    peer: job.peer.clone(),
                    checkout: job.checkout.clone(),
                    job: Some(job.local_job),
                    files: r
                        .0
                        .iter()
                        .take(10)
                        .map(|f| (f.path.clone(), f.reason.code()))
                        .collect(),
                });
                return Err(SnapshotError::Refused(r).into());
            }
            Err(e) => return Err(e.into()),
        };

        // what does the worker hold?
        let tips = match conn.request(Body::SyncOffer {
            checkout: job.checkout.clone(),
            repo: snap.repo.clone(),
        })? {
            Body::SyncHave { tips } => tips,
            _ => return Err(SyncError::Unexpected("sync_offer")),
        };
        let exact = tips
            .iter()
            .find(|t| t.tree == snap.tree && t.extras == snap.extras_tree);

        let now_ms = (self.clock)();
        let (commit, pack_bytes, pack_sha) = if let Some(t) = exact {
            (Some(t.commit.clone()), 0u64, None)
        } else {
            let _g = self.gate.enter(SyncPhase::Pack);
            // commits the worker has that we also still have: exclusions and the delta base
            let have: Vec<GitId> = tips
                .iter()
                .map(|t| t.commit.clone())
                .filter(|c| self.transfer.has_object(&snap.git_dir, c))
                .collect();
            let commit = self
                .snapshot
                .seal(&snap, have.first(), now_ms / 1000)
                .map_err(SyncError::from)?;
            let mut want = vec![commit.clone()];
            if let Some(x) = &snap.extras_tree {
                want.push(x.clone());
            }
            // one raw stream for the whole pack: no round trip per chunk. A peer that refuses
            // the stream (an older build) gets the chunked requests, as before.
            let upload = match conn.open_upload(Body::SyncPackStream {
                checkout: job.checkout.clone(),
                session: job.session,
            }) {
                Ok(u) => Some(u),
                Err(PeerError::Refused(Refusal::Busy { checkout })) => {
                    return Err(SyncError::Peer(PeerError::Refused(Refusal::Busy {
                        checkout,
                    })));
                }
                Err(_) => None,
            };
            let progress = |bytes: u64| {
                self.observer.event(&SyncEvent::Sending {
                    peer: job.peer.clone(),
                    job: Some(job.local_job),
                    bytes,
                });
            };
            let mut sink = ChunkSink {
                conn: &mut *conn,
                checkout: job.checkout,
                session: job.session,
                sent: 0,
                upload,
                buf: Vec::new(),
                sha: Sha256::new(),
                err: None,
                progress: &progress,
            };
            let built = self.transfer.build_pack(
                &PackRequest {
                    git_dir: &snap.git_dir,
                    want: &want,
                    have: &have,
                    max_bytes: self.limits.max_pack_bytes,
                },
                &mut sink,
            );
            if let Some(e) = sink.err.take() {
                return Err(e);
            }
            built?;
            sink.flush()?;
            sink.finish_stream()?;
            (Some(commit), sink.sent, Some(Hash::from_digest(sink.sha)))
        };

        let applied = match conn.request(Body::SyncApply(SyncApply {
            checkout: job.checkout.clone(),
            repo: snap.repo.clone(),
            session: job.session,
            tree: snap.tree.clone(),
            extras_tree: snap.extras_tree.clone(),
            // an exact match needs no commit; the worker finds the one it holds
            commit: if exact.is_some() {
                None
            } else {
                commit.clone()
            },
            base: snap.base.clone(),
            dirty: snap.dirty_files,
            files: snap.files,
            pack_bytes,
            pack_sha256: pack_sha,
        }))? {
            Body::SyncApplied(s) => s,
            _ => return Err(SyncError::Unexpected("sync_apply")),
        };

        self.record_latest(
            &snap.repo,
            job.checkout,
            Latest {
                commit: commit.as_ref().map(ToString::to_string),
                tree: snap.tree.to_string(),
                extras: snap.extras_tree.as_ref().map(ToString::to_string),
                at_ms: now_ms,
            },
        );
        self.observer.event(&SyncEvent::Sent {
            peer: job.peer.clone(),
            checkout: job.checkout.clone(),
            job: Some(job.local_job),
            tree: snap.tree.clone(),
            base: snap.base.clone(),
            dirty: snap.dirty_files,
            files: snap.files,
            bytes: pack_bytes,
        });
        Ok(SyncReport {
            tree: snap.tree,
            extras_tree: snap.extras_tree,
            base: snap.base,
            dirty_files: snap.dirty_files,
            files: snap.files,
            pack_bytes_sent: pack_bytes,
            already_there: exact.is_some(),
            applied,
            io: snap.io,
            extra_files: snap.extra_files,
        })
    }
}
