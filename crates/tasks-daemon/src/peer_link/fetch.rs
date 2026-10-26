//! Bringing a forwarded job's output files back (`--fetch`).
//!
//! When the peer reports that a job passed, the origin's driver thread fetches each requested
//! path over the hashed, bounded fetch of `tasks_peer::outputs`, inside a transfer slot, before
//! the end of the job is published (so nothing sees the job finished while its files are still
//! arriving). Each path is its own request: one missing or oversized file never stops another.
//!
//! - only a job that passed is fetched for (a failed or killed job leaves files of unknown age);
//!   the others get a "not fetched" result so the reason is visible;
//! - files land at the same relative path in the job's checkout through a temporary name and a
//!   rename; no symlink is written through except the coordinator-placed output links;
//! - the results go to the job log (`[tasks] fetched ...`), to a sidecar next to the log that
//!   `get` reads, and (by the scheduler) to the audit log;
//! - while a file arrives, how far it got is kept in a second sidecar (`get`, `status` and the
//!   UI show it) and repeated in the job log every few seconds, so a job whose command is over
//!   does not look stuck while its outputs come back (R-801).

use super::drive::FileLog;
use super::{Inner, Job};
use crate::forwarder::ForwardEnd;
use crate::scheduler::lock;
use std::path::{Path, PathBuf};
use tasks_core::{FetchResult, FetchStatus, Phase, TransferKind, TransferMeter, TransferProgress};
use tasks_peer::outputs::{self, FetchError};
use tasks_peer::sync::SyncPhase;
use tasks_peer::transport::{Connection, PeerError, Transport};
use tasks_peer::{PeerId, Refusal};

/// Where the results of a job's fetch are kept: next to its log.
pub fn sidecar_path(log: &Path) -> PathBuf {
    let mut p = log.as_os_str().to_os_string();
    p.push(".fetch.json");
    PathBuf::from(p)
}

/// The recorded results of a job's fetch, empty when it asked for none.
pub fn read_results(log: &Path) -> Vec<FetchResult> {
    std::fs::read(sidecar_path(log))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_results(log: &Path, results: &[FetchResult]) {
    let path = sidecar_path(log);
    let tmp = path.with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec(results)
        && std::fs::write(&tmp, bytes).is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Where a job's transfer in progress is kept while it moves: next to its log.
pub fn transfer_path(log: &Path) -> PathBuf {
    let mut p = log.as_os_str().to_os_string();
    p.push(".transfer.json");
    PathBuf::from(p)
}

/// The transfer in progress for the job whose log is `log`, if one is moving now.
pub fn read_transfer(log: &Path) -> Option<TransferProgress> {
    std::fs::read(transfer_path(log))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

/// Records how far a transfer got (through a temporary name, so a reader never sees half).
pub fn write_transfer(log: &Path, p: &TransferProgress) {
    let path = transfer_path(log);
    let tmp = path.with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec(p)
        && std::fs::write(&tmp, bytes).is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Forgets the transfer of the job whose log is `log`: it ended.
pub fn clear_transfer(log: &Path) {
    let _ = std::fs::remove_file(transfer_path(log));
}

/// The wall clock, ms since the epoch.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// The paired machine's name for `peer`, or its node id.
fn peer_name<T: Transport>(inner: &Inner<T>, peer: &PeerId) -> String {
    lock(&inner.links)
        .values()
        .find(|l| &l.spec.node_id == peer)
        .map_or_else(|| peer.to_string(), |l| l.spec.name.clone())
}

fn failed(path: &str, reason: String) -> FetchResult {
    FetchResult {
        path: path.to_string(),
        status: FetchStatus::Failed { reason },
    }
}

/// The words for a peer error during a fetch; an older peer that does not know the fetch
/// messages is named as such (R-509).
fn explain(peer: &PeerId, e: &PeerError) -> String {
    match e {
        PeerError::Refused(Refusal::UnknownMessage { .. } | Refusal::Malformed { .. }) => format!(
            "peer {peer} does not support output fetch (it runs an older tasks; upgrade it): {e}"
        ),
        PeerError::Refused(Refusal::NotSynced { .. }) => format!(
            "peer {peer} no longer has this checkout's copy (cleaned by retention?); the job's files are gone: {e}"
        ),
        other => other.to_string(),
    }
}

/// Fetches what `job` asked for into its checkout and records the outcome in `end`, the job
/// log and the sidecar. Does nothing for a job that asked for nothing.
pub(super) fn fetch_outputs<T: Transport>(
    inner: &Inner<T>,
    job: &Job,
    log: &mut FileLog,
    end: &mut ForwardEnd,
) {
    if job.fetch.is_empty() {
        return;
    }
    let (results, notes) = if end.phase == Phase::Passed && end.remote_job.is_some() {
        let out = run(inner, job, log, &end.reason);
        clear_transfer(log.path());
        out
    } else {
        let why = format!("not fetched: the job did not pass ({})", end.phase.word());
        (
            job.fetch.iter().map(|p| failed(p, why.clone())).collect(),
            Vec::new(),
        )
    };
    let mut text = String::new();
    for n in &notes {
        text.push_str("[tasks] ");
        text.push_str(n);
        text.push('\n');
    }
    for r in &results {
        text.push_str("[tasks] ");
        text.push_str(&r.line());
        text.push('\n');
    }
    log.append_local(text.as_bytes());
    write_results(log.path(), &results);
    end.fetch = results;
}

/// The results, and a note for each file whose blob stream failed and came in chunks instead.
fn run<T: Transport>(
    inner: &Inner<T>,
    job: &Job,
    log: &mut FileLog,
    remote_result: &str,
) -> (Vec<FetchResult>, Vec<String>) {
    let peer = peer_name(inner, &job.peer);
    let every = i64::try_from(inner.opts.transfer_report_every.as_millis()).unwrap_or(1000);
    let shown = |path: &str, done: u64, total: Option<u64>, rate: Option<f64>| TransferProgress {
        kind: TransferKind::Fetch,
        peer: peer.clone(),
        path: Some(path.to_string()),
        done_bytes: done,
        total_bytes: total,
        rate_bps: rate,
        remote_result: Some(remote_result.to_string()),
        at_ms: now_ms(),
    };
    // waiting for the transfer slot is already "fetching": the command is over
    if let Some(first) = job.fetch.first() {
        write_transfer(log.path(), &shown(first, 0, None, None));
    }
    // a transfer slot, like a sync: the writes obey the I/O gate
    let _slot = inner.opts.gate.enter(SyncPhase::Fetch);
    let mut conn: Option<Box<dyn Connection>> = None;
    // set once the peer has said it cannot fetch at all: asking again would only repeat it
    let mut unsupported: Option<String> = None;
    let mut out = Vec::new();
    let mut notes = Vec::new();
    for path in &job.fetch {
        if let Some(why) = &unsupported {
            out.push(failed(path, why.clone()));
            continue;
        }
        if conn.is_none() {
            match inner
                .transport
                .connect(&job.peer, inner.opts.policy.connect_timeout)
            {
                Ok(c) => conn = Some(c),
                Err(e) => {
                    out.push(failed(path, explain(&job.peer, &e)));
                    continue;
                }
            }
        }
        let Some(c) = conn.as_mut() else {
            continue;
        };
        let one = [path.clone()];
        let started = now_ms();
        let mut sidecar = TransferMeter::new(started, every);
        let mut logged = TransferMeter::new(started, every.saturating_mul(3));
        let mut report = |p: &str, done: u64, total: u64| {
            let now = now_ms();
            let rate = sidecar.rate(now, done);
            let last = done >= total;
            if sidecar.due(now) || last {
                write_transfer(log.path(), &shown(p, done, Some(total), rate));
            }
            if logged.due(now) || last {
                let line = shown(p, done, Some(total), rate).line();
                log.append_local(format!("[tasks] {line}\n").as_bytes());
            }
        };
        let fetched = outputs::fetch_with_progress(
            c.as_mut(),
            &job.checkout,
            &one,
            &job.root,
            inner.opts.fetch_max_bytes,
            &inner.opts.fetch_links,
            &mut report,
        );
        match fetched {
            Ok(report) => {
                if let Some(why) = report
                    .fetched
                    .first()
                    .and_then(|f| f.blob_fallback.as_ref())
                {
                    notes.push(format!(
                        "note: {path} came in chunks, slower than the blob stream ({why})"
                    ));
                }
                out.push(result_of(path, &report));
            }
            Err(e) => {
                let why = match &e {
                    FetchError::Peer(pe) => explain(&job.peer, pe),
                    other => other.to_string(),
                };
                if matches!(
                    &e,
                    FetchError::Peer(PeerError::Refused(
                        Refusal::UnknownMessage { .. } | Refusal::Malformed { .. }
                    ))
                ) {
                    unsupported = Some(why.clone());
                }
                // the link may be dead: the next path connects again
                conn = None;
                out.push(failed(path, why));
            }
        }
    }
    (out, notes)
}

fn result_of(path: &str, report: &outputs::FetchReport) -> FetchResult {
    let status = if let Some(f) = report.fetched.first() {
        FetchStatus::Fetched {
            bytes: f.size,
            sha256: f.hash.to_string(),
            unchanged: f.unchanged,
        }
    } else if !report.missing.is_empty() {
        FetchStatus::Missing
    } else if let Some((_, bytes, limit)) = report.too_large.first() {
        FetchStatus::TooLarge {
            bytes: *bytes,
            limit: *limit,
        }
    } else if let Some((_, reason)) = report.refused.first() {
        FetchStatus::Failed {
            reason: reason.clone(),
        }
    } else {
        FetchStatus::Failed {
            reason: "the peer answered nothing for this path".to_string(),
        }
    };
    FetchResult {
        path: path.to_string(),
        status,
    }
}
