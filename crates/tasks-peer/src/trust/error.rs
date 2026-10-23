//! Why a trust step failed. Every variant names its reason in plain words; none carries key
//! material.

use crate::proto::Refusal;
use crate::transport::PeerError;

/// A trust failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    /// The text is not a token, ticket or role list.
    #[error("{0}")]
    Malformed(String),
    /// The signature does not verify against the issuer's key.
    #[error("the signature does not verify")]
    BadSignature,
    /// The grant has expired.
    #[error("the grant expired at {exp} (unix seconds)")]
    Expired {
        /// Its expiry.
        exp: u64,
    },
    /// The token's issue time is in the future.
    #[error("the token is not valid yet")]
    NotYetValid,
    /// The token was made for another machine.
    #[error("the grant is addressed to {aud}, not to this machine")]
    WrongAudience {
        /// Who it is for.
        aud: String,
    },
    /// The grant is bound to another device key than the connection's.
    #[error("the grant is bound to device {bound}, but the connection is from {actual}")]
    WrongDevice {
        /// The bound device.
        bound: String,
        /// The connection's node id.
        actual: String,
    },
    /// The grant does not name the peer that presented it or this machine as expected.
    #[error("{0}")]
    WrongParty(String),
    /// This machine never issued the grant.
    #[error("this machine has no such grant")]
    UnknownGrant,
    /// The grant was revoked or renewed away.
    #[error("the grant was {state}")]
    NotLive {
        /// `revoked` or `renewed`.
        state: String,
    },
    /// The scope is not one this build understands.
    #[error("scope {0:?} is not supported")]
    Scope(String),
    /// A newer token or ticket version.
    #[error("{what} version {found} is newer than this build understands ({supported})")]
    Newer {
        /// Token or ticket.
        what: &'static str,
        /// Version found.
        found: String,
        /// Version understood.
        supported: String,
    },
    /// The ticket has expired.
    #[error("the ticket expired at {exp} (unix seconds)")]
    TicketExpired {
        /// Its expiry.
        exp: u64,
    },
    /// The ticket was already redeemed.
    #[error("the ticket was already redeemed")]
    TicketSpent,
    /// `pair reset` invalidated the ticket.
    #[error("the ticket was invalidated by a pair reset")]
    TicketReset,
    /// The ticket's visible fields disagree with its signed invite.
    #[error("the ticket's {field} disagrees with its signed invite")]
    TicketMismatch {
        /// Which field.
        field: &'static str,
    },
    /// No challenge was issued to this connection, or it timed out.
    #[error("no live challenge for this connection")]
    NoChallenge,
    /// The peer name is taken by another machine.
    #[error("the name {0:?} is already used by another machine")]
    NameTaken(String),
    /// No such paired peer.
    #[error("no paired peer named {0:?}")]
    NoPeer(String),
    /// A grant is needed first.
    #[error("{0}")]
    NotAuthorised(String),
    /// The ledger failed.
    #[error("trust ledger: {0}")]
    Ledger(String),
    /// The operating system has no random source.
    #[error("no random source")]
    Random,
    /// The transport failed.
    #[error("{0}")]
    Peer(String),
}

impl TrustError {
    /// The refusal a peer sees for this failure.
    pub fn to_refusal(&self) -> Refusal {
        match self {
            TrustError::Ledger(_) | TrustError::Random => Refusal::Internal {
                detail: "trust state unavailable".to_string(),
            },
            TrustError::TicketExpired { .. }
            | TrustError::TicketSpent
            | TrustError::TicketReset
            | TrustError::TicketMismatch { .. }
            | TrustError::NoChallenge
            | TrustError::NameTaken(_)
            | TrustError::Newer { .. } => Refusal::PairingRefused {
                reason: self.to_string(),
            },
            other => Refusal::Unauthorized {
                reason: other.to_string(),
            },
        }
    }

    /// A short stable code for audit rows.
    pub fn code(&self) -> &'static str {
        match self {
            TrustError::Malformed(_) => "malformed",
            TrustError::BadSignature => "bad_signature",
            TrustError::Expired { .. } => "expired",
            TrustError::NotYetValid => "not_yet_valid",
            TrustError::WrongAudience { .. } => "wrong_audience",
            TrustError::WrongDevice { .. } => "wrong_device",
            TrustError::WrongParty(_) => "wrong_party",
            TrustError::UnknownGrant => "unknown_grant",
            TrustError::NotLive { .. } => "revoked",
            TrustError::Scope(_) => "scope",
            TrustError::Newer { .. } => "newer_version",
            TrustError::TicketExpired { .. } => "ticket_expired",
            TrustError::TicketSpent => "ticket_spent",
            TrustError::TicketReset => "ticket_reset",
            TrustError::TicketMismatch { .. } => "ticket_mismatch",
            TrustError::NoChallenge => "no_challenge",
            TrustError::NameTaken(_) => "name_taken",
            TrustError::NoPeer(_) => "no_peer",
            TrustError::NotAuthorised(_) => "not_authorised",
            TrustError::Ledger(_) => "ledger",
            TrustError::Random => "random",
            TrustError::Peer(_) => "peer",
        }
    }
}

impl From<tasks_ledger::LedgerError> for TrustError {
    fn from(e: tasks_ledger::LedgerError) -> TrustError {
        TrustError::Ledger(e.to_string())
    }
}

impl From<tasks_identity::IdentityError> for TrustError {
    fn from(_: tasks_identity::IdentityError) -> TrustError {
        TrustError::Random
    }
}

impl From<PeerError> for TrustError {
    fn from(e: PeerError) -> TrustError {
        match e {
            PeerError::Refused(Refusal::Unauthorized { reason })
            | PeerError::Refused(Refusal::PairingRefused { reason }) => {
                TrustError::NotAuthorised(reason)
            }
            other => TrustError::Peer(other.to_string()),
        }
    }
}
