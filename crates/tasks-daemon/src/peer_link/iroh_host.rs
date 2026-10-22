//! The real transport (daemon feature `iroh`): an iroh endpoint on a small runtime that lives on
//! threads of its own.
//!
//! The scheduler and the job threads stay plain `std` threads. [`IrohTransport`]'s blocking calls
//! use this runtime's handle from those threads, never from a runtime worker, and the receiver's
//! handlers run on the runtime's blocking pool.

use super::PeerLink;
use super::PeerLinkOptions;
use crate::peers_config::PeerConfig;
use iroh::{EndpointAddr, PublicKey, SecretKey};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use tasks_peer::iroh::{IrohError, IrohTransport, lan_endpoint, serve};
use tasks_peer::receiver::{Allowlist, Receiver};
use tasks_peer::{Handler, PeerId};
use thiserror::Error;

/// Why the peer endpoint could not start.
#[derive(Debug, Error)]
pub enum IrohHostError {
    /// `[peers]` has peers but no `bind`.
    #[error("peers are configured but peers.bind is not set; name this machine's LAN address")]
    NoBind,
    /// The secret key file could not be read or created.
    #[error("peer key {path}: {detail}")]
    Key {
        /// The file.
        path: String,
        /// What went wrong.
        detail: String,
    },
    /// A peer's node id is not an iroh public key.
    #[error("peers.{peer}.node_id is not a valid node id: {detail}")]
    BadNodeId {
        /// The peer's name.
        peer: String,
        /// What was wrong.
        detail: String,
    },
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
    peers: BTreeMap<PeerId, EndpointAddr>,
}

fn key_error(path: &Path, e: impl std::fmt::Display) -> IrohHostError {
    IrohHostError::Key {
        path: path.display().to_string(),
        detail: e.to_string(),
    }
}

/// Reads this node's secret key from `path` (64 hex digits), creating it 0600 from the system's
/// randomness on first start.
fn secret_key(path: &Path) -> Result<SecretKey, IrohHostError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut bytes = [0u8; 32];
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let hex = text.trim();
            if hex.len() != 64 {
                return Err(key_error(path, "expected 64 hex digits"));
            }
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = hex
                    .get(2 * i..2 * i + 2)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| key_error(path, "not hexadecimal"))?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::File::open("/dev/urandom")
                .and_then(|mut f| f.read_exact(&mut bytes))
                .map_err(|e| key_error(path, e))?;
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| key_error(path, e))?;
            writeln!(f, "{hex}").map_err(|e| key_error(path, e))?;
        }
        Err(e) => return Err(key_error(path, e)),
    }
    Ok(SecretKey::from_bytes(&bytes))
}

impl IrohHost {
    /// Binds the LAN endpoint named by `cfg.bind`, with the node key kept in `state_dir`.
    pub fn start(cfg: &PeerConfig, state_dir: &Path) -> Result<IrohHost, IrohHostError> {
        let bind = cfg.bind.ok_or(IrohHostError::NoBind)?;
        let secret = secret_key(&state_dir.join("peer-key"))?;
        let mut peers = BTreeMap::new();
        for p in &cfg.peers {
            let Some(addr) = p.addr else { continue };
            let key: PublicKey =
                p.node_id
                    .as_str()
                    .parse()
                    .map_err(|e: iroh::KeyParsingError| IrohHostError::BadNodeId {
                        peer: p.name.clone(),
                        detail: e.to_string(),
                    })?;
            peers.insert(p.node_id.clone(), EndpointAddr::new(key).with_ip_addr(addr));
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("peer-rt")
            .enable_all()
            .build()
            .map_err(|e| IrohHostError::Runtime(e.to_string()))?;
        let endpoint = rt.block_on(lan_endpoint(secret, bind))?;
        Ok(IrohHost {
            rt,
            endpoint,
            peers,
        })
    }

    /// This node's id, to put in the other machine's `[peers.<name>] node_id`.
    pub fn node_id(&self) -> String {
        self.endpoint.id().to_string()
    }

    /// A forwarder over this endpoint.
    pub fn link(&self, opts: PeerLinkOptions) -> PeerLink<IrohTransport> {
        PeerLink::new(
            IrohTransport::new(
                self.endpoint.clone(),
                self.rt.handle().clone(),
                self.peers.clone(),
            ),
            opts,
        )
    }

    /// Starts serving `receiver` to the allowlisted peers.
    pub fn serve(&self, receiver: Arc<Receiver>, allow: Allowlist) {
        let handler: Arc<dyn Handler> = receiver;
        self.rt.spawn(serve(self.endpoint.clone(), handler, allow));
    }
}

/// Starts the endpoint when the policy names peers, and plugs the forwarder into `opts`.
/// Returns the host, which must outlive the daemon; `None` when no peers are configured.
pub fn attach(
    opts: &mut crate::server::DaemonOptions,
    policy: &crate::machine_policy::MachinePolicy,
) -> Result<Option<IrohHost>, crate::error::DaemonError> {
    let cfg = policy.peers();
    if cfg.peers.is_empty() {
        return Ok(None);
    }
    let host = IrohHost::start(cfg, &opts.config.state_dir)
        .map_err(|e| crate::error::DaemonError::Peer(e.to_string()))?;
    eprintln!("tasks2d: peer node id {}", host.node_id());
    let link = host.link(PeerLinkOptions::new(
        &opts.config.machine,
        cfg.peers.clone(),
    ));
    opts.forwarder = Arc::new(link);
    Ok(Some(host))
}
