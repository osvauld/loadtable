//! The pairing ticket: `tasks1.` + base64url(JSON), one URL-safe word to copy between machines.
//!
//! The visible fields are only for display and dialing; the signed invite is what counts, so any
//! disagreement between the two is an error rather than silently preferring one (design §4).

use super::error::TrustError;
use super::token::{Invite, TOKEN_VERSION};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

/// The text prefix; the digit is the ticket version.
pub const TICKET_PREFIX: &str = "tasks1.";
/// Largest ticket text accepted.
pub const MAX_TICKET_BYTES: usize = 8192;

/// A pairing ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    /// Ticket version.
    pub version: u32,
    /// The inviter's DID.
    pub node_did: String,
    /// The inviter's device key, lowercase hex.
    pub device_public_key: String,
    /// The inviter's node id (the same key, as iroh names it).
    pub node_id: String,
    /// The inviter's name.
    pub name: String,
    /// Its relay, when it has one.
    #[serde(default)]
    pub relay: Option<String>,
    /// Addresses to try first.
    #[serde(default)]
    pub direct_addrs: Vec<String>,
    /// The signed invite (`i1.…`).
    pub invite_token: String,
}

impl Ticket {
    /// The ticket as text.
    pub fn to_text(&self) -> Result<String, TrustError> {
        let json = serde_json::to_vec(self)
            .map_err(|e| TrustError::Malformed(format!("cannot encode the ticket: {e}")))?;
        Ok(format!("{TICKET_PREFIX}{}", URL_SAFE_NO_PAD.encode(json)))
    }

    /// Parses ticket text. A newer version is refused by name; surrounding whitespace is fine.
    pub fn from_text(text: &str) -> Result<Ticket, TrustError> {
        let text = text.trim();
        if text.len() > MAX_TICKET_BYTES {
            return Err(TrustError::Malformed("the ticket is too long".to_string()));
        }
        let Some(rest) = text.strip_prefix("tasks") else {
            return Err(TrustError::Malformed(
                "not a tasks ticket (expected tasks1.…)".to_string(),
            ));
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let body = rest
            .get(digits.len()..)
            .and_then(|r| r.strip_prefix('.'))
            .filter(|_| !digits.is_empty())
            .ok_or_else(|| {
                TrustError::Malformed("not a tasks ticket (expected tasks1.…)".to_string())
            })?;
        if digits != TOKEN_VERSION.to_string() {
            return Err(TrustError::Newer {
                what: "ticket",
                found: digits,
                supported: TOKEN_VERSION.to_string(),
            });
        }
        let json = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| TrustError::Malformed("the ticket is not base64url".to_string()))?;
        let ticket: Ticket = serde_json::from_slice(&json)
            .map_err(|_| TrustError::Malformed("the ticket body is not valid".to_string()))?;
        if ticket.version != TOKEN_VERSION {
            return Err(TrustError::Newer {
                what: "ticket",
                found: ticket.version.to_string(),
                supported: TOKEN_VERSION.to_string(),
            });
        }
        Ok(ticket)
    }

    /// Verifies the signed invite, its expiry and that the visible fields match it. Returns the
    /// invite.
    pub fn verify(&self, now: u64) -> Result<Invite, TrustError> {
        let invite = Invite::parse(&self.invite_token)?;
        let c = &invite.claims;
        if c.iss != self.node_did {
            return Err(TrustError::TicketMismatch { field: "node_did" });
        }
        if c.device != self.device_public_key {
            return Err(TrustError::TicketMismatch {
                field: "device_public_key",
            });
        }
        if self.node_id != self.device_public_key {
            return Err(TrustError::TicketMismatch { field: "node_id" });
        }
        if c.name != self.name {
            return Err(TrustError::TicketMismatch { field: "name" });
        }
        invite.check_time(now)?;
        Ok(invite)
    }
}
