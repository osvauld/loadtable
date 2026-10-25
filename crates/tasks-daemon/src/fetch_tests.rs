//! Output fetch for forwarded jobs (`--fetch`): what lands in the checkout, what is reported,
//! and what is refused. Names carry the regression ids they pin.

use crate::api::*;
use crate::forwarder::{ForwardEnd, ForwardPoll, Forwarder};
use crate::peer_link::{PeerLink, fetch};
use crate::testkit::{PeerWorld, Rig};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tasks_core::{
    FetchResult, FetchStatus, JobId, JobSpec, Kind, MachineName, Owner, Phase, Placement,
    Requirements,
};
use tasks_ledger::{AuditLedger, JobRecord, MemoryStore, Store};
use tasks_peer::RemoteJobId;
use tasks_peer::fake::FakeTransport;
use tasks_peer::sync::{SyncGate, SyncPhase};

const APK: &str = "android/app/build/outputs/apk/debug/app-debug.apk";

fn nixos() -> MachineName {
    MachineName("nixos".into())
}

fn record(store: &MemoryStore, root: &Path, fetch: &[&str]) -> JobRecord {
    let spec = JobSpec {
        owner: Owner::new("tester").unwrap(),
        argv: vec!["./android/gradlew".into(), ":app:assembleDebug".into()],
        cwd: root.to_path_buf(),
        env: vec![],
        kind: Kind::Package,
        requirements: Requirements::default(),
        placement: Placement::Anywhere,
        after: vec![],
        timeout_secs: None,
        broad_test_escape: false,
        script_escape: false,
        priority: tasks_core::Priority::Normal,
        priority_reason: None,
        fetch: fetch.iter().map(ToString::to_string).collect(),
        isolate: false,
        light: false,
        retry_alone: false,
        interactive: false,
    };
    let id = store.submit(&spec).unwrap();
    store.get(id).unwrap().unwrap()
}

fn until(what: &str, cond: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !cond() {
        assert!(t0.elapsed() < Duration::from_secs(10), "timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn remote_of(link: &dyn Forwarder, id: JobId) -> Option<RemoteJobId> {
    match link.poll(id) {
        ForwardPoll::Active(p) if p.accepted => p.remote_job.map(|r| RemoteJobId(r as u64)),
        _ => None,
    }
}

fn end_of(link: &dyn Forwarder, id: JobId) -> ForwardEnd {
    let t0 = Instant::now();
    loop {
        if let ForwardPoll::Done(e) = link.poll(id) {
            return e;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "never ended");
        std::thread::sleep(Duration::from_millis(2));
    }
}

struct Ran {
    end: ForwardEnd,
    log: PathBuf,
    /// The origin's checkout root, where fetched files land.
    dest: PathBuf,
}

/// Forwards a job asking for `fetch`, lets `setup(worker tree, origin root)` make the files it
/// "built" while the job runs, and finishes it with `exit`.
fn run(
    w: &PeerWorld,
    link: &PeerLink<FakeTransport>,
    fetch: &[&str],
    exit: i32,
    setup: impl FnOnce(&Path, &Path),
) -> Ran {
    let store = MemoryStore::new();
    let rec = record(&store, &w.root_a, fetch);
    link.forward(&rec, &nixos()).unwrap();
    let log = w.dir.path().join(format!("job-{}.log", rec.id.0));
    link.start(rec.id, &log).unwrap();
    until("accepted", || remote_of(link, rec.id).is_some());
    let remote = remote_of(link, rec.id).unwrap();
    let dest = PathBuf::from(crate::place::checkout_key(&w.root_a));
    setup(&w.worker_tree(&w.root_a), &dest);
    w.runner.finish(remote, exit, vec![]);
    let end = end_of(link, rec.id);
    Ran { end, log, dest }
}

fn put(tree: &Path, rel: &str, content: &[u8]) {
    let p = tree.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn sha(path: &Path) -> String {
    tasks_peer::sync::hash_file(path).unwrap().0.to_hex()
}

fn fetched(r: &FetchResult) -> (u64, String) {
    match &r.status {
        FetchStatus::Fetched { bytes, sha256, .. } => (*bytes, sha256.clone()),
        other => panic!("not fetched: {other:?}"),
    }
}

// R-505
#[test]
fn r505_a_passed_forwarded_job_brings_its_file_back_with_the_right_hash() {
    let w = PeerWorld::new();
    let link = w.link();
    let apk = "apk bytes ".repeat(60_000);
    let ran = run(&w, &link, &[APK], 0, |tree, _| {
        put(tree, APK, apk.as_bytes())
    });
    assert_eq!(ran.end.phase, Phase::Passed);
    assert_eq!(ran.end.exit_code, Some(0));
    let landed = ran.dest.join(APK);
    assert_eq!(std::fs::read(&landed).unwrap(), apk.as_bytes());
    let [one] = ran.end.fetch.as_slice() else {
        panic!("{:?}", ran.end.fetch)
    };
    assert_eq!(one.path, APK);
    assert_eq!(fetched(one), (apk.len() as u64, sha(&landed)));
    // the log says so, with the size and the hash
    let log = std::fs::read_to_string(&ran.log).unwrap();
    assert!(
        log.contains(&format!(
            "[tasks] fetched {APK} (585.9 KiB, sha256 {})",
            sha(&landed)
        )),
        "{log}"
    );
    // and `get` can read it back later
    assert_eq!(fetch::read_results(&ran.log), ran.end.fetch);
    // nothing but the file was left in its directory
    let names: Vec<_> = std::fs::read_dir(landed.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["app-debug.apk"]);
}

// R-648
#[test]
fn r648_a_forwarded_job_keeps_what_its_sync_sent_and_how_long_it_took() {
    let w = PeerWorld::new();
    let link = w.link();
    let ran = run(&w, &link, &[APK], 0, |tree, _| put(tree, APK, b"x"));
    assert_eq!(ran.end.phase, Phase::Passed);
    let note = crate::peer_link::sync_note::read(&ran.log).expect("a sync note");
    assert!(note.files > 0, "{note:?}");
    assert!(note.bytes > 0 || note.already_there, "{note:?}");
}

// R-506
#[test]
fn r506_a_missing_path_is_reported_and_the_other_files_still_arrive() {
    let w = PeerWorld::new();
    let link = w.link();
    let ran = run(
        &w,
        &link,
        &["out/nope.apk", "out/report.txt"],
        0,
        |tree, _| put(tree, "out/report.txt", b"report"),
    );
    assert_eq!(ran.end.phase, Phase::Passed, "the job's own result stands");
    assert_eq!(ran.end.fetch[0].status, FetchStatus::Missing);
    assert!(ran.end.fetch[1].ok());
    assert_eq!(
        std::fs::read(ran.dest.join("out/report.txt")).unwrap(),
        b"report"
    );
    assert!(!ran.dest.join("out/nope.apk").exists());
    let log = std::fs::read_to_string(&ran.log).unwrap();
    assert!(log.contains("[tasks] fetch failed: out/nope.apk does not exist on the peer"));
    assert!(log.contains("[tasks] fetched out/report.txt"));
}

// R-507
#[test]
fn r507_paths_outside_the_checkout_are_refused_at_submit() {
    let r = Rig::new();
    for bad in ["../secret", "/etc/passwd", "a/../../b", ".git/config", ""] {
        let mut spec = r.spec("agent-1", &["make", "all"], Kind::Run);
        spec.fetch = vec![bad.to_string()];
        let reply = r.sch.handle(Request::Submit { spec });
        match reply {
            Reply::Err(_) => {}
            Reply::Ok(x) => panic!("{bad:?} was accepted: {x:?}"),
        }
    }
    let mut ok = r.spec("agent-1", &["make", "all"], Kind::Run);
    ok.fetch = vec![APK.to_string()];
    assert!(matches!(
        r.sch.handle(Request::Submit { spec: ok }),
        Reply::Ok(_)
    ));
}

// R-508
#[test]
fn r508_a_file_over_the_limit_is_reported_with_its_size_and_limit_and_not_written() {
    let w = PeerWorld::new();
    // the origin's own limit
    let mut opts = w.options();
    opts.fetch_max_bytes = 10;
    let link = PeerLink::new(w.net.transport(&w.origin), opts);
    let ran = run(
        &w,
        &link,
        &["out/big.bin", "out/small.txt"],
        0,
        |tree, _| {
            put(tree, "out/big.bin", &[7u8; 1000]);
            put(tree, "out/small.txt", b"ok");
        },
    );
    assert_eq!(
        ran.end.fetch[0].status,
        FetchStatus::TooLarge {
            bytes: 1000,
            limit: 10
        }
    );
    assert!(ran.end.fetch[1].ok());
    assert!(!ran.dest.join("out/big.bin").exists());
    let log = std::fs::read_to_string(&ran.log).unwrap();
    assert!(
        log.contains("fetch failed: out/big.bin is 1000 B (over the 10 B limit)"),
        "{log}"
    );
    // the peer's limit (a fresh world: the first job's submit key is spent)
    let w = PeerWorld::new();
    w.fetch_policy.lock().unwrap().max_file_bytes = 5;
    w.restart_worker();
    let link = w.link();
    let ran = run(&w, &link, &["out/big.bin"], 0, |tree, _| {
        put(tree, "out/big.bin", &[7u8; 1000]);
    });
    assert_eq!(
        ran.end.fetch[0].status,
        FetchStatus::TooLarge {
            bytes: 1000,
            limit: 5
        }
    );
}

// R-509
#[test]
fn r509_a_peer_without_fetch_support_gives_a_clear_error_and_the_job_still_passed() {
    let w = PeerWorld::new();
    w.restart_worker_without_fetch();
    let link = w.link();
    let ran = run(&w, &link, &["out/a.txt", "out/b.txt"], 0, |tree, _| {
        put(tree, "out/a.txt", b"a");
        put(tree, "out/b.txt", b"b");
    });
    assert_eq!(ran.end.phase, Phase::Passed);
    for r in &ran.end.fetch {
        let FetchStatus::Failed { reason } = &r.status else {
            panic!("{r:?}")
        };
        assert!(
            reason.contains("does not support output fetch") && reason.contains("upgrade"),
            "{reason}"
        );
    }
    assert_eq!(ran.end.fetch.len(), 2);
    assert_eq!(
        w.requests("fetch_list"),
        1,
        "the peer is asked once, not once per path"
    );
    assert!(!ran.dest.join("out/a.txt").exists());
}

// R-510
#[test]
fn r510_only_a_passed_job_is_fetched_for() {
    let w = PeerWorld::new();
    let link = w.link();
    let ran = run(&w, &link, &["out/a.txt"], 3, |tree, _| {
        put(tree, "out/a.txt", b"a")
    });
    assert_eq!(ran.end.phase, Phase::Failed { exit: Some(3) });
    assert_eq!(ran.end.exit_code, Some(3));
    let FetchStatus::Failed { reason } = &ran.end.fetch[0].status else {
        panic!()
    };
    assert!(reason.contains("the job did not pass"), "{reason}");
    assert_eq!(w.requests("fetch_list"), 0, "nothing is asked of the peer");
    assert!(!ran.dest.join("out/a.txt").exists());
}

// R-511
#[cfg(unix)]
#[test]
fn r511_a_placed_output_link_is_written_through_and_no_other_symlink_is() {
    use std::os::unix::fs::symlink;
    let w = PeerWorld::new();
    let mut opts = w.options();
    opts.fetch_links = vec!["android/app/build".to_string()];
    let link = PeerLink::new(w.net.transport(&w.origin), opts);
    let cache = w.dir.path().join("cache/android-app-build");
    let outside = w.dir.path().join("outside");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let ran = run(&w, &link, &[APK, "evil/x.txt"], 0, |tree, root| {
        put(tree, APK, b"apk");
        put(tree, "evil/x.txt", b"x");
        std::fs::create_dir_all(root.join("android/app")).unwrap();
        symlink(&cache, root.join("android/app/build")).unwrap();
        symlink(&outside, root.join("evil")).unwrap();
    });
    // through the placed link: the cache directory holds the file, the link is intact
    assert!(ran.end.fetch[0].ok(), "{:?}", ran.end.fetch);
    assert_eq!(
        std::fs::read(cache.join("outputs/apk/debug/app-debug.apk")).unwrap(),
        b"apk"
    );
    assert!(
        std::fs::symlink_metadata(ran.dest.join("android/app/build"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // through any other symlink: refused, nothing outside the checkout written
    let FetchStatus::Failed { reason } = &ran.end.fetch[1].status else {
        panic!("{:?}", ran.end.fetch[1])
    };
    assert!(reason.contains("symlink"), "{reason}");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

// R-511
#[cfg(unix)]
#[test]
fn r511_an_output_link_the_daemon_does_not_name_is_not_written_through() {
    use std::os::unix::fs::symlink;
    let w = PeerWorld::new();
    let link = w.link();
    let cache = w.dir.path().join("cache/app-build");
    std::fs::create_dir_all(&cache).unwrap();
    let ran = run(&w, &link, &[APK], 0, |tree, root| {
        put(tree, APK, b"apk");
        std::fs::create_dir_all(root.join("android/app")).unwrap();
        symlink(&cache, root.join("android/app/build")).unwrap();
    });
    assert!(!ran.end.fetch[0].ok());
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
}

// R-512
#[test]
fn r512_the_driver_takes_a_transfer_slot_for_a_fetch_and_not_for_a_job_without_one() {
    #[derive(Default)]
    struct Count(AtomicUsize);
    impl SyncGate for Count {
        fn enter(&self, phase: SyncPhase) -> Box<dyn Send> {
            if phase == SyncPhase::Fetch {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            Box::new(())
        }
    }
    let gate = Arc::new(Count::default());
    // one world per job: a second job in the same world would reuse the first one's submit key
    let linked = |w: &PeerWorld| {
        let mut opts = w.options();
        opts.gate = gate.clone();
        PeerLink::new(w.net.transport(&w.origin), opts)
    };
    let w = PeerWorld::new();
    run(&w, &linked(&w), &[], 0, |_, _| {});
    assert_eq!(gate.0.load(Ordering::SeqCst), 0);
    let w = PeerWorld::new();
    let ran = run(
        &w,
        &linked(&w),
        &["out/a.txt", "out/b.txt"],
        0,
        |tree, _| {
            put(tree, "out/a.txt", b"a");
            put(tree, "out/b.txt", b"b");
        },
    );
    assert!(
        ran.end.fetch.iter().all(FetchResult::ok),
        "{:?}",
        ran.end.fetch
    );
    assert_eq!(
        gate.0.load(Ordering::SeqCst),
        1,
        "one slot for the whole fetch"
    );
}

// R-513
#[test]
fn r513_a_job_followed_after_a_daemon_restart_still_fetches() {
    let w = PeerWorld::new();
    let store = MemoryStore::new();
    let rec = record(&store, &w.root_a, &["out/a.txt"]);
    let log = w.dir.path().join("x.log");
    {
        let first = w.link();
        first.forward(&rec, &nixos()).unwrap();
        first.start(rec.id, &log).unwrap();
        until("accepted", || remote_of(&first, rec.id).is_some());
    }
    store
        .set_forward(rec.id, &nixos(), Some(1), Some("laptop:1"))
        .unwrap();
    let rec = store.get(rec.id).unwrap().unwrap();
    put(&w.worker_tree(&w.root_a), "out/a.txt", b"after restart");
    let second = w.link();
    second.resume(&rec, &log).unwrap();
    w.runner.finish(RemoteJobId(1), 0, vec![]);
    let end = end_of(&second, rec.id);
    assert!(end.fetch[0].ok(), "{:?}", end.fetch);
    let dest = PathBuf::from(crate::place::checkout_key(&w.root_a));
    assert_eq!(
        std::fs::read(dest.join("out/a.txt")).unwrap(),
        b"after restart"
    );
}

// R-514
#[test]
fn r514_the_audit_log_gets_one_data_row_per_fetched_file() {
    use crate::forwarder::ForwardEnd;
    use crate::testkit::FakeForwarder;
    let f = Arc::new(FakeForwarder::new(vec![]));
    let r = Rig::with_forwarder(f.clone());
    let mut spec = r.spec("agent-1", &["gradlew", "assembleDebug"], Kind::Package);
    spec.placement = Placement::Anywhere;
    spec.fetch = vec![APK.into(), "out/missing".into()];
    let id = r.submit(&spec);
    r.store.set_forward(id, &nixos(), None, Some("k1")).unwrap();
    f.script(
        id,
        ForwardPoll::Active(crate::forwarder::ForwardProgress {
            mirror: "#7".into(),
            remote_job: Some(7),
            accepted: true,
            remote_phase: Some(Phase::Running),
        }),
    );
    r.until("running", || {
        r.tick();
        r.phase(id) == Phase::Running
    });
    f.script(
        id,
        ForwardPoll::Done(ForwardEnd {
            phase: Phase::Passed,
            reason: "passed on nixos".into(),
            exit_code: Some(0),
            signal: None,
            remote_job: Some(7),
            nothing_submitted: false,
            samples: vec![],
            fetch: vec![
                FetchResult {
                    path: APK.into(),
                    status: FetchStatus::Fetched {
                        bytes: 4096,
                        sha256: "ab".repeat(32),
                        unchanged: false,
                    },
                },
                FetchResult {
                    path: "out/missing".into(),
                    status: FetchStatus::Missing,
                },
            ],
        }),
    );
    assert_eq!(r.until_done(id), Phase::Passed);
    let rows: Vec<_> = r
        .audit_store
        .scan(0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|x| x.action == "data.fetched")
        .collect();
    assert_eq!(rows.len(), 1, "only the file that arrived");
    assert_eq!(rows[0].job, Some(id.0));
    assert_eq!(rows[0].target.as_deref(), Some(APK));
    let d = rows[0].details_json();
    assert_eq!(d["sha256"], "ab".repeat(32));
    assert_eq!(d["bytes"], 4096);
    assert_eq!(d["peer_name"], "nixos");
}

// R-503
#[test]
fn r503_a_job_that_ran_here_says_there_was_nothing_to_fetch_and_get_shows_the_recorded_results() {
    let r = Rig::new();
    r.procs.auto_exit(0);
    let mut spec = r.spec("agent-1", &["make", "all"], Kind::Run);
    spec.fetch = vec!["out/app.apk".into()];
    let id = r.submit(&spec);
    r.tick();
    assert_eq!(r.until_done(id), Phase::Passed);
    let log = r.log_text(id);
    assert!(
        log.contains("[tasks] fetch: this job runs here, so its output is already in the checkout; nothing fetched (out/app.apk)"),
        "{log}"
    );
    // a job without a fetch list has no such line
    let plain = r.submit(&r.spec("agent-1", &["make", "x"], Kind::Run));
    r.tick();
    assert_eq!(r.until_done(plain), Phase::Passed);
    assert!(!r.log_text(plain).contains("[tasks] fetch:"));
    // `get` carries the requested paths and, once recorded, the results
    let get = |id| {
        let Reply::Ok(Response::Job { job }) = r.sch.handle(Request::Get { id }) else {
            panic!("job expected")
        };
        job
    };
    let job = get(id);
    assert_eq!(job.fetch_paths, ["out/app.apk"]);
    assert!(job.fetch.is_empty());
    let rec = r.record(id);
    let results = vec![FetchResult {
        path: "out/app.apk".into(),
        status: FetchStatus::Missing,
    }];
    std::fs::write(
        fetch::sidecar_path(rec.log_path.as_ref().unwrap()),
        serde_json::to_vec(&results).unwrap(),
    )
    .unwrap();
    assert_eq!(get(id).fetch, results);
}
