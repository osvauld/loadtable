//! Retention: the worker drops idle working copies and unused mirrors, the origin prunes its
//! snapshot objects; every deletion is reported with the bytes freed (R-309, R-310).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;
use common::*;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tasks_peer::sync::testrepo::git;
use tasks_peer::sync::{
    Deleted, GixSnapshot, GixTransfer, OriginRetention, OriginSync, SyncDriver, SyncEvent, SyncJob,
    SyncLimits, SyncObserver, SyncStore, WorkerRetention, dir_size,
};
use tasks_peer::transport::Transport;
use tasks_peer::{CheckoutId, PeerId};

const DAY_MS: i64 = 86_400_000;

#[derive(Default)]
struct Events(Mutex<Vec<SyncEvent>>);
impl SyncObserver for Events {
    fn event(&self, e: &SyncEvent) {
        self.0.lock().unwrap().push(e.clone());
    }
}
impl Events {
    fn take(&self) -> Vec<SyncEvent> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

fn push(w: &World, driver: &OriginSync, root: &Path, session: u64) {
    let mut conn = w
        .transport
        .connect(&w.nixos, Duration::from_secs(5))
        .unwrap();
    let id = CheckoutId::derive("laptop", root);
    driver
        .push(
            conn.as_mut(),
            &SyncJob {
                root,
                checkout: &id,
                origin_host: "laptop",
                peer: &w.nixos,
                local_job: 1,
                session,
            },
        )
        .unwrap();
}

struct Rig {
    w: World,
    clock: Arc<AtomicI64>,
    ev: Arc<Events>,
    store: Arc<SyncStore>,
}

fn rig() -> Rig {
    let w = World::new();
    let clock = Arc::new(AtomicI64::new(1_000 * DAY_MS));
    let ev = Arc::new(Events::default());
    let c = clock.clone();
    let store = Arc::new(
        SyncStore::new(
            w.tmp.path().join("ret-store"),
            SyncLimits::default(),
            Arc::new(GixTransfer::new()),
        )
        .with_clock(Arc::new(move || c.load(Ordering::SeqCst)))
        .with_observer(ev.clone()),
    );
    w.serve_store(store.clone());
    Rig {
        w,
        clock,
        ev,
        store,
    }
}

fn pack_count(mirror: &Path) -> usize {
    fs::read_dir(mirror.join("objects/pack"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count()
}

#[test]
fn r309_an_idle_working_copy_is_deleted_with_its_unused_mirror_and_the_bytes_are_reported() {
    let r = rig();
    let w = &r.w;
    let other = w.tmp.path().join("origin/second");
    make_repo(&other);
    write(&other, "assets/blob.txt", &"payload ".repeat(5000));
    push(w, &w.origin_sync, &w.root, 1);
    push(w, &w.origin_sync, &other, 2);
    r.ev.take();
    let a = CheckoutId::derive("laptop", &w.root);
    let b = CheckoutId::derive("laptop", &other);
    let peer_dir = w.tmp.path().join("ret-store/laptop-node");
    let policy = WorkerRetention {
        gc_every: Duration::from_secs(u64::MAX / 4),
        ..WorkerRetention::default()
    };
    let free = |_: &PeerId, _: &Path| false;
    // nothing is idle yet
    assert!(r.store.sweep(&policy, &free).is_empty());
    // six days on, copy A is used again (a fetch of its outputs); two days more and B is 8 days idle
    r.clock.fetch_add(6 * DAY_MS, Ordering::SeqCst);
    r.store.touch(&w.origin, &a);
    r.clock.fetch_add(2 * DAY_MS, Ordering::SeqCst);
    let b_bytes = dir_size(&peer_dir.join(b.as_str()));
    assert!(b_bytes > 0);
    let events = r.store.sweep(&policy, &free);
    let removed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SyncEvent::Deleted {
                kind,
                peer,
                target,
                bytes,
            } => Some((*kind, peer.clone(), target.clone(), *bytes)),
            _ => None,
        })
        .collect();
    assert!(
        removed.contains(&(
            Deleted::WorkingCopy,
            Some(w.origin.clone()),
            b.to_string(),
            b_bytes
        )),
        "{removed:?}"
    );
    let mirror_event = removed
        .iter()
        .find(|(k, ..)| *k == Deleted::Mirror)
        .unwrap();
    assert!(mirror_event.3 > 0, "the mirror's bytes are reported too");
    assert_eq!(removed.len(), 2, "A is still in use: {removed:?}");
    // reported to the observer as well, and the disk agrees
    assert_eq!(r.ev.take().len(), 2);
    assert!(!peer_dir.join(b.as_str()).exists());
    assert!(r.store.is_synced(&w.origin, &a));
    assert_eq!(fs::read_dir(peer_dir.join("mirrors")).unwrap().count(), 1);
    // a copy with a live job is never deleted, however idle
    r.clock.fetch_add(60 * DAY_MS, Ordering::SeqCst);
    let busy = |_: &PeerId, _: &Path| true;
    assert!(r.store.sweep(&policy, &busy).is_empty());
    assert!(r.store.is_synced(&w.origin, &a));
    let events = r.store.sweep(&policy, &free);
    assert!(events.iter().any(|e| matches!(e, SyncEvent::Deleted { kind: Deleted::WorkingCopy, target, .. } if *target == a.to_string())));
    assert!(!r.store.is_synced(&w.origin, &a));
    assert!(
        !peer_dir.join("mirrors").read_dir().unwrap().any(|_| true),
        "no copy needs a mirror now"
    );
    // the idle limit is a policy: one day
    assert_eq!(
        WorkerRetention::idle_days(1).idle,
        Duration::from_secs(86_400)
    );
}

#[test]
fn r309_mirrors_are_collected_on_their_schedule() {
    let r = rig();
    let w = &r.w;
    // several syncs leave several packs in the mirror
    for i in 0..4 {
        write(
            &w.root,
            "src/main.rs",
            &format!("fn main() {{ /* {i} {} */ }}\n", "x".repeat(i * 300)),
        );
        push(w, &w.origin_sync, &w.root, i as u64 + 1);
    }
    let a = CheckoutId::derive("laptop", &w.root);
    let mirror = fs::read_dir(w.tmp.path().join("ret-store/laptop-node/mirrors"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(pack_count(&mirror) >= 4);
    let free = |_: &PeerId, _: &Path| false;
    let never = WorkerRetention {
        gc_every: Duration::from_secs(7 * 86_400),
        ..WorkerRetention::default()
    };
    // the first sweep collects (never collected before); the next, a day later, does not
    r.ev.take();
    r.store.sweep(&never, &free);
    assert_eq!(pack_count(&mirror), 1, "repacked into one");
    assert!(
        r.store.is_synced(&w.origin, &a),
        "collecting keeps the snapshots the copy needs"
    );
    write(&w.root, "src/main.rs", "fn main() { /* after gc */ }\n");
    push(w, &w.origin_sync, &w.root, 9);
    let packs = pack_count(&mirror);
    r.clock.fetch_add(DAY_MS, Ordering::SeqCst);
    r.store.sweep(&never, &free);
    assert_eq!(pack_count(&mirror), packs, "not due yet");
    r.clock.fetch_add(7 * DAY_MS, Ordering::SeqCst);
    r.store.touch(&w.origin, &a);
    r.store.sweep(&never, &free);
    assert_eq!(pack_count(&mirror), 1);
}

#[test]
fn r310_the_origin_keeps_only_the_newest_snapshot_per_checkout_and_reports_what_it_freed() {
    let w = World::new();
    let ev = Arc::new(Events::default());
    let clock = Arc::new(AtomicI64::new(1_000 * DAY_MS));
    let c = clock.clone();
    let origin = OriginSync::new(
        Arc::new(GixSnapshot::new(&w.origin_state)),
        Arc::new(GixTransfer::new()),
        SyncLimits::default(),
        &w.origin_state,
    )
    .with_observer(ev.clone())
    .with_clock(Arc::new(move || c.load(Ordering::SeqCst)));
    for i in 0..3 {
        write(
            &w.root,
            "src/gen.rs",
            &format!("// version {i}\n{}", "line\n".repeat(200 * (i + 1))),
        );
        push(&w, &origin, &w.root, i as u64 + 1);
    }
    let sync_dir = fs::read_dir(w.origin_state.join("sync"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let tasks_git = sync_dir.join("git");
    let blob = |i: usize| {
        let p = w.tmp.path().join(format!("blob{i}"));
        fs::write(
            &p,
            format!("// version {i}\n{}", "line\n".repeat(200 * (i + 1))),
        )
        .unwrap();
        git(&w.root, &["hash-object", p.to_str().unwrap()]).unwrap()
    };
    let has = |id: &str| {
        git(
            &w.root,
            &[
                "--git-dir",
                tasks_git.to_str().unwrap(),
                "cat-file",
                "-e",
                id,
            ],
        )
        .is_ok()
    };
    assert!(has(&blob(0)) && has(&blob(1)) && has(&blob(2)));
    // objects younger than the minimum age are never pruned
    let young = origin.prune(&OriginRetention::default());
    assert!(young.is_empty(), "{young:?}");
    let events = origin.prune(&OriginRetention {
        min_age: Duration::ZERO,
        ..OriginRetention::default()
    });
    assert_eq!(events.len(), 1);
    let SyncEvent::Deleted {
        kind: Deleted::OriginObjects,
        peer: None,
        bytes,
        ..
    } = &events[0]
    else {
        panic!("{events:?}")
    };
    assert!(*bytes > 0);
    assert!(has(&blob(2)), "the newest snapshot is kept");
    assert!(!has(&blob(0)) && !has(&blob(1)), "older snapshots are gone");
    assert_eq!(
        ev.take()
            .iter()
            .filter(|e| matches!(e, SyncEvent::Deleted { .. }))
            .count(),
        1,
        "reported to the observer too"
    );
    // and the next sync still works, sending only what the worker lacks
    write(&w.root, "src/gen.rs", "// version 3\n");
    push(&w, &origin, &w.root, 9);
    // a checkout not synced for a week stops holding objects; its stat cache goes too
    clock.fetch_add(8 * DAY_MS, Ordering::SeqCst);
    let events = origin.prune(&OriginRetention {
        min_age: Duration::ZERO,
        ..OriginRetention::default()
    });
    assert_eq!(events.len(), 1, "{events:?}");
    let id = CheckoutId::derive("laptop", &w.root);
    assert!(!sync_dir.join(format!("{id}.index")).exists());
}

/// Build output kept per checkout, as the daemon's cache would.
#[derive(Default)]
struct Outputs(Mutex<std::collections::BTreeMap<String, u64>>);
impl tasks_peer::OutputKeeper for Outputs {
    fn outputs(&self, _peer: &PeerId) -> Vec<(CheckoutId, u64)> {
        let m = self.0.lock().unwrap();
        m.iter()
            .map(|(k, v)| (CheckoutId::parse(k).unwrap(), *v))
            .collect()
    }
    fn drop_outputs(&self, _peer: &PeerId, checkout: &CheckoutId) -> u64 {
        self.0
            .lock()
            .unwrap()
            .remove(checkout.as_str())
            .unwrap_or(0)
    }
}

#[test]
fn r839_a_peer_reports_each_copy_with_its_output_and_drops_all_but_one_a_job_uses() {
    use tasks_peer::Handler;
    use tasks_peer::fake::{FakeRunner, FakeStatus};
    use tasks_peer::outputs::FetchPolicy;
    use tasks_peer::proto::Body;
    use tasks_peer::receiver::{AllowAll, Allowlist, JobRunner, Receiver};
    let r = rig();
    let w = &r.w;
    let other = w.tmp.path().join("origin/second");
    make_repo(&other);
    push(w, &w.origin_sync, &w.root, 1);
    push(w, &w.origin_sync, &other, 2);
    let a = CheckoutId::derive("laptop", &w.root);
    let b = CheckoutId::derive("laptop", &other);
    // build output left after retention deleted its copy
    let c = CheckoutId::parse(&"c".repeat(32)).unwrap();
    let runner = Arc::new(FakeRunner::new());
    let mut spec = w.spec("");
    spec.cwd = r.store.tree_root(&w.origin, &a);
    runner
        .submit(
            &w.origin,
            &tasks_peer::SubmitKey::new("laptop:1").unwrap(),
            spec,
        )
        .unwrap();
    let outputs = Arc::new(Outputs::default());
    for (id, n) in [(&a, 7), (&b, 9), (&c, 11)] {
        outputs.0.lock().unwrap().insert(id.to_string(), n);
    }
    let recv = Receiver::new(
        "nixos",
        Allowlist::new([w.origin.clone()]),
        r.store.clone(),
        runner,
        Arc::new(FakeStatus::nixos()),
        Arc::new(AllowAll),
        FetchPolicy::default(),
    );
    recv.set_outputs(outputs.clone());
    w.net.serve(&w.nixos, Arc::new(recv) as Arc<dyn Handler>);
    let mut conn = w
        .transport
        .connect(&w.nixos, Duration::from_secs(5))
        .unwrap();

    let Body::CacheHeld { checkouts: held } = conn.request(Body::CacheRequest).unwrap() else {
        panic!("not a cache report");
    };
    let of = |id: &CheckoutId| held.iter().find(|h| &h.checkout == id).unwrap().clone();
    assert!(of(&a).busy && of(&a).copy_bytes > 0 && of(&a).output_bytes == 7);
    assert!(!of(&b).busy && of(&b).copy_bytes > 0 && of(&b).output_bytes == 9);
    assert_eq!(
        (of(&c).copy_bytes, of(&c).output_bytes, of(&c).last_used_ms),
        (0, 11, None)
    );
    let b_copy = of(&b).copy_bytes;

    let Body::CacheDropped { checkouts: dropped } = conn
        .request(Body::CacheDrop {
            checkouts: vec![a.clone(), b.clone(), c.clone()],
        })
        .unwrap()
    else {
        panic!("not a drop answer");
    };
    let of = |id: &CheckoutId| dropped.iter().find(|d| &d.checkout == id).unwrap().clone();
    assert_eq!(of(&a).freed_bytes, 0);
    assert_eq!(of(&a).kept.as_deref(), Some("a job runs or waits in it"));
    assert_eq!((of(&b).freed_bytes, of(&b).kept), (b_copy + 9, None));
    assert_eq!(of(&c).freed_bytes, 11);
    assert!(r.store.tree_root(&w.origin, &a).exists());
    assert!(!r.store.tree_root(&w.origin, &b).exists());
    assert_eq!(
        outputs
            .0
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![a.to_string()]
    );
}
