//! What a peer keeps for this machine's checkouts, and asking it to drop some (R-839).
//!
//! One request per peer on a thread of its own, so `loadtable gc` waits at most
//! [`CACHE_TIMEOUT`] for a peer measuring tens of gigabytes. An older peer refuses the message
//! as unknown: that is said in words, never a failed report.

use super::PeerLink;
use crate::forwarder::ForwardError;
use std::sync::{Arc, mpsc};
use std::time::Duration;
use tasks_core::MachineName;
use tasks_peer::proto::{Body, DroppedCheckout, HeldCheckout};
use tasks_peer::transport::{PeerError, Transport};
use tasks_peer::{CheckoutId, Refusal};

/// How long a peer may take to measure or delete what it keeps.
pub const CACHE_TIMEOUT: Duration = Duration::from_secs(40);

fn explain(peer: &str, e: &PeerError) -> String {
    match e {
        PeerError::Refused(Refusal::UnknownMessage { .. } | Refusal::Malformed { .. }) => {
            format!("{peer} runs an older loadtabled that cannot report its cache; upgrade it")
        }
        PeerError::Offline { .. } | PeerError::Timeout(_) | PeerError::LinkLost(_) => {
            format!("{peer} unreachable")
        }
        other => format!("{peer} could not answer: {other}"),
    }
}

impl<T: Transport + 'static> PeerLink<T> {
    /// Sends `msg` to `peer` and waits at most [`CACHE_TIMEOUT`] for the reply.
    fn ask_cache(&self, peer: &MachineName, msg: Body) -> Result<Body, ForwardError> {
        let Some(spec) = self.spec_of(&peer.0) else {
            return Err(ForwardError::Unavailable(format!(
                "{} is not a configured peer",
                peer.0
            )));
        };
        if self.link_is_down(&peer.0) {
            return Err(ForwardError::Unavailable(format!("{} unreachable", peer.0)));
        }
        let (tx, rx) = mpsc::channel();
        let inner = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("peer-cache".into())
            .spawn(move || {
                let r = inner
                    .transport
                    .connect(&spec.node_id, inner.opts.metrics_timeout.max(CONNECT))
                    .and_then(|mut c| c.request(msg));
                let _ = tx.send(r);
            })
            .map_err(|e| ForwardError::Unavailable(format!("{} not asked: {e}", peer.0)))?;
        match rx.recv_timeout(CACHE_TIMEOUT) {
            Ok(Ok(b)) => Ok(b),
            Ok(Err(e)) => Err(ForwardError::Unavailable(explain(&peer.0, &e))),
            Err(_) => Err(ForwardError::Unavailable(format!(
                "{} did not answer within {}s",
                peer.0,
                CACHE_TIMEOUT.as_secs()
            ))),
        }
    }

    pub(super) fn peer_cache_of(
        &self,
        peer: &MachineName,
    ) -> Result<Vec<HeldCheckout>, ForwardError> {
        match self.ask_cache(peer, Body::CacheRequest)? {
            Body::CacheHeld { checkouts } => Ok(checkouts),
            other => Err(ForwardError::Unavailable(format!(
                "{} answered a cache request with {}",
                peer.0,
                tasks_peer::fake::kind(&other)
            ))),
        }
    }

    pub(super) fn peer_cache_drop_of(
        &self,
        peer: &MachineName,
        checkouts: Vec<CheckoutId>,
    ) -> Result<Vec<DroppedCheckout>, ForwardError> {
        match self.ask_cache(peer, Body::CacheDrop { checkouts })? {
            Body::CacheDropped { checkouts } => Ok(checkouts),
            other => Err(ForwardError::Unavailable(format!(
                "{} answered a cache drop with {}",
                peer.0,
                tasks_peer::fake::kind(&other)
            ))),
        }
    }
}

/// How long dialling a peer may take.
const CONNECT: Duration = Duration::from_secs(10);
