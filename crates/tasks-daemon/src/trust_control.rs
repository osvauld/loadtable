//! Trust control: identity, pairing and peers, as the daemon API exposes them.
//!
//! [`TrustControl`] is the trait the scheduler talks to; [`NodeTrust`] implements it over a
//! [`TrustNode`] and a transport. Views are plain data so the CLI, the UI and MCP can show them.

use crate::forwarder::LinkQuality;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tasks_peer::transport::Transport;
use tasks_peer::trust::{AddrSource, Role, TrustError, TrustNode};

/// Who this machine is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityView {
    /// The machine's DID.
    pub did: String,
    /// Its node id (device key, hex).
    pub node_id: String,
    /// Its name.
    pub name: String,
}

/// A minted ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteView {
    /// The `tasks1.…` text to copy to the other machine.
    pub ticket: String,
    /// The roles it carries.
    pub roles: Vec<String>,
    /// Whether the redeemer grants back.
    pub both_ways: bool,
    /// Expiry, unix seconds.
    pub expires_at: u64,
}

/// A role and when its grant ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleView {
    /// The role.
    pub role: String,
    /// Expiry, unix seconds.
    pub expires_at: u64,
}

/// One paired peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerTrustView {
    /// The name.
    pub name: String,
    /// Its DID.
    pub did: String,
    /// Its node id.
    pub node_id: String,
    /// What this machine may do there.
    pub roles_out: Vec<RoleView>,
    /// What the peer may do here.
    pub roles_in: Vec<RoleView>,
    /// The earliest grant expiry either way, unix seconds.
    pub expires_at: Option<u64>,
    /// True when no live grant remains.
    pub revoked: bool,
    /// `direct`, `relay` or `unknown`.
    pub path: String,
    /// The transport's round-trip time of the selected path (path RTT), milliseconds.
    pub rtt_ms: Option<u64>,
    /// How long the last status request took to be answered (reply time), milliseconds.
    #[serde(default)]
    pub reply_ms: Option<u64>,
}

/// A paired peer, as reported by a successful `pair accept`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedView {
    /// Its name.
    pub name: String,
    /// Its DID.
    pub did: String,
    /// Its node id.
    pub node_id: String,
}

/// What a revoke did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeView {
    /// The peer.
    pub name: String,
    /// Grants revoked.
    pub revoked: usize,
    /// Live connections closed.
    pub closed: usize,
}

/// A grant close to its end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiringView {
    /// The peer.
    pub peer: String,
    /// `issued` or `held`.
    pub direction: String,
    /// The role.
    pub role: String,
    /// Seconds left; negative once expired.
    pub left_secs: i64,
}

/// What the scheduler and the API need from the trust layer.
pub trait TrustControl: Send + Sync {
    /// This machine's identity.
    fn identity(&self) -> IdentityView;
    /// Mints a ticket.
    fn invite(
        &self,
        roles: &[String],
        both_ways: bool,
        ttl_secs: Option<u64>,
    ) -> Result<InviteView, String>;
    /// Redeems a ticket from another machine.
    fn accept(&self, ticket: &str) -> Result<PairedView, String>;
    /// Invalidates unredeemed tickets; returns the new epoch.
    fn reset(&self) -> Result<u64, String>;
    /// Every paired peer.
    fn peers(&self) -> Vec<PeerTrustView>;
    /// Revokes a peer's grants.
    fn revoke(&self, name: &str) -> Result<RevokeView, String>;
    /// Grants inside the alert window or past their end.
    fn expiring(&self) -> Vec<ExpiringView>;
    /// Gives the control the latest link quality per peer, for the peer views.
    fn set_link_quality(&self, quality: Vec<LinkQuality>);
}

/// [`TrustControl`] over a [`TrustNode`].
pub struct NodeTrust {
    node: Arc<TrustNode>,
    /// The transport pairing dials with; `None` in a build without a peer transport.
    transport: Option<Arc<dyn Transport>>,
    addrs: Arc<dyn AddrSource>,
    quality: std::sync::Mutex<Vec<LinkQuality>>,
}

impl NodeTrust {
    /// Control over `node`. `transport` is what `accept` dials with.
    pub fn new(
        node: Arc<TrustNode>,
        transport: Option<Arc<dyn Transport>>,
        addrs: Arc<dyn AddrSource>,
    ) -> NodeTrust {
        NodeTrust {
            node,
            transport,
            addrs,
            quality: std::sync::Mutex::new(Vec::new()),
        }
    }
}

fn fail(e: TrustError) -> String {
    e.to_string()
}

impl TrustControl for NodeTrust {
    fn identity(&self) -> IdentityView {
        IdentityView {
            did: self.node.did().to_string(),
            node_id: self.node.node_id(),
            name: self.node.name().to_string(),
        }
    }

    fn invite(
        &self,
        roles: &[String],
        both_ways: bool,
        ttl_secs: Option<u64>,
    ) -> Result<InviteView, String> {
        let roles: Vec<Role> = roles
            .iter()
            .map(|r| Role::parse(r))
            .collect::<Result<_, _>>()
            .map_err(fail)?;
        let ticket = self
            .node
            .invite(&roles, both_ways, ttl_secs, &self.addrs.current())
            .map_err(fail)?;
        let claims = tasks_peer::trust::Invite::parse(&ticket.invite_token)
            .map_err(fail)?
            .claims;
        Ok(InviteView {
            ticket: ticket.to_text().map_err(fail)?,
            roles: claims.roles.iter().map(|r| r.name().to_string()).collect(),
            both_ways: claims.both_ways,
            expires_at: claims.exp,
        })
    }

    fn accept(&self, ticket: &str) -> Result<PairedView, String> {
        let Some(t) = &self.transport else {
            return Err(
                "this build has no peer transport; rebuild with --features iroh to pair"
                    .to_string(),
            );
        };
        let p = self
            .node
            .accept(
                ticket,
                t.as_ref(),
                &self.addrs.current(),
                Duration::from_secs(20),
            )
            .map_err(fail)?;
        Ok(PairedView {
            name: p.name,
            did: p.did,
            node_id: p.node_id,
        })
    }

    fn reset(&self) -> Result<u64, String> {
        self.node.reset_tickets().map_err(fail)
    }

    fn peers(&self) -> Vec<PeerTrustView> {
        let quality = self.quality.lock().map(|g| g.clone()).unwrap_or_default();
        self.node
            .peers()
            .into_iter()
            .map(|p| {
                let q = quality.iter().find(|q| q.peer == p.name);
                let roles = |v: &[tasks_peer::trust::RoleExpiry]| {
                    v.iter()
                        .map(|r| RoleView {
                            role: r.role.name().to_string(),
                            expires_at: r.exp,
                        })
                        .collect()
                };
                PeerTrustView {
                    roles_out: roles(&p.outbound),
                    roles_in: roles(&p.inbound),
                    name: p.name,
                    did: p.did,
                    node_id: p.node_id,
                    expires_at: p.expires,
                    revoked: p.revoked,
                    path: q.map_or_else(|| "unknown".to_string(), |q| q.path.clone()),
                    rtt_ms: q.and_then(|q| q.rtt_ms),
                    reply_ms: q.and_then(|q| q.reply_ms),
                }
            })
            .collect()
    }

    fn revoke(&self, name: &str) -> Result<RevokeView, String> {
        let r = self.node.revoke_peer(name).map_err(fail)?;
        Ok(RevokeView {
            name: r.name,
            revoked: r.revoked,
            closed: r.closed,
        })
    }

    fn expiring(&self) -> Vec<ExpiringView> {
        self.node
            .expiring()
            .into_iter()
            .map(|e| ExpiringView {
                peer: e.peer,
                direction: e.direction.to_string(),
                role: e.role.name().to_string(),
                left_secs: e.left_secs,
            })
            .collect()
    }

    fn set_link_quality(&self, quality: Vec<LinkQuality>) {
        if let Ok(mut g) = self.quality.lock() {
            *g = quality;
        }
    }
}
