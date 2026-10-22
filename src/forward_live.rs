//! Live numbers of a forwarded job. The local job of a `tasks peer-forward` is only a proxy (a few
//! MB, no CPU); the compile runs under the peer's daemon. The peer's snapshot stream already
//! carries that job's `live`, `progress` and `compiling`, so [`overlay`] copies them onto the local
//! row, matched by `offload.worker` + `offload.remote_job_id`. When the peer is offline, stale or no
//! longer lists the job, the proxy's own sample is dropped: unknown beats a believable 0.0.
use crate::metrics::{JobView, Snapshot};
use crate::peer_link::{PeerSnapshot, is_forward_argv};

/// What [`overlay`] did to one job, for tests and for `--json` readers that want to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Numbers now come from the peer's snapshot.
    Peer,
    /// The proxy's own numbers were removed; the peer gave none.
    Unknown,
}

/// Replaces the live numbers of every executing forwarded job in `jobs` with the peer's.
/// Returns `(job id, source)` for each job it changed.
pub fn overlay(jobs: &mut [JobView], peers: &[PeerSnapshot]) -> Vec<(i64, Source)> {
    let mut changed = Vec::new();
    for job in jobs.iter_mut() {
        if job.phase.terminal() || !is_forward_argv(&job.argv) {
            continue;
        }
        let Some(link) = job.offload.as_ref() else {
            continue;
        };
        let remote = link.remote_job_id.and_then(|rid| {
            let peer = peers
                .iter()
                .find(|p| p.name == link.worker && p.online() && !p.stale)?;
            let snap: Snapshot = serde_json::from_value(peer.snapshot.clone()?).ok()?;
            snap.jobs.into_iter().find(|j| j.id == rid)
        });
        match remote {
            Some(r) if !r.phase.terminal() => {
                job.live = r.live;
                job.progress = r.progress;
                job.compiling = r.compiling;
                changed.push((job.id, Source::Peer));
            }
            _ => {
                job.live = None;
                job.compiling = None;
                changed.push((job.id, Source::Unknown));
            }
        }
    }
    changed
}

/// [`overlay`] with the peers the daemon at `socket` currently streams. Best effort: with no
/// peer runtime, or no reply within `timeout`, the proxies keep their own numbers (no forwarded
/// job can name a worker then, and the answer would not be better than the sample).
pub fn overlay_from_daemon(
    socket: &std::path::Path,
    timeout: std::time::Duration,
    jobs: &mut [JobView],
) -> Vec<(i64, Source)> {
    if !jobs.iter().any(|j| is_forward_argv(&j.argv)) {
        return Vec::new();
    }
    match crate::peer_link::request_peers(socket, timeout) {
        Ok(report) => overlay(jobs, &report.snapshots),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
#[path = "forward_live_tests.rs"]
mod tests;
