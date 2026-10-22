//! The dialing side: [`TrustedTransport`] presents the grants this machine holds as the first
//! frame of every connection, checks who answered, and renews grants that are close to their
//! end over that same live connection.

use super::error::TrustError;
use super::node::TrustNode;
use super::role::Role;
use crate::ids::PeerId;
use crate::proto::{Body, Refusal};
use crate::transport::{Connection, DialHints, PeerError, Transport};
use std::sync::Arc;
use std::time::Duration;

/// A transport that never sends a request before presenting a grant.
pub struct TrustedTransport<T: Transport> {
    inner: T,
    node: Arc<TrustNode>,
}

impl<T: Transport> TrustedTransport<T> {
    /// Wraps `inner`.
    pub fn new(inner: T, node: Arc<TrustNode>) -> TrustedTransport<T> {
        TrustedTransport { inner, node }
    }

    /// The wrapped transport.
    pub fn inner(&self) -> &T {
        &self.inner
    }
}

impl<T: Transport> Transport for TrustedTransport<T> {
    fn connect(&self, peer: &PeerId, timeout: Duration) -> Result<Box<dyn Connection>, PeerError> {
        let held = self.node.held_for(peer.as_str());
        if held.is_empty() {
            let e = TrustError::NotAuthorised(
                "no live grant from this machine; pair with `tasks2 pair accept`".to_string(),
            );
            self.node.note_dial_refused(peer.as_str(), &e);
            return Err(PeerError::Refused(Refusal::Unauthorized {
                reason: e.to_string(),
            }));
        }
        let mut conn = self.inner.connect(peer, timeout)?;
        let ack = match conn.request(Body::Present {
            grants: held.iter().map(|g| g.text().to_string()).collect(),
        }) {
            Ok(Body::PresentAck(ack)) => ack,
            Ok(other) => {
                return Err(PeerError::Protocol(format!(
                    "answered a grant with {}",
                    crate::fake::kind(&other)
                )));
            }
            Err(e) => {
                if let PeerError::Refused(r) = &e {
                    self.node.note_dial_refused(
                        peer.as_str(),
                        &TrustError::NotAuthorised(r.to_string()),
                    );
                }
                return Err(e);
            }
        };
        let issuer = held.first().map(|g| g.claims.iss.as_str()).unwrap_or("");
        if ack.did != issuer {
            let e = TrustError::WrongParty(format!(
                "the machine that answered is {}, not the one that granted",
                ack.did
            ));
            self.node.note_dial_refused(peer.as_str(), &e);
            return Err(PeerError::Refused(Refusal::Unauthorized {
                reason: e.to_string(),
            }));
        }
        self.node.note_dialed(peer.as_str(), &ack.did, &ack.roles);
        self.node.renew_held(&mut *conn, peer.as_str());
        Ok(conn)
    }

    fn connect_hinted(
        &self,
        peer: &PeerId,
        hints: &DialHints,
        timeout: Duration,
    ) -> Result<Box<dyn Connection>, PeerError> {
        // Pairing happens before any grant exists, so nothing is presented here.
        self.inner.connect_hinted(peer, hints, timeout)
    }
}

/// The roles a machine holds from a peer, for status.
pub fn held_roles(node: &TrustNode, peer: &PeerId) -> Vec<Role> {
    let mut r: Vec<Role> = node
        .held_for(peer.as_str())
        .iter()
        .map(|g| g.claims.role)
        .collect();
    r.sort();
    r.dedup();
    r
}
