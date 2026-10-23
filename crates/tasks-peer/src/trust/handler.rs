//! The serving side's gate: [`TrustedHandler`] wraps a [`Handler`] and lets nothing through
//! until the connection has presented a valid grant. Pairing messages are answered here; every
//! other request is checked against the grants' roles first.

use super::node::TrustNode;
use super::role::{Need, need_of};
use crate::ids::{PeerId, RemoteJobId};
use crate::proto::{Body, Refusal};
use crate::transport::{Closer, EventStream, Handler};
use std::sync::Arc;

/// A handler that admits on grants.
pub struct TrustedHandler {
    node: Arc<TrustNode>,
    inner: Arc<dyn Handler>,
}

impl TrustedHandler {
    /// Puts the trust gate in front of `inner`.
    pub fn new(node: Arc<TrustNode>, inner: Arc<dyn Handler>) -> TrustedHandler {
        TrustedHandler { node, inner }
    }
}

impl Handler for TrustedHandler {
    fn handle(&self, from: &PeerId, msg: Body) -> Body {
        let node_id = from.as_str();
        match msg {
            Body::Present { grants } => match self.node.present(node_id, &grants) {
                Ok(ack) => Body::PresentAck(ack),
                Err(e) => Body::Refuse(e.to_refusal()),
            },
            Body::PairBegin => match self.node.pair_begin(node_id) {
                Ok(challenge) => Body::PairChallenge { challenge },
                Err(e) => Body::Refuse(e.to_refusal()),
            },
            Body::PairRedeem(r) => match self.node.pair_redeem(node_id, &r) {
                Ok(done) => Body::PairDone(done),
                Err(e) => Body::Refuse(e.to_refusal()),
            },
            Body::Renew { grant } => {
                if let Err(r) = self.node.authorize(node_id, Need::Any) {
                    return Body::Refuse(r);
                }
                match self.node.renew(node_id, &grant) {
                    Ok(grant) => Body::Renewed { grant },
                    Err(e) => Body::Refuse(e.to_refusal()),
                }
            }
            other => {
                if let Err(r) = self.node.authorize(node_id, need_of(&other)) {
                    return Body::Refuse(r);
                }
                self.inner.handle(from, other)
            }
        }
    }

    fn events(
        &self,
        from: &PeerId,
        remote: RemoteJobId,
        from_offset: u64,
    ) -> Result<Box<dyn EventStream>, Refusal> {
        self.node.authorize(from.as_str(), Need::Any)?;
        self.inner.events(from, remote, from_offset)
    }

    fn accept_upload(&self, from: &PeerId, head: &Body) -> Result<(), Refusal> {
        self.node.authorize(from.as_str(), need_of(head))?;
        self.inner.accept_upload(from, head)
    }

    fn upload(&self, from: &PeerId, head: Body, data: &mut dyn std::io::Read) -> Body {
        if let Err(r) = self.node.authorize(from.as_str(), need_of(&head)) {
            return Body::Refuse(r);
        }
        self.inner.upload(from, head, data)
    }

    fn connection_opened(&self, peer: &PeerId, closer: Closer) -> u64 {
        self.node.connection_opened(peer.as_str(), closer)
    }

    fn connection_closed(&self, peer: &PeerId, conn: u64, reason: &str) {
        self.node.connection_closed(peer.as_str(), conn, reason);
    }

    fn is_admitted(&self, peer: &PeerId) -> bool {
        self.node.is_admitted(peer.as_str())
    }
}
