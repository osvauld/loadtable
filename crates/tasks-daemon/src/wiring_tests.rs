//! The peer link wired into the scheduler: slot gating, fallback, loss, restart and the
//! worker side, over the in-memory network.

use crate::forwarder::{ForwardEnd, ForwardPoll, ForwardProgress, LinkReport, LinkState};
use crate::peer_link::PeerLink;
use crate::scheduler::lock;
use crate::startup::reconcile;
use crate::testkit::{FakeForwarder, PeerWorld, Rig};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tasks_core::{JobId, Kind, MachineName, Phase, Placement};
use tasks_ledger::Store;
use tasks_peer::RemoteJobId;
use tasks_peer::fake::FakeTransport;

type Link = PeerLink<FakeTransport>;

fn nixos() -> MachineName {
    MachineName("nixos".into())
}

fn within(what: &str, mut cond: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !cond() {
        assert!(t0.elapsed() < Duration::from_secs(10), "timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A job pinned to the peer, working in `root`.
fn pinned(r: &Rig, root: &Path) -> JobId {
    let mut s = r.spec("origin-owner", &["cargo", "test"], Kind::Test);
    s.cwd = root.to_path_buf();
    s.placement = Placement::Peer(nixos());
    r.submit(&s)
}

fn origin(w: &PeerWorld, policy: &str) -> (Rig, Arc<Link>) {
    let link = Arc::new(w.link());
    link.probe_all();
    let r = Rig::with_policy_forwarder(policy, link.clone());
    (r, link)
}

/// Jobs among `same` that exist and have not ended on the worker.
fn live_of(w: &PeerWorld, same: &[JobId]) -> usize {
    use tasks_peer::receiver::JobRunner;
    (1..=w.runner.submitted().len() as u64)
        .filter(|i| {
            w.runner
                .view(&w.origin, RemoteJobId(*i))
                .is_some_and(|v| !v.phase.is_terminal())
        })
        .filter(|i| {
            let key = w.runner.submitted()[usize::try_from(*i).unwrap() - 1]
                .0
                .clone();
            same.iter()
                .any(|j| key.as_str() == format!("laptop:{}", j.0))
        })
        .count()
}

fn running_ids(r: &Rig) -> Vec<JobId> {
    lock(&r.sch.running).keys().copied().collect()
}

// R-230, R-161, R-162: end to end through the daemon, over the fake transport.
#[test]
fn r230_three_same_checkout_forwards_and_an_unrelated_one_all_complete_and_no_waiter_holds_a_slot_early()
 {
    let w = PeerWorld::new();
    // two offload slots: the old shape, where the waiters of one checkout took both
    let (r, _link) = origin(&w, "[slots]\noffload = 2\n");
    let a1 = pinned(&r, &w.root_a);
    let a2 = pinned(&r, &w.root_a);
    let a3 = pinned(&r, &w.root_a);
    let b = pinned(&r, &w.root_b);
    let same = [a1, a2, a3];

    // a1 and the unrelated job take the two slots; a2 and a3 hold nothing and wait
    within("a1 and b submitted", || {
        r.tick();
        assert!(
            live_of(&w, &same) <= 1,
            "two jobs of one checkout live on the peer"
        );
        w.runner.submitted().len() == 2
    });
    let keys: Vec<String> = w
        .runner
        .submitted()
        .iter()
        .map(|(k, _)| k.as_str().to_string())
        .collect();
    assert!(keys.contains(&format!("laptop:{}", a1.0)), "{keys:?}");
    assert!(keys.contains(&format!("laptop:{}", b.0)), "{keys:?}");
    let held = running_ids(&r);
    assert!(held.contains(&a1) && held.contains(&b), "{held:?}");
    assert!(!held.contains(&a2) && !held.contains(&a3), "{held:?}");
    for j in [a2, a3] {
        r.tick();
        let rec = r.record(j);
        assert!(!rec.phase.is_terminal() && rec.phase != Phase::Running);
        assert!(
            rec.wait_text
                .as_deref()
                .is_some_and(|t| t.contains("checkout")),
            "{:?}",
            rec.wait_text
        );
    }
    // the driver submits to the peer just before it records the start: wait for that write
    within("a1 recorded as running", || {
        r.record(a1).phase == Phase::Running
    });
    // a1 and b race to the peer; a1's remote id is its position in the peer's submissions
    let at = keys
        .iter()
        .position(|k| *k == format!("laptop:{}", a1.0))
        .unwrap();
    within("a1 records its remote id", || {
        r.tick();
        r.record(a1).forward.unwrap().remote_job == Some(at as i64 + 1)
    });

    // everything completes, a1, a2, a3 strictly in that order
    let mut done: BTreeSet<u64> = BTreeSet::new();
    within("all four passed", || {
        r.tick();
        assert!(
            live_of(&w, &same) <= 1,
            "two jobs of one checkout live on the peer"
        );
        for i in 1..=w.runner.submitted().len() as u64 {
            if done.insert(i) {
                w.runner.finish(RemoteJobId(i), 0, vec![]);
            }
        }
        [a1, a2, a3, b].iter().all(|j| r.phase(*j).is_terminal())
    });
    for j in [a1, a2, a3, b] {
        assert_eq!(r.phase(j), Phase::Passed, "job {}", j.0);
    }
    let order: Vec<String> = w
        .runner
        .submitted()
        .iter()
        .map(|(k, _)| k.as_str().to_string())
        .filter(|k| same.iter().any(|j| *k == format!("laptop:{}", j.0)))
        .collect();
    assert_eq!(
        order,
        same.iter()
            .map(|j| format!("laptop:{}", j.0))
            .collect::<Vec<_>>()
    );
    assert_eq!(r.procs.spawn_count(), 0, "nothing ran on the laptop");
}

// R-230 with scripted progress: a waiter that is not ready takes no slot.
#[test]
fn r230_a_forward_that_is_not_ready_takes_no_offload_slot_and_blocks_nobody() {
    let f = Arc::new(FakeForwarder::new(vec![]));
    let r = Rig::with_policy_forwarder("[slots]\noffload = 2\n", f.clone());
    let mk = |sub: &str| {
        let mut s = r.spec_in("o", &["cargo", "test"], Kind::Test, sub);
        s.placement = Placement::Anywhere;
        let id = r.submit(&s);
        r.store
            .set_forward(id, &nixos(), None, Some(&format!("k{}", id.0)))
            .unwrap();
        id
    };
    let (j1, j2, j3) = (mk("a"), mk("a"), mk("b"));
    f.not_ready.lock().unwrap().insert(j2);
    r.tick();
    assert_eq!(f.started.lock().unwrap().clone(), vec![j1, j3]);
    assert!(!running_ids(&r).contains(&j2));
    assert!(
        r.record(j2).wait_text.unwrap().contains("checkout"),
        "the wait names the checkout"
    );
    // j1 ends; j2 becomes ready and goes
    f.script(
        j1,
        ForwardPoll::Done(ForwardEnd {
            phase: Phase::Passed,
            reason: "command exit 0".into(),
            exit_code: Some(0),
            signal: None,
            remote_job: Some(1),
            nothing_submitted: false,
            samples: vec![],
            fetch: vec![],
        }),
    );
    f.not_ready.lock().unwrap().remove(&j2);
    within("j2 started", || {
        r.tick();
        f.started.lock().unwrap().contains(&j2)
    });
    assert_eq!(r.phase(j1), Phase::Passed);
}

#[test]
fn an_accepted_job_goes_running_with_its_remote_id_and_ends_with_the_peers_result() {
    let f = Arc::new(FakeForwarder::new(vec![]));
    let r = Rig::with_forwarder(f.clone());
    let mut s = r.spec("o", &["cargo", "test"], Kind::Test);
    s.placement = Placement::Anywhere;
    let id = r.submit(&s);
    r.store.set_forward(id, &nixos(), None, Some("k1")).unwrap();
    r.tick();
    within("mirrored", || {
        r.tick();
        r.record(id).wait_text.as_deref() == Some("pressure: syncing checkout")
            || r.record(id)
                .wait_text
                .as_deref()
                .is_some_and(|t| t.contains("syncing checkout"))
    });
    assert!(!matches!(r.phase(id), Phase::Running), "not accepted yet");
    f.script(
        id,
        ForwardPoll::Active(ForwardProgress {
            mirror: "#7 running".into(),
            remote_job: Some(7),
            accepted: true,
            remote_phase: Some(Phase::Running),
        }),
    );
    within("running", || {
        r.tick();
        r.phase(id) == Phase::Running
    });
    let rec = r.record(id);
    assert_eq!(rec.forward.unwrap().remote_job, Some(7));
    assert_eq!(rec.remote_phase, Some(Phase::Running));
    // a cancel reaches the forwarder, which owns telling the peer
    r.store.request_cancel(id, "origin-owner").unwrap();
    within("cancel forwarded", || {
        r.tick();
        !f.cancelled.lock().unwrap().is_empty()
    });
    f.script(
        id,
        ForwardPoll::Done(ForwardEnd {
            phase: Phase::Cancelled,
            reason: "cancelled on nixos".into(),
            exit_code: None,
            signal: Some(15),
            remote_job: Some(7),
            nothing_submitted: false,
            samples: vec![],
            fetch: vec![],
        }),
    );
    assert_eq!(r.until_done(id), Phase::Cancelled);
}

// R-7.5/R-170: nothing submitted -> back to the local queue, with a note.
#[test]
fn an_anywhere_job_whose_peer_was_unreachable_falls_back_to_local_with_a_note() {
    let w = PeerWorld::new();
    let (r, _link) = origin(&w, "");
    r.procs.auto_exit(0);
    let mut s = r.spec("o", &["cargo", "test"], Kind::Test);
    s.cwd = w.root_a.clone();
    s.placement = Placement::Anywhere;
    let id = r.submit(&s);
    // placement chose the peer (recorded with its key) and then the peer went away
    r.store
        .set_forward(id, &nixos(), None, Some("laptop:k"))
        .unwrap();
    w.net.set_down(&w.nixos, true);
    assert_eq!(r.until_done(id), Phase::Passed);
    let rec = r.record(id);
    assert!(rec.forward.is_none(), "{:?}", rec.forward);
    let summary = rec.summary.unwrap();
    assert!(summary.contains("forwarding to nixos failed"), "{summary}");
    assert!(summary.contains("ran here instead"), "{summary}");
    assert_eq!(r.procs.spawn_count(), 1);
    assert_eq!(w.requests("submit"), 0);
}

#[test]
fn a_job_pinned_to_an_unreachable_peer_fails_and_is_never_run_here() {
    let w = PeerWorld::new();
    let (r, _link) = origin(&w, "");
    let id = pinned(&r, &w.root_a);
    w.net.set_down(&w.nixos, true);
    let end = r.until_done(id);
    assert!(
        matches!(&end, Phase::NotStarted { reason } if reason.contains("nothing was submitted")),
        "{end:?}"
    );
    assert!(
        r.record(id)
            .summary
            .unwrap()
            .contains("nothing was submitted")
    );
    assert_eq!(r.procs.spawn_count(), 0);
}

// R-150, R-166
#[test]
fn a_peer_lost_after_it_accepted_the_job_ends_interrupted_and_is_never_run_or_resubmitted() {
    let w = PeerWorld::new();
    let (r, link) = origin(&w, "");
    let id = pinned(&r, &w.root_a);
    within("running", || {
        r.tick();
        r.phase(id) == Phase::Running
    });
    w.net.set_down(&w.nixos, true);
    w.net.cut_streams();
    std::thread::sleep(Duration::from_millis(30));
    link.advance_clock(Duration::from_secs(600));
    assert_eq!(r.until_done(id), Phase::Interrupted);
    assert!(r.record(id).summary.unwrap().contains("not resubmitted"));
    assert_eq!(r.procs.spawn_count(), 0);
    assert_eq!(w.requests("submit"), 1);
}

// R-173: a restarted daemon follows the job by its remote id, read-only.
#[test]
fn r173_a_restarted_daemon_reconciles_a_running_forward_read_only_and_never_resubmits() {
    let w = PeerWorld::new();
    let (r, link) = origin(&w, "");
    let id = pinned(&r, &w.root_a);
    within("running", || {
        r.tick();
        r.phase(id) == Phase::Running
    });
    w.runner.append_log(RemoteJobId(1), b"before the crash\n");
    within("log mirrored", || {
        r.log_text(id).contains("before the crash")
    });
    link.shutdown();
    // startup reconciliation leaves a forwarded job alone: it is not interrupted
    let rep = reconcile(&*r.store, &*r.procs, Duration::from_secs(1), &|_| {}).unwrap();
    assert_eq!(
        (rep.forwarded.clone(), rep.interrupted.clone()),
        (vec![id], vec![])
    );
    assert_eq!(r.phase(id), Phase::Running);

    let link2 = Arc::new(w.link());
    link2.probe_all();
    let sch2 = r.restart_with(link2.clone());
    sch2.tick();
    assert!(running_ids_of(&sch2).contains(&id), "followed again");
    w.runner.append_log(RemoteJobId(1), b"after the restart\n");
    w.runner.finish(RemoteJobId(1), 0, vec![]);
    within("passed", || {
        sch2.tick();
        r.phase(id) == Phase::Passed
    });
    let log = r.log_text(id);
    assert!(
        log.contains("before the crash\nafter the restart\n"),
        "{log}"
    );
    assert_eq!(w.requests("submit"), 1);
    assert_eq!(r.procs.spawn_count(), 0);
}

fn running_ids_of(s: &Arc<crate::Scheduler>) -> Vec<JobId> {
    lock(&s.running).keys().copied().collect()
}

// R-152: the persisted (peer, key) is what a restarted daemon sends.
#[test]
fn r152_the_persisted_key_survives_a_restart_and_is_the_one_submitted() {
    let w = PeerWorld::new();
    let (r, link) = origin(&w, "");
    let id = pinned(&r, &w.root_a);
    r.tick(); // placement only: registered, key persisted, nothing sent yet
    let fw = r.record(id).forward.unwrap();
    assert_eq!(
        fw.remote_key.as_deref(),
        Some(format!("laptop:{}", id.0).as_str())
    );
    assert_eq!(fw.remote_job, None);
    assert!(w.net.requests().len() <= 2, "only status probes so far");
    link.shutdown();
    // the ledger carries a different, older key: that is the one that must be sent
    r.store
        .set_forward(id, &nixos(), None, Some("laptop:from-before"))
        .unwrap();
    let link2 = Arc::new(w.link());
    link2.probe_all();
    let sch2 = r.restart_with(link2);
    within("submitted", || {
        sch2.tick();
        !w.runner.submitted().is_empty()
    });
    assert_eq!(w.runner.submitted()[0].0.as_str(), "laptop:from-before");
}

// R-173, R-152: a submit whose reply was lost in a crash is never run a second time.
#[test]
fn a_restart_between_the_submit_and_the_ledger_write_never_runs_the_job_twice() {
    let w = PeerWorld::new();
    let (r, link) = origin(&w, "");
    let id = pinned(&r, &w.root_a);
    r.tick();
    // the first daemon got as far as the peer accepting the job, then died before the row
    // learned the remote id
    let rec = r.record(id);
    let first = w.link();
    first.forward_and_start(&rec, &w.dir.path().join("lost.log"));
    within("accepted by the peer", || !w.runner.submitted().is_empty());
    first.shutdown();
    link.shutdown();
    assert_eq!(r.phase(id), Phase::Queued);

    let link2 = Arc::new(w.link());
    link2.probe_all();
    let sch2 = r.restart_with(link2.clone());
    std::thread::sleep(Duration::from_millis(50));
    for _ in 0..10 {
        sch2.tick();
        std::thread::sleep(Duration::from_millis(5));
    }
    // the peer still has a live job of this checkout, so nothing more is sent; after the grace
    // the job is interrupted, never run here
    link2.advance_clock(Duration::from_secs(600));
    within("interrupted", || {
        sch2.tick();
        r.phase(id).is_terminal()
    });
    assert_eq!(r.phase(id), Phase::Interrupted);
    assert_eq!(w.runner.submitted().len(), 1, "one submission in total");
    assert_eq!(r.procs.spawn_count(), 0);
}

// R-166
#[test]
fn r166_a_forced_reroute_cancels_the_peers_copy_first_and_never_moves_a_started_job() {
    let f = Arc::new(FakeForwarder::new(vec![]));
    let r = Rig::with_forwarder(f.clone());
    r.procs.auto_exit(0);
    let mut s = r.spec("o", &["make", "x"], Kind::Run);
    s.placement = Placement::Anywhere;
    let id = r.submit(&s);
    // no remote id: the peer may hold it with nothing recorded, so there is nothing to cancel
    r.store.set_forward(id, &nixos(), None, Some("k")).unwrap();
    assert!(
        r.sch
            .reroute_local(id, "peer offline")
            .unwrap_err()
            .contains("may already hold")
    );
    r.store.set_forward(id, &nixos(), Some(5), None).unwrap();
    // unreachable: the job must stay where it is, or it would run twice
    *f.cancel_remote_fails.lock().unwrap() = true;
    let e = r.sch.reroute_local(id, "peer offline").unwrap_err();
    assert!(e.contains("not moved"), "{e}");
    assert!(r.record(id).forward.is_some());
    assert!(f.remote_cancelled.lock().unwrap().is_empty());
    // confirmed cancelled: it comes back and runs here, saying so
    *f.cancel_remote_fails.lock().unwrap() = false;
    r.sch.reroute_local(id, "peer offline").unwrap();
    assert_eq!(
        f.remote_cancelled.lock().unwrap().clone(),
        vec![("nixos".to_string(), 5)]
    );
    assert!(r.record(id).forward.is_none());
    assert_eq!(r.until_done(id), Phase::Passed);
    assert!(r.record(id).summary.unwrap().contains("moved off nixos"));
    // a job that started is never moved
    let id2 = r.submit(&s);
    r.store.set_forward(id2, &nixos(), Some(6), None).unwrap();
    r.store.set_phase(id2, Phase::Running).unwrap();
    assert!(
        r.sch
            .reroute_local(id2, "x")
            .unwrap_err()
            .contains("never moves")
    );
    assert_eq!(f.remote_cancelled.lock().unwrap().len(), 1);
}

// R-222: stale and offline peers alert after the grace; link state is in the status.
#[test]
fn r222_stale_and_offline_peers_alert_after_the_grace_period_and_clear() {
    let f = Arc::new(FakeForwarder::new(vec![]));
    let r = Rig::with_policy_forwarder("[alerts]\npeer_grace_secs = 100\n", f.clone());
    let report = |state, ago| LinkReport {
        peer: "nixos".into(),
        node_id: "nixos-node".into(),
        state,
        last_seen_ago_secs: ago,
        detail: "connect timed out".into(),
        status: None,
    };
    *f.links.lock().unwrap() = vec![report(LinkState::Up, Some(1))];
    r.tick();
    assert!(r.sch.status().alerts.is_empty());
    assert_eq!(r.sch.status().peers[0].state, "up");

    *f.links.lock().unwrap() = vec![report(LinkState::Offline, Some(500))];
    r.tick();
    r.clock.advance(99_000);
    r.tick();
    assert!(
        r.sch.status().alerts.is_empty(),
        "not before the grace period"
    );
    r.clock.advance(2_000);
    r.tick();
    let st = r.sch.status();
    let a = st
        .alerts
        .iter()
        .find(|a| a.id == "peer-nixos-offline")
        .unwrap();
    assert!(
        a.message.contains("connect timed out") && a.message.contains("500 s ago"),
        "{}",
        a.message
    );
    assert_eq!(
        (st.peers[0].state.as_str(), st.peers[0].last_seen_ago_secs),
        ("offline", Some(500))
    );

    // stale is its own alert; coming back up clears everything
    *f.links.lock().unwrap() = vec![report(LinkState::Stale, Some(90))];
    r.tick();
    assert!(
        r.sch
            .status()
            .alerts
            .iter()
            .any(|a| a.id == "peer-nixos-stale")
    );
    assert!(
        !r.sch
            .status()
            .alerts
            .iter()
            .any(|a| a.id == "peer-nixos-offline")
    );
    *f.links.lock().unwrap() = vec![report(LinkState::Up, Some(1))];
    r.tick();
    assert!(r.sch.status().alerts.is_empty());
}

// R-690 (replaces the R-259 alert id)
#[test]
fn r690_paired_peers_without_a_peer_transport_alert_at_once() {
    let r = Rig::new();
    let stub = Arc::new(crate::testkit::StubTrust::default());
    stub.set_peers(vec![
        crate::testkit::peer_view("nixos", "did:key:zN", &["submit"], &[]),
        // paired only inbound: it still cannot reach a daemon that has no transport
        crate::testkit::peer_view("mac", "did:key:zM", &[], &["submit"]),
    ]);
    r.sch.set_trust(stub);
    r.tick();
    let st = r.sch.status();
    let a = st
        .alerts
        .iter()
        .find(|a| a.id == "no-peer-transport")
        .unwrap_or_else(|| panic!("{:?}", st.alerts));
    assert!(
        a.message.contains("built without peer transport")
            && a.message.contains("nixos, mac")
            && a.message.contains("--features iroh"),
        "{}",
        a.message
    );
    // and the endpoint says so too (R-692)
    assert_eq!(
        st.endpoint,
        Some(crate::trust_control::EndpointView::none())
    );
}

// R-690
#[test]
fn r690_no_alert_without_paired_peers_or_with_a_transport() {
    // a build without transport but nobody paired: nothing to warn about
    let r = Rig::new();
    r.sch
        .set_trust(Arc::new(crate::testkit::StubTrust::default()));
    r.tick();
    assert!(r.sch.status().alerts.is_empty());
    // a build with a transport and paired peers: no such alert, whatever the links say
    let f = Arc::new(FakeForwarder::new(vec![]));
    let r = Rig::with_forwarder(f);
    let stub = Arc::new(crate::testkit::StubTrust::default());
    stub.set_peers(vec![crate::testkit::peer_view(
        "nixos",
        "did:key:zN",
        &["submit"],
        &[],
    )]);
    r.sch.set_trust(stub);
    r.tick();
    let st = r.sch.status();
    assert!(
        !st.alerts.iter().any(|a| a.id == "no-peer-transport"),
        "{:?}",
        st.alerts
    );
    assert_ne!(
        st.endpoint,
        Some(crate::trust_control::EndpointView::none())
    );
}

// The worker side, end to end: a second daemon hosts the receiver and runs the job.
#[test]
fn a_job_forwarded_to_a_worker_daemon_runs_there_as_an_ordinary_queue_job() {
    let w = PeerWorld::new();
    let worker = Rig::new();
    worker.procs.auto_exit(0);
    worker.procs.output(b"built on the worker\n");
    let receiver =
        crate::peer_link::serve::build_receiver(&worker.sch, worker.dir.path().join("peer-store"));
    w.net.serve(&w.nixos, receiver);
    let (r, _link) = origin(&w, "");
    let id = pinned(&r, &w.root_a);
    within("passed on the laptop", || {
        r.tick();
        worker.tick();
        r.phase(id) == Phase::Passed
    });
    // the worker ran it as its own job, with the origin's owner, in its synced copy
    let jobs = worker.store.list(&Default::default()).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].spec.owner.as_str(), "origin-owner");
    assert_eq!(jobs[0].spec.placement, Placement::Here);
    assert!(
        jobs[0]
            .spec
            .cwd
            .starts_with(worker.dir.path().join("peer-store"))
    );
    assert_eq!(jobs[0].origin.as_ref().unwrap().peer, "laptop-node");
    assert_eq!(worker.procs.spawn_count(), 1);
    assert_eq!(r.procs.spawn_count(), 0);
    assert!(r.log_text(id).contains("built on the worker"));
    assert_eq!(r.record(id).exit_code, Some(0));
    // the worker's timing sample came back
    assert!(r.record(id).forward.is_some());
}

// R-315
#[test]
fn r315_a_worker_daemon_audits_the_snapshot_it_applied() {
    let w = PeerWorld::new();
    let worker = Rig::new();
    worker.procs.auto_exit(0);
    let receiver =
        crate::peer_link::serve::build_receiver(&worker.sch, worker.dir.path().join("peer-store"));
    w.net.serve(&w.nixos, receiver);
    let (origin_rig, _link) = origin(&w, "");
    let id = pinned(&origin_rig, &w.root_a);
    within("passed on the laptop", || {
        origin_rig.tick();
        worker.tick();
        origin_rig.phase(id) == Phase::Passed
    });
    use tasks_ledger::AuditLedger as _;
    let rows = worker.audit_store.scan(0, 1000).unwrap();
    let got = rows
        .iter()
        .find(|r| r.action == "data.sync_received")
        .expect("the worker audits what it applied");
    let d = got.details_json();
    assert_eq!(d["tree"].as_str().map(str::len), Some(40));
    assert!(d["bytes"].as_u64().unwrap() > 0);
    assert_eq!(d["written"], 2);
}
