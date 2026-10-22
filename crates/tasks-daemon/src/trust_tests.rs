//! Trust in the daemon: identity at start, the pairing API, peer status, grant alerts and a
//! forwarded job through the admission gate, all over the in-memory network.

use crate::api::{ErrorCode, Reply, Request, Response};
use crate::forwarder::{LinkQuality, LinkReport, LinkState};
use crate::peer_link::PeerLink;
use crate::testkit::{FakeForwarder, PeerWorld, Rig, StubTrust, peer_view};
use crate::testkit::TempDir;
use crate::tests::{call, daemon_options};
use crate::trust_control::{ExpiringView, NodeTrust, TrustControl};
use crate::trust_runtime::NodePeers;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tasks_core::{JobId, Kind, MachineName, Phase, Placement};
use tasks_identity::generate;
use tasks_ledger::{MemoryTrust, Store, SystemClock};
use tasks_peer::fake::{FakeNet, FakeTransport};
use tasks_peer::trust::{
    NoEvents, SelfAddr, TrustNode, TrustPolicy, TrustedHandler, TrustedTransport,
};
use tasks_peer::{Handler, PeerId};

fn ok(r: Reply) -> Response {
    match r {
        Reply::Ok(r) => r,
        Reply::Err(e) => panic!("{e:?}"),
    }
}

fn node(name: &str, clock: Arc<dyn tasks_ledger::Clock>) -> (Arc<TrustNode>, PeerId) {
    let (ident, _) = generate().unwrap();
    let n = TrustNode::new(
        Arc::new(ident),
        name,
        Arc::new(MemoryTrust::new()),
        Arc::new(NoEvents),
        clock,
        TrustPolicy::default(),
    )
    .unwrap();
    let id = PeerId::new(&n.node_id()).unwrap();
    (n, id)
}

fn status_peers(r: &Rig) -> Vec<crate::api::PeerView> {
    r.sch.status().peers
}

// R-263
#[test]
fn r263_the_first_start_creates_the_identity_and_every_start_keeps_it() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("ident");
    let procs = Arc::new(crate::testkit::FakeProcs::new());
    let opts = daemon_options(&dir, &procs);
    let state = opts.config.state_dir.clone();
    let d = crate::Daemon::start(opts).unwrap();
    let key = state.join("identity.json");
    assert_eq!(
        std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let sock = daemon_sock(&state);
    let handle = std::thread::spawn(move || d.run());
    let Response::Identity(first) = ok(call(&sock, &Request::Identity)) else {
        panic!("identity expected");
    };
    assert!(first.did.starts_with("did:key:z6Mk"));
    assert_eq!(first.name, "testbox");
    assert_eq!(first.node_id.len(), 64);
    let public = tasks_identity::read_public(&state).unwrap();
    assert_eq!((public.did.as_str(), public.node_id.as_str()), (first.did.as_str(), first.node_id.as_str()));
    // a second daemon on the same state is refused before it touches the keystore
    let before = std::fs::read(&key).unwrap();
    assert!(crate::Daemon::start(daemon_options(&dir, &procs)).is_err());
    assert_eq!(std::fs::read(&key).unwrap(), before);
    ok(call(&sock, &Request::Shutdown));
    handle.join().unwrap().unwrap();
    // a restart keeps the identity
    let d = crate::Daemon::start(daemon_options(&dir, &procs)).unwrap();
    let sock = daemon_sock(&state);
    let handle = std::thread::spawn(move || d.run());
    let Response::Identity(again) = ok(call(&sock, &Request::Identity)) else {
        panic!("identity expected");
    };
    assert_eq!(again, first);
    ok(call(&sock, &Request::Shutdown));
    handle.join().unwrap().unwrap();
}

fn daemon_sock(state: &std::path::Path) -> std::path::PathBuf {
    // the socket name is the daemon's own business; find it
    std::fs::read_dir(state)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "sock"))
        .unwrap()
}

// R-260
#[test]
fn r260_the_api_pairs_two_daemons_lists_the_peer_and_revokes_it() {
    let net = FakeNet::new();
    let clock: Arc<dyn tasks_ledger::Clock> = Arc::new(SystemClock);
    let (wn, wid) = node("nixos", clock.clone());
    let (ln, lid) = node("kurukkan", clock.clone());
    let worker = Rig::new();
    let laptop = Rig::new();
    net.serve(
        &wid,
        Arc::new(TrustedHandler::new(
            wn.clone(),
            crate::peer_link::serve::build_receiver(&worker.sch, worker.dir.path().join("ps")),
        )) as Arc<dyn Handler>,
    );
    net.serve(
        &lid,
        Arc::new(TrustedHandler::new(
            ln.clone(),
            crate::peer_link::serve::build_receiver(&laptop.sch, laptop.dir.path().join("ps")),
        )) as Arc<dyn Handler>,
    );
    worker.sch.set_trust(Arc::new(NodeTrust::new(
        wn.clone(),
        Some(Arc::new(TrustedTransport::new(net.transport(&wid), wn.clone()))),
        Arc::new(SelfAddr::default()),
    )));
    laptop.sch.set_trust(Arc::new(NodeTrust::new(
        ln.clone(),
        Some(Arc::new(TrustedTransport::new(net.transport(&lid), ln.clone()))),
        Arc::new(SelfAddr::default()),
    )));
    // identity
    let Response::Identity(id) = ok(worker.sch.handle(Request::Identity)) else {
        panic!()
    };
    assert_eq!((id.did.as_str(), id.node_id.as_str()), (wn.did(), wid.as_str()));
    // a bad role is a named refusal
    let Reply::Err(e) = worker.sch.handle(Request::PairInvite {
        roles: vec!["root".into()],
        both_ways: false,
        ttl_secs: None,
    }) else {
        panic!("refusal expected")
    };
    assert_eq!(e.code, ErrorCode::Refused);
    assert!(e.message.contains("unknown role"), "{}", e.message);
    // invite on the worker, accept on the laptop
    let Response::Invite(inv) = ok(worker.sch.handle(Request::PairInvite {
        roles: vec!["submit".into(), "observe".into()],
        both_ways: true,
        ttl_secs: None,
    })) else {
        panic!()
    };
    assert!(inv.ticket.starts_with("tasks1."));
    assert_eq!(inv.roles, ["submit", "observe"]);
    let Response::Paired(p) = ok(laptop.sch.handle(Request::PairAccept {
        ticket: inv.ticket.clone(),
    })) else {
        panic!()
    };
    assert_eq!((p.name.as_str(), p.did.as_str()), ("nixos", wn.did()));
    // the ticket is single use
    let Reply::Err(e) = laptop.sch.handle(Request::PairAccept { ticket: inv.ticket }) else {
        panic!("a second redemption must be refused")
    };
    assert!(e.message.contains("already redeemed") || e.message.contains("name"), "{}", e.message);
    // peers on both sides
    let Response::Peers { peers } = ok(laptop.sch.handle(Request::Peers)) else {
        panic!()
    };
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].name, "nixos");
    assert_eq!(peers[0].did, wn.did());
    assert_eq!(peers[0].roles_out.len(), 2);
    assert_eq!(peers[0].roles_in.len(), 2, "both ways");
    assert!(peers[0].expires_at.is_some());
    let Response::Peers { peers } = ok(worker.sch.handle(Request::Peers)) else {
        panic!()
    };
    assert_eq!(peers[0].name, "kurukkan");
    // reset reports the new epoch
    let Response::PairReset { epoch } = ok(worker.sch.handle(Request::PairReset)) else {
        panic!()
    };
    assert_eq!(epoch, 1);
    // revoke on the worker: the laptop can no longer connect, and nothing is deleted
    let Response::Revoked(r) = ok(worker.sch.handle(Request::PeerRevoke {
        name: "kurukkan".into(),
    })) else {
        panic!()
    };
    assert_eq!((r.revoked, r.name.as_str()), (4, "kurukkan"));
    let Reply::Err(e) = worker.sch.handle(Request::PeerRevoke { name: "nobody".into() }) else {
        panic!()
    };
    assert!(e.message.contains("no paired peer"), "{}", e.message);
    let Response::Peers { peers } = ok(worker.sch.handle(Request::Peers)) else {
        panic!()
    };
    assert!(peers[0].revoked);
}

// R-260
#[test]
fn r260_trust_requests_without_a_trust_runtime_are_named_refusals() {
    let r = Rig::new();
    for req in [Request::Identity, Request::Peers, Request::PairReset] {
        let Reply::Err(e) = r.sch.handle(req) else {
            panic!("refusal expected")
        };
        assert_eq!(e.code, ErrorCode::Refused);
        assert!(e.message.contains("no identity"), "{}", e.message);
    }
}

// R-258
#[test]
fn r258_status_shows_each_peer_with_did_roles_expiry_path_and_round_trip() {
    let f = Arc::new(FakeForwarder::new(vec![]));
    let r = Rig::with_forwarder(f.clone());
    let stub = Arc::new(StubTrust::default());
    stub.set_peers(vec![
        peer_view("nixos", "did:key:zNixos", &["submit", "observe"], &["observe"]),
        peer_view("phone", "did:key:zPhone", &[], &["submit"]),
    ]);
    r.sch.set_trust(stub);
    *f.links.lock().unwrap() = vec![LinkReport {
        peer: "nixos".into(),
        node_id: "nixos-node".into(),
        state: LinkState::Up,
        last_seen_ago_secs: Some(2),
        detail: String::new(),
    }];
    *f.quality.lock().unwrap() = vec![LinkQuality {
        peer: "nixos".into(),
        path: "relay".into(),
        rtt_ms: Some(48),
    }];
    let peers = status_peers(&r);
    let n = peers.iter().find(|p| p.name == "nixos").unwrap();
    assert_eq!(n.did, "did:key:zNixos");
    assert_eq!(n.roles_out, ["submit", "observe"]);
    assert_eq!(n.roles_in, ["observe"]);
    assert_eq!(n.grant_expires_at, Some(2_000_000_000));
    assert_eq!((n.path.as_str(), n.rtt_ms), ("relay", Some(48)));
    assert_eq!(n.state, "up");
    // a peer that only connects to this machine has no link but is listed with its grants
    let p = peers.iter().find(|p| p.name == "phone").unwrap();
    assert_eq!(p.state, "inbound");
    assert_eq!(p.roles_in, ["submit"]);
    assert_eq!(p.path, "unknown");
}

// R-259
#[test]
fn r259_a_grant_inside_seven_days_alerts_and_clears_when_renewed() {
    let r = Rig::new();
    let stub = Arc::new(StubTrust::default());
    r.sch.set_trust(stub.clone());
    r.tick();
    assert!(r.sch.status().alerts.is_empty());
    stub.set_expiring(vec![ExpiringView {
        peer: "nixos".into(),
        direction: "held".into(),
        role: "submit".into(),
        left_secs: 5 * 86_400 + 3 * 3600,
    }]);
    r.tick();
    let st = r.sch.status();
    let a = st
        .alerts
        .iter()
        .find(|a| a.id == "grant-nixos-held-submit")
        .unwrap();
    assert_eq!(a.level, "warn");
    assert!(
        a.message.contains("5 d 3 h") && a.message.contains("could not be renewed"),
        "{}",
        a.message
    );
    // past the end it is critical
    stub.set_expiring(vec![ExpiringView {
        peer: "nixos".into(),
        direction: "held".into(),
        role: "submit".into(),
        left_secs: -10,
    }]);
    r.tick();
    let st = r.sch.status();
    let a = st.alerts.iter().find(|a| a.id == "grant-nixos-held-submit").unwrap();
    assert_eq!(a.level, "critical");
    assert!(a.message.contains("has expired"));
    // an issued grant names the peer that must renew it
    stub.set_expiring(vec![ExpiringView {
        peer: "kurukkan".into(),
        direction: "issued".into(),
        role: "observe".into(),
        left_secs: 86_400,
    }]);
    r.tick();
    let st = r.sch.status();
    assert!(st.alerts.iter().any(|a| a.id == "grant-kurukkan-issued-observe"
        && a.message.contains("has not renewed")));
    assert!(!st.alerts.iter().any(|a| a.id == "grant-nixos-held-submit"));
    // renewed: the grant leaves the list and the alert clears
    stub.set_expiring(vec![]);
    r.tick();
    assert!(r.sch.status().alerts.is_empty());
}

fn within(what: &str, mut cond: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !cond() {
        assert!(t0.elapsed() < Duration::from_secs(10), "timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

// R-261
#[test]
fn r261_a_job_is_forwarded_through_the_admission_gate_and_stops_when_the_peer_is_revoked() {
    let w = PeerWorld::new();
    let net = w.net.clone();
    let clock: Arc<dyn tasks_ledger::Clock> = Arc::new(SystemClock);
    let (wn, wid) = node("nixos", clock.clone());
    let (ln, lid) = node("kurukkan", clock);
    // the worker daemon: a rig whose receiver sits behind the trust gate
    let worker = Rig::new();
    worker.procs.auto_exit(0);
    worker.procs.output(b"built behind the gate\n");
    let receiver =
        crate::peer_link::serve::build_receiver(&worker.sch, worker.dir.path().join("peer-store"));
    net.serve(&wid, Arc::new(TrustedHandler::new(wn.clone(), receiver)) as Arc<dyn Handler>);
    // pairing: the worker invites, the laptop redeems
    let ticket = wn
        .invite(&[tasks_peer::trust::Role::Submit], false, None, &SelfAddr::default())
        .unwrap();
    let laptop_tr = TrustedTransport::new(net.transport(&lid), ln.clone());
    ln.accept(
        &ticket.to_text().unwrap(),
        &laptop_tr,
        &SelfAddr::default(),
        Duration::from_secs(5),
    )
    .unwrap();
    // the laptop's forwarder learns the peer from the trust node, not from configuration
    let mut opts = w.options();
    opts.peers.clear();
    opts.source = Some(Arc::new(NodePeers(ln.clone())));
    let link: Arc<PeerLink<TrustedTransport<FakeTransport>>> = Arc::new(PeerLink::new(
        TrustedTransport::new(net.transport(&lid), ln.clone()),
        opts,
    ));
    link.probe_all();
    let origin = Rig::with_forwarder(link.clone());
    let pin = |root: &std::path::Path| -> JobId {
        let mut s = origin.spec("origin-owner", &["cargo", "test"], Kind::Test);
        s.cwd = root.to_path_buf();
        s.placement = Placement::Peer(MachineName("nixos".into()));
        origin.submit(&s)
    };
    let id = pin(&w.root_a);
    within("passed on the laptop", || {
        origin.tick();
        worker.tick();
        origin.phase(id) == Phase::Passed
    });
    let jobs = worker.store.list(&Default::default()).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].origin.as_ref().unwrap().peer,
        lid.as_str(),
        "the peer is identified by its proven device key"
    );
    assert!(origin.log_text(id).contains("built behind the gate"));
    // revoke on the worker: a new job is refused, nothing more runs there
    wn.revoke_peer("kurukkan").unwrap();
    let id2 = pin(&w.root_b);
    let end = origin.until_done(id2);
    assert!(matches!(end, Phase::NotStarted { .. }), "{end:?}");
    assert_eq!(worker.procs.spawn_count(), 1);
    assert_eq!(origin.procs.spawn_count(), 0);
}

// R-261
#[test]
fn r261_a_revoked_peer_goes_inactive_in_the_forwarder_and_shows_why() {
    let w = PeerWorld::new();
    let net = w.net.clone();
    let clock: Arc<dyn tasks_ledger::Clock> = Arc::new(SystemClock);
    let (wn, wid) = node("nixos", clock.clone());
    let (ln, lid) = node("kurukkan", clock);
    let worker = Rig::new();
    let receiver =
        crate::peer_link::serve::build_receiver(&worker.sch, worker.dir.path().join("peer-store"));
    net.serve(&wid, Arc::new(TrustedHandler::new(wn.clone(), receiver)) as Arc<dyn Handler>);
    let t = wn
        .invite(&[tasks_peer::trust::Role::Submit], false, None, &SelfAddr::default())
        .unwrap();
    ln.accept(
        &t.to_text().unwrap(),
        &TrustedTransport::new(net.transport(&lid), ln.clone()),
        &SelfAddr::default(),
        Duration::from_secs(5),
    )
    .unwrap();
    let mut opts = w.options();
    opts.peers.clear();
    opts.source = Some(Arc::new(NodePeers(ln.clone())));
    let link = PeerLink::new(TrustedTransport::new(net.transport(&lid), ln.clone()), opts);
    use crate::forwarder::Forwarder;
    link.probe_all();
    let l = link.links();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].state, LinkState::Up);
    // the laptop revokes the peer on its side: it is no longer paired
    ln.revoke_peer("nixos").unwrap();
    link.probe_all();
    let l = link.links();
    assert_eq!(l[0].state, LinkState::Offline);
    assert!(l[0].detail.contains("no longer paired"), "{}", l[0].detail);
    assert!(link.link_quality().is_empty());
    let _: &dyn TrustControl = &NodeTrust::new(ln, None, Arc::new(SelfAddr::default()));
}
