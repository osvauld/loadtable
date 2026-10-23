//! Roles a machine can grant another, and which requests each allows (design §3).

use super::error::TrustError;
use crate::proto::Body;
use serde::{Deserialize, Serialize};

/// What a grant lets its holder do on the issuer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Send jobs, sync checkouts and fetch outputs of its own jobs.
    Submit,
    /// Read status, the queue and logs of its own jobs.
    Observe,
    /// Pair, revoke and change peer policy (the owner's other machines only).
    Admin,
}

impl Role {
    /// The role's name as typed and stored.
    pub fn name(self) -> &'static str {
        match self {
            Role::Submit => "submit",
            Role::Observe => "observe",
            Role::Admin => "admin",
        }
    }

    /// Parses one role name.
    pub fn parse(s: &str) -> Result<Role, TrustError> {
        match s.trim() {
            "submit" => Ok(Role::Submit),
            "observe" => Ok(Role::Observe),
            "admin" => Ok(Role::Admin),
            other => Err(TrustError::Malformed(format!(
                "unknown role {other:?}; roles are submit, observe, admin"
            ))),
        }
    }

    /// Parses `submit,observe`: at least one role, duplicates folded, sorted.
    pub fn parse_list(s: &str) -> Result<Vec<Role>, TrustError> {
        let mut roles: Vec<Role> = s
            .split(',')
            .filter(|p| !p.trim().is_empty())
            .map(Role::parse)
            .collect::<Result<_, _>>()?;
        roles.sort();
        roles.dedup();
        if roles.is_empty() {
            return Err(TrustError::Malformed("name at least one role".to_string()));
        }
        Ok(roles)
    }

    /// True when this role covers what `need` asks.
    pub fn covers(self, need: Need) -> bool {
        matches!(
            (self, need),
            (Role::Admin, _) | (_, Need::Any) | (Role::Submit, Need::Submit)
        )
    }
}

/// What a request needs from the grants behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Any valid grant (hello, status, events of the peer's own jobs).
    Any,
    /// `submit`: jobs, sync and fetch.
    Submit,
}

impl Need {
    /// The role name to report in a refusal.
    pub fn role_name(self) -> &'static str {
        match self {
            Need::Any => "any role",
            Need::Submit => "submit",
        }
    }
}

/// What `body` needs. Unknown or reply-shaped messages need `submit`, the strictest.
pub fn need_of(body: &Body) -> Need {
    match body {
        Body::Hello(_)
        | Body::StatusRequest
        | Body::Subscribe { .. }
        | Body::JobMetricsRequest { .. } => Need::Any,
        _ => Need::Submit,
    }
}

/// The best check of a set of roles against a need.
pub fn allows(roles: &[Role], need: Need) -> bool {
    roles.iter().any(|r| r.covers(need))
}
