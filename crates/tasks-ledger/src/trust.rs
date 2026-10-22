//! Durable trust state: paired peers, live grants (issued and held) and spent invite nonces.
//!
//! Kept apart from [`crate::Store`] so the job store and its shared suite stay as they are.
//! Revoking a grant deletes nothing: the row stays with its state and time (design §3).

use crate::sqlite::SqliteStore;
use crate::{LedgerError, Result};
use rusqlite::{OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};

/// Which way a grant points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GrantDirection {
    /// This machine signed it: the peer may use this machine.
    Issued,
    /// The peer signed it: this machine may use the peer.
    Held,
}

impl GrantDirection {
    fn text(self) -> &'static str {
        match self {
            GrantDirection::Issued => "issued",
            GrantDirection::Held => "held",
        }
    }

    fn parse(s: &str) -> Result<GrantDirection> {
        match s {
            "issued" => Ok(GrantDirection::Issued),
            "held" => Ok(GrantDirection::Held),
            _ => Err(LedgerError::OutOfRange("unknown grant direction")),
        }
    }
}

/// A stored grant. `state` is `live`, `revoked` or `renewed`; only `live` grants admit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRecord {
    /// Hash of the signed payload; names the grant everywhere.
    pub id: String,
    /// Issued or held.
    pub direction: GrantDirection,
    /// The other machine's DID.
    pub peer_did: String,
    /// The other machine's node id (device key, hex).
    pub peer_node: String,
    /// `submit`, `observe` or `admin`.
    pub role: String,
    /// The invite nonce or renewal nonce it was minted under.
    pub nonce: String,
    /// Issued at, unix seconds.
    pub iat: i64,
    /// Expires at, unix seconds.
    pub exp: i64,
    /// The signed token text.
    pub token: String,
    /// `live`, `revoked` or `renewed`.
    pub state: String,
    /// When the state last changed, ms since the epoch.
    pub state_at_ms: i64,
}

/// A paired peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecord {
    /// The name placement and `--peer` use.
    pub name: String,
    /// Its DID.
    pub did: String,
    /// Its node id (device key, hex).
    pub node_id: String,
    /// Its relay hint, if any.
    pub relay: Option<String>,
    /// Its latest known direct addresses.
    pub addrs: Vec<String>,
    /// When it was paired, ms since the epoch.
    pub paired_ms: i64,
}

/// Persistence of trust state. Every method is one short transaction.
pub trait TrustLedger: Send + Sync {
    /// Inserts a grant, or replaces the row with the same id.
    fn grant_put(&self, g: &GrantRecord) -> Result<()>;
    /// Every grant, oldest first.
    fn grants(&self) -> Result<Vec<GrantRecord>>;
    /// Moves a grant to `state`; `true` when it existed in another state.
    fn grant_set_state(&self, id: &str, state: &str, at_ms: i64) -> Result<bool>;
    /// Marks an invite nonce spent. `true` when this call spent it, `false` when it was already
    /// spent: the check and the write are one atomic step.
    fn nonce_spend(&self, nonce: &str, at_ms: i64) -> Result<bool>;
    /// Inserts a peer or replaces the one with the same name.
    fn peer_put(&self, p: &PeerRecord) -> Result<()>;
    /// Every peer, by name.
    fn peers(&self) -> Result<Vec<PeerRecord>>;
    /// A small named value (the ticket epoch lives here).
    fn meta_get(&self, key: &str) -> Result<Option<String>>;
    /// Sets a small named value.
    fn meta_set(&self, key: &str, value: &str) -> Result<()>;
}

fn grant_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(GrantRecordRaw,)> {
    Ok((GrantRecordRaw {
        id: r.get(0)?,
        direction: r.get(1)?,
        peer_did: r.get(2)?,
        peer_node: r.get(3)?,
        role: r.get(4)?,
        nonce: r.get(5)?,
        iat: r.get(6)?,
        exp: r.get(7)?,
        token: r.get(8)?,
        state: r.get(9)?,
        state_at_ms: r.get(10)?,
    },))
}

struct GrantRecordRaw {
    id: String,
    direction: String,
    peer_did: String,
    peer_node: String,
    role: String,
    nonce: String,
    iat: i64,
    exp: i64,
    token: String,
    state: String,
    state_at_ms: i64,
}

impl GrantRecordRaw {
    fn finish(self) -> Result<GrantRecord> {
        Ok(GrantRecord {
            direction: GrantDirection::parse(&self.direction)?,
            id: self.id,
            peer_did: self.peer_did,
            peer_node: self.peer_node,
            role: self.role,
            nonce: self.nonce,
            iat: self.iat,
            exp: self.exp,
            token: self.token,
            state: self.state,
            state_at_ms: self.state_at_ms,
        })
    }
}

const GRANT_COLS: &str =
    "id, direction, peer_did, peer_node, role, nonce, iat, exp, token, state, state_at_ms";

impl TrustLedger for SqliteStore {
    fn grant_put(&self, g: &GrantRecord) -> Result<()> {
        self.write(|tx, _| {
            tx.execute(
                &format!(
                    "INSERT OR REPLACE INTO trust_grants({GRANT_COLS}) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)"
                ),
                params![
                    g.id,
                    g.direction.text(),
                    g.peer_did,
                    g.peer_node,
                    g.role,
                    g.nonce,
                    g.iat,
                    g.exp,
                    g.token,
                    g.state,
                    g.state_at_ms
                ],
            )?;
            Ok(())
        })
    }

    fn grants(&self) -> Result<Vec<GrantRecord>> {
        let conn = self.lock()?;
        let mut st = conn.prepare(&format!(
            "SELECT {GRANT_COLS} FROM trust_grants ORDER BY iat, id"
        ))?;
        let rows = st.query_map([], grant_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?.0.finish()?);
        }
        Ok(out)
    }

    fn grant_set_state(&self, id: &str, state: &str, at_ms: i64) -> Result<bool> {
        self.write(|tx, _| {
            let n = tx.execute(
                "UPDATE trust_grants SET state = ?2, state_at_ms = ?3 WHERE id = ?1 AND state <> ?2",
                params![id, state, at_ms],
            )?;
            Ok(n > 0)
        })
    }

    fn nonce_spend(&self, nonce: &str, at_ms: i64) -> Result<bool> {
        self.write(|tx, _| {
            let n = tx.execute(
                "INSERT OR IGNORE INTO trust_spent_nonces(nonce, spent_at_ms) VALUES (?1, ?2)",
                params![nonce, at_ms],
            )?;
            Ok(n > 0)
        })
    }

    fn peer_put(&self, p: &PeerRecord) -> Result<()> {
        let addrs = serde_json::to_string(&p.addrs)?;
        self.write(|tx, _| {
            tx.execute(
                "INSERT OR REPLACE INTO trust_peers(name, did, node_id, relay, addrs, paired_ms) \
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![p.name, p.did, p.node_id, p.relay, addrs, p.paired_ms],
            )?;
            Ok(())
        })
    }

    fn peers(&self) -> Result<Vec<PeerRecord>> {
        let conn = self.lock()?;
        let mut st = conn.prepare(
            "SELECT name, did, node_id, relay, addrs, paired_ms FROM trust_peers ORDER BY name",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (name, did, node_id, relay, addrs, paired_ms) = r?;
            out.push(PeerRecord {
                name,
                did,
                node_id,
                relay,
                addrs: serde_json::from_str(&addrs)?,
                paired_ms,
            });
        }
        Ok(out)
    }

    fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        Ok(conn
            .query_row(
                "SELECT value FROM trust_meta WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.write(|tx, _| {
            tx.execute(
                "INSERT OR REPLACE INTO trust_meta(key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
            Ok(())
        })
    }
}

#[derive(Default)]
struct MemTrust {
    grants: BTreeMap<String, GrantRecord>,
    nonces: BTreeSet<String>,
    peers: BTreeMap<String, PeerRecord>,
    meta: BTreeMap<String, String>,
}

/// An in-memory [`TrustLedger`] for tests and embedding.
#[derive(Default)]
pub struct MemoryTrust {
    inner: std::sync::Mutex<MemTrust>,
}

impl MemoryTrust {
    /// An empty ledger.
    pub fn new() -> MemoryTrust {
        MemoryTrust::default()
    }

    fn with<T>(&self, f: impl FnOnce(&mut MemTrust) -> T) -> Result<T> {
        let mut g = self.inner.lock().map_err(|_| LedgerError::Poisoned)?;
        Ok(f(&mut g))
    }
}

impl TrustLedger for MemoryTrust {
    fn grant_put(&self, g: &GrantRecord) -> Result<()> {
        self.with(|m| {
            m.grants.insert(g.id.clone(), g.clone());
        })
    }
    fn grants(&self) -> Result<Vec<GrantRecord>> {
        self.with(|m| {
            let mut v: Vec<GrantRecord> = m.grants.values().cloned().collect();
            v.sort_by(|a, b| (a.iat, &a.id).cmp(&(b.iat, &b.id)));
            v
        })
    }
    fn grant_set_state(&self, id: &str, state: &str, at_ms: i64) -> Result<bool> {
        self.with(|m| match m.grants.get_mut(id) {
            Some(g) if g.state != state => {
                g.state = state.to_string();
                g.state_at_ms = at_ms;
                true
            }
            _ => false,
        })
    }
    fn nonce_spend(&self, nonce: &str, _at_ms: i64) -> Result<bool> {
        self.with(|m| m.nonces.insert(nonce.to_string()))
    }
    fn peer_put(&self, p: &PeerRecord) -> Result<()> {
        self.with(|m| {
            m.peers.insert(p.name.clone(), p.clone());
        })
    }
    fn peers(&self) -> Result<Vec<PeerRecord>> {
        self.with(|m| m.peers.values().cloned().collect())
    }
    fn meta_get(&self, key: &str) -> Result<Option<String>> {
        self.with(|m| m.meta.get(key).cloned())
    }
    fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.with(|m| {
            m.meta.insert(key.to_string(), value.to_string());
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(id: &str, state: &str) -> GrantRecord {
        GrantRecord {
            id: id.into(),
            direction: GrantDirection::Issued,
            peer_did: "did:key:zPeer".into(),
            peer_node: "ab".repeat(32),
            role: "submit".into(),
            nonce: format!("n-{id}"),
            iat: 100,
            exp: 200,
            token: format!("g1.{id}"),
            state: state.into(),
            state_at_ms: 1,
        }
    }

    fn stores() -> Vec<(&'static str, Box<dyn TrustLedger>)> {
        vec![
            ("sqlite", Box::new(SqliteStore::open_in_memory().unwrap())),
            ("memory", Box::new(MemoryTrust::new())),
        ]
    }

    // R-235
    #[test]
    fn r235_grants_persist_and_revoking_deletes_nothing() {
        for (name, s) in stores() {
            s.grant_put(&grant("a", "live")).unwrap();
            let mut held = grant("b", "live");
            held.direction = GrantDirection::Held;
            s.grant_put(&held).unwrap();
            assert!(s.grant_set_state("a", "revoked", 9).unwrap(), "{name}");
            assert!(!s.grant_set_state("a", "revoked", 10).unwrap(), "{name}");
            assert!(!s.grant_set_state("zzz", "revoked", 10).unwrap(), "{name}");
            let all = s.grants().unwrap();
            assert_eq!(all.len(), 2, "{name}");
            let a = all.iter().find(|g| g.id == "a").unwrap();
            assert_eq!((a.state.as_str(), a.state_at_ms), ("revoked", 9), "{name}");
            assert_eq!(a.token, "g1.a");
            let b = all.iter().find(|g| g.id == "b").unwrap();
            assert_eq!(b.direction, GrantDirection::Held, "{name}");
        }
    }

    // R-236
    #[test]
    fn r236_a_nonce_is_spent_exactly_once() {
        for (name, s) in stores() {
            assert!(s.nonce_spend("n1", 1).unwrap(), "{name}");
            assert!(!s.nonce_spend("n1", 2).unwrap(), "{name}");
            assert!(s.nonce_spend("n2", 3).unwrap(), "{name}");
        }
    }

    // R-236
    #[test]
    fn r236_spent_nonces_survive_a_reopen() {
        let dir = std::env::temp_dir().join(format!("trust-reopen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("l.db");
        {
            let s = SqliteStore::open(&path).unwrap();
            assert!(s.nonce_spend("once", 1).unwrap());
            s.grant_put(&grant("g", "live")).unwrap();
            s.meta_set("ticket_epoch", "3").unwrap();
        }
        let s = SqliteStore::open(&path).unwrap();
        assert!(!s.nonce_spend("once", 2).unwrap());
        assert_eq!(s.grants().unwrap().len(), 1);
        assert_eq!(s.meta_get("ticket_epoch").unwrap().as_deref(), Some("3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R-237
    #[test]
    fn r237_peers_and_meta_round_trip() {
        for (name, s) in stores() {
            let p = PeerRecord {
                name: "nixos".into(),
                did: "did:key:zX".into(),
                node_id: "cd".repeat(32),
                relay: Some("https://relay.example/".into()),
                addrs: vec!["192.168.1.7:7843".into()],
                paired_ms: 5,
            };
            s.peer_put(&p).unwrap();
            let mut moved = p.clone();
            moved.addrs = vec!["10.0.0.2:1".into()];
            s.peer_put(&moved).unwrap();
            assert_eq!(s.peers().unwrap(), vec![moved], "{name}");
            assert_eq!(s.meta_get("k").unwrap(), None, "{name}");
            s.meta_set("k", "v").unwrap();
            s.meta_set("k", "w").unwrap();
            assert_eq!(s.meta_get("k").unwrap().as_deref(), Some("w"), "{name}");
        }
    }
}
