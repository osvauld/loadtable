//! The daemon's trust runtime: the machine identity (keystore in the state dir, created on the
//! first start), the trust ledger and the [`TrustNode`] over them. Independent of the peer
//! transport, so every daemon has an identity and `tasks2 identity show` works on any build.

use crate::audited::AuditSink;
use crate::config::DaemonConfig;
use crate::error::DaemonError;
use crate::forwarder::Forwarder;
use crate::machine_policy::MachinePolicy;
use crate::peer_link::PeerSource;
use crate::peers_config::PeerSpec;
use crate::trust_control::{NodeTrust, TrustControl};
use std::sync::Arc;
use tasks_identity::Identity;
use tasks_ledger::{Clock, SqliteStore, SystemClock};
use tasks_peer::trust::{
    AuditEvents, FanoutEvents, SelfAddr, Side, TrustEvent, TrustEvents, TrustNode, TrustPolicy,
};
use tasks_peer::{PeerId, Receiver};

/// Logs the trust events an operator needs to see, one line each. Never a token, a key or a
/// ticket. The audit table keeps the full record; this is what an operator sees live.
pub struct StderrEvents;

impl TrustEvents for StderrEvents {
    fn record(&self, e: TrustEvent) {
        let line = match &e {
            TrustEvent::TicketRefused { code, reason, .. } => {
                format!("pairing refused ({code}): {reason}")
            }
            TrustEvent::TicketRedeemed {
                peer_name,
                peer_did,
                ..
            } => format!("ticket redeemed by {peer_name} ({peer_did})"),
            TrustEvent::TicketsReset { epoch } => {
                format!("unredeemed tickets reset (epoch {epoch})")
            }
            TrustEvent::GrantRevoked {
                direction,
                peer_did,
                role,
                ..
            } => format!("{direction} {role} grant for {peer_did} revoked"),
            TrustEvent::ConnectionRefused {
                side: Side::Served,
                node_id,
                code,
                reason,
            } => format!("refused {} ({code}): {reason}", short(node_id)),
            _ => return,
        };
        eprintln!("tasks2d: trust: {line}");
    }
}

fn short(node_id: &str) -> &str {
    node_id.get(..12).unwrap_or(node_id)
}

/// Identity and trust state of this daemon.
pub struct TrustRuntime {
    /// The machine identity.
    pub identity: Arc<Identity>,
    /// The trust rules over the ledger.
    pub node: Arc<TrustNode>,
    /// True when this start created the keystore.
    pub created: bool,
}

/// What the daemon hands a peer runtime: the identity (opened once by the daemon, which also
/// signs the audit log with it) and the audit sink.
pub struct PeerContext {
    /// The machine identity.
    pub identity: Arc<Identity>,
    /// True when this start created the keystore.
    pub created: bool,
    /// Where audit rows go.
    pub audit: Arc<AuditSink>,
}

/// Opens the trust ledger and builds the [`TrustNode`] over the identity in `ctx`. Trust events
/// go to the log line and to the audit table. Call after the one-daemon lock is held.
pub fn open_trust(
    cfg: &DaemonConfig,
    ctx: &PeerContext,
    clock: Arc<dyn Clock>,
) -> Result<TrustRuntime, DaemonError> {
    let identity = ctx.identity.clone();
    // A second connection to the ledger (WAL): trust state is kept apart from the job store.
    let ledger = Arc::new(SqliteStore::open(&cfg.db_path())?);
    let events: Arc<dyn TrustEvents> = Arc::new(FanoutEvents(vec![
        Arc::new(StderrEvents),
        Arc::new(AuditEvents::new(ctx.audit.log().clone(), clock.clone())),
    ]));
    let node = TrustNode::new(
        identity.clone(),
        &cfg.machine,
        ledger,
        events,
        clock,
        TrustPolicy::default(),
    )
    .map_err(|e| DaemonError::Identity(e.to_string()))?;
    Ok(TrustRuntime {
        identity,
        node,
        created: ctx.created,
    })
}

/// The paired machines a forwarder may dial: those this machine holds a live grant from.
pub struct NodePeers(pub Arc<TrustNode>);

impl PeerSource for NodePeers {
    fn peers(&self) -> Vec<PeerSpec> {
        self.0
            .paired_peers()
            .into_iter()
            .filter_map(|p| {
                Some(PeerSpec {
                    node_id: PeerId::new(&p.node_id).ok()?,
                    name: p.name,
                    did: p.did,
                })
            })
            .collect()
    }
}

/// What the peer runtime hands the daemon.
pub struct PeerRuntime {
    /// The forwarder; `None` keeps the one in the options.
    pub forwarder: Option<Arc<dyn Forwarder>>,
    /// Identity, pairing and peers for the API.
    pub trust: Arc<dyn TrustControl>,
    /// Starts serving the receiver to paired peers, once the scheduler exists.
    pub serving: Option<Box<dyn FnOnce(Arc<Receiver>) + Send>>,
    /// Kept alive as long as the daemon (the transport runtime lives here).
    pub keepalive: Box<dyn std::any::Any + Send>,
}

/// Builds the peer runtime inside [`crate::Daemon::start`], after the one-daemon lock is held
/// and before the scheduler exists. A stray second daemon therefore never binds an endpoint.
pub trait PeerRuntimeFactory: Send {
    /// Starts identity, trust and (when the build has one) the transport.
    fn start(
        self: Box<Self>,
        cfg: &DaemonConfig,
        policy: &MachinePolicy,
        ctx: &PeerContext,
    ) -> Result<PeerRuntime, DaemonError>;
}

/// Identity and pairing bookkeeping only: no transport, so tickets can be minted and read but
/// nothing is dialed or served. The default for builds without the `iroh` feature and for tests.
pub struct LocalOnly;

impl PeerRuntimeFactory for LocalOnly {
    fn start(
        self: Box<Self>,
        cfg: &DaemonConfig,
        _policy: &MachinePolicy,
        ctx: &PeerContext,
    ) -> Result<PeerRuntime, DaemonError> {
        let rt = open_trust(cfg, ctx, Arc::new(SystemClock))?;
        let trust = NodeTrust::new(rt.node.clone(), None, Arc::new(SelfAddr::default()));
        Ok(PeerRuntime {
            forwarder: None,
            trust: Arc::new(trust),
            serving: None,
            keepalive: Box::new(rt),
        })
    }
}
