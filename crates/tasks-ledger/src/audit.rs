//! The audit chain: an append-only list of who did what, each row carrying the hash of the one
//! before it (design §8).
//!
//! [`AuditLedger`] is the storage trait (SQLite and in-memory implementations). Everything that
//! defines the chain lives here and is shared by both: the canonical encoding that is hashed
//! ([`seal`]), the checkpoint and prune records, and [`verify_chain`], which walks the rows and
//! reports the first break.
//!
//! What the chain proves. An edited row no longer matches its own hash; a deleted or reordered
//! row leaves a gap or a link that does not match. Whoever can rewrite every later row can still
//! forge an unsigned chain, which is why checkpoints are signed by the machine key: a forger
//! cannot produce a valid signature over the new head. Rows appended after the newest checkpoint
//! are protected by the chain alone, and removing rows from the very end is only noticed if a
//! later checkpoint exists. `signed_through` in the report says how far the signatures reach.

use crate::{LedgerError, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Mutex;
use tasks_identity::hex;

/// `prev_hash` of the first row ever written.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Action of a signed checkpoint row.
pub const ACTION_CHECKPOINT: &str = "audit.checkpoint";
/// Action of the row written before old rows are pruned.
pub const ACTION_PRUNED: &str = "audit.pruned";

const ROW_DOMAIN: &[u8] = b"tasks-audit/1\0";
const CHECKPOINT_DOMAIN: &[u8] = b"tasks-audit-checkpoint/1\0";
const PRUNE_DOMAIN: &[u8] = b"tasks-audit-prune/1\0";

/// A row to append. The store assigns the sequence number and the hashes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewAudit {
    /// When it happened, ms since the epoch.
    pub at_ms: i64,
    /// The machine that did it (its DID).
    pub actor: String,
    /// The agent or person the action was for or by; empty when none applies.
    pub owner: String,
    /// What happened (a dotted name such as `job.cancelled`; see `audit_log::action`).
    pub action: String,
    /// The job it concerns.
    pub job: Option<i64>,
    /// The other machine it concerns (its DID).
    pub peer: Option<String>,
    /// Anything else it names: a checkout, a path, a node id.
    pub target: Option<String>,
    /// A JSON object with the details, as stored text. Never a secret.
    pub details: String,
}

/// A stored row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    /// Position in the chain, from 1.
    pub seq: i64,
    /// When it happened, ms since the epoch.
    pub at_ms: i64,
    /// The machine that did it (its DID).
    pub actor: String,
    /// The agent or person; empty when none applies.
    pub owner: String,
    /// What happened.
    pub action: String,
    /// The job it concerns.
    pub job: Option<i64>,
    /// The other machine it concerns (its DID).
    pub peer: Option<String>,
    /// Anything else it names.
    pub target: Option<String>,
    /// JSON object text.
    pub details: String,
    /// Hash of the previous row ([`GENESIS_HASH`] for the first).
    pub prev_hash: String,
    /// Hash of this row.
    pub hash: String,
}

impl AuditRow {
    /// The details parsed, or `Null` when they are not valid JSON.
    pub fn details_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.details).unwrap_or(serde_json::Value::Null)
    }
}

/// Which rows to return, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuditQuery {
    /// At or after this time, ms.
    pub since_ms: Option<i64>,
    /// At or before this time, ms.
    pub until_ms: Option<i64>,
    /// Rows naming this DID as the peer or the actor.
    pub peer: Option<String>,
    /// Rows about this job.
    pub job: Option<i64>,
    /// This action, or every action under this prefix (`pair` matches `pair.ticket_minted`).
    pub action: Option<String>,
    /// Most rows returned.
    pub limit: usize,
}

impl AuditQuery {
    /// True when `row` passes every filter (the in-memory store and tests use this).
    pub fn matches(&self, row: &AuditRow) -> bool {
        self.since_ms.is_none_or(|t| row.at_ms >= t)
            && self.until_ms.is_none_or(|t| row.at_ms <= t)
            && self
                .peer
                .as_deref()
                .is_none_or(|p| row.peer.as_deref() == Some(p) || row.actor == p)
            && self.job.is_none_or(|j| row.job == Some(j))
            && self
                .action
                .as_deref()
                .is_none_or(|a| action_matches(a, &row.action))
    }
}

/// True when `action` is `filter` or sits under it (`filter` followed by a dot).
pub fn action_matches(filter: &str, action: &str) -> bool {
    action == filter
        || action
            .strip_prefix(filter)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// What a prune removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditPruned {
    /// Rows deleted.
    pub rows: u64,
    /// Sequence number of the last deleted row.
    pub through_seq: i64,
    /// Sequence number of the record written to account for them.
    pub record_seq: i64,
}

/// Storage of the audit chain. Appends are atomic with reading the head, so two writers can never
/// produce two rows with the same predecessor.
pub trait AuditLedger: Send + Sync {
    /// Appends a row after the current head.
    fn append(&self, row: &NewAudit) -> Result<AuditRow> {
        let row = row.clone();
        self.append_with(&move |_| Ok(row.clone()))
    }
    /// Appends the row `make` builds from the current head (`None` on an empty chain). Used for
    /// checkpoints, which sign the head they sit after.
    fn append_with(&self, make: &dyn Fn(Option<&AuditRow>) -> Result<NewAudit>)
    -> Result<AuditRow>;
    /// Rows matching the query, newest first.
    fn query(&self, q: &AuditQuery) -> Result<Vec<AuditRow>>;
    /// Up to `limit` rows with `seq` greater than `after_seq`, oldest first.
    fn scan(&self, after_seq: i64, limit: usize) -> Result<Vec<AuditRow>>;
    /// The newest row.
    fn head(&self) -> Result<Option<AuditRow>>;
    /// Sequence number of the newest checkpoint row.
    fn last_checkpoint_seq(&self) -> Result<Option<i64>>;
    /// Writes the record `make` builds from the last row that is old enough to go (given the
    /// number of rows going), then deletes every row up to and including it: one atomic step.
    /// Only a prefix is pruned: rows are removed from the front until the first row at or after
    /// `cutoff_ms`. `None` when nothing is old enough.
    fn prune_before(
        &self,
        cutoff_ms: i64,
        make: &dyn Fn(&AuditRow, u64) -> Result<NewAudit>,
    ) -> Result<Option<AuditPruned>>;
}

/// Appends `field` as a length-prefixed byte string.
fn put(buf: &mut Vec<u8>, field: &[u8]) {
    buf.extend_from_slice(&(field.len() as u64).to_be_bytes());
    buf.extend_from_slice(field);
}

fn put_opt(buf: &mut Vec<u8>, field: Option<&[u8]>) {
    match field {
        None => buf.push(0),
        Some(f) => {
            buf.push(1);
            put(buf, f);
        }
    }
}

/// The canonical bytes a row's hash covers: a domain tag, then every field in a fixed order, each
/// length-prefixed (so no two different rows encode alike), ending with `prev_hash`.
pub fn canonical(seq: i64, row: &NewAudit, prev_hash: &str) -> Vec<u8> {
    let mut b = ROW_DOMAIN.to_vec();
    b.extend_from_slice(&seq.to_be_bytes());
    b.extend_from_slice(&row.at_ms.to_be_bytes());
    put(&mut b, row.actor.as_bytes());
    put(&mut b, row.owner.as_bytes());
    put(&mut b, row.action.as_bytes());
    put_opt(
        &mut b,
        row.job.map(i64::to_be_bytes).as_ref().map(|a| a.as_slice()),
    );
    put_opt(&mut b, row.peer.as_deref().map(str::as_bytes));
    put_opt(&mut b, row.target.as_deref().map(str::as_bytes));
    put(&mut b, row.details.as_bytes());
    put(&mut b, prev_hash.as_bytes());
    b
}

/// Hex SHA-256 of the canonical bytes.
pub fn hash_of(seq: i64, row: &NewAudit, prev_hash: &str) -> String {
    hex::encode(&Sha256::digest(canonical(seq, row, prev_hash)))
}

/// Places `row` after `prev` (or first in the chain) and computes its hash.
pub fn seal(prev: Option<&AuditRow>, row: &NewAudit) -> AuditRow {
    let seq = prev.map_or(1, |p| p.seq.saturating_add(1));
    let prev_hash = prev.map_or_else(|| GENESIS_HASH.to_string(), |p| p.hash.clone());
    let hash = hash_of(seq, row, &prev_hash);
    AuditRow {
        seq,
        at_ms: row.at_ms,
        actor: row.actor.clone(),
        owner: row.owner.clone(),
        action: row.action.clone(),
        job: row.job,
        peer: row.peer.clone(),
        target: row.target.clone(),
        details: row.details.clone(),
        prev_hash,
        hash,
    }
}

fn recompute(row: &AuditRow) -> String {
    let new = NewAudit {
        at_ms: row.at_ms,
        actor: row.actor.clone(),
        owner: row.owner.clone(),
        action: row.action.clone(),
        job: row.job,
        peer: row.peer.clone(),
        target: row.target.clone(),
        details: row.details.clone(),
    };
    hash_of(row.seq, &new, &row.prev_hash)
}

/// The bytes a checkpoint signs: the head it covers.
pub fn checkpoint_message(head_seq: i64, head_hash: &str) -> Vec<u8> {
    let mut m = CHECKPOINT_DOMAIN.to_vec();
    m.extend_from_slice(&head_seq.to_be_bytes());
    m.extend_from_slice(head_hash.as_bytes());
    m
}

/// The bytes a prune record signs: the last row it removed.
pub fn prune_message(through_seq: i64, through_hash: &str) -> Vec<u8> {
    let mut m = PRUNE_DOMAIN.to_vec();
    m.extend_from_slice(&through_seq.to_be_bytes());
    m.extend_from_slice(through_hash.as_bytes());
    m
}

/// What went wrong in a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemKind {
    /// A row's content no longer matches its hash: it was edited.
    Edited,
    /// A row does not follow its predecessor: the link does not match.
    BrokenLink,
    /// Rows are missing between two that survive.
    Gap,
    /// A row sits where another belongs: the sequence runs backwards.
    Reordered,
    /// The oldest row's predecessor is missing and no prune record accounts for it.
    MissingHistory,
    /// A checkpoint or prune record is malformed or contradicts the chain.
    BadRecord,
    /// A checkpoint signature does not verify.
    BadSignature,
    /// A checkpoint is signed by a key other than this machine's.
    UnexpectedSigner,
}

impl ProblemKind {
    /// A stable name for the API and the CLI.
    pub fn name(self) -> &'static str {
        match self {
            ProblemKind::Edited => "edited",
            ProblemKind::BrokenLink => "broken_link",
            ProblemKind::Gap => "gap",
            ProblemKind::Reordered => "reordered",
            ProblemKind::MissingHistory => "missing_history",
            ProblemKind::BadRecord => "bad_record",
            ProblemKind::BadSignature => "bad_signature",
            ProblemKind::UnexpectedSigner => "unexpected_signer",
        }
    }
}

/// The first break found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainProblem {
    /// The row it was found at.
    pub seq: i64,
    /// What kind.
    pub kind: ProblemKind,
    /// A sentence for people.
    pub message: String,
}

/// The result of walking the chain.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChainReport {
    /// Rows checked.
    pub rows: u64,
    /// Oldest row seen.
    pub first_seq: Option<i64>,
    /// Newest row seen.
    pub last_seq: Option<i64>,
    /// Checkpoints whose signature verified.
    pub checkpoints: u64,
    /// The newest verified checkpoint's own row: everything before it is covered by a signature.
    pub signed_through: Option<i64>,
    /// Rows deleted by retention before the oldest surviving one.
    pub pruned_before: Option<i64>,
    /// The first break, if any.
    pub problem: Option<ChainProblem>,
}

impl ChainReport {
    /// True when no break was found.
    pub fn ok(&self) -> bool {
        self.problem.is_none()
    }
}

/// Checks a signature on behalf of [`verify_chain`].
pub trait SignatureCheck {
    /// True when `sig` (hex) over `message` verifies for the DID `signer`.
    fn check(&self, signer: &str, message: &[u8], sig: &str) -> SigVerdict;
}

/// The answer of a [`SignatureCheck`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigVerdict {
    /// Valid.
    Valid,
    /// Signed by a key that is not the expected one.
    WrongSigner,
    /// Does not verify.
    Invalid,
}

/// Verifies `did:key` Ed25519 signatures, pinned to `expected` when given.
pub struct DidSignatures<'a> {
    /// The machine whose key must have signed every checkpoint; `None` accepts any valid one.
    pub expected: Option<&'a str>,
}

impl SignatureCheck for DidSignatures<'_> {
    fn check(&self, signer: &str, message: &[u8], sig: &str) -> SigVerdict {
        if self.expected.is_some_and(|e| e != signer) {
            return SigVerdict::WrongSigner;
        }
        let (Some(public), Some(sig)) = (
            tasks_identity::public_key_from_did(signer),
            hex::decode::<64>(sig),
        ) else {
            return SigVerdict::Invalid;
        };
        if tasks_identity::verify(&public, message, &sig) {
            SigVerdict::Valid
        } else {
            SigVerdict::Invalid
        }
    }
}

fn problem(seq: i64, kind: ProblemKind, message: String) -> Option<ChainProblem> {
    Some(ChainProblem { seq, kind, message })
}

/// Rows read per query while verifying.
const SCAN_BATCH: usize = 500;

/// Walks the whole chain from the oldest surviving row and reports the first break.
pub fn verify_chain(store: &dyn AuditLedger, sigs: &dyn SignatureCheck) -> Result<ChainReport> {
    let mut rep = ChainReport::default();
    let mut after = 0i64;
    let mut prev: Option<AuditRow> = None;
    // (seq, prev_hash) of the oldest row when it does not start the chain
    let mut anchor: Option<(i64, String)> = None;
    // prune records seen: through_seq -> through_hash
    let mut prunes: BTreeMap<i64, String> = BTreeMap::new();
    loop {
        let batch = store.scan(after, SCAN_BATCH)?;
        if batch.is_empty() {
            break;
        }
        for row in batch {
            after = row.seq;
            if let Some(p) = check_row(&row, prev.as_ref(), sigs, &mut rep, &mut prunes) {
                rep.problem = Some(p);
                return Ok(rep);
            }
            if prev.is_none() && row.seq != 1 {
                anchor = Some((row.seq, row.prev_hash.clone()));
                rep.pruned_before = Some(row.seq);
            }
            rep.rows += 1;
            rep.first_seq.get_or_insert(row.seq);
            rep.last_seq = Some(row.seq);
            prev = Some(row);
        }
    }
    if let Some((seq, prev_hash)) = anchor {
        let accounted = prunes
            .get(&seq.saturating_sub(1))
            .is_some_and(|h| *h == prev_hash);
        if !accounted {
            rep.problem = problem(
                seq,
                ProblemKind::MissingHistory,
                format!("rows before {seq} are missing and no prune record accounts for them"),
            );
        }
    }
    Ok(rep)
}

fn check_row(
    row: &AuditRow,
    prev: Option<&AuditRow>,
    sigs: &dyn SignatureCheck,
    rep: &mut ChainReport,
    prunes: &mut BTreeMap<i64, String>,
) -> Option<ChainProblem> {
    let seq = row.seq;
    match prev {
        None if seq == 1 => {
            if row.prev_hash != GENESIS_HASH {
                return problem(
                    seq,
                    ProblemKind::BrokenLink,
                    "the first row does not start from the genesis hash".to_string(),
                );
            }
        }
        None => {}
        Some(p) => {
            if seq < p.seq.saturating_add(1) {
                return problem(
                    seq,
                    ProblemKind::Reordered,
                    format!("row {seq} follows row {}: the order was changed", p.seq),
                );
            }
            if seq > p.seq.saturating_add(1) {
                return problem(
                    seq,
                    ProblemKind::Gap,
                    format!(
                        "rows {}..{} are missing between row {} and row {seq}",
                        p.seq.saturating_add(1),
                        seq.saturating_sub(1),
                        p.seq
                    ),
                );
            }
        }
    }
    if recompute(row) != row.hash {
        return problem(
            seq,
            ProblemKind::Edited,
            format!("row {seq} no longer matches its hash: it was edited"),
        );
    }
    if let Some(p) = prev
        && row.prev_hash != p.hash
    {
        return problem(
            seq,
            ProblemKind::BrokenLink,
            format!(
                "row {seq} does not follow row {}: its prev_hash differs",
                p.seq
            ),
        );
    }
    match row.action.as_str() {
        ACTION_CHECKPOINT => check_checkpoint(row, sigs, rep),
        ACTION_PRUNED => check_prune(row, sigs, prunes),
        _ => None,
    }
}

fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(serde_json::Value::as_str)
}

fn check_checkpoint(
    row: &AuditRow,
    sigs: &dyn SignatureCheck,
    rep: &mut ChainReport,
) -> Option<ChainProblem> {
    let d = row.details_json();
    let bad = |m: &str| problem(row.seq, ProblemKind::BadRecord, m.to_string());
    let head_seq = d.get("head_seq").and_then(serde_json::Value::as_i64);
    let Some(head_hash) = str_field(&d, "head_hash") else {
        return bad("the checkpoint names no head hash");
    };
    if head_hash != row.prev_hash || head_seq != Some(row.seq.saturating_sub(1)) {
        return bad("the checkpoint does not describe the row before it");
    }
    let (Some(signer), Some(sig)) = (str_field(&d, "signer"), str_field(&d, "sig")) else {
        // written by a daemon without a key: it vouches for nothing
        return None;
    };
    let head = row.seq.saturating_sub(1);
    match sigs.check(signer, &checkpoint_message(head, head_hash), sig) {
        SigVerdict::Valid => {
            rep.checkpoints += 1;
            rep.signed_through = Some(row.seq);
            None
        }
        SigVerdict::WrongSigner => problem(
            row.seq,
            ProblemKind::UnexpectedSigner,
            format!(
                "checkpoint {} is signed by {signer}, not by this machine",
                row.seq
            ),
        ),
        SigVerdict::Invalid => problem(
            row.seq,
            ProblemKind::BadSignature,
            format!("the signature of checkpoint {} does not verify", row.seq),
        ),
    }
}

fn check_prune(
    row: &AuditRow,
    sigs: &dyn SignatureCheck,
    prunes: &mut BTreeMap<i64, String>,
) -> Option<ChainProblem> {
    let d = row.details_json();
    let through = d.get("through_seq").and_then(serde_json::Value::as_i64);
    let (Some(through), Some(hash)) = (through, str_field(&d, "through_hash")) else {
        return problem(
            row.seq,
            ProblemKind::BadRecord,
            "the prune record names no last pruned row".to_string(),
        );
    };
    if let (Some(signer), Some(sig)) = (str_field(&d, "signer"), str_field(&d, "sig")) {
        match sigs.check(signer, &prune_message(through, hash), sig) {
            SigVerdict::Valid => {}
            SigVerdict::WrongSigner => {
                return problem(
                    row.seq,
                    ProblemKind::UnexpectedSigner,
                    format!(
                        "prune record {} is signed by {signer}, not by this machine",
                        row.seq
                    ),
                );
            }
            SigVerdict::Invalid => {
                return problem(
                    row.seq,
                    ProblemKind::BadSignature,
                    format!("the signature of prune record {} does not verify", row.seq),
                );
            }
        }
    }
    prunes.insert(through, hash.to_string());
    None
}

#[derive(Default)]
struct MemAudit {
    rows: Vec<AuditRow>,
}

/// An in-memory [`AuditLedger`] for tests and embedding. Also lets tests tamper with rows.
#[derive(Default)]
pub struct MemoryAudit {
    inner: Mutex<MemAudit>,
}

impl MemoryAudit {
    /// An empty chain.
    pub fn new() -> MemoryAudit {
        MemoryAudit::default()
    }

    fn with<T>(&self, f: impl FnOnce(&mut MemAudit) -> T) -> Result<T> {
        let mut g = self.inner.lock().map_err(|_| LedgerError::Poisoned)?;
        Ok(f(&mut g))
    }

    /// Changes the stored rows without touching their hashes, as an attacker with the file would.
    /// For tests.
    pub fn tamper(&self, f: impl FnOnce(&mut Vec<AuditRow>)) {
        let _ = self.with(|m| f(&mut m.rows));
    }
}

impl AuditLedger for MemoryAudit {
    fn append_with(
        &self,
        make: &dyn Fn(Option<&AuditRow>) -> Result<NewAudit>,
    ) -> Result<AuditRow> {
        let mut g = self.inner.lock().map_err(|_| LedgerError::Poisoned)?;
        let new = make(g.rows.last())?;
        let row = seal(g.rows.last(), &new);
        g.rows.push(row.clone());
        Ok(row)
    }

    fn query(&self, q: &AuditQuery) -> Result<Vec<AuditRow>> {
        self.with(|m| {
            m.rows
                .iter()
                .rev()
                .filter(|r| q.matches(r))
                .take(q.limit)
                .cloned()
                .collect()
        })
    }

    fn scan(&self, after_seq: i64, limit: usize) -> Result<Vec<AuditRow>> {
        self.with(|m| {
            m.rows
                .iter()
                .filter(|r| r.seq > after_seq)
                .take(limit)
                .cloned()
                .collect()
        })
    }

    fn head(&self) -> Result<Option<AuditRow>> {
        self.with(|m| m.rows.last().cloned())
    }

    fn last_checkpoint_seq(&self) -> Result<Option<i64>> {
        self.with(|m| {
            m.rows
                .iter()
                .rev()
                .find(|r| r.action == ACTION_CHECKPOINT)
                .map(|r| r.seq)
        })
    }

    fn prune_before(
        &self,
        cutoff_ms: i64,
        make: &dyn Fn(&AuditRow, u64) -> Result<NewAudit>,
    ) -> Result<Option<AuditPruned>> {
        let mut g = self.inner.lock().map_err(|_| LedgerError::Poisoned)?;
        let n = g.rows.iter().take_while(|r| r.at_ms < cutoff_ms).count();
        let Some(last) = n.checked_sub(1).and_then(|i| g.rows.get(i)).cloned() else {
            return Ok(None);
        };
        let new = make(&last, n as u64)?;
        let record = seal(g.rows.last(), &new);
        let record_seq = record.seq;
        g.rows.push(record);
        g.rows.drain(..n);
        Ok(Some(AuditPruned {
            rows: n as u64,
            through_seq: last.seq,
            record_seq,
        }))
    }
}
