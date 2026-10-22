//! Pairing and admission payloads that ride inside [`crate::proto::Body`].

use super::role::Role;
use serde::{Deserialize, Serialize};

/// The presented grants were accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentAck {
    /// The serving machine's DID.
    pub did: String,
    /// Its name.
    pub name: String,
    /// The roles the accepted grants give.
    pub roles: Vec<Role>,
    /// The earliest expiry among them, unix seconds.
    pub expires: u64,
}

/// Redeeming an invite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRedeem {
    /// The invite token (`i1.…`) from the ticket.
    pub invite: String,
    /// The redeemer's DID.
    pub did: String,
    /// The name the redeemer calls itself.
    pub name: String,
    /// Signature, by the DID's key, over the inviter's challenge (lowercase hex).
    pub challenge_sig: String,
    /// The redeemer's relay, when it has one.
    #[serde(default)]
    pub relay: Option<String>,
    /// Addresses the inviter can reach the redeemer at.
    #[serde(default)]
    pub direct_addrs: Vec<String>,
    /// With `both_ways`: grants the redeemer minted for the inviter, one per role.
    #[serde(default)]
    pub reverse: Vec<String>,
}

/// The invite was redeemed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairDone {
    /// The inviter's DID.
    pub did: String,
    /// Its name.
    pub name: String,
    /// Its node id.
    pub node_id: String,
    /// The grants minted for the redeemer, one per role.
    pub grants: Vec<String>,
}
