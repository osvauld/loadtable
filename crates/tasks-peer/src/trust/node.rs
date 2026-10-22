//! [`TrustNode`]: this machine's side of trust. It mints and checks grants, runs the invite and
//! redeem steps, keeps the live sessions and closes the connections of a revoked grant.
//!
//! State is mirrored in memory (loaded from the ledger at start, written through on every
//! change) so a request is checked without touching SQLite. One daemon owns the ledger.

use super::error::TrustError;
use super::events::{Side, TrustEvent, TrustEvents};
use super::role::{Need, Role, allows};
use super::ticket::Ticket;
use super::token::{
    Binds, GRANT_PREFIX, Grant, GrantClaims, Invite, InviteClaims, SCOPE_MACHINE, TOKEN_VERSION,
};
use super::wire::{PairDone, PairRedeem, PresentAck};
use crate::ids::PeerId;
use crate::proto::{Body, Refusal};
use crate::transport::{Closer, Connection, DialHints, Transport};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tasks_identity::{Signer, hex, public_key_from_did, random_bytes, verify};
use tasks_ledger::{Clock, GrantDirection, GrantRecord, PeerRecord, TrustLedger};

const PAIR_DOMAIN: &[u8] = b"tasks/pair/v1\0";
const MAX_PRESENTED: usize = 16;
const MAX_CHALLENGES: usize = 64;
const STATE_LIVE: &str = "live";
const STATE_REVOKED: &str = "revoked";
const STATE_RENEWED: &str = "renewed";
const META_EPOCH: &str = "ticket_epoch";

/// Durations the trust layer works to. [`TrustPolicy::default`] gives the decided values:
/// grants live a year, tickets 15 minutes, renewal starts 30 days out, alerts 7 days out.
#[derive(Debug, Clone)]
pub struct TrustPolicy {
    /// How long a new or renewed grant lives.
    pub grant_ttl_secs: u64,
    /// The default ticket lifetime.
    pub ticket_ttl_secs: u64,
    /// The longest ticket lifetime accepted.
    pub max_ticket_ttl_secs: u64,
    /// A held grant with less than this left is renewed on the next connection.
    pub renew_before_secs: u64,
    /// A grant with less than this left raises an alert.
    pub alert_before_secs: u64,
    /// How long a pairing challenge stays valid.
    pub challenge_ttl_secs: u64,
}

impl Default for TrustPolicy {
    fn default() -> TrustPolicy {
        TrustPolicy {
            grant_ttl_secs: 365 * 86_400,
            ticket_ttl_secs: 15 * 60,
            max_ticket_ttl_secs: 24 * 3600,
            renew_before_secs: 30 * 86_400,
            alert_before_secs: 7 * 86_400,
            challenge_ttl_secs: 120,
        }
    }
}

/// How the other machine can reach this one; goes into tickets and redemptions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelfAddr {
    /// The relay this endpoint uses.
    pub relay: Option<String>,
    /// Direct socket addresses.
    pub direct_addrs: Vec<String>,
}

/// Where tickets and redemptions get this machine's reachability from.
pub trait AddrSource: Send + Sync {
    /// The current addresses.
    fn current(&self) -> SelfAddr;
}

/// A fixed answer, for tests and for hosts without an endpoint.
impl AddrSource for SelfAddr {
    fn current(&self) -> SelfAddr {
        self.clone()
    }
}

/// Who a dialed address belongs to, for the transport.
pub trait AddressBook: Send + Sync {
    /// What is known about reaching `peer`, or `None` when it is not paired.
    fn hints(&self, peer: &PeerId) -> Option<DialHints>;
}

#[derive(Clone)]
struct LiveGrant {
    id: String,
    role: Role,
    exp: u64,
}

struct Conn {
    id: u64,
    closer: Closer,
    closed: bool,
}

#[derive(Default)]
struct Session {
    did: String,
    grants: Vec<LiveGrant>,
    conns: Vec<Conn>,
    announced: bool,
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<String, Session>,
    challenges: BTreeMap<String, ([u8; 32], u64)>,
    next_conn: u64,
    grants: BTreeMap<String, GrantRecord>,
    peers: BTreeMap<String, PeerRecord>,
    spent: BTreeSet<String>,
    epoch: u64,
}

/// A role a peer holds and when it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleExpiry {
    /// The role.
    pub role: Role,
    /// Expiry, unix seconds.
    pub exp: u64,
    /// The grant's id.
    pub grant: String,
}

/// One paired peer as `tasks2 peers` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerTrust {
    /// The name placement and `--peer` use.
    pub name: String,
    /// Its DID.
    pub did: String,
    /// Its node id.
    pub node_id: String,
    /// What this machine may do there (grants held).
    pub outbound: Vec<RoleExpiry>,
    /// What the peer may do here (grants issued).
    pub inbound: Vec<RoleExpiry>,
    /// The earliest expiry among live grants, either way.
    pub expires: Option<u64>,
    /// True when no live grant remains either way.
    pub revoked: bool,
    /// When it was paired, ms since the epoch.
    pub paired_ms: i64,
}

/// A paired peer a forwarder may dial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedPeer {
    /// The name.
    pub name: String,
    /// Its DID.
    pub did: String,
    /// Its node id.
    pub node_id: String,
}

/// What a revoke did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeReport {
    /// The peer.
    pub name: String,
    /// Grants revoked.
    pub revoked: usize,
    /// Live connections closed.
    pub closed: usize,
}

/// A grant close to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiringGrant {
    /// The peer's name.
    pub peer: String,
    /// `issued` or `held`.
    pub direction: &'static str,
    /// The role.
    pub role: Role,
    /// Expiry, unix seconds.
    pub exp: u64,
    /// Seconds left; negative once expired.
    pub left_secs: i64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn dir_name(d: GrantDirection) -> &'static str {
    match d {
        GrantDirection::Issued => "issued",
        GrantDirection::Held => "held",
    }
}

/// A peer name safe to use as a label and a directory: letters, digits, `.`, `-`, `_`.
pub fn sanitize_name(raw: &str) -> String {
    let s: String = raw
        .trim()
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() || s.chars().all(|c| c == '.') {
        "peer".to_string()
    } else {
        s
    }
}

pub(super) fn pair_message(challenge: &[u8; 32], nonce: &str, node_id: &str) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(PAIR_DOMAIN);
    m.extend_from_slice(challenge);
    let n = u16::try_from(nonce.len()).unwrap_or(u16::MAX);
    m.extend_from_slice(&n.to_be_bytes());
    m.extend_from_slice(nonce.as_bytes());
    m.extend_from_slice(node_id.as_bytes());
    m
}

/// This machine's trust state and rules.
pub struct TrustNode {
    signer: Arc<dyn Signer>,
    name: String,
    ledger: Arc<dyn TrustLedger>,
    events: Arc<dyn TrustEvents>,
    clock: Arc<dyn Clock>,
    policy: TrustPolicy,
    state: Mutex<State>,
}

impl TrustNode {
    /// Loads the ledger's trust state. `name` is this machine's label.
    pub fn new(
        signer: Arc<dyn Signer>,
        name: &str,
        ledger: Arc<dyn TrustLedger>,
        events: Arc<dyn TrustEvents>,
        clock: Arc<dyn Clock>,
        policy: TrustPolicy,
    ) -> Result<Arc<TrustNode>, TrustError> {
        let mut st = State::default();
        for g in ledger.grants()? {
            st.grants.insert(g.id.clone(), g);
        }
        for p in ledger.peers()? {
            st.peers.insert(p.name.clone(), p);
        }
        st.epoch = ledger
            .meta_get(META_EPOCH)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        Ok(Arc::new(TrustNode {
            signer,
            name: sanitize_name(name),
            ledger,
            events,
            clock,
            policy,
            state: Mutex::new(st),
        }))
    }

    /// This machine's DID.
    pub fn did(&self) -> &str {
        self.signer.did()
    }

    /// This machine's node id: the device key, lowercase hex.
    pub fn node_id(&self) -> String {
        hex::encode(&self.signer.device_public_key())
    }

    /// This machine's label.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The durations in force.
    pub fn policy(&self) -> &TrustPolicy {
        &self.policy
    }

    fn now(&self) -> u64 {
        u64::try_from(self.clock.now_ms() / 1000).unwrap_or(0)
    }

    fn st(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn emit(&self, e: TrustEvent) {
        self.events.record(e);
    }

    fn nonce() -> Result<String, TrustError> {
        Ok(hex::encode(&random_bytes::<16>()?))
    }

    // ---- minting ------------------------------------------------------------------------

    fn mint_grant(&self, to_did: &str, to_node: &str, role: Role) -> Result<Grant, TrustError> {
        let now = self.now();
        Grant::issue(
            &*self.signer,
            GrantClaims {
                v: TOKEN_VERSION,
                iss: self.did().to_string(),
                aud: self.did().to_string(),
                sub: to_did.to_string(),
                role,
                scope: SCOPE_MACHINE.to_string(),
                nonce: Self::nonce()?,
                iat: now,
                exp: now + self.policy.grant_ttl_secs,
                binds: Binds {
                    device: to_node.to_string(),
                },
            },
        )
    }

    fn store_grant(
        &self,
        g: &Grant,
        direction: GrantDirection,
        peer_did: &str,
        peer_node: &str,
    ) -> Result<(), TrustError> {
        let rec = GrantRecord {
            id: g.id.clone(),
            direction,
            peer_did: peer_did.to_string(),
            peer_node: peer_node.to_string(),
            role: g.claims.role.name().to_string(),
            nonce: g.claims.nonce.clone(),
            iat: i64::try_from(g.claims.iat).unwrap_or(0),
            exp: i64::try_from(g.claims.exp).unwrap_or(0),
            token: g.text().to_string(),
            state: STATE_LIVE.to_string(),
            state_at_ms: self.clock.now_ms(),
        };
        self.ledger.grant_put(&rec)?;
        self.st().grants.insert(rec.id.clone(), rec);
        self.emit(TrustEvent::GrantIssued {
            grant: g.id.clone(),
            direction: dir_name(direction).to_string(),
            peer_did: peer_did.to_string(),
            role: g.claims.role.name().to_string(),
            exp: g.claims.exp,
        });
        Ok(())
    }

    fn set_state(&self, id: &str, state: &str) -> Result<bool, TrustError> {
        let at = self.clock.now_ms();
        let changed = self.ledger.grant_set_state(id, state, at)?;
        if let Some(g) = self.st().grants.get_mut(id) {
            g.state = state.to_string();
            g.state_at_ms = at;
        }
        Ok(changed)
    }

    // ---- tickets ------------------------------------------------------------------------

    /// Mints a ticket. `ttl_secs` of `None` means the default (15 minutes).
    pub fn invite(
        &self,
        roles: &[Role],
        both_ways: bool,
        ttl_secs: Option<u64>,
        addr: &SelfAddr,
    ) -> Result<Ticket, TrustError> {
        if roles.is_empty() {
            return Err(TrustError::Malformed("name at least one role".to_string()));
        }
        let ttl = ttl_secs.unwrap_or(self.policy.ticket_ttl_secs);
        if ttl == 0 || ttl > self.policy.max_ticket_ttl_secs {
            return Err(TrustError::Malformed(format!(
                "a ticket lives from 1 second to {} seconds",
                self.policy.max_ticket_ttl_secs
            )));
        }
        let now = self.now();
        let mut roles = roles.to_vec();
        roles.sort();
        roles.dedup();
        let claims = InviteClaims {
            v: TOKEN_VERSION,
            iss: self.did().to_string(),
            name: self.name.clone(),
            device: self.node_id(),
            roles: roles.clone(),
            both_ways,
            scope: SCOPE_MACHINE.to_string(),
            nonce: Self::nonce()?,
            epoch: self.st().epoch,
            iat: now,
            exp: now + ttl,
        };
        let invite = Invite::issue(&*self.signer, claims)?;
        let ticket = Ticket {
            version: TOKEN_VERSION,
            node_did: self.did().to_string(),
            device_public_key: self.node_id(),
            node_id: self.node_id(),
            name: self.name.clone(),
            relay: addr.relay.clone(),
            direct_addrs: addr.direct_addrs.clone(),
            invite_token: invite.text().to_string(),
        };
        self.emit(TrustEvent::TicketMinted {
            nonce: invite.claims.nonce.clone(),
            roles: roles.iter().map(|r| r.name().to_string()).collect(),
            both_ways,
            exp: invite.claims.exp,
        });
        Ok(ticket)
    }

    /// Invalidates every unredeemed ticket by raising the ticket epoch. Returns the new epoch.
    pub fn reset_tickets(&self) -> Result<u64, TrustError> {
        let epoch = self.st().epoch + 1;
        self.ledger.meta_set(META_EPOCH, &epoch.to_string())?;
        self.st().epoch = epoch;
        self.emit(TrustEvent::TicketsReset { epoch });
        Ok(epoch)
    }

    // ---- inviter side of the redeem -----------------------------------------------------

    /// Step 1 of a redemption: a fresh challenge for the connection `from`.
    pub fn pair_begin(&self, from: &str) -> Result<String, TrustError> {
        let now = self.now();
        let challenge = random_bytes::<32>()?;
        let mut st = self.st();
        let ttl = self.policy.challenge_ttl_secs;
        st.challenges.retain(|_, (_, at)| *at + ttl > now);
        if st.challenges.len() >= MAX_CHALLENGES && !st.challenges.contains_key(from) {
            return Err(TrustError::NoChallenge);
        }
        st.challenges.insert(from.to_string(), (challenge, now));
        Ok(hex::encode(&challenge))
    }

    /// Step 2: checks the invite, the proof of the DID's key and any reverse grants, spends the
    /// nonce and mints the grants. Refusals and the redemption are reported as events.
    pub fn pair_redeem(&self, from: &str, r: &PairRedeem) -> Result<PairDone, TrustError> {
        match self.redeem_inner(from, r) {
            Ok(done) => Ok(done),
            Err(e) => {
                self.emit(TrustEvent::TicketRefused {
                    code: e.code().to_string(),
                    reason: e.to_string(),
                    node_id: Some(from.to_string()),
                });
                Err(e)
            }
        }
    }

    fn redeem_inner(&self, from: &str, r: &PairRedeem) -> Result<PairDone, TrustError> {
        let now = self.now();
        let challenge = {
            let mut st = self.st();
            let (c, at) = st
                .challenges
                .remove(from)
                .ok_or(TrustError::NoChallenge)?;
            if at + self.policy.challenge_ttl_secs <= now {
                return Err(TrustError::NoChallenge);
            }
            c
        };
        let invite = Invite::parse(&r.invite)?;
        let c = &invite.claims;
        if c.iss != self.did() {
            return Err(TrustError::WrongParty(
                "the invite was not issued by this machine".to_string(),
            ));
        }
        invite.check_time(now)?;
        if c.scope != SCOPE_MACHINE {
            return Err(TrustError::Scope(c.scope.clone()));
        }
        {
            let st = self.st();
            if c.epoch != st.epoch {
                return Err(TrustError::TicketReset);
            }
            if st.spent.contains(&c.nonce) {
                return Err(TrustError::TicketSpent);
            }
        }
        // Proof that the redeemer holds the key of the DID it claims.
        let key = public_key_from_did(&r.did)
            .ok_or_else(|| TrustError::Malformed("the redeemer's DID is not a did:key".into()))?;
        let sig: [u8; 64] = hex::decode(&r.challenge_sig)
            .ok_or_else(|| TrustError::Malformed("the challenge signature is not hex".into()))?;
        if !verify(&key, &pair_message(&challenge, &c.nonce, from), &sig) {
            return Err(TrustError::BadSignature);
        }
        if r.did == self.did() {
            return Err(TrustError::WrongParty(
                "a machine cannot pair with itself".to_string(),
            ));
        }
        let name = sanitize_name(&r.name);
        self.ensure_name_free(&name, &r.did)?;
        let reverse = self.check_reverse(&invite, r, now)?;
        // Everything checked: spend the nonce. A concurrent redemption loses here.
        if !self.ledger.nonce_spend(&c.nonce, self.clock.now_ms())? {
            return Err(TrustError::TicketSpent);
        }
        self.st().spent.insert(c.nonce.clone());
        self.emit(TrustEvent::TicketRedeemed {
            nonce: c.nonce.clone(),
            peer_did: r.did.clone(),
            peer_name: name.clone(),
            node_id: from.to_string(),
        });
        self.save_peer(&name, &r.did, from, r.relay.clone(), r.direct_addrs.clone())?;
        let mut tokens = Vec::new();
        for role in &c.roles {
            self.supersede(&r.did, GrantDirection::Issued, *role)?;
            let g = self.mint_grant(&r.did, from, *role)?;
            self.store_grant(&g, GrantDirection::Issued, &r.did, from)?;
            tokens.push(g.text().to_string());
        }
        for g in &reverse {
            self.supersede(&r.did, GrantDirection::Held, g.claims.role)?;
            self.store_grant(g, GrantDirection::Held, &r.did, from)?;
        }
        Ok(PairDone {
            did: self.did().to_string(),
            name: self.name.clone(),
            node_id: self.node_id(),
            grants: tokens,
        })
    }

    fn check_reverse(
        &self,
        invite: &Invite,
        r: &PairRedeem,
        now: u64,
    ) -> Result<Vec<Grant>, TrustError> {
        let c = &invite.claims;
        if !c.both_ways {
            if r.reverse.is_empty() {
                return Ok(Vec::new());
            }
            return Err(TrustError::Malformed(
                "the invite is one-way but the redeemer sent grants".to_string(),
            ));
        }
        let mut out = Vec::new();
        let mut roles = Vec::new();
        for t in r.reverse.iter().take(MAX_PRESENTED) {
            let g = Grant::parse(t)?;
            let k = &g.claims;
            if k.iss != r.did || k.aud != r.did {
                return Err(TrustError::WrongParty(
                    "a reverse grant is not issued by the redeemer".to_string(),
                ));
            }
            if k.sub != self.did() {
                return Err(TrustError::WrongParty(
                    "a reverse grant is not addressed to this machine".to_string(),
                ));
            }
            if k.binds.device != self.node_id() {
                return Err(TrustError::WrongDevice {
                    bound: k.binds.device.clone(),
                    actual: self.node_id(),
                });
            }
            if k.scope != SCOPE_MACHINE {
                return Err(TrustError::Scope(k.scope.clone()));
            }
            g.check_time(now)?;
            roles.push(k.role);
            out.push(g);
        }
        roles.sort();
        roles.dedup();
        if roles != c.roles || out.len() != roles.len() {
            return Err(TrustError::Malformed(
                "a both-ways invite needs exactly one reverse grant per role".to_string(),
            ));
        }
        Ok(out)
    }

    fn ensure_name_free(&self, name: &str, did: &str) -> Result<(), TrustError> {
        match self.st().peers.get(name) {
            Some(p) if p.did != did => Err(TrustError::NameTaken(name.to_string())),
            _ => Ok(()),
        }
    }

    fn save_peer(
        &self,
        name: &str,
        did: &str,
        node: &str,
        relay: Option<String>,
        addrs: Vec<String>,
    ) -> Result<(), TrustError> {
        let rec = PeerRecord {
            name: name.to_string(),
            did: did.to_string(),
            node_id: node.to_string(),
            relay,
            addrs,
            paired_ms: self.clock.now_ms(),
        };
        self.ledger.peer_put(&rec)?;
        self.st().peers.insert(rec.name.clone(), rec);
        Ok(())
    }

    /// A re-pairing replaces earlier live grants of the same peer, direction and role.
    fn supersede(
        &self,
        did: &str,
        direction: GrantDirection,
        role: Role,
    ) -> Result<(), TrustError> {
        let old: Vec<String> = self
            .st()
            .grants
            .values()
            .filter(|g| {
                g.peer_did == did
                    && g.direction == direction
                    && g.role == role.name()
                    && g.state == STATE_LIVE
            })
            .map(|g| g.id.clone())
            .collect();
        for id in old {
            self.set_state(&id, STATE_RENEWED)?;
        }
        Ok(())
    }

    // ---- redeemer side ------------------------------------------------------------------

    /// Redeems a ticket: dial, prove, receive the grants (and mint the reverse ones for a
    /// both-ways invite). Nothing is stored until the inviter has confirmed.
    pub fn accept(
        &self,
        ticket_text: &str,
        transport: &dyn Transport,
        me: &SelfAddr,
        timeout: Duration,
    ) -> Result<PairedPeer, TrustError> {
        let result = self.accept_inner(ticket_text, transport, me, timeout);
        if let Err(e) = &result {
            self.emit(TrustEvent::TicketRefused {
                code: e.code().to_string(),
                reason: e.to_string(),
                node_id: None,
            });
        }
        result
    }

    fn accept_inner(
        &self,
        ticket_text: &str,
        transport: &dyn Transport,
        me: &SelfAddr,
        timeout: Duration,
    ) -> Result<PairedPeer, TrustError> {
        let now = self.now();
        let ticket = Ticket::from_text(ticket_text)?;
        let invite = ticket.verify(now)?;
        let c = invite.claims.clone();
        if c.iss == self.did() {
            return Err(TrustError::WrongParty(
                "this ticket was made by this machine".to_string(),
            ));
        }
        let name = sanitize_name(&ticket.name);
        self.ensure_name_free(&name, &c.iss)?;
        let node = PeerId::new(&ticket.node_id)
            .map_err(|e| TrustError::Malformed(format!("the ticket's node id: {e}")))?;
        let hints = DialHints {
            relay: ticket.relay.clone(),
            direct_addrs: ticket.direct_addrs.clone(),
        };
        let mut conn = transport.connect_hinted(&node, &hints, timeout)?;
        let challenge: [u8; 32] = match conn.request(Body::PairBegin)? {
            Body::PairChallenge { challenge } => hex::decode(&challenge)
                .ok_or_else(|| TrustError::Peer("the challenge is not 32 bytes of hex".into()))?,
            other => {
                return Err(TrustError::Peer(format!(
                    "answered a pairing start with {}",
                    crate::fake::kind(&other)
                )));
            }
        };
        let my_node = self.node_id();
        let sig = self
            .signer
            .sign(&pair_message(&challenge, &c.nonce, &my_node));
        let mut reverse = Vec::new();
        if c.both_ways {
            for role in &c.roles {
                reverse.push(self.mint_grant(&c.iss, &ticket.node_id, *role)?);
            }
        }
        let done = match conn.request(Body::PairRedeem(PairRedeem {
            invite: ticket.invite_token.clone(),
            did: self.did().to_string(),
            name: self.name.clone(),
            challenge_sig: hex::encode(&sig),
            relay: me.relay.clone(),
            direct_addrs: me.direct_addrs.clone(),
            reverse: reverse.iter().map(|g| g.text().to_string()).collect(),
        }))? {
            Body::PairDone(d) => d,
            other => {
                return Err(TrustError::Peer(format!(
                    "answered a redemption with {}",
                    crate::fake::kind(&other)
                )));
            }
        };
        if done.did != c.iss || done.node_id != ticket.node_id {
            return Err(TrustError::WrongParty(
                "the machine that answered is not the one the ticket names".to_string(),
            ));
        }
        let mut got = Vec::new();
        let mut roles = Vec::new();
        for t in done.grants.iter().take(MAX_PRESENTED) {
            let g = Grant::parse(t)?;
            let k = &g.claims;
            if k.iss != c.iss || k.aud != c.iss {
                return Err(TrustError::WrongParty(
                    "a grant is not issued by the inviter".to_string(),
                ));
            }
            if k.sub != self.did() {
                return Err(TrustError::WrongParty(
                    "a grant is not addressed to this machine".to_string(),
                ));
            }
            if k.binds.device != my_node {
                return Err(TrustError::WrongDevice {
                    bound: k.binds.device.clone(),
                    actual: my_node.clone(),
                });
            }
            if k.scope != SCOPE_MACHINE {
                return Err(TrustError::Scope(k.scope.clone()));
            }
            g.check_time(now)?;
            roles.push(k.role);
            got.push(g);
        }
        roles.sort();
        roles.dedup();
        if roles != c.roles || got.len() != roles.len() {
            return Err(TrustError::Malformed(
                "the inviter returned grants that differ from the invite".to_string(),
            ));
        }
        self.save_peer(&name, &c.iss, &ticket.node_id, ticket.relay, ticket.direct_addrs)?;
        for g in &got {
            self.supersede(&c.iss, GrantDirection::Held, g.claims.role)?;
            self.store_grant(g, GrantDirection::Held, &c.iss, &ticket.node_id)?;
        }
        for g in &reverse {
            self.supersede(&c.iss, GrantDirection::Issued, g.claims.role)?;
            self.store_grant(g, GrantDirection::Issued, &c.iss, &ticket.node_id)?;
        }
        Ok(PairedPeer {
            name,
            did: c.iss,
            node_id: ticket.node_id,
        })
    }

    // ---- admission ----------------------------------------------------------------------

    /// Checks one presented token against everything: signature, audience, device binding,
    /// time, and that this machine issued it and it is still live.
    fn check_issued(&self, from: &str, token: &str, now: u64) -> Result<Grant, TrustError> {
        let g = Grant::parse(token)?;
        let k = &g.claims;
        if k.iss != self.did() || k.aud != self.did() {
            return Err(TrustError::WrongAudience { aud: k.aud.clone() });
        }
        if k.scope != SCOPE_MACHINE {
            return Err(TrustError::Scope(k.scope.clone()));
        }
        if k.binds.device != from {
            return Err(TrustError::WrongDevice {
                bound: k.binds.device.clone(),
                actual: from.to_string(),
            });
        }
        let st = self.st();
        match st.grants.get(&g.id) {
            Some(rec) if rec.direction == GrantDirection::Issued => {
                if rec.state != STATE_LIVE {
                    return Err(TrustError::NotLive {
                        state: rec.state.clone(),
                    });
                }
            }
            _ => return Err(TrustError::UnknownGrant),
        }
        drop(st);
        g.check_time(now)?;
        Ok(g)
    }

    /// Admits `from` on the grants it presents. At least one must be fully valid; the
    /// connection then holds exactly the valid ones' roles. A refusal closes the connection.
    pub fn present(&self, from: &str, tokens: &[String]) -> Result<PresentAck, TrustError> {
        let now = self.now();
        let mut live = Vec::new();
        let mut did = String::new();
        let mut first_err = None;
        for t in tokens.iter().take(MAX_PRESENTED) {
            match self.check_issued(from, t, now) {
                Ok(g) => {
                    did = g.claims.sub.clone();
                    live.push(LiveGrant {
                        id: g.id.clone(),
                        role: g.claims.role,
                        exp: g.claims.exp,
                    });
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        if live.is_empty() {
            let e = first_err
                .unwrap_or_else(|| TrustError::NotAuthorised("no grant was presented".into()));
            self.refuse_connection(from, &e);
            return Err(e);
        }
        let mut roles: Vec<Role> = live.iter().map(|g| g.role).collect();
        roles.sort();
        roles.dedup();
        let expires = live.iter().map(|g| g.exp).min().unwrap_or(0);
        let announce = {
            let mut st = self.st();
            let s = st.sessions.entry(from.to_string()).or_default();
            s.did = did.clone();
            s.grants = live;
            !std::mem::replace(&mut s.announced, true)
        };
        if announce {
            self.emit(TrustEvent::ConnectionOpened {
                side: Side::Served,
                node_id: from.to_string(),
                peer_did: did,
                roles: roles.iter().map(|r| r.name().to_string()).collect(),
            });
        }
        Ok(PresentAck {
            did: self.did().to_string(),
            name: self.name.clone(),
            roles,
            expires,
        })
    }

    fn refuse_connection(&self, from: &str, e: &TrustError) {
        self.emit(TrustEvent::ConnectionRefused {
            side: Side::Served,
            node_id: from.to_string(),
            code: e.code().to_string(),
            reason: e.to_string(),
        });
        let closers = self.take_closers(from, &format!("refused: {e}"));
        for c in closers {
            c(&format!("refused: {e}"));
        }
    }

    /// Marks every open connection of `from` closed and returns their closers.
    fn take_closers(&self, from: &str, reason: &str) -> Vec<Closer> {
        let mut out = Vec::new();
        let mut st = self.st();
        if let Some(s) = st.sessions.get_mut(from) {
            s.grants.clear();
            for c in s.conns.iter_mut().filter(|c| !c.closed) {
                c.closed = true;
                out.push(c.closer.clone());
            }
        }
        drop(st);
        if !out.is_empty() {
            self.emit(TrustEvent::ConnectionClosed {
                side: Side::Served,
                node_id: from.to_string(),
                reason: reason.to_string(),
            });
        }
        out
    }

    /// Checks a request from `from` against its session: a grant must still be live, unexpired
    /// and cover `need`.
    pub fn authorize(&self, from: &str, need: Need) -> Result<(), Refusal> {
        let now = self.now();
        let verdict = {
            let st = self.st();
            match st.sessions.get(from) {
                None => Err(TrustError::NotAuthorised(
                    "no grant was presented on this connection".to_string(),
                )),
                Some(s) if s.grants.is_empty() => Err(if s.announced {
                    TrustError::NotLive {
                        state: STATE_REVOKED.to_string(),
                    }
                } else {
                    TrustError::NotAuthorised(
                        "no grant was presented on this connection".to_string(),
                    )
                }),
                Some(s) => {
                    let mut roles = Vec::new();
                    let mut revoked = false;
                    let last_exp = s.grants.iter().map(|g| g.exp).max().unwrap_or(0);
                    for g in &s.grants {
                        let live = st.grants.get(&g.id).is_some_and(|r| r.state == STATE_LIVE);
                        if !live {
                            revoked = true;
                        } else if g.exp > now {
                            roles.push(g.role);
                        }
                    }
                    if roles.is_empty() {
                        Err(if revoked {
                            TrustError::NotLive {
                                state: STATE_REVOKED.to_string(),
                            }
                        } else {
                            TrustError::Expired { exp: last_exp }
                        })
                    } else if allows(&roles, need) {
                        Ok(())
                    } else {
                        let have: Vec<String> =
                            roles.iter().map(|r| r.name().to_string()).collect();
                        drop(st);
                        self.emit(TrustEvent::ConnectionRefused {
                            side: Side::Served,
                            node_id: from.to_string(),
                            code: "role_denied".to_string(),
                            reason: format!(
                                "the request needs {}; the peer holds {have:?}",
                                need.role_name()
                            ),
                        });
                        return Err(Refusal::RoleDenied {
                            need: need.role_name().to_string(),
                            have,
                        });
                    }
                }
            }
        };
        match verdict {
            Ok(()) => Ok(()),
            Err(e) => {
                self.refuse_connection(from, &e);
                Err(e.to_refusal())
            }
        }
    }

    /// Renews a grant this machine issued, over the live connection of its holder.
    pub fn renew(&self, from: &str, token: &str) -> Result<String, TrustError> {
        let now = self.now();
        let old = self.check_issued(from, token, now)?;
        {
            let st = self.st();
            let in_session = st
                .sessions
                .get(from)
                .is_some_and(|s| s.grants.iter().any(|g| g.id == old.id));
            if !in_session {
                return Err(TrustError::NotAuthorised(
                    "renew a grant that this connection presented".to_string(),
                ));
            }
        }
        let new = self.mint_grant(&old.claims.sub, from, old.claims.role)?;
        self.store_grant(&new, GrantDirection::Issued, &old.claims.sub, from)?;
        self.set_state(&old.id, STATE_RENEWED)?;
        {
            let mut st = self.st();
            if let Some(s) = st.sessions.get_mut(from) {
                s.grants.retain(|g| g.id != old.id);
                s.grants.push(LiveGrant {
                    id: new.id.clone(),
                    role: new.claims.role,
                    exp: new.claims.exp,
                });
            }
        }
        self.emit(TrustEvent::GrantRenewed {
            old: old.id,
            new: new.id.clone(),
            direction: "issued".to_string(),
            peer_did: new.claims.sub.clone(),
            role: new.claims.role.name().to_string(),
            exp: new.claims.exp,
        });
        Ok(new.text().to_string())
    }

    /// True when `from` has a session with at least one grant.
    pub fn is_admitted(&self, from: &str) -> bool {
        self.st()
            .sessions
            .get(from)
            .is_some_and(|s| !s.grants.is_empty())
    }

    /// A transport connection from `from` opened; `closer` shuts it. Returns its id.
    pub fn connection_opened(&self, from: &str, closer: Closer) -> u64 {
        let mut st = self.st();
        st.next_conn += 1;
        let id = st.next_conn;
        st.sessions
            .entry(from.to_string())
            .or_default()
            .conns
            .push(Conn {
                id,
                closer,
                closed: false,
            });
        id
    }

    /// The transport connection `conn` from `from` ended.
    pub fn connection_closed(&self, from: &str, conn: u64, reason: &str) {
        let mut announce = false;
        {
            let mut st = self.st();
            if let Some(s) = st.sessions.get_mut(from) {
                if let Some(pos) = s.conns.iter().position(|c| c.id == conn) {
                    let c = s.conns.remove(pos);
                    announce = !c.closed && s.announced;
                }
                if s.conns.is_empty() {
                    st.sessions.remove(from);
                }
            }
            st.challenges.remove(from);
        }
        if announce {
            self.emit(TrustEvent::ConnectionClosed {
                side: Side::Served,
                node_id: from.to_string(),
                reason: reason.to_string(),
            });
        }
    }

    // ---- revocation ---------------------------------------------------------------------

    fn revoke_ids(&self, ids: &[String]) -> Result<usize, TrustError> {
        let mut closers: Vec<(String, Closer)> = Vec::new();
        let mut closed_nodes = BTreeSet::new();
        let mut n = 0;
        for id in ids {
            let rec = self.st().grants.get(id).cloned();
            let Some(rec) = rec else { continue };
            if !self.set_state(id, STATE_REVOKED)? {
                continue;
            }
            n += 1;
            self.emit(TrustEvent::GrantRevoked {
                grant: id.clone(),
                direction: dir_name(rec.direction).to_string(),
                peer_did: rec.peer_did.clone(),
                role: rec.role.clone(),
            });
            let mut st = self.st();
            for (node, s) in &mut st.sessions {
                if s.grants.iter().any(|g| &g.id == id) {
                    s.grants.retain(|g| &g.id != id);
                    if s.grants.is_empty() {
                        for c in s.conns.iter_mut().filter(|c| !c.closed) {
                            c.closed = true;
                            closers.push((node.clone(), c.closer.clone()));
                        }
                        closed_nodes.insert(node.clone());
                    }
                }
            }
        }
        for node in &closed_nodes {
            self.emit(TrustEvent::ConnectionClosed {
                side: Side::Served,
                node_id: node.clone(),
                reason: "grant revoked".to_string(),
            });
        }
        for (_, c) in closers {
            c("grant revoked");
        }
        Ok(n)
    }

    /// Revokes every live grant, either way, of the peer called `name`. Live connections that
    /// held them are closed. Nothing is deleted.
    pub fn revoke_peer(&self, name: &str) -> Result<RevokeReport, TrustError> {
        let (ids, open) = {
            let st = self.st();
            let p = st
                .peers
                .get(name)
                .ok_or_else(|| TrustError::NoPeer(name.to_string()))?;
            let ids: Vec<String> = st
                .grants
                .values()
                .filter(|g| g.peer_did == p.did && g.state == STATE_LIVE)
                .map(|g| g.id.clone())
                .collect();
            let open = st
                .sessions
                .get(&p.node_id)
                .map_or(0, |s| s.conns.iter().filter(|c| !c.closed).count());
            (ids, open)
        };
        let revoked = self.revoke_ids(&ids)?;
        Ok(RevokeReport {
            name: name.to_string(),
            revoked,
            closed: open,
        })
    }

    // ---- views --------------------------------------------------------------------------

    /// Every paired peer with its live grants both ways.
    pub fn peers(&self) -> Vec<PeerTrust> {
        let now = self.now();
        let st = self.st();
        st.peers
            .values()
            .map(|p| {
                let mut out = PeerTrust {
                    name: p.name.clone(),
                    did: p.did.clone(),
                    node_id: p.node_id.clone(),
                    outbound: Vec::new(),
                    inbound: Vec::new(),
                    expires: None,
                    revoked: false,
                    paired_ms: p.paired_ms,
                };
                for g in st.grants.values().filter(|g| g.peer_did == p.did) {
                    if g.state != STATE_LIVE {
                        continue;
                    }
                    let Ok(role) = Role::parse(&g.role) else {
                        continue;
                    };
                    let exp = u64::try_from(g.exp).unwrap_or(0);
                    if exp <= now {
                        continue;
                    }
                    let item = RoleExpiry {
                        role,
                        exp,
                        grant: g.id.clone(),
                    };
                    out.expires = Some(out.expires.map_or(exp, |e| e.min(exp)));
                    match g.direction {
                        GrantDirection::Held => out.outbound.push(item),
                        GrantDirection::Issued => out.inbound.push(item),
                    }
                }
                out.revoked = out.outbound.is_empty() && out.inbound.is_empty();
                out
            })
            .collect()
    }

    /// Peers this machine holds a live, unexpired grant from: the ones a forwarder may dial.
    pub fn paired_peers(&self) -> Vec<PairedPeer> {
        self.peers()
            .into_iter()
            .filter(|p| !p.outbound.is_empty())
            .map(|p| PairedPeer {
                name: p.name,
                did: p.did,
                node_id: p.node_id,
            })
            .collect()
    }

    /// Live grants with less than the alert window left (or already past their end).
    pub fn expiring(&self) -> Vec<ExpiringGrant> {
        let now = i64::try_from(self.now()).unwrap_or(i64::MAX);
        let window = i64::try_from(self.policy.alert_before_secs).unwrap_or(i64::MAX);
        let st = self.st();
        let mut out = Vec::new();
        for g in st.grants.values().filter(|g| g.state == STATE_LIVE) {
            let left = g.exp - now;
            if left > window {
                continue;
            }
            let Some(p) = st.peers.values().find(|p| p.did == g.peer_did) else {
                continue;
            };
            let Ok(role) = Role::parse(&g.role) else {
                continue;
            };
            out.push(ExpiringGrant {
                peer: p.name.clone(),
                direction: dir_name(g.direction),
                role,
                exp: u64::try_from(g.exp).unwrap_or(0),
                left_secs: left,
            });
        }
        out.sort_by(|a, b| (&a.peer, a.direction, a.role).cmp(&(&b.peer, b.direction, b.role)));
        out
    }

    /// Remembers the latest addresses a peer was reached at.
    pub fn remember_addrs(
        &self,
        node_id: &str,
        relay: Option<String>,
        addrs: Vec<String>,
    ) -> Result<(), TrustError> {
        let found = self
            .st()
            .peers
            .values()
            .find(|p| p.node_id == node_id)
            .cloned();
        if let Some(mut p) = found {
            if p.relay != relay || p.addrs != addrs {
                p.relay = relay;
                p.addrs = addrs;
                self.ledger.peer_put(&p)?;
                self.st().peers.insert(p.name.clone(), p);
            }
        }
        Ok(())
    }

    // ---- the dialing side's grants ------------------------------------------------------

    /// The live, unexpired grants this machine holds from the peer with node id `node`.
    pub fn held_for(&self, node: &str) -> Vec<Grant> {
        let now = self.now();
        let recs: Vec<GrantRecord> = self
            .st()
            .grants
            .values()
            .filter(|g| {
                g.direction == GrantDirection::Held && g.peer_node == node && g.state == STATE_LIVE
            })
            .cloned()
            .collect();
        recs.iter()
            .filter_map(|r| Grant::parse(&r.token).ok())
            .filter(|g| g.check_time(now).is_ok())
            .collect()
    }

    /// Asks the issuer, over the live connection `conn`, to renew every held grant of `node`
    /// that has less than the renewal window left. Returns how many were renewed; a failure
    /// leaves the old grant in place.
    pub fn renew_held(&self, conn: &mut dyn Connection, node: &str) -> usize {
        let now = self.now();
        let mut renewed = 0;
        for g in self.held_for(node) {
            if g.claims.exp.saturating_sub(now) > self.policy.renew_before_secs {
                continue;
            }
            let Ok(Body::Renewed { grant }) = conn.request(Body::Renew {
                grant: g.text().to_string(),
            }) else {
                continue;
            };
            if self.accept_renewed(&g, &grant).is_ok() {
                renewed += 1;
            }
        }
        renewed
    }

    fn accept_renewed(&self, old: &Grant, text: &str) -> Result<(), TrustError> {
        let new = Grant::parse(text)?;
        let (a, b) = (&old.claims, &new.claims);
        if a.iss != b.iss
            || a.sub != b.sub
            || a.role != b.role
            || a.binds != b.binds
            || a.scope != b.scope
            || b.exp <= a.exp
        {
            return Err(TrustError::WrongParty(
                "the renewed grant differs from the one renewed".to_string(),
            ));
        }
        new.check_time(self.now())?;
        let peer_node = self
            .st()
            .grants
            .get(&old.id)
            .map(|r| r.peer_node.clone())
            .unwrap_or_default();
        self.store_grant(&new, GrantDirection::Held, &b.iss, &peer_node)?;
        self.set_state(&old.id, STATE_RENEWED)?;
        self.emit(TrustEvent::GrantRenewed {
            old: old.id.clone(),
            new: new.id.clone(),
            direction: "held".to_string(),
            peer_did: b.iss.clone(),
            role: b.role.name().to_string(),
            exp: b.exp,
        });
        Ok(())
    }

    /// Records that this machine dialed `node_id` and was admitted (or refused).
    pub fn note_dialed(&self, node_id: &str, peer_did: &str, roles: &[Role]) {
        self.emit(TrustEvent::ConnectionOpened {
            side: Side::Dialed,
            node_id: node_id.to_string(),
            peer_did: peer_did.to_string(),
            roles: roles.iter().map(|r| r.name().to_string()).collect(),
        });
    }

    /// Records a refusal of a dial this machine made.
    pub fn note_dial_refused(&self, node_id: &str, e: &TrustError) {
        self.emit(TrustEvent::ConnectionRefused {
            side: Side::Dialed,
            node_id: node_id.to_string(),
            code: e.code().to_string(),
            reason: e.to_string(),
        });
    }
}

impl AddressBook for TrustNode {
    fn hints(&self, peer: &PeerId) -> Option<DialHints> {
        self.st()
            .peers
            .values()
            .find(|p| p.node_id == peer.as_str())
            .map(|p| DialHints {
                relay: p.relay.clone(),
                direct_addrs: p.addrs.clone(),
            })
    }
}

/// True when `text` looks like a grant token (for log scrubbing and tests).
pub fn looks_like_grant(text: &str) -> bool {
    text.starts_with(GRANT_PREFIX)
}
