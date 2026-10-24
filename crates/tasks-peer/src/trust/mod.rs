//! Machine trust: grants, roles, pairing tickets and identity-based admission (design §3–§5).
//!
//! * [`token`]: signed grants and invites, bound to a device key.
//! * [`ticket`]: the `tasks1.` pairing ticket.
//! * [`node`]: [`TrustNode`], one machine's rules, sessions and revocation.
//! * [`handler`] / [`client`]: the gate in front of a receiver and the grant-presenting
//!   transport wrapper.
//! * [`events`]: the single [`TrustEvents`] sink; [`audit`] is the one that writes the audit table.

pub mod audit;
pub mod client;
pub mod error;
pub mod events;
pub mod handler;
pub mod node;
pub mod role;
pub mod ticket;
pub mod token;
pub mod wire;

#[cfg(test)]
mod tests;

pub use audit::AuditEvents;
pub use client::TrustedTransport;
pub use error::TrustError;
pub use events::{FanoutEvents, NoEvents, RecordingEvents, Side, TrustEvent, TrustEvents};
pub use handler::TrustedHandler;
pub use node::{
    AddrSource, AddressBook, ExpiringGrant, PairedPeer, PeerTrust, RevokeReport, RoleExpiry,
    SelfAddr, SelfReach, TrustNode, TrustPolicy, sanitize_name,
};
pub use role::{Need, Role, allows, need_of};
pub use ticket::Ticket;
pub use token::{Grant, GrantClaims, Invite, InviteClaims};
