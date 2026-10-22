//! Every trust event goes through one trait, so the audit table (step 2) plugs in without
//! touching the trust code. [`NoEvents`] drops them; [`RecordingEvents`] keeps them for tests.

use std::sync::Mutex;

/// Which side of a connection saw an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// This machine accepted the connection.
    Served,
    /// This machine dialed it.
    Dialed,
}

/// One thing that happened in the trust layer. Reasons are stable codes
/// ([`crate::trust::TrustError::code`]) plus a human sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustEvent {
    /// An invite ticket was minted.
    TicketMinted {
        /// Its nonce.
        nonce: String,
        /// Roles it carries.
        roles: Vec<String>,
        /// Whether the redeemer grants back.
        both_ways: bool,
        /// Expiry, unix seconds.
        exp: u64,
    },
    /// A ticket was redeemed (seen by the inviter).
    TicketRedeemed {
        /// Its nonce.
        nonce: String,
        /// The redeemer's DID.
        peer_did: String,
        /// The redeemer's name.
        peer_name: String,
        /// The redeemer's node id.
        node_id: String,
    },
    /// A ticket redemption was refused (inviter) or failed (redeemer).
    TicketRefused {
        /// Stable code.
        code: String,
        /// Human reason.
        reason: String,
        /// The other side's node id, when known.
        node_id: Option<String>,
    },
    /// `pair reset` invalidated every unredeemed ticket.
    TicketsReset {
        /// The new epoch.
        epoch: u64,
    },
    /// A grant was issued by this machine or accepted from a peer.
    GrantIssued {
        /// Grant id.
        grant: String,
        /// `issued` or `held`.
        direction: String,
        /// The other machine.
        peer_did: String,
        /// The role.
        role: String,
        /// Expiry, unix seconds.
        exp: u64,
    },
    /// A grant was renewed.
    GrantRenewed {
        /// The old grant id.
        old: String,
        /// The new grant id.
        new: String,
        /// `issued` or `held`.
        direction: String,
        /// The other machine.
        peer_did: String,
        /// The role.
        role: String,
        /// New expiry, unix seconds.
        exp: u64,
    },
    /// A grant was revoked.
    GrantRevoked {
        /// Grant id.
        grant: String,
        /// `issued` or `held`.
        direction: String,
        /// The other machine.
        peer_did: String,
        /// The role.
        role: String,
    },
    /// A connection was admitted.
    ConnectionOpened {
        /// Which side saw it.
        side: Side,
        /// The peer's node id.
        node_id: String,
        /// The peer's DID.
        peer_did: String,
        /// Roles the connection holds.
        roles: Vec<String>,
    },
    /// A connection ended.
    ConnectionClosed {
        /// Which side saw it.
        side: Side,
        /// The peer's node id.
        node_id: String,
        /// Why (`peer left`, `grant revoked`, …).
        reason: String,
    },
    /// A connection or request was refused.
    ConnectionRefused {
        /// Which side saw it.
        side: Side,
        /// The peer's node id.
        node_id: String,
        /// Stable code.
        code: String,
        /// Human reason.
        reason: String,
    },
}

impl TrustEvent {
    /// A short stable action name (the audit `action` column).
    pub fn action(&self) -> &'static str {
        match self {
            TrustEvent::TicketMinted { .. } => "pair.ticket_minted",
            TrustEvent::TicketRedeemed { .. } => "pair.ticket_redeemed",
            TrustEvent::TicketRefused { .. } => "pair.ticket_refused",
            TrustEvent::TicketsReset { .. } => "pair.tickets_reset",
            TrustEvent::GrantIssued { .. } => "grant.issued",
            TrustEvent::GrantRenewed { .. } => "grant.renewed",
            TrustEvent::GrantRevoked { .. } => "grant.revoked",
            TrustEvent::ConnectionOpened { .. } => "conn.opened",
            TrustEvent::ConnectionClosed { .. } => "conn.closed",
            TrustEvent::ConnectionRefused { .. } => "conn.refused",
        }
    }
}

/// The one sink for trust events.
pub trait TrustEvents: Send + Sync {
    /// Records an event. Must not block for long and must not fail the caller.
    fn record(&self, event: TrustEvent);
}

/// Drops every event.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoEvents;

impl TrustEvents for NoEvents {
    fn record(&self, _event: TrustEvent) {}
}

/// Keeps every event in order, for tests.
#[derive(Debug, Default)]
pub struct RecordingEvents {
    seen: Mutex<Vec<TrustEvent>>,
}

impl RecordingEvents {
    /// An empty recorder.
    pub fn new() -> RecordingEvents {
        RecordingEvents::default()
    }

    /// Everything recorded so far.
    pub fn events(&self) -> Vec<TrustEvent> {
        self.seen.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// The action names recorded so far, in order.
    pub fn actions(&self) -> Vec<&'static str> {
        self.events().iter().map(TrustEvent::action).collect()
    }
}

impl TrustEvents for RecordingEvents {
    fn record(&self, event: TrustEvent) {
        if let Ok(mut g) = self.seen.lock() {
            g.push(event);
        }
    }
}
