//! Live metrics of a forwarded job, asked of the peer that runs it.
//!
//! The origin keeps no samples for such a job: the work, and so the numbers, are on the peer.
//! One bounded request per answer, on a thread of its own so the caller waits at most
//! [`PeerLinkOptions::metrics_timeout`]; a recent answer is reused so a UI polling every second
//! does not double the traffic. An unreachable or older peer is a note on an empty view, never
//! an error that breaks the status screen.

use super::{Inner, PeerLink};
use crate::forwarder::ForwardError;
use crate::scheduler::lock;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tasks_core::MachineName;
use tasks_core::metrics::JobMetricsView;
use tasks_peer::proto::Body;
use tasks_peer::transport::{PeerError, Transport};
use tasks_peer::{PeerId, Refusal, RemoteJobId};

/// How long an answer is reused.
const CACHE_FOR: Duration = Duration::from_secs(1);

/// An answer kept for [`CACHE_FOR`].
pub(super) type Cached = (Instant, JobMetricsView);

/// A view with nothing to draw and the reason.
pub fn empty_view(note: String) -> JobMetricsView {
    JobMetricsView {
        quality: None,
        samples: Vec::new(),
        procs: Vec::new(),
        usual_peak_mem_bytes: None,
        note: Some(note),
        source: None,
    }
}

/// The words for a failed request about `remote` on `peer`.
pub fn explain(peer: &str, remote: i64, e: &PeerError) -> String {
    match e {
        PeerError::Refused(Refusal::UnknownMessage { .. } | Refusal::Malformed { .. }) => {
            format!("peer {peer} does not serve job metrics; upgrade it")
        }
        PeerError::Refused(Refusal::UnknownJob { .. }) => {
            format!("{peer} no longer has remote job #{remote}")
        }
        PeerError::Offline { .. } | PeerError::Timeout(_) | PeerError::LinkLost(_) => {
            format!("{peer} unreachable")
        }
        other => format!("{peer} could not give metrics: {other}"),
    }
}

fn ask<T: Transport>(
    inner: &Inner<T>,
    node: &PeerId,
    peer: &str,
    remote: i64,
) -> Result<JobMetricsView, PeerError> {
    let id = u64::try_from(remote)
        .map(RemoteJobId)
        .map_err(|_| PeerError::Protocol(format!("bad remote job id {remote}")))?;
    let mut conn = inner.transport.connect(node, inner.opts.metrics_timeout)?;
    match conn.request(Body::JobMetricsRequest { remote: id })? {
        Body::JobMetrics(mut v) => {
            v.source = Some(format!("on {peer}, remote job #{remote}"));
            Ok(v)
        }
        other => Err(PeerError::Protocol(format!(
            "answered a metrics request with {}",
            tasks_peer::fake::kind(&other)
        ))),
    }
}

impl<T: Transport + 'static> PeerLink<T> {
    /// The metrics of remote job `remote` on `peer`: from the cache, else asked of the peer
    /// within the metrics timeout.
    pub(super) fn remote_metrics_of(
        &self,
        peer: &MachineName,
        remote: i64,
    ) -> Result<JobMetricsView, ForwardError> {
        let Some(spec) = self.spec_of(&peer.0) else {
            return Err(ForwardError::Unavailable(format!(
                "{} is not a configured peer",
                peer.0
            )));
        };
        let key = (peer.0.clone(), remote);
        if let Some((at, v)) = lock(&self.inner.metrics_cache).get(&key)
            && at.elapsed() < CACHE_FOR
        {
            return Ok(v.clone());
        }
        let name = peer.0.clone();
        let view = if self.link_is_down(&peer.0) {
            // the status probe already knows; do not make every poll wait for a timeout
            empty_view(format!("{name} unreachable"))
        } else {
            let (tx, rx) = mpsc::channel();
            let inner = Arc::clone(&self.inner);
            let n = name.clone();
            let spawned = std::thread::Builder::new()
                .name("peer-metrics".into())
                .spawn(move || {
                    let _ = tx.send(ask(&inner, &spec.node_id, &n, remote));
                });
            match spawned {
                Err(e) => empty_view(format!("{name} not asked: {e}")),
                Ok(_) => match rx.recv_timeout(self.inner.opts.metrics_timeout) {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => empty_view(explain(&name, remote, &e)),
                    Err(_) => empty_view(format!("{name} unreachable")),
                },
            }
        };
        let mut cache = lock(&self.inner.metrics_cache);
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_FOR);
        cache.insert(key, (Instant::now(), view.clone()));
        Ok(view)
    }
}
