use super::*;
use crate::metrics::Snapshot;
use crate::peer_link::{STREAM_OFFLINE, STREAM_ONLINE};
use serde_json::{Value, json};

const CHECKOUT: &str = "/home/dev/src/app/.claude/worktrees/feature";

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/snapshot/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

/// A job view from the busy-laptop fixture's waiting job, with the given fields.
fn view(id: i64, phase: &str, argv: &str, patch: Value) -> Value {
    let base = fixture("metrics-laptop-busy");
    let mut job = base["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == 302)
        .unwrap()
        .clone();
    let n = crate::metrics::command::normalize(&placement::strip_jobs(&words(argv)));
    job["id"] = json!(id);
    job["phase"] = json!(phase);
    job["argv"] = json!(words(argv));
    job["command"] = json!(argv);
    job["kind_key"] = json!(n.kind_key);
    job["checkout"] = json!(CHECKOUT);
    job["queue"] = Value::Null;
    job["estimate"] = Value::Null;
    job["result"] = Value::Null;
    for (k, v) in patch.as_object().unwrap() {
        job[k] = v.clone();
    }
    job
}

/// A machine: compile slot held by `holder` (id, remaining s) or free, these jobs.
fn machine(id: &str, holder: Option<(i64, f64)>, mut jobs: Vec<Value>, cap: u64) -> Snapshot {
    let mut s = fixture("metrics-idle");
    s["machine"]["id"] = json!(id);
    s["machine"]["blocker"] = Value::Null;
    s["machine"]["cargo_jobs_max"] = json!(cap);
    for slot in s["machine"]["slots"].as_array_mut().unwrap() {
        slot["busy"] = json!(false);
        slot["holder_job"] = Value::Null;
        slot["holder_who"] = Value::Null;
        if slot["kind"] == "compile"
            && let Some((h, _)) = holder
        {
            slot["busy"] = json!(true);
            slot["holder_job"] = json!(h);
        }
    }
    if let Some((h, remaining)) = holder {
        jobs.push(view(
            h,
            "building",
            "./gradlew assembleDebug",
            json!({"estimate": {"total_s": remaining + 600.0, "remaining_s": remaining,
                                "samples": 3, "basis": "median of 3 runs"}}),
        ));
    }
    s["jobs"] = json!(jobs);
    serde_json::from_value(s).unwrap()
}

fn waiting(id: i64, argv: &str, run_s: f64, position: u32, warmth: &str) -> Value {
    view(
        id,
        "waiting_build",
        argv,
        json!({"estimate": {"total_s": run_s, "remaining_s": run_s, "samples": 4,
                            "basis": "median of 4 runs of this kind on this machine"},
               "queue": {"position": position, "waiting_for": "compile slot"},
               "warmth": warmth}),
    )
}

fn passed(id: i64, argv: &str, wall_s: f64, warmth: &str, checkout: &str) -> Value {
    view(
        id,
        "passed",
        argv,
        json!({"checkout": checkout,
               "result": {"exit_code": 0, "wall_s": wall_s, "cpu_user_s": null,
                          "cpu_sys_s": null, "peak_rss_bytes": null,
                          "crates_compiled": null, "warmth": warmth}}),
    )
}

fn peer(name: &str, snapshot: Snapshot) -> Peer {
    Peer {
        name: name.into(),
        role: "worker".into(),
        state: STREAM_ONLINE.into(),
        age_s: Some(1.0),
        snapshot: Some(snapshot),
        ..Default::default()
    }
}

fn candidate(id: i64, argv: &str) -> Candidate {
    Candidate {
        id,
        phase: Phase::WaitingBuild,
        argv: words(argv),
        resource: Resource::Compile,
        source: CHECKOUT.into(),
        owner: "agent".into(),
        stages: 0,
        cancel_requested: false,
        moves: 0,
        pinned: None,
        admitted_here: false,
    }
}

const TEST: &str = "cargo test --offline --locked -j 2 -p video";

fn one(c: &[Candidate], local: &Snapshot, peers: &[Peer], policy: &Policy) -> Vec<Verdict> {
    decide(c, local, 2, peers, policy, &NoCapabilityMarks)
}

#[test]
fn a_job_behind_a_long_holder_moves_to_an_idle_peer_with_its_cold_build_cost() {
    // Today's case: kurukkan's compile slot held ~40 min by #494, #496 waits behind it, the
    // worker is idle and once built something cold in ~6 min.
    let local = machine(
        "kurukkan",
        Some((494, 2400.0)),
        vec![waiting(496, TEST, 30.0, 1, "warm")],
        2,
    );
    let worker = machine(
        "nixos",
        None,
        vec![passed(10, TEST, 360.0, "cold", "/elsewhere")],
        6,
    );
    let v = one(
        &[candidate(496, TEST)],
        &local,
        &[peer("nixos", worker)],
        &Policy::default(),
    );
    assert_eq!(v.len(), 1);
    let Action::Move {
        peer,
        argv,
        cargo_jobs,
    } = &v[0].action
    else {
        panic!("{v:#?}");
    };
    assert_eq!(peer, "nixos");
    assert_eq!(*cargo_jobs, Some(6));
    assert_eq!(
        argv,
        &words("env CARGO_BUILD_JOBS=6 cargo test --offline --locked -p video")
    );
    assert_eq!(v[0].local_wait_s, 2400.0);
    assert_eq!(v[0].local_run_s, 30.0);
    let there = &v[0].peers[0];
    assert_eq!(there.run_label, "cold build");
    assert_eq!(there.run_s, 360.0);
    assert!(
        v[0].line
            .starts_with("rerouted: kurukkan → nixos (~40m wait"),
        "{}",
        v[0].line
    );
    assert!(v[0].line.contains("~6m cold build there"), "{}", v[0].line);
}

#[test]
fn without_any_cold_history_the_peer_pays_the_cold_guess() {
    let local = machine(
        "kurukkan",
        Some((494, 2400.0)),
        vec![waiting(496, TEST, 30.0, 1, "warm")],
        2,
    );
    let v = one(
        &[candidate(496, TEST)],
        &local,
        &[peer("nixos", machine("nixos", None, vec![], 6))],
        &Policy::default(),
    );
    assert_eq!(v[0].peers[0].run_s, COLD_GUESS_S);
    assert!(v[0].moves().is_some(), "{}", v[0].line);
}

#[test]
fn a_warm_short_job_stays_when_the_peer_would_build_cold() {
    // #497: a warm 30 s test behind a holder with a minute left; the peer never built it.
    let local = machine(
        "kurukkan",
        Some((494, 60.0)),
        vec![waiting(497, TEST, 30.0, 1, "warm")],
        2,
    );
    let v = one(
        &[candidate(497, TEST)],
        &local,
        &[peer("nixos", machine("nixos", None, vec![], 6))],
        &Policy::default(),
    );
    assert!(v[0].moves().is_none(), "{v:#?}");
    assert!(v[0].line.starts_with("stays: ~2m here"), "{}", v[0].line);
    assert!(v[0].line.contains("needs >= 2x"), "{}", v[0].line);
}

#[test]
fn a_peer_that_ran_this_checkout_is_warm_there() {
    let local = machine(
        "kurukkan",
        Some((494, 900.0)),
        vec![waiting(496, TEST, 100.0, 1, "warm")],
        2,
    );
    let worker = machine(
        "nixos",
        None,
        vec![passed(11, TEST, 40.0, "warm", CHECKOUT)],
        6,
    );
    let v = one(
        &[candidate(496, TEST)],
        &local,
        &[peer("nixos", worker)],
        &Policy::default(),
    );
    assert_eq!(v[0].peers[0].run_label, "warm run");
    assert_eq!(v[0].peers[0].run_s, 40.0);
    assert!(v[0].moves().is_some(), "{}", v[0].line);
    assert!(v[0].line.contains("warm run there"), "{}", v[0].line);
}

#[test]
fn device_run_forward_pinned_started_moved_and_cancelled_jobs_never_move() {
    let local = machine("kurukkan", Some((494, 7200.0)), vec![], 2);
    let worker = || machine("nixos", None, vec![], 6);
    let mut cases: Vec<(Candidate, &str)> = vec![];
    let mut c = candidate(1, "adb -s SERIAL1 install -r app.apk");
    c.resource = Resource::Device("SERIAL1".into());
    cases.push((c, "device class"));
    let mut c = candidate(2, "./target/debug/test-bin");
    c.resource = Resource::Run;
    cases.push((c, "run class"));
    let mut c = candidate(
        3,
        "tasks peer-forward --state /s --peer nixos --who a -- true",
    );
    c.resource = Resource::Offload;
    cases.push((c, "already a forward"));
    let mut c = candidate(4, TEST);
    c.pinned = Some("--pin at submit".into());
    cases.push((c, "pinned (--pin at submit)"));
    let mut c = candidate(5, TEST);
    c.phase = Phase::Building;
    cases.push((c, "already started"));
    let mut c = candidate(6, TEST);
    c.stages = 1;
    cases.push((c, "already started"));
    let mut c = candidate(7, TEST);
    c.admitted_here = true;
    cases.push((c, "already started"));
    let mut c = candidate(8, TEST);
    c.moves = 1;
    cases.push((c, "already moved once"));
    let mut c = candidate(9, TEST);
    c.cancel_requested = true;
    cases.push((c, "cancel requested"));
    let mut c = candidate(10, TEST);
    c.phase = Phase::Queued;
    cases.push((c, "not approved"));
    let list: Vec<Candidate> = cases.iter().map(|(c, _)| c.clone()).collect();
    // Even forced (test mode) none of them moves.
    let policy = Policy {
        force: true,
        ..Policy::default()
    };
    let v = one(&list, &local, &[peer("nixos", worker())], &policy);
    for ((c, why), verdict) in cases.iter().zip(&v) {
        assert_eq!(verdict.job, c.id);
        assert!(verdict.moves().is_none(), "{c:?} moved: {verdict:?}");
        assert!(verdict.line.contains(why), "{c:?}: {}", verdict.line);
    }
}

#[test]
fn thresholds_are_both_a_ratio_and_a_minimum_gain() {
    // Here 1000 s (900 wait + 100 run), there 450 s (0 wait + 450 cold): 2.2x, 550 s sooner.
    let local = machine(
        "kurukkan",
        Some((494, 900.0)),
        vec![waiting(496, TEST, 100.0, 1, "warm")],
        2,
    );
    let worker = machine(
        "nixos",
        None,
        vec![passed(10, TEST, 450.0, "cold", "/x")],
        6,
    );
    let peers = [peer("nixos", worker)];
    let c = [candidate(496, TEST)];
    assert!(
        one(&c, &local, &peers, &Policy::default())[0]
            .moves()
            .is_some()
    );
    let ratio = Policy {
        min_ratio: 2.5,
        ..Policy::default()
    };
    let v = one(&c, &local, &peers, &ratio);
    assert!(
        v[0].moves().is_none() && v[0].line.contains("needs >= 2.5x"),
        "{}",
        v[0].line
    );
    let gain = Policy {
        min_gain_s: 600.0,
        ..Policy::default()
    };
    let v = one(&c, &local, &peers, &gain);
    assert!(
        v[0].moves().is_none() && v[0].line.contains(">= 10m sooner"),
        "{}",
        v[0].line
    );
}

#[test]
fn moves_in_one_round_load_the_peer_and_free_the_local_queue() {
    // Three equal jobs behind a 40-minute holder; each move adds its run to the peer's wait,
    // so the peer fills up and the last job stays (no herd onto one node).
    let local = machine(
        "kurukkan",
        Some((494, 2400.0)),
        vec![
            waiting(496, TEST, 30.0, 1, "warm"),
            waiting(497, TEST, 30.0, 2, "warm"),
            waiting(498, TEST, 30.0, 3, "warm"),
        ],
        2,
    );
    let worker = machine(
        "nixos",
        None,
        vec![passed(10, TEST, 500.0, "cold", "/x")],
        6,
    );
    let c = [
        candidate(498, TEST),
        candidate(496, TEST),
        candidate(497, TEST),
    ];
    let v = one(&c, &local, &[peer("nixos", worker)], &Policy::default());
    // Queue order, not input order.
    assert_eq!(v.iter().map(|v| v.job).collect::<Vec<_>>(), [496, 497, 498]);
    assert!(v[0].moves().is_some(), "{}", v[0].line);
    assert!(v[1].moves().is_some(), "{}", v[1].line);
    assert!(
        v[1].peers[0].wait_basis.contains("moved there now"),
        "{:?}",
        v[1].peers[0]
    );
    // 498 here: 2400 wait (496/497 left the queue) + 30 = 2430; there 1000 wait + 500.
    assert_eq!(v[2].local_wait_s, 2400.0);
    assert!(v[2].moves().is_none(), "{}", v[2].line);
}

#[test]
fn offline_or_marked_peers_are_skipped_with_the_reason() {
    let local = machine(
        "kurukkan",
        Some((494, 2400.0)),
        vec![waiting(496, TEST, 30.0, 1, "warm")],
        2,
    );
    let mut off = peer("nixos", machine("nixos", None, vec![], 6));
    off.state = STREAM_OFFLINE.into();
    off.error = Some("connection refused".into());
    let v = one(&[candidate(496, TEST)], &local, &[off], &Policy::default());
    assert!(v[0].moves().is_none());
    assert!(
        v[0].line
            .contains("no usable worker peer (nixos offline: connection refused)"),
        "{}",
        v[0].line
    );
    struct NoLibclang;
    impl Capabilities for NoLibclang {
        fn lacks(&self, node: &str, _argv: &[String]) -> Option<String> {
            (node == "nixos").then(|| "libclang".into())
        }
    }
    let v = decide(
        &[candidate(496, TEST)],
        &local,
        2,
        &[peer("nixos", machine("nixos", None, vec![], 6))],
        &Policy::default(),
        &NoLibclang,
    );
    assert!(v[0].line.contains("nixos lacks libclang"), "{}", v[0].line);
}

#[test]
fn policy_reads_its_knobs_and_refuses_bad_values() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    };
    assert_eq!(Policy::from_lookup(env(&[])).unwrap(), Policy::default());
    let p = Policy::from_lookup(env(&[
        (ENABLE_ENV, "off"),
        (INTERVAL_ENV, "15"),
        (MIN_RATIO_ENV, "3"),
        (MIN_GAIN_ENV, "120"),
    ]))
    .unwrap();
    assert!(!p.enabled);
    assert_eq!(p.interval, Duration::from_secs(15));
    assert_eq!((p.min_ratio, p.min_gain_s), (3.0, 120.0));
    for bad in [
        (ENABLE_ENV, "maybe"),
        (INTERVAL_ENV, "0"),
        (MIN_RATIO_ENV, "0.5"),
        (MIN_GAIN_ENV, "-1"),
        (MIN_GAIN_ENV, "NaN"),
    ] {
        let pairs: &'static [(&str, &str)] = Box::leak(Box::new([bad]));
        assert!(Policy::from_lookup(env(pairs)).is_err(), "{bad:?}");
    }
}

// ---- ledger transactions ------------------------------------------------------------------------

struct Ledger {
    dir: PathBuf,
    store: Store,
    id: i64,
    spec: JobSpec,
}

impl Drop for Ledger {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn ledger(tag: &str) -> Ledger {
    let dir = std::env::temp_dir().join(format!("reroute-unit-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut store = Store::open(&dir.join("tasks.sqlite")).unwrap();
    let spec = Recipe {
        argv: words("cargo test -j 2"),
        resource: Resource::Compile,
    }
    .unchecked_spec("agent".into(), dir.clone())
    .unwrap();
    let id = store.submit(&spec).unwrap().id;
    store.approve_command(id).unwrap();
    // Claimed by a worker (preparing), as every job is before it waits for a slot.
    let me = crate::worker::identity(std::process::id()).unwrap();
    assert_eq!(store.claim(me).unwrap().unwrap().id, id);
    Ledger {
        dir,
        store,
        id,
        spec,
    }
}

fn record() -> Reroute {
    Reroute {
        state: STATE_MOVING.into(),
        moved_from: "kurukkan".into(),
        moved_to: "nixos".into(),
        reason: "rerouted: kurukkan → nixos (test)".into(),
        at: 1,
        moves: 0,
        local_s: Some(1000.0),
        peer_s: Some(100.0),
        remote_job_id: None,
        note: None,
        returned_at: None,
        log_pending: true,
        original_spec: None,
    }
}

fn moved(l: &Ledger) -> JobSpec {
    moved_spec(
        &l.spec,
        &l.spec_argv(),
        "nixos",
        &l.dir,
        Path::new("/usr/bin/tasks"),
    )
    .unwrap()
}

impl Ledger {
    fn spec_argv(&self) -> Vec<String> {
        Recipe::from_spec_unvalidated(&self.spec).unwrap().argv
    }
}

#[test]
fn a_committed_admission_refuses_the_move_and_a_move_withdraws_a_later_admission() {
    // Admission first: the marker is written in its transaction, the claim is refused.
    let mut l = ledger("admit-first");
    let key = spec_key(&l.spec).unwrap();
    assert!(
        l.store
            .commit_admission(&l.dir, l.id, "build", &key)
            .unwrap()
    );
    let to = moved(&l);
    let claim = l
        .store
        .claim_move(&l.dir, l.id, &l.spec.clone(), &to, &record())
        .unwrap();
    assert_eq!(
        claim,
        Claim::Refused("local admission already committed".into())
    );
    assert_eq!(l.store.job(l.id).unwrap().spec, l.spec);

    // Move first: the later admission commit sees another spec and withdraws.
    let mut l = ledger("move-first");
    let key = spec_key(&l.spec).unwrap();
    let to = moved(&l);
    let spec = l.spec.clone();
    assert_eq!(
        l.store
            .claim_move(&l.dir, l.id, &spec, &to, &record())
            .unwrap(),
        Claim::Claimed
    );
    assert!(l.store.spec_changed(l.id, &key).unwrap());
    assert!(
        !l.store
            .commit_admission(&l.dir, l.id, "build", &key)
            .unwrap()
    );
    assert!(!admitted_here(&l.dir, l.id));
    let job = l.store.job(l.id).unwrap();
    assert_eq!(job.spec, to);
    let r = job.execution.unwrap().reroute.unwrap();
    assert_eq!((r.moves, r.state.as_str()), (1, STATE_MOVING));
    assert_eq!(r.original_spec.as_ref(), Some(&spec));

    // At most one move: a second claim is refused, even from the restored spec.
    l.store.revert_reroute(l.id, "peer offline").unwrap();
    let job = l.store.job(l.id).unwrap();
    assert_eq!(job.spec, spec);
    let r = job.execution.as_ref().unwrap().reroute.clone().unwrap();
    assert_eq!(r.state, STATE_RETURNED);
    assert_eq!(r.note.as_deref(), Some("peer offline"));
    assert!(
        r.summary()
            .contains("back in the local queue: peer offline")
    );
    assert_eq!(
        l.store
            .claim_move(&l.dir, l.id, &spec, &to, &record())
            .unwrap(),
        Claim::Refused("already moved once".into())
    );
    // The restored job's admission commits normally again.
    assert!(
        l.store
            .commit_admission(&l.dir, l.id, "build", &key)
            .unwrap()
    );
}

#[test]
fn a_stale_execution_update_never_drops_the_move_record() {
    let mut l = ledger("stale-update");
    let stale = l.store.execution(l.id).unwrap().unwrap();
    let (spec, to) = (l.spec.clone(), moved(&l));
    l.store
        .claim_move(&l.dir, l.id, &spec, &to, &record())
        .unwrap();
    l.store
        .update_execution(l.id, Phase::WaitingBuild, stale)
        .unwrap();
    assert!(
        l.store
            .job(l.id)
            .unwrap()
            .execution
            .unwrap()
            .reroute
            .is_some()
    );
}

#[test]
fn the_move_line_reaches_the_job_log_exactly_once() {
    let mut l = ledger("log-line");
    let (spec, to) = (l.spec.clone(), moved(&l));
    l.store
        .claim_move(&l.dir, l.id, &spec, &to, &record())
        .unwrap();
    // No job directory yet: stays pending.
    flush_pending_line(&l.dir, &mut l.store, l.id);
    assert!(
        l.store
            .job(l.id)
            .unwrap()
            .execution
            .unwrap()
            .reroute
            .unwrap()
            .log_pending
    );
    std::fs::create_dir_all(job_dir(&l.dir, l.id)).unwrap();
    flush_pending_line(&l.dir, &mut l.store, l.id);
    flush_pending_line(&l.dir, &mut l.store, l.id);
    let log = std::fs::read_to_string(job_dir(&l.dir, l.id).join("job.log")).unwrap();
    assert_eq!(
        log.matches("rerouted: kurukkan → nixos").count(),
        1,
        "{log}"
    );
}

#[test]
fn pins_and_placement_requests_keep_a_job_in_place() {
    let dir = std::env::temp_dir().join(format!("reroute-unit-pins-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(pinned(&dir, 1), None);
    pin(&dir, 1, "--pin at submit").unwrap();
    assert_eq!(pinned(&dir, 1).as_deref(), Some("--pin at submit"));
    for (id, requested, want) in [
        (2, "local", true),
        (3, "nixos", true),
        (4, "anywhere", false),
    ] {
        let path = placement::record_path(&dir, id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json!({"requested": requested}).to_string()).unwrap();
        assert_eq!(pinned(&dir, id).is_some(), want, "{requested}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn queue_time_is_kept_for_a_second_attempt() {
    let dir = std::env::temp_dir().join(format!("reroute-unit-queued-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(queued_since(&dir, "build"), None);
    let first = queued_since(&dir, "build").unwrap();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(queued_since(&dir, "build"), Some(first));
    // An aborted attempt's files move aside, the first queue time stays.
    std::fs::write(dir.join("build-waiting.json"), "1").unwrap();
    archive_stage_files(&dir, "build", "moved", true);
    assert!(!dir.join("build-waiting.json").exists());
    assert!(dir.join("build-waiting.json.reroute-moved").exists());
    assert_eq!(queued_since(&dir, "build"), Some(first));
    let _ = std::fs::remove_dir_all(&dir);
}
