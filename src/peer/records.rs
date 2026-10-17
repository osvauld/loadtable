//! Durable link records, outside the SQLite schema (small JSON files, written atomically):
//!
//! * `<state>/peer/links/<local job id>.json` — outbound: what this daemon knows about a job it
//!   forwards (peer, idempotency key, remote job id, remote phase/state, durations).
//! * `<state>/peer/inbound/<peer endpoint>/<sha256(key)[..32]>.json` — inbound: the job this
//!   daemon created for a peer's submission key. Written as `submitting` BEFORE the ledger insert
//!   and `submitted` with the id after it, so a repeated key never creates a second job.
use crate::JobSpec;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Paths {
    pub state: PathBuf,
}

impl Paths {
    pub fn new(state: &Path) -> Self {
        Self {
            state: state.to_owned(),
        }
    }
    pub fn dir(&self) -> PathBuf {
        self.state.join("peer")
    }
    pub fn socket(&self) -> PathBuf {
        self.state.join("peer.sock")
    }
    pub fn links(&self) -> PathBuf {
        self.dir().join("links")
    }
    pub fn link(&self, job: i64) -> PathBuf {
        self.links().join(format!("{job}.json"))
    }
    pub fn inbound(&self, endpoint: &str) -> PathBuf {
        self.dir().join("inbound").join(endpoint)
    }
    pub fn inbound_record(&self, endpoint: &str, key: &str) -> PathBuf {
        let digest = task_manager_peer::sha256_hex(key.as_bytes());
        self.inbound(endpoint)
            .join(format!("{}.json", &digest[..32]))
    }
    pub fn blobs(&self) -> PathBuf {
        self.dir().join("blobs")
    }
    pub fn job_log(&self, job: i64) -> PathBuf {
        self.state
            .join("jobs")
            .join(job.to_string())
            .join("job.log")
    }
}

/// Creates `dir` (and parents) with mode 0700.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

use std::os::unix::fs::DirBuilderExt;

/// Temp file in the same directory, fsync, rename: readers see the old or the new document.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    private_dir(dir)?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
    ));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(value)?)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Outbound link of one forwarded local job.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OutboundLink {
    pub version: u32,
    pub local_job_id: i64,
    pub peer: String,
    pub peer_endpoint: String,
    pub key: String,
    /// Owner on the peer: `WHO@this host`.
    pub owner: String,
    pub checkout: Option<String>,
    pub sub: String,
    /// `syncing`, `submitting`, `running`, `fetching`, `done`.
    pub stage: String,
    pub remote_job_id: Option<i64>,
    pub remote_phase: Option<String>,
    /// `attached`, `uncertain`, `terminal`, `not_submitted`, `refused`, `interrupted`.
    pub remote_state: Option<String>,
    pub exit: Option<i32>,
    pub sync_s: Option<f64>,
    pub run_s: Option<f64>,
    pub fetch_s: Option<f64>,
    pub fetch_dest: Option<String>,
    pub blobs_sent: Option<usize>,
    pub cancel_sent: bool,
    pub ledger_linked: bool,
    pub started_at: f64,
    pub updated_at: f64,
    pub error: Option<String>,
}

impl OutboundLink {
    pub fn done(&self) -> bool {
        self.stage == "done"
    }
}

/// Inbound record of one peer submission key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InboundRecord {
    pub version: u32,
    pub key: String,
    pub peer: String,
    pub peer_endpoint: String,
    pub origin_host: String,
    pub origin_job_id: Option<i64>,
    pub owner: String,
    /// `submitting`, `submitted`, `refused`.
    pub state: String,
    pub remote_id: Option<i64>,
    /// Unix seconds before the ledger insert (reconciliation bound).
    pub since: i64,
    pub spec: JobSpec,
    pub refused: Option<String>,
    #[serde(default)]
    pub ledger_linked: bool,
}

pub fn outbound_links(paths: &Paths) -> Vec<OutboundLink> {
    let Ok(entries) = std::fs::read_dir(paths.links()) else {
        return vec![];
    };
    let mut out: Vec<OutboundLink> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.ends_with(".json") && !n.starts_with('.'))
        })
        .filter_map(|e| read_json(&e.path()))
        .collect();
    out.sort_by_key(|l| l.local_job_id);
    out
}

pub fn inbound_records(paths: &Paths) -> Vec<InboundRecord> {
    let Ok(peers) = std::fs::read_dir(paths.dir().join("inbound")) else {
        return vec![];
    };
    let mut out = vec![];
    for peer in peers.flatten() {
        let Ok(entries) = std::fs::read_dir(peer.path()) else {
            continue;
        };
        out.extend(
            entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.ends_with(".json") && !n.starts_with('.'))
                })
                .filter_map(|e| read_json::<InboundRecord>(&e.path())),
        );
    }
    out.sort_by_key(|r| r.remote_id);
    out
}
