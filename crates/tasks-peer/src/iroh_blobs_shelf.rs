//! Fetched outputs over iroh-blobs (R-795, feature `iroh`).
//!
//! A fetch listing offers each found file on the worker's blob store: the store references the
//! file where it is (no copy) and records its BLAKE3 tree, and the requesting peer is granted that
//! one hash for [`GRANT_TTL`]. The origin then streams the file on the blobs ALPN, verified piece
//! by piece, instead of one request per chunk.
//!
//! Nothing else is served: a blobs connection from a machine with no live grant is refused, a get
//! for a hash not granted to the asking machine is refused, and get-many, push and observe are
//! off. Grants come only from fetch listings, which the trusted handler has admitted, so the
//! blobs ALPN never widens what a peer may read. Expired grants drop their tags and the store's
//! GC forgets the entries.

use crate::ids::PeerId;
use crate::outputs::BlobShelf;
use iroh_blobs::api::blobs::AddPathOptions;
use iroh_blobs::api::proto::ImportMode;
use iroh_blobs::provider::events::{
    AbortReason, ConnectMode, EventMask, EventSender, ObserveMode, ProviderMessage, RequestMode,
    ThrottleMode,
};
use iroh_blobs::store::GcConfig;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::store::fs::options::Options;
use iroh_blobs::{BlobFormat, BlobsProtocol, Hash};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

/// How long a listed file stays fetchable by the machine that listed it.
pub const GRANT_TTL: Duration = Duration::from_secs(30 * 60);
/// How often the store forgets entries no grant holds.
const GC_EVERY: Duration = Duration::from_secs(10 * 60);

#[derive(Default)]
struct Grants {
    /// node id → blob → (its tag, when the grant ends)
    by_peer: HashMap<String, HashMap<Hash, (iroh_blobs::api::Tag, Instant)>>,
    /// blobs connection id → the node id it came from
    conns: HashMap<u64, String>,
}

impl Grants {
    fn live(&self, peer: &str, now: Instant) -> bool {
        self.by_peer
            .get(peer)
            .is_some_and(|m| m.values().any(|(_, until)| *until > now))
    }

    fn allows(&self, conn: u64, hash: &Hash, now: Instant) -> bool {
        self.conns
            .get(&conn)
            .and_then(|p| self.by_peer.get(p))
            .and_then(|m| m.get(hash))
            .is_some_and(|(_, until)| *until > now)
    }

    /// Takes out the expired grants and returns their tags.
    fn expire(&mut self, now: Instant) -> Vec<iroh_blobs::api::Tag> {
        let mut gone = Vec::new();
        for m in self.by_peer.values_mut() {
            m.retain(|_, (tag, until)| {
                let keep = *until > now;
                if !keep {
                    gone.push(tag.clone());
                }
                keep
            });
        }
        self.by_peer.retain(|_, m| !m.is_empty());
        gone
    }
}

/// The worker's blob store and the protocol that serves it.
pub struct BlobServe {
    store: FsStore,
    protocol: BlobsProtocol,
    grants: Arc<Mutex<Grants>>,
    rt: Handle,
}

impl BlobServe {
    /// Opens (or creates) the store in `dir`. Must run on the runtime the endpoint serves on.
    pub async fn open(dir: &Path) -> Result<BlobServe, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut opts = Options::new(dir);
        opts.gc = Some(GcConfig {
            interval: GC_EVERY,
            add_protected: None,
        });
        let store = FsStore::load_with_opts(dir.join("blobs.db"), opts)
            .await
            .map_err(|e| format!("blob store {}: {e}", dir.display()))?;
        let grants = Arc::new(Mutex::new(Grants::default()));
        let mask = EventMask {
            connected: ConnectMode::Intercept,
            get: RequestMode::Intercept,
            get_many: RequestMode::Disabled,
            push: RequestMode::Disabled,
            observe: ObserveMode::Intercept,
            throttle: ThrottleMode::None,
        };
        let (events, rx) = EventSender::channel(32, mask);
        tokio::spawn(gatekeeper(rx, Arc::clone(&grants)));
        let protocol = BlobsProtocol::new(&store, Some(events));
        Ok(BlobServe {
            store,
            protocol,
            grants,
            rt: Handle::current(),
        })
    }

    /// The protocol handler for connections on [`iroh_blobs::ALPN`].
    pub fn protocol(&self) -> BlobsProtocol {
        self.protocol.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Grants> {
        self.grants.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl BlobShelf for BlobServe {
    /// Called from the handler's blocking thread, never from a runtime worker.
    fn offer(&self, to: &PeerId, file: &Path) -> Option<String> {
        let now = Instant::now();
        let file = file.canonicalize().ok()?;
        let gone = self.lock().expire(now);
        let store = self.store.clone();
        let added = self.rt.block_on(async move {
            for tag in gone {
                let _ = store.tags().delete(tag).await;
            }
            store
                .add_path_with_opts(AddPathOptions {
                    path: file,
                    format: BlobFormat::Raw,
                    mode: ImportMode::TryReference,
                })
                .with_tag()
                .await
        });
        let info = added.ok()?;
        let old = self
            .lock()
            .by_peer
            .entry(to.as_str().to_string())
            .or_default()
            .insert(info.hash, (info.name, now + GRANT_TTL));
        if let Some((tag, _)) = old {
            let store = self.store.clone();
            let _ = self
                .rt
                .block_on(async move { store.tags().delete(tag).await });
        }
        Some(info.hash.to_hex())
    }
}

/// Answers the provider's intercepted events from the grants.
async fn gatekeeper(
    mut rx: tokio::sync::mpsc::Receiver<ProviderMessage>,
    grants: Arc<Mutex<Grants>>,
) {
    let lock = || grants.lock().unwrap_or_else(PoisonError::into_inner);
    while let Some(msg) = rx.recv().await {
        let now = Instant::now();
        match msg {
            ProviderMessage::ClientConnected(m) => {
                let ok = match m.endpoint_id.map(|id| id.to_string()) {
                    Some(p) => {
                        let mut g = lock();
                        if g.live(&p, now) {
                            g.conns.insert(m.connection_id, p);
                            Ok(())
                        } else {
                            Err(AbortReason::Permission)
                        }
                    }
                    None => Err(AbortReason::Permission),
                };
                m.tx.send(ok).await.ok();
            }
            ProviderMessage::ConnectionClosed(m) => {
                lock().conns.remove(&m.connection_id);
            }
            ProviderMessage::GetRequestReceived(m) => {
                let ok = if m.request.ranges.is_blob()
                    && lock().allows(m.connection_id, &m.request.hash, now)
                {
                    Ok(())
                } else {
                    Err(AbortReason::Permission)
                };
                m.tx.send(ok).await.ok();
            }
            ProviderMessage::ObserveRequestReceived(m) => {
                m.tx.send(Err(AbortReason::Permission)).await.ok();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(s: &str) -> iroh_blobs::api::Tag {
        iroh_blobs::api::Tag::from(s)
    }

    #[test]
    fn r795_a_grant_covers_one_hash_for_one_machine_until_it_expires() {
        let now = Instant::now();
        let (a, b) = (Hash::new(b"a"), Hash::new(b"b"));
        let mut g = Grants::default();
        g.by_peer
            .entry("nixos".into())
            .or_default()
            .insert(a, (tag("t-a"), now + GRANT_TTL));
        g.conns.insert(1, "nixos".into());
        g.conns.insert(2, "mac".into());
        assert!(g.live("nixos", now));
        assert!(!g.live("mac", now), "no grant, no connection");
        assert!(g.allows(1, &a, now));
        assert!(!g.allows(1, &b, now), "only the listed hash");
        assert!(!g.allows(2, &a, now), "only the machine that listed it");
        let later = now + GRANT_TTL + Duration::from_secs(1);
        assert!(!g.allows(1, &a, later));
        let gone = g.expire(later);
        assert_eq!(gone, vec![tag("t-a")]);
        assert!(!g.live("nixos", later));
    }
}
