//! The trust events as audit rows (design §8).
//!
//! Pairing and grant events are rare and always recorded. Connections are not: the link probes
//! every peer every few seconds, and each probe opens and closes a connection, so
//! [`TrustEvent::ConnectionOpened`] and [`TrustEvent::ConnectionClosed`] are not recorded here;
//! the link's state changes (up, down, direct or relay) are, by the daemon's link prober.
//! Refusals are recorded, but a peer retrying a refused request every few seconds is throttled
//! to a couple of rows a minute, the next row counting what was dropped.
//!
//! A ticket is named by the first eight characters of its nonce, never the ticket or its token;
//! no grant token is ever passed on.

use super::events::{Side, TrustEvent, TrustEvents};
use std::sync::Arc;
use tasks_ledger::{AuditEvent, AuditLog, Clock, Throttle, action};

/// Rows allowed per refused (side, node, code) in one window.
const REFUSAL_BURST: u32 = 2;
/// The window for repeated refusals, ms.
const REFUSAL_WINDOW_MS: i64 = 60_000;

/// Writes trust events to the audit log.
pub struct AuditEvents {
    log: Arc<AuditLog>,
    clock: Arc<dyn Clock>,
    refusals: Throttle,
}

/// The short name a ticket goes by in the audit log.
fn ticket_id(nonce: &str) -> String {
    nonce.chars().take(8).collect()
}

fn side_name(s: Side) -> &'static str {
    match s {
        Side::Served => "served",
        Side::Dialed => "dialed",
    }
}

impl AuditEvents {
    /// Records into `log`; `clock` paces the refusal throttle.
    pub fn new(log: Arc<AuditLog>, clock: Arc<dyn Clock>) -> AuditEvents {
        AuditEvents {
            log,
            clock,
            refusals: Throttle::new(REFUSAL_WINDOW_MS, REFUSAL_BURST),
        }
    }

    /// Records a refusal unless the same one was just recorded; `ev` gets the dropped count.
    fn refusal(&self, key: String, ev: AuditEvent) {
        if let Some(dropped) = self.refusals.admit(&key, self.clock.now_ms()) {
            self.log
                .record(ev.detail_opt("suppressed", (dropped > 0).then_some(dropped)));
        }
    }
}

impl TrustEvents for AuditEvents {
    fn record(&self, event: TrustEvent) {
        match event {
            TrustEvent::TicketMinted {
                nonce,
                roles,
                both_ways,
                exp,
            } => self.log.record(
                AuditEvent::new(action::TICKET_MINTED)
                    .target(ticket_id(&nonce))
                    .detail("roles", roles)
                    .detail("both_ways", both_ways)
                    .detail("expires_at", exp),
            ),
            TrustEvent::TicketRedeemed {
                nonce,
                peer_did,
                peer_name,
                node_id,
            } => self.log.record(
                AuditEvent::new(action::TICKET_REDEEMED)
                    .by_actor(peer_did.clone())
                    .peer(peer_did)
                    .target(ticket_id(&nonce))
                    .detail("name", peer_name)
                    .detail("node_id", node_id),
            ),
            TrustEvent::TicketRefused {
                code,
                reason,
                node_id,
            } => {
                let key = format!("ticket:{}:{code}", node_id.as_deref().unwrap_or("-"));
                let mut ev = AuditEvent::new(action::TICKET_REFUSED)
                    .detail("code", code)
                    .detail("reason", reason);
                if let Some(n) = node_id {
                    ev = ev.target(n);
                }
                self.refusal(key, ev);
            }
            TrustEvent::TicketsReset { epoch } => self
                .log
                .record(AuditEvent::new(action::TICKETS_RESET).detail("epoch", epoch)),
            TrustEvent::GrantIssued {
                grant,
                direction,
                peer_did,
                role,
                exp,
            } => self.log.record(
                AuditEvent::new(action::GRANT_ISSUED)
                    .peer(peer_did)
                    .target(grant)
                    .detail("direction", direction)
                    .detail("role", role)
                    .detail("expires_at", exp),
            ),
            TrustEvent::GrantRenewed {
                old,
                new,
                direction,
                peer_did,
                role,
                exp,
            } => self.log.record(
                AuditEvent::new(action::GRANT_RENEWED)
                    .peer(peer_did)
                    .target(new)
                    .detail("replaces", old)
                    .detail("direction", direction)
                    .detail("role", role)
                    .detail("expires_at", exp),
            ),
            TrustEvent::GrantRevoked {
                grant,
                direction,
                peer_did,
                role,
            } => self.log.record(
                AuditEvent::new(action::GRANT_REVOKED)
                    .peer(peer_did)
                    .target(grant)
                    .detail("direction", direction)
                    .detail("role", role),
            ),
            // one per probe: the link prober records the state changes instead
            TrustEvent::ConnectionOpened { .. } | TrustEvent::ConnectionClosed { .. } => {}
            TrustEvent::ConnectionRefused {
                side,
                node_id,
                code,
                reason,
            } => {
                let key = format!("conn:{}:{node_id}:{code}", side_name(side));
                self.refusal(
                    key,
                    AuditEvent::new(action::CONN_REFUSED)
                        .target(node_id)
                        .detail("side", side_name(side))
                        .detail("code", code)
                        .detail("reason", reason),
                );
            }
        }
    }
}
