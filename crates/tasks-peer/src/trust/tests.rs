//! Trust rules over the in-memory network, which models identity: a connection's node id is the
//! device key hex of the machine that dialed. No test here uses the real network.

use super::*;
use crate::fake::{FakeNet, FakeTransport};
use crate::ids::PeerId;
use crate::proto::{Body, Hello, Refusal};
use crate::transport::{Connection, EventStream, Handler, PeerError, Transport};
use crate::trust::token::{Binds, GrantClaims, SCOPE_MACHINE, TOKEN_VERSION};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tasks_identity::{Identity, Signer, generate};
use tasks_ledger::{Clock, GrantDirection, ManualClock, MemoryTrust, TrustLedger};

const T0_MS: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;
const TIMEOUT: Duration = Duration::from_secs(5);

/// The "daemon" behind the gate: counts what got through.
#[derive(Default)]
struct Probe {
    seen: Mutex<Vec<&'static str>>,
    calls: AtomicUsize,
}

impl Handler for Probe {
    fn handle(&self, _from: &PeerId, msg: Body) -> Body {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut s) = self.seen.lock() {
            s.push(crate::fake::kind(&msg));
        }
        Body::Ok
    }
    fn events(
        &self,
        _from: &PeerId,
        remote: crate::ids::RemoteJobId,
        _from_offset: u64,
    ) -> Result<Box<dyn EventStream>, Refusal> {
        Err(Refusal::UnknownJob { remote })
    }
}

struct M {
    id: PeerId,
    ident: Arc<Identity>,
    node: Arc<TrustNode>,
    ledger: Arc<MemoryTrust>,
    events: Arc<RecordingEvents>,
    probe: Arc<Probe>,
}

fn addr() -> SelfAddr {
    SelfAddr {
        relay: Some("https://relay.example/".into()),
        direct_addrs: vec!["192.168.1.7:7843".into()],
    }
}

fn machine_on(
    name: &str,
    net: &FakeNet,
    clock: &Arc<ManualClock>,
    ident: Arc<Identity>,
    ledger: Arc<MemoryTrust>,
) -> M {
    let events = Arc::new(RecordingEvents::new());
    let node = TrustNode::new(
        ident.clone(),
        name,
        ledger.clone(),
        events.clone(),
        clock.clone(),
        TrustPolicy::default(),
    )
    .unwrap();
    let probe = Arc::new(Probe::default());
    let id = PeerId::new(&node.node_id()).unwrap();
    net.serve(
        &id,
        Arc::new(TrustedHandler::new(node.clone(), probe.clone())),
    );
    M {
        id,
        ident,
        node,
        ledger,
        events,
        probe,
    }
}

fn machine(name: &str, net: &FakeNet, clock: &Arc<ManualClock>) -> M {
    let (ident, _) = generate().unwrap();
    machine_on(
        name,
        net,
        clock,
        Arc::new(ident),
        Arc::new(MemoryTrust::new()),
    )
}

fn world() -> (FakeNet, Arc<ManualClock>, M, M) {
    let net = FakeNet::new();
    let clock = Arc::new(ManualClock::new(T0_MS));
    let worker = machine("nixos", &net, &clock);
    let laptop = machine("kurukkan", &net, &clock);
    (net, clock, worker, laptop)
}

fn tr(net: &FakeNet, m: &M) -> TrustedTransport<FakeTransport> {
    TrustedTransport::new(net.transport(&m.id), m.node.clone())
}

fn pair(net: &FakeNet, worker: &M, laptop: &M, roles: &[Role], both: bool) -> PairedPeer {
    let ticket = worker.node.invite(roles, both, None, &addr()).unwrap();
    laptop
        .node
        .accept(
            &ticket.to_text().unwrap(),
            &tr(net, laptop),
            &SelfAddr::default(),
            TIMEOUT,
        )
        .unwrap()
}

fn submit_body() -> Body {
    Body::Cancel {
        remote: crate::ids::RemoteJobId(1),
    }
}

fn raw_connect(net: &FakeNet, from: &PeerId, to: &M) -> Box<dyn Connection> {
    net.transport(from).connect(&to.id, TIMEOUT).unwrap()
}

fn refusal(r: Result<Body, PeerError>) -> Refusal {
    match r {
        Err(PeerError::Refused(r)) => r,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn unauthorized_reason(r: Result<Body, PeerError>) -> String {
    match refusal(r) {
        Refusal::Unauthorized { reason } => reason,
        other => panic!("expected unauthorized, got {other:?}"),
    }
}

// ---- tokens -----------------------------------------------------------------------------

fn claims_for(iss: &Identity, sub: &Identity, role: Role, now: u64) -> GrantClaims {
    GrantClaims {
        v: TOKEN_VERSION,
        iss: iss.did().to_string(),
        aud: iss.did().to_string(),
        sub: sub.did().to_string(),
        role,
        scope: SCOPE_MACHINE.to_string(),
        nonce: "n1".to_string(),
        iat: now,
        exp: now + 1000,
        binds: Binds {
            device: tasks_identity::hex::encode(&sub.device_public_key()),
        },
    }
}

// R-239
#[test]
fn r239_a_grant_round_trips_and_every_tamper_is_caught() {
    let (a, _) = generate().unwrap();
    let (b, _) = generate().unwrap();
    let g = Grant::issue(&a, claims_for(&a, &b, Role::Submit, 100)).unwrap();
    assert!(g.text().starts_with("g1."));
    let back = Grant::parse(g.text()).unwrap();
    assert_eq!(back.claims, g.claims);
    assert_eq!(back.id, g.id);
    // signed by someone else's key: the issuer in the payload no longer matches
    let (c, _) = generate().unwrap();
    assert!(matches!(
        Grant::issue(&c, claims_for(&a, &b, Role::Submit, 100)),
        Err(TrustError::WrongParty(_))
    ));
    // flipping a character of the payload or the signature fails verification or decoding
    let mut parts: Vec<String> = g.text().split('.').map(str::to_string).collect();
    let payload = parts[1].clone();
    let forged_claims = claims_for(&a, &b, Role::Admin, 100);
    parts[1] = base64_of(&serde_json::to_vec(&forged_claims).unwrap());
    assert_eq!(
        Grant::parse(&parts.join(".")).unwrap_err(),
        TrustError::BadSignature
    );
    parts[1] = payload;
    parts[2] = base64_of(&[7u8; 64]);
    assert_eq!(
        Grant::parse(&parts.join(".")).unwrap_err(),
        TrustError::BadSignature
    );
    for junk in ["", "g1.", "g1.a.b", "x1.a.b", "g1.%%%.%%%"] {
        assert!(
            matches!(Grant::parse(junk), Err(TrustError::Malformed(_))),
            "{junk}"
        );
    }
}

fn base64_of(b: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

// R-239
#[test]
fn r239_the_device_binding_is_inside_the_signed_payload_and_time_is_checked() {
    let (a, _) = generate().unwrap();
    let (b, _) = generate().unwrap();
    let g = Grant::issue(&a, claims_for(&a, &b, Role::Observe, 100)).unwrap();
    assert_eq!(
        g.claims.binds.device,
        tasks_identity::hex::encode(&b.device_public_key())
    );
    assert!(g.check_time(100).is_ok());
    assert_eq!(g.check_time(1100), Err(TrustError::Expired { exp: 1100 }));
    let late = Grant::issue(&a, claims_for(&a, &b, Role::Observe, 10_000)).unwrap();
    assert_eq!(late.check_time(9_000), Err(TrustError::NotYetValid));
    assert!(late.check_time(9_800).is_ok(), "within the clock skew");
    // a newer token version is refused by name
    let mut newer = claims_for(&a, &b, Role::Observe, 100);
    newer.v = 2;
    let payload = serde_json::to_vec(&newer).unwrap();
    let text = format!("g1.{}.{}", base64_of(&payload), base64_of(&[0u8; 64]));
    assert!(matches!(
        Grant::parse(&text),
        Err(TrustError::Newer { what: "grant", .. })
    ));
}

// R-241
#[test]
fn r241_roles_parse_and_each_covers_only_what_it_should() {
    assert_eq!(
        Role::parse_list("submit,observe,submit").unwrap(),
        vec![Role::Submit, Role::Observe]
    );
    assert!(Role::parse_list("").is_err());
    assert!(Role::parse_list("root").is_err());
    assert!(Role::Submit.covers(Need::Submit) && Role::Submit.covers(Need::Any));
    assert!(!Role::Observe.covers(Need::Submit) && Role::Observe.covers(Need::Any));
    assert!(Role::Admin.covers(Need::Submit));
    assert_eq!(need_of(&submit_body()), Need::Submit);
    assert_eq!(need_of(&Body::StatusRequest), Need::Any);
    assert_eq!(need_of(&Body::Hello(Hello::local("x"))), Need::Any);
}

// ---- tickets ----------------------------------------------------------------------------

// R-240
#[test]
fn r240_a_ticket_round_trips_as_one_word_and_names_the_machine() {
    let (_, _, worker, _) = world();
    let t = worker
        .node
        .invite(&[Role::Submit, Role::Observe], true, None, &addr())
        .unwrap();
    let text = t.to_text().unwrap();
    assert!(text.starts_with("tasks1."));
    assert!(!text.contains(char::is_whitespace));
    let back = Ticket::from_text(&format!("  {text}\n")).unwrap();
    assert_eq!(back, t);
    assert_eq!(back.node_did, worker.node.did());
    assert_eq!(back.node_id, worker.node.node_id());
    assert_eq!(back.relay.as_deref(), Some("https://relay.example/"));
    let invite = back.verify(T0_MS as u64 / 1000).unwrap();
    assert_eq!(invite.claims.roles, vec![Role::Submit, Role::Observe]);
    assert!(invite.claims.both_ways);
    assert_eq!(invite.claims.exp - invite.claims.iat, 15 * 60);
}

// R-240
#[test]
fn r240_a_newer_ticket_version_is_refused_by_name() {
    let err = Ticket::from_text("tasks2.eyJ2ZXJzaW9uIjoyfQ").unwrap_err();
    assert!(
        matches!(&err, TrustError::Newer { what: "ticket", found, .. } if found == "2"),
        "{err:?}"
    );
    assert!(err.to_string().contains("newer"));
    for junk in [
        "",
        "tasks",
        "tasks.",
        "tasks1",
        "tasks1.%%%",
        "osv1.abc",
        "tasks1.e30",
    ] {
        assert!(Ticket::from_text(junk).is_err(), "{junk}");
    }
    let huge = format!("tasks1.{}", "A".repeat(9000));
    assert!(matches!(
        Ticket::from_text(&huge),
        Err(TrustError::Malformed(_))
    ));
}

// R-240
#[test]
fn r240_visible_fields_that_disagree_with_the_signed_invite_are_an_error() {
    let (_, _, worker, _) = world();
    let now = T0_MS as u64 / 1000;
    let good = worker
        .node
        .invite(&[Role::Submit], false, None, &addr())
        .unwrap();
    let (other, _) = generate().unwrap();
    let mut t = good.clone();
    t.node_did = other.did().to_string();
    assert_eq!(
        t.verify(now).unwrap_err(),
        TrustError::TicketMismatch { field: "node_did" }
    );
    let mut t = good.clone();
    t.device_public_key = tasks_identity::hex::encode(&other.device_public_key());
    assert_eq!(
        t.verify(now).unwrap_err(),
        TrustError::TicketMismatch {
            field: "device_public_key"
        }
    );
    let mut t = good.clone();
    t.node_id = tasks_identity::hex::encode(&other.device_public_key());
    assert_eq!(
        t.verify(now).unwrap_err(),
        TrustError::TicketMismatch { field: "node_id" }
    );
    let mut t = good.clone();
    t.name = "evil".into();
    assert_eq!(
        t.verify(now).unwrap_err(),
        TrustError::TicketMismatch { field: "name" }
    );
    assert!(good.verify(now).is_ok());
}

// R-245
#[test]
fn r245_ticket_lifetimes_are_bounded_and_expire() {
    let (_, clock, worker, _) = world();
    let a = SelfAddr::default();
    assert!(
        worker
            .node
            .invite(&[Role::Submit], false, Some(0), &a)
            .is_err()
    );
    assert!(
        worker
            .node
            .invite(&[Role::Submit], false, Some(25 * 3600), &a)
            .is_err()
    );
    assert!(worker.node.invite(&[], false, None, &a).is_err());
    let t = worker
        .node
        .invite(&[Role::Submit], false, Some(60), &a)
        .unwrap();
    let now = clock.now_ms() as u64 / 1000;
    assert!(t.verify(now + 59).is_ok());
    assert!(matches!(
        t.verify(now + 61),
        Err(TrustError::TicketExpired { .. })
    ));
}

// ---- pairing ----------------------------------------------------------------------------

// R-242
#[test]
fn r242_pairing_dials_proves_and_mints_a_grant_bound_to_the_redeemers_device() {
    let (net, _, worker, laptop) = world();
    let paired = pair(
        &net,
        &worker,
        &laptop,
        &[Role::Submit, Role::Observe],
        false,
    );
    assert_eq!(paired.name, "nixos");
    assert_eq!(paired.did, worker.node.did());
    // the worker issued one grant per role, bound to the laptop's device key
    let issued: Vec<_> = worker
        .ledger
        .grants()
        .unwrap()
        .into_iter()
        .filter(|g| g.direction == GrantDirection::Issued)
        .collect();
    assert_eq!(issued.len(), 2);
    for g in &issued {
        let parsed = Grant::parse(&g.token).unwrap();
        assert_eq!(parsed.claims.sub, laptop.node.did());
        assert_eq!(parsed.claims.binds.device, laptop.node.node_id());
        assert_eq!(parsed.claims.iss, worker.node.did());
        assert_eq!(parsed.claims.exp - parsed.claims.iat, 365 * 86_400);
    }
    // the laptop holds the same two, one way only
    let held: Vec<_> = laptop
        .ledger
        .grants()
        .unwrap()
        .into_iter()
        .filter(|g| g.direction == GrantDirection::Held)
        .collect();
    assert_eq!(held.len(), 2);
    assert!(
        laptop
            .ledger
            .grants()
            .unwrap()
            .iter()
            .all(|g| g.direction == GrantDirection::Held)
    );
    // both sides know the other by name and DID
    assert_eq!(worker.ledger.peers().unwrap()[0].name, "kurukkan");
    assert_eq!(worker.ledger.peers().unwrap()[0].did, laptop.node.did());
    assert_eq!(
        laptop.ledger.peers().unwrap()[0].relay.as_deref(),
        Some("https://relay.example/")
    );
    // and the laptop can now talk to the worker
    let t = tr(&net, &laptop);
    let mut c = t.connect(&worker.id, TIMEOUT).unwrap();
    assert!(matches!(c.request(Body::StatusRequest), Ok(Body::Ok)));
    assert!(matches!(c.request(submit_body()), Ok(Body::Ok)));
}

// R-243
#[test]
fn r243_a_both_ways_ticket_grants_each_side_the_roles_on_the_other() {
    let (net, _, worker, laptop) = world();
    pair(&net, &worker, &laptop, &[Role::Submit, Role::Observe], true);
    // laptop -> worker
    let mut c = tr(&net, &laptop).connect(&worker.id, TIMEOUT).unwrap();
    assert!(matches!(c.request(submit_body()), Ok(Body::Ok)));
    drop(c);
    // worker -> laptop, on the reverse grants the laptop minted during the same exchange
    let mut c = tr(&net, &worker).connect(&laptop.id, TIMEOUT).unwrap();
    assert!(matches!(c.request(submit_body()), Ok(Body::Ok)));
    assert_eq!(laptop.probe.calls.load(Ordering::SeqCst), 1);
    // a one-way pairing does not give the reverse
    let (net2, _, w2, l2) = world();
    pair(&net2, &w2, &l2, &[Role::Submit], false);
    assert!(matches!(
        tr(&net2, &w2).connect(&l2.id, TIMEOUT),
        Err(PeerError::Refused(Refusal::Unauthorized { .. }))
    ));
}

// R-244
#[test]
fn r244_a_ticket_works_once_also_after_the_inviter_restarts() {
    let (net, clock, worker, laptop) = world();
    let ticket = worker
        .node
        .invite(&[Role::Submit], false, None, &addr())
        .unwrap();
    let text = ticket.to_text().unwrap();
    laptop
        .node
        .accept(&text, &tr(&net, &laptop), &SelfAddr::default(), TIMEOUT)
        .unwrap();
    // a third machine tries the same ticket
    let third = machine("third", &net, &clock);
    let err = third
        .node
        .accept(&text, &tr(&net, &third), &SelfAddr::default(), TIMEOUT)
        .unwrap_err();
    assert!(err.to_string().contains("already redeemed"), "{err}");
    assert!(third.ledger.grants().unwrap().is_empty());
    // the worker restarts on the same ledger: the nonce is still spent
    let worker2 = machine_on(
        "nixos",
        &net,
        &clock,
        worker.ident.clone(),
        worker.ledger.clone(),
    );
    let fourth = machine("fourth", &net, &clock);
    let err = fourth
        .node
        .accept(&text, &tr(&net, &fourth), &SelfAddr::default(), TIMEOUT)
        .unwrap_err();
    assert!(err.to_string().contains("already redeemed"), "{err}");
    assert_eq!(worker2.ledger.peers().unwrap().len(), 1);
    assert!(
        worker2
            .events
            .events()
            .iter()
            .all(|e| !matches!(e, TrustEvent::TicketRedeemed { .. }))
    );
}

// R-245
#[test]
fn r245_an_expired_ticket_is_refused_by_the_redeemer_and_by_the_inviter() {
    let (net, clock, worker, laptop) = world();
    let ticket = worker
        .node
        .invite(&[Role::Submit], false, Some(60), &addr())
        .unwrap();
    let text = ticket.to_text().unwrap();
    clock.advance(61_000);
    let err = laptop
        .node
        .accept(&text, &tr(&net, &laptop), &SelfAddr::default(), TIMEOUT)
        .unwrap_err();
    assert!(matches!(err, TrustError::TicketExpired { .. }), "{err:?}");
    assert!(worker.ledger.grants().unwrap().is_empty());
    // the inviter checks its own clock too: a redeemer with a slow clock still fails
    let slow = Arc::new(ManualClock::new(T0_MS));
    let laptop2 = machine_on(
        "slowpoke",
        &net,
        &slow,
        Arc::new(generate().unwrap().0),
        Arc::new(MemoryTrust::new()),
    );
    let err = laptop2
        .node
        .accept(&text, &tr(&net, &laptop2), &SelfAddr::default(), TIMEOUT)
        .unwrap_err();
    assert!(matches!(err, TrustError::NotAuthorised(_)), "{err:?}");
    assert!(err.to_string().contains("expired"), "{err}");
    assert!(worker.ledger.grants().unwrap().is_empty());
}

// R-245
#[test]
fn r245_pair_reset_invalidates_every_unredeemed_ticket_and_new_ones_work() {
    let (net, _, worker, laptop) = world();
    let old1 = worker
        .node
        .invite(&[Role::Submit], false, None, &addr())
        .unwrap();
    let old2 = worker
        .node
        .invite(&[Role::Observe], false, None, &addr())
        .unwrap();
    assert_eq!(worker.node.reset_tickets().unwrap(), 1);
    for t in [&old1, &old2] {
        let err = laptop
            .node
            .accept(
                &t.to_text().unwrap(),
                &tr(&net, &laptop),
                &SelfAddr::default(),
                TIMEOUT,
            )
            .unwrap_err();
        assert!(err.to_string().contains("pair reset"), "{err}");
    }
    assert!(worker.ledger.grants().unwrap().is_empty());
    let fresh = worker
        .node
        .invite(&[Role::Submit], false, None, &addr())
        .unwrap();
    laptop
        .node
        .accept(
            &fresh.to_text().unwrap(),
            &tr(&net, &laptop),
            &SelfAddr::default(),
            TIMEOUT,
        )
        .unwrap();
    // the epoch survives a restart
    let worker2 = machine_on(
        "nixos",
        &net,
        &Arc::new(ManualClock::new(T0_MS)),
        worker.ident.clone(),
        worker.ledger.clone(),
    );
    let again = worker2
        .node
        .invite(&[Role::Submit], false, None, &addr())
        .unwrap();
    let c = Invite::parse(&again.invite_token).unwrap();
    assert_eq!(c.claims.epoch, 1);
}

// R-246
#[test]
fn r246_a_failed_attempt_does_not_burn_the_ticket() {
    let (net, _, worker, laptop) = world();
    let ticket = worker
        .node
        .invite(&[Role::Submit], false, None, &addr())
        .unwrap();
    // an attacker holding the ticket text but not the DID key it claims
    let (victim, _) = generate().unwrap();
    let (me, _) = generate().unwrap();
    let id = PeerId::new(&tasks_identity::hex::encode(&me.device_public_key())).unwrap();
    let mut c = raw_connect(&net, &id, &worker);
    let Ok(Body::PairChallenge { challenge }) = c.request(Body::PairBegin) else {
        panic!("challenge expected")
    };
    let _ = challenge;
    let r = c.request(Body::PairRedeem(wire::PairRedeem {
        invite: ticket.invite_token.clone(),
        did: victim.did().to_string(),
        name: "attacker".into(),
        challenge_sig: tasks_identity::hex::encode(&[1u8; 64]),
        relay: None,
        direct_addrs: vec![],
        reverse: vec![],
    }));
    assert!(matches!(refusal(r), Refusal::Unauthorized { .. }));
    assert!(worker.ledger.grants().unwrap().is_empty());
    // the real laptop still redeems it
    laptop
        .node
        .accept(
            &ticket.to_text().unwrap(),
            &tr(&net, &laptop),
            &SelfAddr::default(),
            TIMEOUT,
        )
        .unwrap();
    assert!(worker.events.events().iter().any(|e| matches!(
        e,
        TrustEvent::TicketRefused { code, .. } if code == "bad_signature"
    )));
}

// R-246
#[test]
fn r246_a_redemption_needs_a_fresh_challenge_each_time() {
    let (net, _, worker, laptop) = world();
    let ticket = worker
        .node
        .invite(&[Role::Submit], false, None, &addr())
        .unwrap();
    let mut c = raw_connect(&net, &laptop.id, &worker);
    // no PairBegin first
    let redeem = |sig: [u8; 64]| wire::PairRedeem {
        invite: ticket.invite_token.clone(),
        did: laptop.node.did().to_string(),
        name: "kurukkan".into(),
        challenge_sig: tasks_identity::hex::encode(&sig),
        relay: None,
        direct_addrs: vec![],
        reverse: vec![],
    };
    let r = c.request(Body::PairRedeem(redeem([0; 64])));
    assert!(
        matches!(refusal(r), Refusal::PairingRefused { reason } if reason.contains("challenge"))
    );
    // a signature over one challenge does not satisfy the next
    let Ok(Body::PairChallenge { challenge }) = c.request(Body::PairBegin) else {
        panic!()
    };
    let first: [u8; 32] = tasks_identity::hex::decode(&challenge).unwrap();
    let Ok(Body::PairChallenge { challenge: second }) = c.request(Body::PairBegin) else {
        panic!()
    };
    assert_ne!(challenge, second);
    let invite = Invite::parse(&ticket.invite_token).unwrap();
    let old_sig = laptop.ident.sign(&super::node::pair_message(
        &first,
        &invite.claims.nonce,
        laptop.node.node_id().as_str(),
    ));
    let r = c.request(Body::PairRedeem(redeem(old_sig)));
    assert!(matches!(refusal(r), Refusal::Unauthorized { .. }));
    assert!(worker.ledger.grants().unwrap().is_empty());
}

// R-246
#[test]
fn r246_the_answering_machine_must_be_the_one_the_ticket_names() {
    let (net, clock, worker, laptop) = world();
    let ticket = worker
        .node
        .invite(&[Role::Submit], false, None, &addr())
        .unwrap();
    // an impostor answers at the worker's node id
    let impostor = machine("impostor", &net, &clock);
    net.serve(
        &worker.id,
        Arc::new(TrustedHandler::new(
            impostor.node.clone(),
            impostor.probe.clone(),
        )),
    );
    let err = laptop
        .node
        .accept(
            &ticket.to_text().unwrap(),
            &tr(&net, &laptop),
            &SelfAddr::default(),
            TIMEOUT,
        )
        .unwrap_err();
    assert!(matches!(err, TrustError::NotAuthorised(_)), "{err:?}");
    assert!(laptop.ledger.peers().unwrap().is_empty());
    assert!(laptop.ledger.grants().unwrap().is_empty());
    // a ticket from this very machine is refused before any dialing
    let own = laptop
        .node
        .invite(&[Role::Submit], false, None, &addr())
        .unwrap();
    assert!(
        laptop
            .node
            .accept(
                &own.to_text().unwrap(),
                &tr(&net, &laptop),
                &SelfAddr::default(),
                TIMEOUT
            )
            .is_err()
    );
}

// ---- admission --------------------------------------------------------------------------

// R-247
#[test]
fn r247_nothing_is_read_before_a_grant_is_presented() {
    let (net, _, worker, _) = world();
    let (stranger, _) = generate().unwrap();
    let sid = PeerId::new(&tasks_identity::hex::encode(&stranger.device_public_key())).unwrap();
    for msg in [
        Body::Hello(Hello::local("x")),
        Body::StatusRequest,
        submit_body(),
        Body::Renew {
            grant: "g1.x.y".into(),
        },
    ] {
        // a refusal closes the connection, so each message gets a fresh one
        let mut c = raw_connect(&net, &sid, &worker);
        let reason = unauthorized_reason(c.request(msg));
        assert!(reason.contains("no grant"), "{reason}");
        assert!(matches!(
            c.request(Body::StatusRequest),
            Err(PeerError::LinkLost(_))
        ));
    }
    let mut c = raw_connect(&net, &sid, &worker);
    assert!(matches!(
        c.subscribe(crate::ids::RemoteJobId(1), 0),
        Err(PeerError::Refused(Refusal::Unauthorized { .. }))
    ));
    assert_eq!(worker.probe.calls.load(Ordering::SeqCst), 0);
    assert!(worker.events.events().iter().any(|e| matches!(
        e,
        TrustEvent::ConnectionRefused {
            side: Side::Served,
            ..
        }
    )));
}

// R-247
#[test]
fn r247_a_machine_without_a_grant_cannot_even_connect_through_the_trusted_transport() {
    let (net, _, worker, laptop) = world();
    let r = tr(&net, &laptop).connect(&worker.id, TIMEOUT);
    assert!(matches!(
        r,
        Err(PeerError::Refused(Refusal::Unauthorized { .. }))
    ));
    assert!(laptop.events.events().iter().any(|e| matches!(
        e,
        TrustEvent::ConnectionRefused {
            side: Side::Dialed,
            ..
        }
    )));
}

// R-248
#[test]
fn r248_a_stolen_grant_is_useless_from_another_device() {
    let (net, _, worker, laptop) = world();
    pair(&net, &worker, &laptop, &[Role::Submit], false);
    let token = laptop.node.held_for(&worker.node.node_id())[0]
        .text()
        .to_string();
    let (thief, _) = generate().unwrap();
    let tid = PeerId::new(&tasks_identity::hex::encode(&thief.device_public_key())).unwrap();
    let mut c = raw_connect(&net, &tid, &worker);
    let reason = unauthorized_reason(c.request(Body::Present {
        grants: vec![token],
    }));
    assert!(reason.contains("bound to device"), "{reason}");
    assert!(matches!(
        c.request(submit_body()),
        Err(PeerError::LinkLost(_)) | Err(PeerError::Refused(_))
    ));
    assert_eq!(worker.probe.calls.load(Ordering::SeqCst), 0);
    assert!(worker.events.events().iter().any(|e| matches!(
        e,
        TrustEvent::ConnectionRefused { code, .. } if code == "wrong_device"
    )));
}

// R-248
#[test]
fn r248_unknown_foreign_and_garbage_grants_are_each_refused_with_their_reason() {
    let (net, _, worker, laptop) = world();
    let now = T0_MS as u64 / 1000;
    let present = |tokens: Vec<String>| {
        let mut c = raw_connect(&net, &laptop.id, &worker);
        unauthorized_reason(c.request(Body::Present { grants: tokens }))
    };
    // validly signed by the worker but never recorded
    let unrecorded = Grant::issue(
        &*worker.ident,
        claims_for(&worker.ident, &laptop.ident, Role::Admin, now),
    )
    .unwrap();
    assert!(present(vec![unrecorded.text().to_string()]).contains("no such grant"));
    // signed by some other machine
    let (other, _) = generate().unwrap();
    let foreign =
        Grant::issue(&other, claims_for(&other, &laptop.ident, Role::Submit, now)).unwrap();
    assert!(present(vec![foreign.text().to_string()]).contains("not to this machine"));
    // junk and nothing at all
    assert!(!present(vec!["garbage".into()]).is_empty());
    assert!(present(vec![]).contains("no grant"));
}

// R-248
#[test]
fn r248_a_grant_past_its_expiry_is_refused() {
    let (net, clock, worker, laptop) = world();
    pair(&net, &worker, &laptop, &[Role::Submit], false);
    clock.advance(366 * DAY);
    let r = tr(&net, &laptop).connect(&worker.id, TIMEOUT);
    assert!(matches!(
        r,
        Err(PeerError::Refused(Refusal::Unauthorized { .. }))
    ));
    // presenting the expired token directly gives the server's reason
    let g = Grant::parse(&worker.ledger.grants().unwrap()[0].token).unwrap();
    let mut c = raw_connect(&net, &laptop.id, &worker);
    let reason = unauthorized_reason(c.request(Body::Present {
        grants: vec![g.text().to_string()],
    }));
    assert!(reason.contains("expired"), "{reason}");
}

// R-251
#[test]
fn r251_a_grant_that_expires_during_a_connection_fails_the_next_request() {
    let (net, clock, worker, laptop) = world();
    pair(&net, &worker, &laptop, &[Role::Submit], false);
    let mut c = tr(&net, &laptop).connect(&worker.id, TIMEOUT).unwrap();
    assert!(matches!(c.request(submit_body()), Ok(Body::Ok)));
    clock.advance(366 * DAY);
    let reason = unauthorized_reason(c.request(submit_body()));
    assert!(reason.contains("expired"), "{reason}");
    assert_eq!(worker.probe.calls.load(Ordering::SeqCst), 1);
}

// R-249
#[test]
fn r249_each_request_is_checked_against_the_grants_role() {
    let (net, clock, worker, _) = world();
    let observer = machine("watcher", &net, &clock);
    let submitter = machine("builder", &net, &clock);
    let admin = machine("owner", &net, &clock);
    pair(&net, &worker, &observer, &[Role::Observe], false);
    pair(&net, &worker, &submitter, &[Role::Submit], false);
    pair(&net, &worker, &admin, &[Role::Admin], false);
    // observe: status and hello yes; submit/cancel/sync/fetch no
    let mut c = tr(&net, &observer).connect(&worker.id, TIMEOUT).unwrap();
    assert!(matches!(c.request(Body::StatusRequest), Ok(Body::Ok)));
    assert!(matches!(
        c.request(Body::Hello(Hello::local("w"))),
        Ok(Body::Ok)
    ));
    let r = refusal(c.request(submit_body()));
    assert!(
        matches!(&r, Refusal::RoleDenied { need, have } if need == "submit" && have == &["observe"]),
        "{r:?}"
    );
    let before = worker.probe.calls.load(Ordering::SeqCst);
    // submit: jobs yes
    let mut c = tr(&net, &submitter).connect(&worker.id, TIMEOUT).unwrap();
    assert!(matches!(c.request(submit_body()), Ok(Body::Ok)));
    assert!(matches!(c.request(Body::StatusRequest), Ok(Body::Ok)));
    // admin covers everything the others do
    let mut c = tr(&net, &admin).connect(&worker.id, TIMEOUT).unwrap();
    assert!(matches!(c.request(submit_body()), Ok(Body::Ok)));
    assert_eq!(worker.probe.calls.load(Ordering::SeqCst), before + 3);
}

// R-250
#[test]
fn r250_revoking_fails_the_next_request_closes_the_live_connection_and_deletes_nothing() {
    let (net, clock, worker, laptop) = world();
    let other = machine("other", &net, &clock);
    pair(
        &net,
        &worker,
        &laptop,
        &[Role::Submit, Role::Observe],
        false,
    );
    pair(&net, &worker, &other, &[Role::Submit], false);
    let mut live = tr(&net, &laptop).connect(&worker.id, TIMEOUT).unwrap();
    let mut keep = tr(&net, &other).connect(&worker.id, TIMEOUT).unwrap();
    assert!(matches!(live.request(submit_body()), Ok(Body::Ok)));
    let rows_before = worker.ledger.grants().unwrap().len();
    let report = worker.node.revoke_peer("kurukkan").unwrap();
    assert_eq!(report.revoked, 2);
    assert_eq!(report.closed, 1);
    // the live connection is closed, not merely refused
    let err = live.request(submit_body()).unwrap_err();
    assert!(
        matches!(&err, PeerError::LinkLost(m) if m.contains("grant revoked")),
        "{err:?}"
    );
    // a new connection with the same grants fails with the reason
    let r = tr(&net, &laptop).connect(&worker.id, TIMEOUT);
    assert!(matches!(
        r,
        Err(PeerError::Refused(Refusal::Unauthorized { .. }))
    ));
    // the other peer is untouched
    assert!(matches!(keep.request(submit_body()), Ok(Body::Ok)));
    // revoking deletes nothing: the rows stay, marked revoked
    let rows = worker.ledger.grants().unwrap();
    assert_eq!(rows.len(), rows_before);
    assert_eq!(rows.iter().filter(|g| g.state == "revoked").count(), 2);
    // revoking again changes nothing
    assert_eq!(worker.node.revoke_peer("kurukkan").unwrap().revoked, 0);
    assert!(matches!(
        worker.node.revoke_peer("nobody"),
        Err(TrustError::NoPeer(_))
    ));
    let revoked_events = worker
        .events
        .events()
        .into_iter()
        .filter(|e| matches!(e, TrustEvent::GrantRevoked { .. }))
        .count();
    assert_eq!(revoked_events, 2);
}

// R-250
#[test]
fn r250_a_revoked_grant_stays_revoked_after_a_restart() {
    let (net, clock, worker, laptop) = world();
    pair(&net, &worker, &laptop, &[Role::Submit], false);
    worker.node.revoke_peer("kurukkan").unwrap();
    let worker2 = machine_on(
        "nixos",
        &net,
        &clock,
        worker.ident.clone(),
        worker.ledger.clone(),
    );
    let r = tr(&net, &laptop).connect(&worker2.id, TIMEOUT);
    assert!(matches!(
        r,
        Err(PeerError::Refused(Refusal::Unauthorized { .. }))
    ));
    let p = &worker2.node.peers()[0];
    assert!(p.revoked && p.inbound.is_empty());
}

// R-252
#[test]
fn r252_a_grant_near_its_end_is_renewed_over_the_live_connection() {
    let (net, clock, worker, laptop) = world();
    pair(&net, &worker, &laptop, &[Role::Submit], false);
    let old = laptop.node.held_for(&worker.node.node_id())[0].clone();
    // far from expiry: connecting renews nothing
    drop(tr(&net, &laptop).connect(&worker.id, TIMEOUT).unwrap());
    assert_eq!(laptop.node.held_for(&worker.node.node_id())[0].id, old.id);
    // 340 days on: 25 days left, inside the 30-day window
    clock.advance(340 * DAY);
    let mut c = tr(&net, &laptop).connect(&worker.id, TIMEOUT).unwrap();
    let now = clock.now_ms() as u64 / 1000;
    let new = laptop.node.held_for(&worker.node.node_id());
    assert_eq!(new.len(), 1);
    assert_ne!(new[0].id, old.id);
    assert_eq!(new[0].claims.exp, now + 365 * 86_400);
    assert_eq!(new[0].claims.role, Role::Submit);
    assert_eq!(new[0].claims.binds, old.claims.binds);
    // the same live connection keeps working on the renewed grant
    assert!(matches!(c.request(submit_body()), Ok(Body::Ok)));
    // old is marked renewed on both sides, never deleted
    for m in [&worker, &laptop] {
        let rows = m.ledger.grants().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|g| g.id == old.id && g.state == "renewed"));
        assert!(rows.iter().any(|g| g.id == new[0].id && g.state == "live"));
    }
    // the old token no longer admits
    let mut raw = raw_connect(&net, &laptop.id, &worker);
    let reason = unauthorized_reason(raw.request(Body::Present {
        grants: vec![old.text().to_string()],
    }));
    assert!(reason.contains("renewed"), "{reason}");
    let actions = laptop.events.actions();
    assert!(actions.contains(&"grant.renewed"), "{actions:?}");
    assert!(worker.events.actions().contains(&"grant.renewed"));
}

// R-252
#[test]
fn r252_only_the_issuer_renews_and_only_a_presented_unexpired_grant() {
    let (net, clock, worker, laptop) = world();
    pair(&net, &worker, &laptop, &[Role::Submit], false);
    let other = machine("other", &net, &clock);
    pair(&net, &worker, &other, &[Role::Submit], false);
    let theirs = other.node.held_for(&worker.node.node_id())[0]
        .text()
        .to_string();
    // the laptop's connection cannot renew the other machine's grant
    let mut c = tr(&net, &laptop).connect(&worker.id, TIMEOUT).unwrap();
    let r = refusal(c.request(Body::Renew { grant: theirs }));
    assert!(matches!(r, Refusal::Unauthorized { .. }), "{r:?}");
    // after expiry nothing renews: the offline machine needs a new pairing
    clock.advance(400 * DAY);
    assert!(tr(&net, &laptop).connect(&worker.id, TIMEOUT).is_err());
    assert!(laptop.node.held_for(&worker.node.node_id()).is_empty());
}

// R-253
#[test]
fn r253_grants_inside_the_alert_window_are_listed_until_they_are_renewed() {
    let (net, clock, worker, laptop) = world();
    pair(&net, &worker, &laptop, &[Role::Submit], true);
    assert!(laptop.node.expiring().is_empty());
    clock.advance((365 - 6) * DAY);
    let e = laptop.node.expiring();
    assert_eq!(e.len(), 2, "one held, one issued: {e:?}");
    assert!(
        e.iter()
            .all(|g| g.peer == "nixos" && g.left_secs <= 7 * 86_400)
    );
    assert!(e.iter().any(|g| g.direction == "held"));
    // a successful connection renews the held grant and clears its entry
    drop(tr(&net, &laptop).connect(&worker.id, TIMEOUT).unwrap());
    let e = laptop.node.expiring();
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].direction, "issued");
    clock.advance(10 * DAY);
    assert!(laptop.node.expiring().iter().any(|g| g.left_secs < 0));
}

// R-254
#[test]
fn r254_every_trust_event_goes_through_the_one_sink() {
    let (net, _, worker, laptop) = world();
    pair(&net, &worker, &laptop, &[Role::Submit], false);
    let mut c = tr(&net, &laptop).connect(&worker.id, TIMEOUT).unwrap();
    assert!(matches!(c.request(submit_body()), Ok(Body::Ok)));
    drop(c);
    worker.node.revoke_peer("kurukkan").unwrap();
    worker.node.reset_tickets().unwrap();
    assert_eq!(
        worker.events.actions(),
        [
            "pair.ticket_minted",
            "pair.ticket_redeemed",
            "grant.issued",
            "conn.opened",
            "conn.closed",
            "grant.revoked",
            "pair.tickets_reset",
        ]
    );
    assert_eq!(
        laptop.events.actions(),
        ["grant.issued", "conn.opened"],
        "the redeemer records the grant it holds and the connection it dialed"
    );
    // the no-op sink accepts events silently
    NoEvents.record(TrustEvent::TicketsReset { epoch: 1 });
}

// R-254
#[test]
fn r254_refused_redemptions_carry_their_reason_code() {
    let (net, _, worker, laptop) = world();
    let t = worker
        .node
        .invite(&[Role::Submit], false, None, &addr())
        .unwrap();
    worker.node.reset_tickets().unwrap();
    let _ = laptop.node.accept(
        &t.to_text().unwrap(),
        &tr(&net, &laptop),
        &SelfAddr::default(),
        TIMEOUT,
    );
    let refused: Vec<_> = worker
        .events
        .events()
        .into_iter()
        .filter_map(|e| match e {
            TrustEvent::TicketRefused { code, node_id, .. } => Some((code, node_id)),
            _ => None,
        })
        .collect();
    assert_eq!(
        refused,
        [("ticket_reset".to_string(), Some(laptop.node.node_id()))]
    );
    assert!(laptop.events.actions().contains(&"pair.ticket_refused"));
}

// R-255
#[test]
fn r255_peers_show_did_name_roles_each_way_and_expiry() {
    let (net, _, worker, laptop) = world();
    assert!(laptop.node.peers().is_empty());
    pair(
        &net,
        &worker,
        &laptop,
        &[Role::Submit, Role::Observe],
        false,
    );
    let p = laptop.node.peers();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].name, "nixos");
    assert_eq!(p[0].did, worker.node.did());
    assert_eq!(p[0].node_id, worker.node.node_id());
    let out: Vec<Role> = p[0].outbound.iter().map(|r| r.role).collect();
    assert!(out.contains(&Role::Submit) && out.contains(&Role::Observe));
    assert!(p[0].inbound.is_empty());
    assert_eq!(p[0].expires, Some(T0_MS as u64 / 1000 + 365 * 86_400));
    assert!(!p[0].revoked);
    let w = worker.node.peers();
    assert!(w[0].outbound.is_empty() && w[0].inbound.len() == 2);
    assert_eq!(laptop.node.paired_peers().len(), 1);
    assert!(
        worker.node.paired_peers().is_empty(),
        "the worker holds no grant from the laptop"
    );
    laptop.node.revoke_peer("nixos").unwrap();
    assert!(laptop.node.paired_peers().is_empty());
    assert!(laptop.node.peers()[0].revoked);
}

// R-256
#[test]
fn r256_a_name_belongs_to_one_did_and_re_pairing_replaces_older_grants() {
    let (net, clock, worker, laptop) = world();
    pair(&net, &worker, &laptop, &[Role::Submit], false);
    let first = laptop.node.held_for(&worker.node.node_id())[0].id.clone();
    // pairing again with the same machine replaces its live grant of that role
    pair(&net, &worker, &laptop, &[Role::Submit], false);
    let held = laptop.node.held_for(&worker.node.node_id());
    assert_eq!(held.len(), 1);
    assert_ne!(held[0].id, first);
    assert_eq!(laptop.node.peers().len(), 1);
    // a different machine that calls itself "kurukkan" is refused
    let imposter = machine("kurukkan", &net, &clock);
    let t = worker
        .node
        .invite(&[Role::Submit], false, None, &addr())
        .unwrap();
    let err = imposter
        .node
        .accept(
            &t.to_text().unwrap(),
            &tr(&net, &imposter),
            &SelfAddr::default(),
            TIMEOUT,
        )
        .unwrap_err();
    assert!(
        matches!(err, TrustError::NotAuthorised(m) if m.contains("already used")),
        "name taken"
    );
    assert_eq!(worker.node.peers().len(), 1);
}

// R-256
#[test]
fn r256_names_are_made_safe_for_labels_and_directories() {
    assert_eq!(sanitize_name("kurukkan"), "kurukkan");
    assert_eq!(sanitize_name("my laptop/../x"), "my-laptop-..-x");
    assert_eq!(sanitize_name(""), "peer");
    assert_eq!(sanitize_name(".."), "peer");
    assert_eq!(sanitize_name(&"a".repeat(100)).len(), 64);
}

// R-255
#[test]
fn r255_latest_addresses_are_remembered_for_the_next_dial() {
    let (net, _, worker, laptop) = world();
    pair(&net, &worker, &laptop, &[Role::Submit], false);
    laptop
        .node
        .remember_addrs(&worker.node.node_id(), None, vec!["10.0.0.9:1".into()])
        .unwrap();
    let hints = AddressBook::hints(&*laptop.node, &worker.id).unwrap();
    assert_eq!(hints.direct_addrs, vec!["10.0.0.9:1".to_string()]);
    assert_eq!(hints.relay, None);
    assert!(AddressBook::hints(&*laptop.node, &PeerId::new("unknown").unwrap()).is_none());
}

// ---- audit -----------------------------------------------------------------------------

/// A worker whose trust events go to a recorder and to an audit log over `audit`.
fn audited_worker(
    net: &FakeNet,
    clock: &Arc<ManualClock>,
    audit: &Arc<tasks_ledger::MemoryAudit>,
) -> (M, Arc<tasks_ledger::AuditLog>) {
    let (ident, _) = generate().unwrap();
    let ident = Arc::new(ident);
    let log = Arc::new(
        tasks_ledger::AuditLog::new(audit.clone(), clock.clone(), ident.did())
            .with_signer(ident.clone()),
    );
    let events = Arc::new(RecordingEvents::new());
    let sinks: Vec<Arc<dyn TrustEvents>> = vec![
        events.clone(),
        Arc::new(AuditEvents::new(log.clone(), clock.clone())),
    ];
    let ledger = Arc::new(MemoryTrust::new());
    let node = TrustNode::new(
        ident.clone(),
        "nixos",
        ledger.clone(),
        Arc::new(FanoutEvents(sinks)),
        clock.clone(),
        TrustPolicy::default(),
    )
    .unwrap();
    let probe = Arc::new(Probe::default());
    let id = PeerId::new(&node.node_id()).unwrap();
    net.serve(
        &id,
        Arc::new(TrustedHandler::new(node.clone(), probe.clone())),
    );
    (
        M {
            id,
            ident,
            node,
            ledger,
            events,
            probe,
        },
        log,
    )
}

fn audit_actions(audit: &tasks_ledger::MemoryAudit) -> Vec<String> {
    use tasks_ledger::AuditLedger;
    audit
        .scan(0, 1000)
        .unwrap()
        .into_iter()
        .map(|r| r.action)
        .collect()
}

// R-278
#[test]
fn r278_pairing_grants_and_revocation_are_audited_and_probes_are_not() {
    use tasks_ledger::AuditLedger;
    let net = FakeNet::new();
    let clock = Arc::new(ManualClock::new(T0_MS));
    let audit = Arc::new(tasks_ledger::MemoryAudit::new());
    let (worker, log) = audited_worker(&net, &clock, &audit);
    let laptop = machine("kurukkan", &net, &clock);

    let ticket = worker
        .node
        .invite(&[Role::Submit], false, None, &addr())
        .unwrap();
    let ticket_text = ticket.to_text().unwrap();
    laptop
        .node
        .accept(
            &ticket_text,
            &tr(&net, &laptop),
            &SelfAddr::default(),
            TIMEOUT,
        )
        .unwrap();
    // the link probes every few seconds: each probe opens and closes a connection
    for _ in 0..50 {
        let mut c = tr(&net, &laptop).connect(&worker.id, TIMEOUT).unwrap();
        assert!(matches!(c.request(submit_body()), Ok(Body::Ok)));
    }
    let opened = worker
        .events
        .actions()
        .iter()
        .filter(|a| **a == "conn.opened")
        .count();
    assert!(opened >= 50, "the trust layer saw every probe ({opened})");
    worker.node.revoke_peer("kurukkan").unwrap();
    worker.node.reset_tickets().unwrap();

    let actions = audit_actions(&audit);
    assert!(
        !actions
            .iter()
            .any(|a| a == "conn.opened" || a == "conn.closed"),
        "{actions:?}"
    );
    assert_eq!(
        actions,
        [
            "pair.ticket_minted",
            "pair.ticket_redeemed",
            "grant.issued",
            "grant.revoked",
            "pair.tickets_reset",
        ]
    );
    let rows = audit.scan(0, 100).unwrap();
    let redeemed = rows
        .iter()
        .find(|r| r.action == "pair.ticket_redeemed")
        .unwrap();
    let laptop_did = laptop.ident.did().to_string();
    assert_eq!(
        (redeemed.peer.as_deref(), redeemed.actor.as_str()),
        (Some(laptop_did.as_str()), laptop_did.as_str())
    );
    let granted = rows.iter().find(|r| r.action == "grant.issued").unwrap();
    assert_eq!(granted.peer.as_deref(), Some(laptop_did.as_str()));
    assert_eq!(granted.details_json()["role"], "submit");
    assert!(log.verify().unwrap().ok());
}

// R-276
#[test]
fn r276_no_ticket_token_or_grant_text_reaches_the_audit_log() {
    use tasks_ledger::AuditLedger;
    let net = FakeNet::new();
    let clock = Arc::new(ManualClock::new(T0_MS));
    let audit = Arc::new(tasks_ledger::MemoryAudit::new());
    let (worker, _log) = audited_worker(&net, &clock, &audit);
    let laptop = machine("kurukkan", &net, &clock);
    let ticket = worker
        .node
        .invite(&[Role::Submit], true, None, &addr())
        .unwrap();
    let invite_token = ticket.invite_token.clone();
    let ticket_text = ticket.to_text().unwrap();
    let claims = Invite::parse(&invite_token).unwrap().claims;
    laptop
        .node
        .accept(
            &ticket_text,
            &tr(&net, &laptop),
            &SelfAddr::default(),
            TIMEOUT,
        )
        .unwrap();
    // a second try with the spent ticket is a refusal whose text must not echo the ticket
    let again = laptop.node.accept(
        &ticket_text,
        &tr(&net, &laptop),
        &SelfAddr::default(),
        TIMEOUT,
    );
    assert!(again.is_err());

    let mut secrets = vec![invite_token, ticket_text, claims.nonce.clone()];
    secrets.extend(worker.ledger.grants().unwrap().into_iter().map(|g| g.token));
    secrets.extend(laptop.ledger.grants().unwrap().into_iter().map(|g| g.token));
    assert!(secrets.iter().all(|s| s.len() > 16));
    let dump = format!("{:?}", audit.scan(0, 1000).unwrap());
    assert!(dump.contains("pair.ticket_minted"));
    for s in &secrets {
        assert!(
            !dump.contains(s.as_str()),
            "a secret reached the audit log: {s}"
        );
    }
    assert!(!dump.contains("g1.") && !dump.contains("i1.") && !dump.contains("tasks1."));
    // the ticket is named by a short id both rows share
    let rows = audit.scan(0, 1000).unwrap();
    let minted = rows
        .iter()
        .find(|r| r.action == "pair.ticket_minted")
        .unwrap();
    let redeemed = rows
        .iter()
        .find(|r| r.action == "pair.ticket_redeemed")
        .unwrap();
    assert_eq!(minted.target, redeemed.target);
    assert_eq!(minted.target.as_ref().unwrap().len(), 8);
}

// R-277
#[test]
fn r277_a_peer_retrying_a_refused_connection_is_throttled_and_the_next_row_counts_the_rest() {
    let net = FakeNet::new();
    let clock = Arc::new(ManualClock::new(T0_MS));
    let audit = Arc::new(tasks_ledger::MemoryAudit::new());
    let (worker, _log) = audited_worker(&net, &clock, &audit);
    let (stranger, _) = generate().unwrap();
    let sid = PeerId::new(&tasks_identity::hex::encode(&stranger.device_public_key())).unwrap();
    for _ in 0..60 {
        let mut c = raw_connect(&net, &sid, &worker);
        let _ = c.request(submit_body());
    }
    let refused = |a: &Arc<tasks_ledger::MemoryAudit>| {
        audit_actions(a)
            .iter()
            .filter(|x| *x == "conn.refused")
            .count()
    };
    let seen = worker
        .events
        .actions()
        .iter()
        .filter(|a| **a == "conn.refused")
        .count();
    assert!(seen >= 60, "the trust layer saw every refusal ({seen})");
    assert_eq!(refused(&audit), 2, "a burst of two, then silence");
    // a minute later the same peer is recorded again, with what was dropped
    clock.advance(61_000);
    let mut c = raw_connect(&net, &sid, &worker);
    let _ = c.request(submit_body());
    assert_eq!(refused(&audit), 3);
    use tasks_ledger::AuditLedger;
    let last = audit.head().unwrap().unwrap();
    assert_eq!(last.action, "conn.refused");
    assert!(last.details_json()["suppressed"].as_u64().unwrap() >= 50);
    assert_eq!(last.details_json()["side"], "served");
    assert_eq!(last.details_json()["code"], "not_authorised");
}
