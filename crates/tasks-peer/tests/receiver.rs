//! The receiving side: allowlist, policy, idempotent submit, version refusal (R-152..R-154).
mod common;
use common::*;
use std::sync::Arc;
use tasks_core::{JobId, Placement};
use tasks_peer::fake::FakeRunner;
use tasks_peer::outputs::FetchPolicy;
use tasks_peer::proto::{Body, Hello, SubmitJob};
use tasks_peer::receiver::{AllowAll, SubmitPolicy};
use tasks_peer::transport::{Connection, PeerError, Transport};
use tasks_peer::{CheckoutId, Handler, PeerId, Refusal, SubmitKey};

fn conn(w: &World) -> Box<dyn Connection> {
    w.transport
        .connect(&w.nixos, std::time::Duration::from_secs(5))
        .unwrap()
}

fn synced_world() -> World {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), std::time::Duration::ZERO);
    // Run only far enough to sync and submit.
    w.run(&mut f, &mut log, &mut clock, 3, |_, _| {});
    assert!(f.remote().is_some());
    w
}

fn submit(w: &World, key: &str, rel_cwd: &str) -> Result<Body, PeerError> {
    conn(w).request(Body::Submit(SubmitJob {
        key: SubmitKey::new(key).unwrap(),
        checkout: CheckoutId::derive("laptop", &w.root),
        rel_cwd: rel_cwd.into(),
        spec: w.spec(rel_cwd),
    }))
}

#[test]
fn r154_unknown_peers_get_a_named_refusal_for_everything() {
    let w = World::new();
    let stranger = PeerId::new("stranger").unwrap();
    let t = w.net.transport(&stranger);
    let mut c = t
        .connect(&w.nixos, std::time::Duration::from_secs(5))
        .unwrap();
    for msg in [
        Body::Hello(Hello::local("x")),
        Body::StatusRequest,
        Body::Cancel {
            remote: tasks_peer::RemoteJobId(1),
        },
    ] {
        let r = c.request(msg);
        assert!(
            matches!(r, Err(PeerError::Refused(Refusal::NotAllowed { ref peer })) if peer == "stranger"),
            "{r:?}"
        );
    }
    assert!(matches!(
        c.subscribe(tasks_peer::RemoteJobId(1), 0),
        Err(PeerError::Refused(Refusal::NotAllowed { .. }))
    ));
}

#[test]
fn r154_an_allowlisted_peer_gets_hello_and_status() {
    let w = World::new();
    let mut c = conn(&w);
    let Body::HelloAck(h) = c.request(Body::Hello(Hello::local("laptop"))).unwrap() else {
        panic!("no ack");
    };
    assert_eq!(h.name, "nixos");
    let Body::Status(s) = c.request(Body::StatusRequest).unwrap() else {
        panic!("no status");
    };
    assert_eq!(s.machine.cores, 16);
    let mut future = Hello::local("laptop");
    future.min_version = 7;
    future.max_version = 9;
    let r = c.request(Body::Hello(future));
    assert!(matches!(
        r,
        Err(PeerError::Refused(Refusal::UnsupportedVersion {
            theirs: 9,
            ..
        }))
    ));
}

#[test]
fn r152_submit_is_idempotent_per_key_and_maps_the_spec_into_the_synced_copy() {
    let w = synced_world();
    // `synced_world` already submitted key laptop:1.
    let Body::Submitted(a) = submit(&w, "laptop:1", "").unwrap() else {
        panic!()
    };
    assert!(a.duplicate);
    assert_eq!(w.runner.submitted().len(), 1);
    let Body::Submitted(b) = submit(&w, "laptop:2", "src").unwrap() else {
        panic!()
    };
    assert!(!b.duplicate);
    let subs = w.runner.submitted();
    assert_eq!(subs.len(), 2);
    let spec = &subs[1].1;
    let tree = w
        .store
        .tree_root(&w.origin, &CheckoutId::derive("laptop", &w.root));
    assert_eq!(spec.cwd, tree.join("src"));
    assert_eq!(spec.placement, Placement::Here, "never forwarded again");
    assert!(spec.after.is_empty());
    assert_eq!(spec.argv, ["cargo", "test"]);
}

#[test]
fn r152_keys_are_scoped_to_the_submitting_peer() {
    let w = synced_world();
    let other = PeerId::new("other-node").unwrap();
    let r = Arc::clone(&w.runner);
    let k = SubmitKey::new("laptop:1").unwrap();
    use tasks_peer::JobRunner;
    let (_, dup) = r.submit(&other, &k, w.spec("")).unwrap();
    assert!(!dup, "another peer's key is a different key");
    // And it cannot see or cancel the first peer's job.
    assert!(r.view(&other, tasks_peer::RemoteJobId(1)).is_none());
    assert!(r.cancel(&other, tasks_peer::RemoteJobId(1)).is_none());
}

#[test]
fn r153_the_receiver_applies_its_own_policy() {
    let w = synced_world();
    // Device denylist comes from the job rules.
    let mut spec = w.spec("");
    spec.argv = vec![
        "adb".into(),
        "-s".into(),
        "ZD22257P6G".into(),
        "shell".into(),
    ];
    let r = conn(&w).request(Body::Submit(SubmitJob {
        key: SubmitKey::new("laptop:9").unwrap(),
        checkout: CheckoutId::derive("laptop", &w.root),
        rel_cwd: String::new(),
        spec,
    }));
    assert!(
        matches!(r, Err(PeerError::Refused(Refusal::PolicyDenied { .. }))),
        "{r:?}"
    );

    // A custom policy (broad tests, Cargo cap) refuses with its own reason.
    struct NoCargoTest;
    impl SubmitPolicy for NoCargoTest {
        fn check(&self, spec: &tasks_core::JobSpec) -> Result<(), String> {
            if spec.argv.iter().any(|a| a == "test") {
                Err("broad cargo tests are refused on this peer".into())
            } else {
                Ok(())
            }
        }
    }
    let runner = Arc::new(FakeRunner::new());
    let strict = w.receiver_with(runner, Arc::new(NoCargoTest), FetchPolicy::default());
    w.net.serve(&w.nixos, strict as Arc<dyn Handler>);
    let r = submit(&w, "laptop:10", "");
    match r {
        Err(PeerError::Refused(Refusal::PolicyDenied { reason })) => {
            assert!(reason.contains("broad cargo tests"))
        }
        other => panic!("{other:?}"),
    }
    let _ = AllowAll;
}

#[test]
fn unsynced_checkouts_and_unsafe_cwds_are_refused() {
    let w = World::new();
    let r = submit(&w, "laptop:1", "");
    assert!(matches!(
        r,
        Err(PeerError::Refused(Refusal::NotSynced { .. }))
    ));
    let w = synced_world();
    for bad in ["../..", "/etc", "missing/dir"] {
        let r = submit(&w, "laptop:5", bad);
        assert!(
            matches!(r, Err(PeerError::Refused(Refusal::BadPath { .. }))),
            "{bad}: {r:?}"
        );
    }
    let _ = JobId(0);
}
