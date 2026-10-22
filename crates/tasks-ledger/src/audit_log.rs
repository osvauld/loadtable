//! The audit writer: builds rows from events, keeps secrets out of them, signs checkpoints with
//! the machine key and applies retention. [`AuditLog`] is what the daemon and the trust layer
//! hold; the chain itself is in [`crate::audit`].
//!
//! Actions are plain dotted strings with a known set ([`action::KNOWN`]) rather than an enum:
//! the audit table outlives any one build, a newer daemon's rows must still be readable by an
//! older CLI, and the data-transfer and pressure steps add actions without a schema or API
//! change. The known set is what the UI offers as a filter; an unknown action is shown as is.

use crate::audit::{
    ACTION_CHECKPOINT, ACTION_PRUNED, AuditLedger, AuditPruned, AuditQuery, AuditRow, ChainReport,
    DidSignatures, NewAudit, checkpoint_message, prune_message, verify_chain,
};
use crate::{Clock, LedgerError, Result};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tasks_identity::{Signer, hex};

/// The action names this build writes.
pub mod action {
    /// A pairing ticket was minted.
    pub const TICKET_MINTED: &str = "pair.ticket_minted";
    /// A pairing ticket was redeemed.
    pub const TICKET_REDEEMED: &str = "pair.ticket_redeemed";
    /// A redemption was refused or failed.
    pub const TICKET_REFUSED: &str = "pair.ticket_refused";
    /// Unredeemed tickets were invalidated.
    pub const TICKETS_RESET: &str = "pair.tickets_reset";
    /// A grant was issued or accepted.
    pub const GRANT_ISSUED: &str = "grant.issued";
    /// A grant was renewed.
    pub const GRANT_RENEWED: &str = "grant.renewed";
    /// A grant was revoked.
    pub const GRANT_REVOKED: &str = "grant.revoked";
    /// A connection or request was refused.
    pub const CONN_REFUSED: &str = "conn.refused";
    /// A peer link came up.
    pub const CONN_UP: &str = "conn.up";
    /// A peer link went down.
    pub const CONN_DOWN: &str = "conn.down";
    /// A peer link moved between a direct path and the relay.
    pub const CONN_PATH: &str = "conn.path";
    /// A job was submitted here.
    pub const JOB_SUBMITTED: &str = "job.submitted";
    /// A job was forwarded to a peer.
    pub const JOB_FORWARDED: &str = "job.forwarded";
    /// A job arrived from a peer.
    pub const JOB_RECEIVED: &str = "job.received";
    /// A job started.
    pub const JOB_STARTED: &str = "job.started";
    /// A job ended.
    pub const JOB_ENDED: &str = "job.ended";
    /// A job was cancelled, or a cancel was requested of a running one.
    pub const JOB_CANCELLED: &str = "job.cancelled";
    /// A job hit its wall or idle limit.
    pub const JOB_TIMEOUT: &str = "job.timeout";
    /// A submit rule refused a job.
    pub const JOB_REFUSED: &str = "job.refused";
    /// A checkout snapshot was sent to a peer (tree id, bytes).
    pub const DATA_SYNC_SENT: &str = "data.sync_sent";
    /// A checkout snapshot was received from a peer and applied.
    pub const DATA_SYNC_RECEIVED: &str = "data.sync_received";
    /// A sync was refused before anything was sent, naming the files and why.
    pub const DATA_SYNC_REFUSED: &str = "data.sync_refused";
    /// Sync data was deleted by retention (what, bytes freed).
    pub const DATA_RETENTION: &str = "data.retention";
    /// The queue was drained.
    pub const QUEUE_DRAIN: &str = "queue.drain";
    /// A drain was lifted.
    pub const QUEUE_RESUME: &str = "queue.resume";

    /// Every action above plus the chain's own two, for filters and help text.
    pub const KNOWN: &[&str] = &[
        TICKET_MINTED,
        TICKET_REDEEMED,
        TICKET_REFUSED,
        TICKETS_RESET,
        GRANT_ISSUED,
        GRANT_RENEWED,
        GRANT_REVOKED,
        CONN_REFUSED,
        CONN_UP,
        CONN_DOWN,
        CONN_PATH,
        JOB_SUBMITTED,
        JOB_FORWARDED,
        JOB_RECEIVED,
        JOB_STARTED,
        JOB_ENDED,
        JOB_CANCELLED,
        JOB_TIMEOUT,
        JOB_REFUSED,
        DATA_SYNC_SENT,
        DATA_SYNC_RECEIVED,
        DATA_SYNC_REFUSED,
        DATA_RETENTION,
        QUEUE_DRAIN,
        QUEUE_RESUME,
        crate::audit::ACTION_CHECKPOINT,
        crate::audit::ACTION_PRUNED,
    ];
}

/// Largest details text stored, in bytes. A bigger one is replaced by a note saying so.
pub const MAX_DETAILS_BYTES: usize = 4096;
/// Longest string kept inside details, in characters.
const MAX_DETAIL_CHARS: usize = 500;

/// Words that mark a detail key as secret. Such a value is never stored.
const SECRET_KEYS: &[&str] = &[
    "token",
    "secret",
    "mnemonic",
    "password",
    "passphrase",
    "private",
    "seed",
    "signing_key",
    "api_key",
    "credential",
];
/// Text forms of the signed tokens and tickets; a value that starts like one is never stored.
const SECRET_PREFIXES: &[&str] = &["g1.", "i1.", "tasks1."];

/// One thing to record.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEvent {
    /// What happened; see [`action`].
    pub action: String,
    /// The machine that did it, when not this one (a peer's DID).
    pub actor: Option<String>,
    /// The agent or person.
    pub owner: Option<String>,
    /// The job.
    pub job: Option<i64>,
    /// The other machine (its DID).
    pub peer: Option<String>,
    /// A checkout, path, node id.
    pub target: Option<String>,
    details: Map<String, Value>,
}

impl AuditEvent {
    /// An event of `action` with nothing else set.
    pub fn new(action: &str) -> AuditEvent {
        AuditEvent {
            action: action.to_string(),
            actor: None,
            owner: None,
            job: None,
            peer: None,
            target: None,
            details: Map::new(),
        }
    }

    /// Done by another machine.
    pub fn by_actor(mut self, did: impl Into<String>) -> AuditEvent {
        self.actor = Some(did.into());
        self
    }

    /// For or by this agent or person.
    pub fn owner(mut self, owner: impl Into<String>) -> AuditEvent {
        self.owner = Some(owner.into());
        self
    }

    /// About this job.
    pub fn job(mut self, id: i64) -> AuditEvent {
        self.job = Some(id);
        self
    }

    /// About this peer (its DID).
    pub fn peer(mut self, did: impl Into<String>) -> AuditEvent {
        self.peer = Some(did.into());
        self
    }

    /// Naming this checkout, path or node id.
    pub fn target(mut self, target: impl Into<String>) -> AuditEvent {
        self.target = Some(target.into());
        self
    }

    /// Adds one detail.
    pub fn detail(mut self, key: &str, value: impl Into<Value>) -> AuditEvent {
        self.details.insert(key.to_string(), value.into());
        self
    }

    /// Adds a detail when there is one.
    pub fn detail_opt<V: Into<Value>>(self, key: &str, value: Option<V>) -> AuditEvent {
        match value {
            Some(v) => self.detail(key, v),
            None => self,
        }
    }
}

fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|w| k.contains(w))
}

/// Replaces every word that starts like a signed token or ticket.
fn redact_words(s: String) -> String {
    if !SECRET_PREFIXES.iter().any(|p| s.contains(p)) {
        return s;
    }
    s.split(' ')
        .map(|w| {
            let bare = w.trim_start_matches(['"', '\'', '(', '`', ':', '=']);
            if SECRET_PREFIXES.iter().any(|p| bare.starts_with(p)) {
                "[redacted]"
            } else {
                w
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn scrub_value(v: Value) -> Value {
    match v {
        Value::String(s) => {
            let s = redact_words(s);
            if s.chars().count() > MAX_DETAIL_CHARS {
                let head: String = s.chars().take(MAX_DETAIL_CHARS).collect();
                Value::String(format!("{head}..."))
            } else {
                Value::String(s)
            }
        }
        Value::Array(a) => Value::Array(a.into_iter().map(scrub_value).collect()),
        Value::Object(o) => Value::Object(scrub_map(o)),
        other => other,
    }
}

fn scrub_map(m: Map<String, Value>) -> Map<String, Value> {
    m.into_iter()
        .map(|(k, v)| {
            let v = if is_secret_key(&k) {
                Value::String("[redacted]".to_string())
            } else {
                scrub_value(v)
            };
            (k, v)
        })
        .collect()
}

/// The details as stored text: secrets removed, long strings cut, the whole bounded.
fn details_text(details: Map<String, Value>) -> String {
    let text = serde_json::to_string(&Value::Object(scrub_map(details))).unwrap_or_default();
    if text.len() <= MAX_DETAILS_BYTES {
        return text;
    }
    json!({"truncated": true, "bytes": text.len()}).to_string()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Writes the audit chain for one machine.
pub struct AuditLog {
    store: Arc<dyn AuditLedger>,
    clock: Arc<dyn Clock>,
    actor: String,
    signer: Option<Arc<dyn Signer>>,
    checkpoint_every: AtomicU64,
    pending: AtomicU64,
    last_error: Mutex<Option<String>>,
}

/// Rows between checkpoints unless set otherwise.
pub const DEFAULT_CHECKPOINT_EVERY: u64 = 100;

impl AuditLog {
    /// A log over `store`. `actor` is this machine's DID.
    pub fn new(
        store: Arc<dyn AuditLedger>,
        clock: Arc<dyn Clock>,
        actor: impl Into<String>,
    ) -> AuditLog {
        let since = match (store.head(), store.last_checkpoint_seq()) {
            (Ok(Some(h)), Ok(c)) => h.seq.saturating_sub(c.unwrap_or(0)),
            _ => 0,
        };
        AuditLog {
            store,
            clock,
            actor: actor.into(),
            signer: None,
            checkpoint_every: AtomicU64::new(DEFAULT_CHECKPOINT_EVERY),
            pending: AtomicU64::new(u64::try_from(since).unwrap_or(0)),
            last_error: Mutex::new(None),
        }
    }

    /// Signs checkpoints with `signer`; its DID becomes the actor of rows this machine writes.
    pub fn with_signer(mut self, signer: Arc<dyn Signer>) -> AuditLog {
        self.actor = signer.did().to_string();
        self.signer = Some(signer);
        self
    }

    /// Rows between checkpoints; 0 turns automatic checkpoints off.
    pub fn set_checkpoint_every(&self, n: u64) {
        self.checkpoint_every.store(n, Ordering::SeqCst);
    }

    /// This machine's DID, the actor of its own rows.
    pub fn actor(&self) -> &str {
        &self.actor
    }

    /// The storage, for readers.
    pub fn store(&self) -> &Arc<dyn AuditLedger> {
        &self.store
    }

    /// The last write error, if any. A failed audit write never fails the action it describes;
    /// the daemon shows this instead.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.last_error).clone()
    }

    fn note(&self, e: &LedgerError) {
        *lock(&self.last_error) = Some(e.to_string());
    }

    /// Records an event. Never fails the caller: an error is kept in [`AuditLog::last_error`].
    pub fn record(&self, ev: AuditEvent) {
        if let Err(e) = self.try_record(ev) {
            self.note(&e);
        }
    }

    /// Records an event and returns the row.
    pub fn try_record(&self, ev: AuditEvent) -> Result<AuditRow> {
        let new = NewAudit {
            at_ms: self.clock.now_ms(),
            actor: ev.actor.unwrap_or_else(|| self.actor.clone()),
            owner: ev.owner.unwrap_or_default(),
            action: ev.action,
            job: ev.job,
            peer: ev.peer,
            target: ev.target,
            details: details_text(ev.details),
        };
        let row = self.store.append(&new)?;
        let every = self.checkpoint_every.load(Ordering::SeqCst);
        let pending = self.pending.fetch_add(1, Ordering::SeqCst) + 1;
        if every > 0 && pending >= every {
            self.checkpoint()?;
        }
        Ok(row)
    }

    /// Signs the head of the chain into a checkpoint row now. `None` on an empty chain.
    pub fn checkpoint(&self) -> Result<Option<AuditRow>> {
        if self.store.head()?.is_none() {
            return Ok(None);
        }
        let row = self.store.append_with(&|head| {
            let head = head.ok_or(LedgerError::OutOfRange("checkpoint on an empty chain"))?;
            let mut d = Map::new();
            d.insert("head_seq".into(), head.seq.into());
            d.insert("head_hash".into(), head.hash.clone().into());
            if let Some(s) = &self.signer {
                let sig = s.sign(&checkpoint_message(head.seq, &head.hash));
                d.insert("signer".into(), s.did().into());
                d.insert("sig".into(), hex::encode(&sig).into());
            }
            Ok(NewAudit {
                at_ms: self.clock.now_ms(),
                actor: self.actor.clone(),
                action: ACTION_CHECKPOINT.to_string(),
                details: Value::Object(d).to_string(),
                ..NewAudit::default()
            })
        })?;
        self.pending.store(0, Ordering::SeqCst);
        Ok(Some(row))
    }

    /// Deletes rows older than `retention`, first writing the record that carries the hash of
    /// the last row deleted, so [`AuditLog::verify`] still works from the oldest survivor.
    pub fn prune(&self, retention: Duration) -> Result<Option<AuditPruned>> {
        let cutoff = self
            .clock
            .now_ms()
            .saturating_sub(i64::try_from(retention.as_millis()).unwrap_or(i64::MAX));
        self.store.prune_before(cutoff, &|last, rows| {
            let mut d = Map::new();
            d.insert("through_seq".into(), last.seq.into());
            d.insert("through_hash".into(), last.hash.clone().into());
            d.insert("through_ms".into(), last.at_ms.into());
            d.insert("rows".into(), rows.into());
            if let Some(s) = &self.signer {
                let sig = s.sign(&prune_message(last.seq, &last.hash));
                d.insert("signer".into(), s.did().into());
                d.insert("sig".into(), hex::encode(&sig).into());
            }
            Ok(NewAudit {
                at_ms: self.clock.now_ms(),
                actor: self.actor.clone(),
                action: ACTION_PRUNED.to_string(),
                details: Value::Object(d).to_string(),
                ..NewAudit::default()
            })
        })
    }

    /// Walks the chain and checks every signature against this machine's key.
    pub fn verify(&self) -> Result<ChainReport> {
        let sigs = DidSignatures {
            expected: Some(self.actor.as_str()),
        };
        verify_chain(&*self.store, &sigs)
    }

    /// Rows matching the query, newest first.
    pub fn query(&self, q: &AuditQuery) -> Result<Vec<AuditRow>> {
        self.store.query(q)
    }
}

/// Lets a repeating event through a few times per window and counts the rest, so a flapping link
/// or a refused peer retrying every few seconds cannot flood the chain. Keyed by what repeats.
pub struct Throttle {
    window_ms: i64,
    burst: u32,
    keys: Mutex<BTreeMap<String, Slot>>,
}

#[derive(Default)]
struct Slot {
    window_start: i64,
    admitted: u32,
    suppressed: u64,
}

impl Throttle {
    /// At most `burst` events per `window_ms` for each key.
    pub fn new(window_ms: i64, burst: u32) -> Throttle {
        Throttle {
            window_ms,
            burst,
            keys: Mutex::new(BTreeMap::new()),
        }
    }

    /// `Some(n)` when the event may be recorded now, `n` being how many of the same key were
    /// dropped since the last one that was; `None` when it is dropped.
    pub fn admit(&self, key: &str, now_ms: i64) -> Option<u64> {
        let mut keys = lock(&self.keys);
        // keys that stayed quiet for a whole window are forgotten, so the map stays small
        if keys.len() > 1024 {
            keys.retain(|_, s| now_ms - s.window_start < self.window_ms);
        }
        let s = keys.entry(key.to_string()).or_insert_with(|| Slot {
            window_start: now_ms,
            ..Slot::default()
        });
        if now_ms - s.window_start >= self.window_ms {
            s.window_start = now_ms;
            s.admitted = 0;
        }
        if s.admitted < self.burst {
            s.admitted += 1;
            Some(std::mem::take(&mut s.suppressed))
        } else {
            s.suppressed += 1;
            None
        }
    }
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
