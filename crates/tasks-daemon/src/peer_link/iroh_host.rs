//! The real transport (daemon feature `iroh`): an iroh endpoint on a small runtime that lives on
//! threads of its own.
//!
//! The endpoint key is the machine's device key from the keystore, so the node id is part of the
//! identity and survives a restart. The scheduler and the job threads stay plain `std` threads.
//! [`IrohTransport`]'s blocking calls use this runtime's handle from those threads, never from a
//! runtime worker, and the receiver's handlers run on the runtime's blocking pool.

use super::{PeerLink, PeerLinkOptions};
use crate::config::DaemonConfig;
use crate::error::DaemonError;
use crate::machine_policy::MachinePolicy;
use crate::peers_config::{PeerConfig, RelaySetting};
use crate::trust_control::NodeTrust;
use crate::trust_runtime::{NodePeers, PeerRuntime, PeerRuntimeFactory, StderrEvents, open_trust};
use iroh::SecretKey;
use std::sync::Arc;
use tasks_ledger::SystemClock;
use tasks_peer::iroh::{
    EndpointAddrs, EndpointOptions, IrohError, IrohTransport, RelayChoice, endpoint, serve,
};
use tasks_peer::trust::{AddressBook, TrustedHandler, TrustedTransport};
use tasks_peer::{Handler, Receiver};
use thiserror::Error;

/// Why the peer endpoint could not start.
#[derive(Debug, Error)]
pub enum IrohHostError {
    /// The runtime could not be built.
    #[error("peer runtime: {0}")]
    Runtime(String),
    /// The endpoint could not be bound.
    #[error(transparent)]
    Endpoint(#[from] IrohError),
}

/// A running endpoint and the runtime behind it.
pub struct IrohHost {
    rt: tokio::runtime::Runtime,
    endpoint: iroh::Endpoint,
}

fn relay_choice(c: &PeerConfig) -> RelayChoice {
    match &c.relay {
        RelaySetting::Default => RelayChoice::Default,
        RelaySetting::Off => RelayChoice::Disabled,
        RelaySetting::Custom(u) => RelayChoice::Custom(u.clone()),
    }
}

impl IrohHost {
    /// Binds the endpoint with the device key as its key.
    pub fn start(secret: &[u8; 32], cfg: &PeerConfig) -> Result<IrohHost, IrohHostError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("peer-rt")
            .enable_all()
            .build()
            .map_err(|e| IrohHostError::Runtime(e.to_string()))?;
        let opts = EndpointOptions {
            relay: relay_choice(cfg),
            bind: cfg.bind,
        };
        let endpoint = rt.block_on(endpoint(SecretKey::from_bytes(secret), &opts))?;
        Ok(IrohHost { rt, endpoint })
    }

    /// This node's id: the device key, lowercase hex.
    pub fn node_id(&self) -> String {
        self.endpoint.id().to_string()
    }
}

/// Starts identity, trust and the iroh endpoint.
pub struct IrohFactory;

impl PeerRuntimeFactory for IrohFactory {
    fn start(
        self: Box<Self>,
        cfg: &DaemonConfig,
        policy: &MachinePolicy,
    ) -> Result<PeerRuntime, DaemonError> {
        let rt = open_trust(cfg, Arc::new(StderrEvents), Arc::new(SystemClock))?;
        if rt.created {
            eprintln!("tasks2d: created this machine's identity {}", rt.node.did());
        }
        let secret = rt.identity.device_secret();
        let host = IrohHost::start(&secret, policy.peers())
            .map_err(|e| DaemonError::Peer(e.to_string()))?;
        eprintln!(
            "tasks2d: peer node id {} ({})",
            host.node_id(),
            rt.node.did()
        );
        let book: Arc<dyn AddressBook> = rt.node.clone();
        let raw = IrohTransport::new(host.endpoint.clone(), host.rt.handle().clone(), book);
        let trusted = Arc::new(TrustedTransport::new(raw.clone(), rt.node.clone()));
        let trust = NodeTrust::new(
            rt.node.clone(),
            Some(trusted.clone()),
            Arc::new(EndpointAddrs(host.endpoint.clone())),
        );
        let mut opts = PeerLinkOptions::new(&cfg.machine, Vec::new());
        opts.source = Some(Arc::new(NodePeers(rt.node.clone())));
        let link = PeerLink::new(TrustedTransport::new(raw, rt.node.clone()), opts);
        let node = rt.node.clone();
        let endpoint = host.endpoint.clone();
        let handle = host.rt.handle().clone();
        let serving: Box<dyn FnOnce(Arc<Receiver>) + Send> = Box::new(move |receiver| {
            let inner: Arc<dyn Handler> = receiver;
            let gate: Arc<dyn Handler> = Arc::new(TrustedHandler::new(node, inner));
            handle.spawn(serve(endpoint, gate));
        });
        Ok(PeerRuntime {
            forwarder: Some(Arc::new(link)),
            trust: Arc::new(trust),
            serving: Some(serving),
            keepalive: Box::new((host, rt)),
        })
    }
}
