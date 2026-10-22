//! Signed tokens: grants and invites share one text form, `PREFIX.base64url(payload).base64url(sig)`.
//!
//! The signature covers a domain tag plus the exact payload bytes, so nothing is re-serialised
//! before checking. The issuer's key comes from the DID in `iss`; no lookup is needed.

use super::error::TrustError;
use super::role::Role;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tasks_identity::{Signer, hex, public_key_from_did, verify};

/// Prefix of a grant token.
pub const GRANT_PREFIX: &str = "g1.";
/// Prefix of an invite token.
pub const INVITE_PREFIX: &str = "i1.";
/// The only scope today: the whole machine.
pub const SCOPE_MACHINE: &str = "machine";
/// The token version this build writes and reads.
pub const TOKEN_VERSION: u32 = 1;
/// Clock skew tolerated on `iat`, in seconds.
pub const SKEW_SECS: u64 = 300;
/// Largest token text accepted.
pub const MAX_TOKEN_BYTES: usize = 4096;

const GRANT_DOMAIN: &[u8] = b"tasks/grant/v1\0";
const INVITE_DOMAIN: &[u8] = b"tasks/invite/v1\0";

/// Public key material a grant is about. It is inside the signed payload because a binding
/// carried beside a signature proves nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binds {
    /// The holder's device key (its node id, lowercase hex).
    pub device: String,
}

/// What a grant says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantClaims {
    /// Token version.
    pub v: u32,
    /// The granting machine's DID (the signer).
    pub iss: String,
    /// The machine that honours the grant: the issuer itself.
    pub aud: String,
    /// The holder's DID.
    pub sub: String,
    /// What the holder may do.
    pub role: Role,
    /// What it covers.
    pub scope: String,
    /// Unique per grant.
    pub nonce: String,
    /// Issued at, unix seconds.
    pub iat: u64,
    /// Expires at, unix seconds.
    pub exp: u64,
    /// The holder's device key.
    pub binds: Binds,
}

/// What an invite says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteClaims {
    /// Token version.
    pub v: u32,
    /// The inviting machine's DID (the signer).
    pub iss: String,
    /// Its name.
    pub name: String,
    /// Its device key (node id).
    pub device: String,
    /// The roles the redeemer receives.
    pub roles: Vec<Role>,
    /// True when the redeemer also grants the same roles back.
    pub both_ways: bool,
    /// What the grants cover.
    pub scope: String,
    /// Single-use nonce.
    pub nonce: String,
    /// The ticket epoch it was minted in; `pair reset` raises the epoch.
    pub epoch: u64,
    /// Minted at, unix seconds.
    pub iat: u64,
    /// Expires at, unix seconds.
    pub exp: u64,
}

/// A signed token of claims `C`. Holding one means its signature verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed<C> {
    /// The claims.
    pub claims: C,
    /// Hash of the signed payload, lowercase hex; names the token.
    pub id: String,
    text: String,
}

impl<C> Signed<C> {
    /// The token text.
    pub fn text(&self) -> &str {
        &self.text
    }
}

fn input(domain: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(domain.len() + payload.len());
    v.extend_from_slice(domain);
    v.extend_from_slice(payload);
    v
}

fn sign<C: Serialize>(
    signer: &dyn Signer,
    prefix: &str,
    domain: &[u8],
    claims: C,
) -> Result<Signed<C>, TrustError> {
    let payload = serde_json::to_vec(&claims)
        .map_err(|e| TrustError::Malformed(format!("cannot encode claims: {e}")))?;
    let sig = signer.sign(&input(domain, &payload));
    let text = format!(
        "{prefix}{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(sig)
    );
    Ok(Signed {
        claims,
        id: hex::encode(&Sha256::digest(&payload)),
        text,
    })
}

/// Splits and checks a token; `issuer_of` names the DID whose key must have signed it.
fn open<C: DeserializeOwned>(
    text: &str,
    prefix: &str,
    domain: &[u8],
    what: &'static str,
    version_of: impl Fn(&C) -> u32,
    issuer_of: impl Fn(&C) -> &str,
) -> Result<Signed<C>, TrustError> {
    let text = text.trim();
    if text.len() > MAX_TOKEN_BYTES {
        return Err(TrustError::Malformed(format!("{what} is too long")));
    }
    let rest = text
        .strip_prefix(prefix)
        .ok_or_else(|| TrustError::Malformed(format!("not a {what} (expected {prefix}…)")))?;
    let (p, s) = rest
        .split_once('.')
        .ok_or_else(|| TrustError::Malformed(format!("{what} has no signature")))?;
    let payload = URL_SAFE_NO_PAD
        .decode(p)
        .map_err(|_| TrustError::Malformed(format!("{what} payload is not base64url")))?;
    let sig: [u8; 64] = URL_SAFE_NO_PAD
        .decode(s)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| TrustError::Malformed(format!("{what} signature is not 64 bytes")))?;
    let claims: C = serde_json::from_slice(&payload)
        .map_err(|_| TrustError::Malformed(format!("{what} payload is not valid")))?;
    let v = version_of(&claims);
    if v != TOKEN_VERSION {
        return Err(TrustError::Newer {
            what,
            found: v.to_string(),
            supported: TOKEN_VERSION.to_string(),
        });
    }
    let key = public_key_from_did(issuer_of(&claims))
        .ok_or_else(|| TrustError::Malformed(format!("{what} issuer is not a did:key")))?;
    if !verify(&key, &input(domain, &payload), &sig) {
        return Err(TrustError::BadSignature);
    }
    Ok(Signed {
        claims,
        id: hex::encode(&Sha256::digest(&payload)),
        text: text.to_string(),
    })
}

/// A signed grant.
pub type Grant = Signed<GrantClaims>;
/// A signed invite.
pub type Invite = Signed<InviteClaims>;

impl Grant {
    /// Signs `claims` as `signer` (whose DID must be `claims.iss`).
    pub fn issue(signer: &dyn Signer, claims: GrantClaims) -> Result<Grant, TrustError> {
        if claims.iss != signer.did() {
            return Err(TrustError::WrongParty(
                "a grant is signed by its issuer".to_string(),
            ));
        }
        sign(signer, GRANT_PREFIX, GRANT_DOMAIN, claims)
    }

    /// Parses a token and verifies its signature (not its time, party or revocation).
    pub fn parse(text: &str) -> Result<Grant, TrustError> {
        open(
            text,
            GRANT_PREFIX,
            GRANT_DOMAIN,
            "grant",
            |c: &GrantClaims| c.v,
            |c| &c.iss,
        )
    }

    /// Checks the grant against `now` (unix seconds).
    pub fn check_time(&self, now: u64) -> Result<(), TrustError> {
        if self.claims.exp <= now {
            return Err(TrustError::Expired {
                exp: self.claims.exp,
            });
        }
        if self.claims.iat > now.saturating_add(SKEW_SECS) {
            return Err(TrustError::NotYetValid);
        }
        Ok(())
    }
}

impl Invite {
    /// Signs `claims` as `signer` (whose DID must be `claims.iss`).
    pub fn issue(signer: &dyn Signer, claims: InviteClaims) -> Result<Invite, TrustError> {
        if claims.iss != signer.did() {
            return Err(TrustError::WrongParty(
                "an invite is signed by its issuer".to_string(),
            ));
        }
        sign(signer, INVITE_PREFIX, INVITE_DOMAIN, claims)
    }

    /// Parses an invite and verifies its signature.
    pub fn parse(text: &str) -> Result<Invite, TrustError> {
        open(
            text,
            INVITE_PREFIX,
            INVITE_DOMAIN,
            "invite",
            |c: &InviteClaims| c.v,
            |c| &c.iss,
        )
    }

    /// Checks the invite against `now` (unix seconds).
    pub fn check_time(&self, now: u64) -> Result<(), TrustError> {
        if self.claims.exp <= now {
            return Err(TrustError::TicketExpired {
                exp: self.claims.exp,
            });
        }
        Ok(())
    }
}
