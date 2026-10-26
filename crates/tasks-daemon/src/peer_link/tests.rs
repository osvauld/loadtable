//! The adapter over the in-memory network: every behaviour the scheduler relies on.

use super::*;
use crate::forwarder::{ForwardPoll, Forwarder, LinkState};
use crate::testkit::PeerWorld;
use std::path::PathBuf;
use std::time::Duration;
use tasks_core::{JobSpec, Kind, Owner, Phase, Placement, Requirements};
use tasks_ledger::{MemoryStore, Store};
use tasks_peer::fake::kind;

fn until(what: &str, cond: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !cond() {
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn record(store: &MemoryStore, root: &Path) -> JobRecord {
    let spec = JobSpec {
        owner: Owner::new("tester").unwrap(),
        argv: vec!["cargo".into(), "test".into()],
        cwd: root.to_path_buf(),
        env: vec![],
        kind: Kind::Test,
        requirements: Requirements::default(),
        placement: Placement::Anywhere,
        after: vec![],
        timeout_secs: None,
        broad_test_escape: false,
        script_escape: false,
        priority: tasks_core::Priority::Normal,
        priority_reason: None,
        fetch: Vec::new(),
        isolate: false,
        light: false,
        retry_alone: false,
        interactive: false,
    };
    let id = store.submit(&spec).unwrap();
    store.get(id).unwrap().unwrap()
}

fn nixos() -> MachineName {
    MachineName("nixos".into())
}

fn poll_until_done(link: &dyn Forwarder, id: JobId) -> crate::forwarder::ForwardEnd {
    let t0 = Instant::now();
    loop {
        if let ForwardPoll::Done(e) = link.poll(id) {
            return e;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "never ended");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn accepted(link: &dyn Forwarder, id: JobId) -> bool {
    matches!(link.poll(id), ForwardPoll::Active(p) if p.accepted)
}

// R-222 (link state), R-133
#[test]
fn r222_link_state_follows_probes_and_status_feeds_placement() {
    let w = PeerWorld::new();
    let link = w.link();
    assert_eq!(link.links()[0].state, LinkState::Unknown);
    let down = &link.peers()[0];
    assert!(
        matches!(down.link, Link::Down { .. }),
        "unprobed peers are not usable"
    );
    link.probe_all();
    let l = &link.links()[0];
    assert_eq!((l.state, l.peer.as_str()), (LinkState::Up, "nixos"));
    let p = &link.peers()[0];
    assert!(matches!(p.link, Link::Up { .. }));
    assert_eq!(p.machine.name.0, "nixos");
    assert_eq!(p.free_slots[&SlotKind::Offload], 4);
    // it answered, but a long time ago
    link.advance_clock(Duration::from_secs(60));
    assert_eq!(link.links()[0].state, LinkState::Stale);
    assert_eq!(link.links()[0].last_seen_ago_secs, Some(60));
    // unreachable
    w.net.set_down(&w.nixos, true);
    link.probe_all();
    let l = &link.links()[0];
    assert_eq!(l.state, LinkState::Offline);
    assert!(l.detail.contains("offline"), "{}", l.detail);
    assert!(matches!(
        link.peers()[0].link,
        Link::Down {
            last_seen_ago_secs: Some(_)
        }
    ));
    // and back
    w.net.set_down(&w.nixos, false);
    link.probe_all();
    assert_eq!(link.links()[0].state, LinkState::Up);
}

#[test]
fn registration_touches_no_network_and_a_started_job_runs_to_the_peers_exit_code() {
    let w = PeerWorld::new();
    let link = w.link();
    let store = MemoryStore::new();
    let rec = record(&store, &w.root_a);
    let t = link.forward(&rec, &nixos()).unwrap();
    assert_eq!(
        t.remote_key.as_deref(),
        Some(format!("laptop:{}", rec.id.0).as_str())
    );
    assert!(link.registered(rec.id));
    assert!(w.net.requests().is_empty(), "forward() only registers");
    assert!(link.ready_to_start(rec.id));
    // idempotent
    assert_eq!(link.forward(&rec, &nixos()).unwrap(), t);
    let log = w.dir.path().join("job.log");
    link.start(rec.id, &log).unwrap();
    until("accepted", || accepted(&link, rec.id));
    w.runner
        .append_log(tasks_peer::RemoteJobId(1), b"compiling\n");
    w.runner.finish(tasks_peer::RemoteJobId(1), 3, vec![]);
    let end = poll_until_done(&link, rec.id);
    assert_eq!(end.phase, Phase::Failed { exit: Some(3) });
    assert_eq!((end.exit_code, end.nothing_submitted), (Some(3), false));
    assert_eq!(end.remote_job, Some(1));
    assert_eq!(std::fs::read(&log).unwrap(), b"compiling\n");
    assert!(!link.registered(rec.id), "an ended job is forgotten");
    assert_eq!(w.requests("submit"), 1);
}

// R-230
#[test]
fn r230_only_the_first_of_a_checkout_is_ready_and_an_unrelated_one_never_waits() {
    let w = PeerWorld::new();
    let link = w.link();
    let store = MemoryStore::new();
    let a1 = record(&store, &w.root_a);
    let a2 = record(&store, &w.root_a);
    let a3 = record(&store, &w.root_a);
    let b1 = record(&store, &w.root_b);
    for r in [&a1, &a2, &a3, &b1] {
        link.forward(r, &nixos()).unwrap();
    }
    assert!(link.ready_to_start(a1.id));
    assert!(!link.ready_to_start(a2.id) && !link.ready_to_start(a3.id));
    assert!(link.ready_to_start(b1.id));
    link.start(a1.id, &w.dir.path().join("a1.log")).unwrap();
    until("a1 accepted", || accepted(&link, a1.id));
    let why = link.wait_reason(a2.id).unwrap();
    assert!(why.contains(&format!("job #{}", a1.id.0)), "{why}");
    // while a1 runs the others still wait, and starting is a no-op for what is not ready
    assert!(!link.ready_to_start(a2.id));
    w.runner.finish(tasks_peer::RemoteJobId(1), 0, vec![]);
    assert_eq!(poll_until_done(&link, a1.id).phase, Phase::Passed);
    assert!(link.ready_to_start(a2.id), "the next in line is ready");
    assert!(!link.ready_to_start(a3.id), "and only one at a time");
}

// R-803: each peer holds its own copy of a checkout, so its lock is per peer too
#[test]
fn r803_a_job_on_one_peer_never_waits_for_the_same_checkout_on_another() {
    let w = PeerWorld::new();
    let mut o = w.options();
    o.peers.push(crate::peers_config::PeerSpec {
        name: "mac".into(),
        node_id: tasks_peer::PeerId::new("mac").unwrap(),
        did: String::new(),
    });
    let link = PeerLink::new(w.net.transport(&w.origin), o);
    let store = MemoryStore::new();
    let on_nixos = record(&store, &w.root_a);
    let on_mac = record(&store, &w.root_a);
    let after = record(&store, &w.root_a);
    link.forward(&on_nixos, &nixos()).unwrap();
    link.forward(&on_mac, &MachineName("mac".into())).unwrap();
    link.forward(&after, &nixos()).unwrap();
    link.start(on_nixos.id, &w.dir.path().join("n.log"))
        .unwrap();
    until("accepted on nixos", || accepted(&link, on_nixos.id));
    assert!(link.ready_to_start(on_mac.id), "the mac's copy is its own");
    assert!(
        !link
            .wait_reason(on_mac.id)
            .is_some_and(|t| t.contains("is using the remote copy"))
    );
    assert!(
        !link.ready_to_start(after.id),
        "nixos's copy is still one job at a time"
    );
}

// R-230: a waiter that vanished must not wedge the queue
#[test]
fn r230_forgetting_or_sweeping_an_unstarted_registration_frees_its_place() {
    let w = PeerWorld::new();
    let link = w.link();
    let store = MemoryStore::new();
    let a1 = record(&store, &w.root_a);
    let a2 = record(&store, &w.root_a);
    let a3 = record(&store, &w.root_a);
    for r in [&a1, &a2, &a3] {
        link.forward(r, &nixos()).unwrap();
    }
    link.forget(a1.id);
    assert!(link.ready_to_start(a2.id));
    // a2 was cancelled while waiting: the scheduler lists only the live jobs
    link.forget_unlisted(&[a3.id].into());
    assert!(!link.registered(a2.id));
    assert!(link.ready_to_start(a3.id));
}

#[test]
fn an_offline_peer_ends_the_forward_with_nothing_submitted() {
    let w = PeerWorld::new();
    w.net.set_down(&w.nixos, true);
    let link = w.link();
    let store = MemoryStore::new();
    let rec = record(&store, &w.root_a);
    link.forward(&rec, &nixos()).unwrap();
    link.start(rec.id, &w.dir.path().join("x.log")).unwrap();
    let end = poll_until_done(&link, rec.id);
    assert!(end.nothing_submitted, "{end:?}");
    assert!(matches!(end.phase, Phase::Failed { .. }));
    assert!(
        end.reason.contains("nothing was submitted"),
        "{}",
        end.reason
    );
    assert_eq!(w.requests("submit"), 0);
}

// R-150
#[test]
fn r150_a_peer_lost_after_the_submit_ends_interrupted_and_is_not_nothing_submitted() {
    let w = PeerWorld::new();
    let link = w.link();
    let store = MemoryStore::new();
    let rec = record(&store, &w.root_a);
    link.forward(&rec, &nixos()).unwrap();
    link.start(rec.id, &w.dir.path().join("x.log")).unwrap();
    until("accepted", || accepted(&link, rec.id));
    w.net.set_down(&w.nixos, true);
    w.net.cut_streams();
    // let the driver notice the loss, then let the grace period pass
    std::thread::sleep(Duration::from_millis(30));
    link.advance_clock(Duration::from_secs(600));
    let end = poll_until_done(&link, rec.id);
    assert_eq!(end.phase, Phase::Interrupted);
    assert!(!end.nothing_submitted);
    assert_eq!(end.remote_job, Some(1));
    assert_eq!(w.requests("submit"), 1, "never resubmitted");
}

// R-151
#[test]
fn r151_cancel_reaches_the_peer_and_the_end_is_cancelled() {
    let w = PeerWorld::new();
    let link = w.link();
    let store = MemoryStore::new();
    let rec = record(&store, &w.root_a);
    link.forward(&rec, &nixos()).unwrap();
    link.start(rec.id, &w.dir.path().join("x.log")).unwrap();
    until("accepted", || accepted(&link, rec.id));
    link.cancel(rec.id, "user asked");
    let end = poll_until_done(&link, rec.id);
    assert_eq!(end.phase, Phase::Cancelled);
    assert!(w.runner.is_cancelled(tasks_peer::RemoteJobId(1)));
}

// R-152
#[test]
fn r152_a_key_the_ledger_holds_is_the_key_that_is_sent() {
    let w = PeerWorld::new();
    let link = w.link();
    let store = MemoryStore::new();
    let rec = record(&store, &w.root_a);
    store
        .set_forward(rec.id, &nixos(), None, Some("laptop:old-7"))
        .unwrap();
    let rec = store.get(rec.id).unwrap().unwrap();
    let t = link.forward(&rec, &nixos()).unwrap();
    assert_eq!(t.remote_key.as_deref(), Some("laptop:old-7"));
    link.start(rec.id, &w.dir.path().join("x.log")).unwrap();
    until("accepted", || accepted(&link, rec.id));
    assert_eq!(w.runner.submitted()[0].0.as_str(), "laptop:old-7");
}

// R-173
#[test]
fn r173_a_restarted_daemon_follows_the_job_by_its_remote_id_and_never_submits() {
    let w = PeerWorld::new();
    let store = MemoryStore::new();
    let rec = record(&store, &w.root_a);
    {
        let first = w.link();
        first.forward(&rec, &nixos()).unwrap();
        first.start(rec.id, &w.dir.path().join("x.log")).unwrap();
        until("accepted", || accepted(&first, rec.id));
        w.runner
            .append_log(tasks_peer::RemoteJobId(1), b"part one\n");
        until("log arrived", || {
            std::fs::read(w.dir.path().join("x.log")).is_ok_and(|b| b == b"part one\n")
        });
    } // the daemon "restarts": the link and its threads are gone
    store
        .set_forward(rec.id, &nixos(), Some(1), Some("laptop:1"))
        .unwrap();
    let rec = store.get(rec.id).unwrap().unwrap();
    let second = w.link();
    second.resume(&rec, &w.dir.path().join("x.log")).unwrap();
    // a later job of the same checkout waits for the resumed one
    let next = record(&store, &w.root_a);
    second.forward(&next, &nixos()).unwrap();
    assert!(!second.ready_to_start(next.id));
    w.runner
        .append_log(tasks_peer::RemoteJobId(1), b"part two\n");
    w.runner.finish(tasks_peer::RemoteJobId(1), 0, vec![]);
    let end = poll_until_done(&second, rec.id);
    assert_eq!(end.phase, Phase::Passed);
    assert_eq!(
        std::fs::read(w.dir.path().join("x.log")).unwrap(),
        b"part one\npart two\n",
        "no lost and no duplicated bytes"
    );
    assert_eq!(w.requests("submit"), 1, "reconciling is read-only");
    until("lock released", || second.ready_to_start(next.id));
}

#[test]
fn r173_a_job_without_a_remote_id_is_interrupted_on_resume_without_any_request() {
    let w = PeerWorld::new();
    let store = MemoryStore::new();
    let rec = record(&store, &w.root_a);
    store
        .set_forward(rec.id, &nixos(), None, Some("laptop:9"))
        .unwrap();
    let rec = store.get(rec.id).unwrap().unwrap();
    let link = w.link();
    link.resume(&rec, &w.dir.path().join("x.log")).unwrap();
    let end = poll_until_done(&link, rec.id);
    assert_eq!(end.phase, Phase::Interrupted);
    assert!(!end.nothing_submitted);
    assert!(
        end.reason.contains("neither rerun nor resubmitted"),
        "{}",
        end.reason
    );
    assert!(w.net.requests().is_empty());
}

#[test]
fn r173_an_unreachable_peer_is_never_success_it_is_interrupted_after_the_grace() {
    let w = PeerWorld::new();
    let store = MemoryStore::new();
    let rec = record(&store, &w.root_a);
    store
        .set_forward(rec.id, &nixos(), Some(1), Some("k"))
        .unwrap();
    let rec = store.get(rec.id).unwrap().unwrap();
    w.net.set_down(&w.nixos, true);
    let link = w.link();
    link.resume(&rec, &w.dir.path().join("x.log")).unwrap();
    std::thread::sleep(Duration::from_millis(30));
    link.advance_clock(Duration::from_secs(600));
    let end = poll_until_done(&link, rec.id);
    assert_eq!(end.phase, Phase::Interrupted);
    assert!(end.reason.contains("result unknown"), "{}", end.reason);
}

// R-166
#[test]
fn r166_cancel_remote_confirms_the_job_cancelled_or_gone_and_fails_when_unreachable() {
    let w = PeerWorld::new();
    let link = w.link();
    let store = MemoryStore::new();
    let rec = record(&store, &w.root_a);
    link.forward(&rec, &nixos()).unwrap();
    link.start(rec.id, &w.dir.path().join("x.log")).unwrap();
    until("accepted", || accepted(&link, rec.id));
    link.cancel_remote(&nixos(), 1).unwrap();
    assert!(w.runner.is_cancelled(tasks_peer::RemoteJobId(1)));
    // a job the peer does not know is gone, which is as good
    link.cancel_remote(&nixos(), 99).unwrap();
    assert!(link.cancel_remote(&MachineName("ghost".into()), 1).is_err());
    w.net.set_down(&w.nixos, true);
    let e = link.cancel_remote(&nixos(), 1).unwrap_err();
    assert!(e.to_string().contains("offline"), "{e}");
    assert_eq!(
        kind(&tasks_peer::proto::Body::StatusRequest),
        "status_request"
    );
}

#[test]
fn an_unknown_peer_is_a_named_error() {
    let w = PeerWorld::new();
    let link = w.link();
    let store = MemoryStore::new();
    let rec = record(&store, &PathBuf::from(&w.root_a));
    let e = link
        .forward(&rec, &MachineName("ghost".into()))
        .unwrap_err();
    assert!(e.to_string().contains("ghost"));
}

// R-322
#[test]
fn r322_the_repo_run_policy_names_the_ignored_files_that_travel() {
    use tasks_peer::sync::ExtrasProvider;
    let w = PeerWorld::new();
    let none = super::PolicyExtras.extras(&w.root_a);
    assert_eq!(none, Ok(Vec::new()), "no policy file means no extras");
    std::fs::create_dir_all(w.root_a.join(".tasks")).unwrap();
    std::fs::write(
        w.root_a.join(".tasks/run-policy.toml"),
        "version = 1\n[sync]\nextra = [\"local.properties\", \"config/*.json\"]\n",
    )
    .unwrap();
    assert_eq!(
        super::PolicyExtras.extras(&w.root_a),
        Ok(vec![
            "local.properties".to_string(),
            "config/*.json".to_string()
        ])
    );
    std::fs::write(
        w.root_a.join(".tasks/run-policy.toml"),
        "version = 1\n[sync]\nextra = [\"../x\"]\n",
    )
    .unwrap();
    let bad = super::PolicyExtras.extras(&w.root_a).unwrap_err();
    assert!(
        bad.contains("run policy cannot be used") && bad.contains("[sync] extra"),
        "{bad}"
    );
}

// R-363
#[test]
fn r363_path_rtt_is_the_transports_and_reply_time_is_the_status_request_taking_its_course() {
    use tasks_peer::transport::{LinkInfo as Info, PathKind};
    let w = PeerWorld::new();
    let link = w.link();
    // the transport reports 3 ms on a direct path; answering a request takes 60 ms (the peer's
    // handler, the scheduler in front of it): the two must not be mistaken for each other
    w.net.set_link_info(Some(Info {
        path: PathKind::Direct,
        rtt: Some(Duration::from_millis(3)),
    }));
    w.net.set_latency(Duration::from_millis(60));
    link.probe_all();
    let q = link.link_quality();
    let n = q.iter().find(|q| q.peer == "nixos").unwrap();
    assert_eq!(n.path, "direct");
    assert_eq!(n.rtt_ms, Some(3), "path RTT is the transport's figure");
    let reply = n.reply_ms.unwrap();
    assert!(
        (60..1000).contains(&reply),
        "reply time covers the whole request: {reply} ms"
    );
    // a transport that reports no path RTT leaves it unknown; the reply time is still measured
    w.net.set_link_info(None);
    w.net.set_latency(Duration::ZERO);
    link.probe_all();
    let q = link.link_quality();
    let n = q.iter().find(|q| q.peer == "nixos").unwrap();
    assert_eq!(n.rtt_ms, None);
    assert!(n.reply_ms.unwrap() < 60);
}

// ---------------------------------------------------------------- forwarded job metrics

fn metrics_view() -> tasks_core::metrics::JobMetricsView {
    tasks_core::metrics::JobMetricsView {
        quality: Some(tasks_core::Attribution::Cgroup),
        samples: vec![tasks_core::metrics::MetricSample {
            t_ms: 5,
            cpu_usec: Some(7),
            mem_bytes: Some(1 << 20),
            io_read_bytes: None,
            io_write_bytes: None,
            io_some_us: None,
        }],
        procs: vec![tasks_core::metrics::ProcGroup {
            name: "rustc".into(),
            count: 2,
            cpu_cores: Some(3.5),
            rss_bytes: 1 << 30,
            others: false,
        }],
        usual_peak_mem_bytes: None,
        note: None,
        source: None,
    }
}

/// A remote job on the worker that `w.origin` submitted, with metrics set.
fn remote_with_metrics(w: &PeerWorld) -> tasks_peer::RemoteJobId {
    use tasks_peer::receiver::JobRunner;
    let spec = record(&MemoryStore::new(), &w.root_a).spec;
    let key = tasks_peer::SubmitKey::new("laptop:m1").unwrap();
    let (id, _) = w.runner.submit(&w.origin, &key, spec).unwrap();
    w.runner.set_metrics(id, metrics_view());
    id
}

// R-621
#[test]
fn r621_the_origin_gets_a_forwarded_jobs_metrics_from_the_peer_that_runs_it() {
    let w = PeerWorld::new();
    let id = remote_with_metrics(&w);
    let link = w.link();
    link.probe_all();
    let v = link.remote_metrics(&nixos(), id.0 as i64).unwrap();
    assert_eq!(v.samples.len(), 1);
    assert_eq!(v.procs[0].name, "rustc");
    assert_eq!(v.source.as_deref(), Some("on nixos, remote job #1"));
    assert_eq!(v.note, None);
    assert_eq!(w.requests("job_metrics"), 1);
    // a second ask within the second is answered from the cache
    link.remote_metrics(&nixos(), 1).unwrap();
    assert_eq!(w.requests("job_metrics"), 1);
}

// R-622
#[test]
fn r622_a_job_the_origin_did_not_submit_is_unknown_to_it() {
    let w = PeerWorld::new();
    let link = w.link();
    link.probe_all();
    // the worker holds a job of another peer under id 1
    {
        use tasks_peer::receiver::JobRunner;
        let spec = record(&MemoryStore::new(), &w.root_a).spec;
        let key = tasks_peer::SubmitKey::new("mac:1").unwrap();
        let other = tasks_peer::PeerId::new("mac-node").unwrap();
        let (id, _) = w.runner.submit(&other, &key, spec).unwrap();
        w.runner.set_metrics(id, metrics_view());
    }
    let v = link.remote_metrics(&nixos(), 1).unwrap();
    assert!(v.samples.is_empty());
    assert!(
        v.note
            .as_deref()
            .unwrap()
            .contains("no longer has remote job #1"),
        "{v:?}"
    );
}

// R-624
#[test]
fn r624_an_older_peer_gets_a_clear_upgrade_note() {
    struct Old(Arc<dyn tasks_peer::Handler>);
    impl tasks_peer::Handler for Old {
        fn handle(
            &self,
            from: &tasks_peer::PeerId,
            msg: tasks_peer::proto::Body,
        ) -> tasks_peer::proto::Body {
            if matches!(msg, tasks_peer::proto::Body::JobMetricsRequest { .. }) {
                return tasks_peer::proto::Body::Refuse(Refusal::UnknownMessage {
                    name: "job_metrics_request".into(),
                });
            }
            self.0.handle(from, msg)
        }
        fn events(
            &self,
            from: &tasks_peer::PeerId,
            remote: tasks_peer::RemoteJobId,
            off: u64,
        ) -> Result<Box<dyn tasks_peer::transport::EventStream>, Refusal> {
            self.0.events(from, remote, off)
        }
    }
    let w = PeerWorld::new();
    remote_with_metrics(&w);
    w.net.serve(
        &w.nixos,
        Arc::new(Old(Arc::new(w.receiver_for_test()))) as Arc<dyn tasks_peer::Handler>,
    );
    let link = w.link();
    link.probe_all();
    let v = link.remote_metrics(&nixos(), 1).unwrap();
    assert!(v.samples.is_empty());
    assert_eq!(
        v.note.as_deref(),
        Some("peer nixos does not serve job metrics; upgrade it")
    );
}

// R-625
#[test]
fn r625_an_unreachable_or_stalled_peer_gives_an_empty_view_within_the_bound() {
    let w = PeerWorld::new();
    remote_with_metrics(&w);
    let mut o = w.options();
    o.metrics_timeout = Duration::from_millis(150);
    let link = PeerLink::new(w.net.transport(&w.origin), o);
    link.probe_all();
    // stalled: the worker answers after far longer than the bound
    w.net.set_latency(Duration::from_secs(3));
    let t0 = Instant::now();
    let v = link.remote_metrics(&nixos(), 1).unwrap();
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    assert!(v.samples.is_empty());
    assert_eq!(v.note.as_deref(), Some("nixos unreachable"));
    // down: the next probe knows, and the ask returns at once
    w.net.set_latency(Duration::ZERO);
    w.net.set_down(&w.nixos, true);
    link.probe_all();
    let t0 = Instant::now();
    let v = link.remote_metrics(&nixos(), 2).unwrap();
    assert!(t0.elapsed() < Duration::from_millis(100));
    assert_eq!(v.note.as_deref(), Some("nixos unreachable"));
}
