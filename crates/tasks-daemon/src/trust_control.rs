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

/// How reachable this machine is: its relay connection and its direct addresses, or that it has
/// no peer transport at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointView {
    /// False in a build without a peer transport: nothing else is known then.
    pub transport: bool,
    /// The relay this machine uses.
    #[serde(default)]
    pub relay: Option<String>,
    /// Whether the relay connection is up; absent when the transport cannot say.
    #[serde(default)]
    pub relay_connected: Option<bool>,
    /// Why the relay connection is down, when known.
    #[serde(default)]
    pub relay_error: Option<String>,
    /// Direct socket addresses.
    #[serde(default)]
    pub direct_addrs: Vec<String>,
}

impl EndpointView {
    /// A build without a peer transport.
    pub fn none() -> EndpointView {
        EndpointView {
            transport: false,
            relay: None,
            relay_connected: None,
            relay_error: None,
            direct_addrs: Vec::new(),
        }
    }

    /// `this machine: relay https://r/ (connected), direct 192.168.1.25:1234`; `this machine: no
    /// peer transport`.
    pub fn line(&self) -> String {
        if !self.transport {
            return "this machine: no peer transport".to_string();
        }
        let relay = match (&self.relay, self.relay_connected) {
            (Some(u), Some(true)) => format!("relay {u} (connected)"),
            (Some(u), Some(false)) => format!("relay {u} (not connected{})", self.why()),
            (Some(u), None) => format!("relay {u}"),
            (None, Some(false)) => format!("relay: not connected{}", self.why()),
            (None, _) => "no relay".to_string(),
        };
        let direct = if self.direct_addrs.is_empty() {
            "no direct addresses".to_string()
        } else {
            format!("direct {}", self.direct_addrs.join(", "))
        };
        format!("this machine: {relay}, {direct}")
    }

    fn why(&self) -> String {
        self.relay_error
            .as_ref()
            .map_or(String::new(), |e| format!(": {e}"))
    }
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
    /// How reachable this machine is now; `None` when the control cannot tell.
    fn endpoint(&self) -> Option<EndpointView> {
        None
    }
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

    fn endpoint(&self) -> Option<EndpointView> {
        // a control without a transport (LocalOnly) is not built over an endpoint
        self.transport.as_ref()?;
        let r = self.addrs.reach();
        Some(EndpointView {
            transport: true,
            relay: r.relay,
            relay_connected: r.relay_connected,
            relay_error: r.relay_error,
            direct_addrs: r.direct_addrs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_peer::trust::SelfReach;

    fn view(
        relay: Option<&str>,
        up: Option<bool>,
        err: Option<&str>,
        addrs: &[&str],
    ) -> EndpointView {
        EndpointView {
            transport: true,
            relay: relay.map(String::from),
            relay_connected: up,
            relay_error: err.map(String::from),
            direct_addrs: addrs.iter().map(|a| a.to_string()).collect(),
        }
    }

    // R-692
    #[test]
    fn r692_the_endpoint_line_says_relay_state_and_addresses_or_no_transport() {
        assert_eq!(
            view(
                Some("https://aps1-1.relay.n0.iroh.link./"),
                Some(true),
                None,
                &["192.168.1.25:1234", "10.0.0.2:99"]
            )
            .line(),
            "this machine: relay https://aps1-1.relay.n0.iroh.link./ (connected), \
             direct 192.168.1.25:1234, 10.0.0.2:99"
        );
        assert_eq!(
            view(
                Some("https://r/"),
                Some(false),
                Some("dns lookup failed"),
                &[]
            )
            .line(),
            "this machine: relay https://r/ (not connected: dns lookup failed), no direct addresses"
        );
        assert_eq!(
            view(None, Some(false), None, &["1.2.3.4:5"]).line(),
            "this machine: relay: not connected, direct 1.2.3.4:5"
        );
        assert_eq!(
            view(Some("https://r/"), None, None, &[]).line(),
            "this machine: relay https://r/, no direct addresses"
        );
        assert_eq!(
            view(None, None, None, &[]).line(),
            "this machine: no relay, no direct addresses"
        );
        assert_eq!(
            EndpointView::none().line(),
            "this machine: no peer transport"
        );
    }

    // R-692
    #[test]
    fn r692_an_older_daemons_endpoint_json_still_reads() {
        let v: EndpointView = serde_json::from_str(r#"{"transport":true}"#).unwrap();
        assert_eq!(v.relay, None);
        assert!(v.direct_addrs.is_empty());
    }

    struct Reach(SelfReach);
    impl AddrSource for Reach {
        fn current(&self) -> tasks_peer::trust::SelfAddr {
            tasks_peer::trust::SelfAddr {
                relay: self.0.relay.clone(),
                direct_addrs: self.0.direct_addrs.clone(),
            }
        }
        fn reach(&self) -> SelfReach {
            self.0.clone()
        }
    }

    // R-692
    #[test]
    fn r692_node_trust_reports_the_transports_reach_and_none_without_a_transport() {
        let (ident, _) = tasks_identity::generate().unwrap();
        let node = TrustNode::new(
            Arc::new(ident),
            "box",
            Arc::new(tasks_ledger::MemoryTrust::new()),
            Arc::new(tasks_peer::trust::NoEvents),
            Arc::new(tasks_ledger::SystemClock),
            tasks_peer::trust::TrustPolicy::default(),
        )
        .unwrap();
        let reach = SelfReach {
            relay: Some("https://r/".into()),
            relay_connected: Some(true),
            relay_error: None,
            direct_addrs: vec!["192.168.1.25:1".into()],
        };
        let net = tasks_peer::fake::FakeNet::new();
        let id = tasks_peer::PeerId::new(&node.node_id()).unwrap();
        let transport: Arc<dyn Transport> = Arc::new(net.transport(&id));
        let with = NodeTrust::new(node.clone(), Some(transport), Arc::new(Reach(reach)));
        let e = with.endpoint().unwrap();
        assert_eq!(
            (e.transport, e.relay_connected, e.direct_addrs.len()),
            (true, Some(true), 1)
        );
        let without = NodeTrust::new(node, None, Arc::new(tasks_peer::trust::SelfAddr::default()));
        assert_eq!(without.endpoint(), None);
    }
}
