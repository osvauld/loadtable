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
        fn lacks(&self, node: &str, _argv: &[String], _cwd: Option<&Path>) -> Option<String> {
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
    // health-alerts' marks (what reroute::capabilities() returns) skip the peer the same way,
    // with the mark's evidence.
    let nixos = peer("nixos", machine("nixos", None, vec![], 6));
    let mark = crate::health::capability::CapabilityMark {
        node: "nixos".into(),
        capability: "libclang".into(),
        family: "libclang".into(),
        job: 3,
        remote_job: None,
        kind_key: "cargo test".into(),
        line: "Unable to find libclang".into(),
        marked_at: 0.0,
        expires_at: f64::MAX,
        source: "forwarded job log".into(),
    };
    let marks = crate::health::capability::PeerMarks::new(std::slice::from_ref(&nixos), &[mark]);
    let v = decide(
        &[candidate(496, TEST)],
        &local,
        2,
        std::slice::from_ref(&nixos),
        &Policy::default(),
        &marks,
    );
    assert!(v[0].moves().is_none(), "{}", v[0].line);
    assert!(
        v[0].line.contains("nixos lacks libclang") && v[0].line.contains("job #3"),
        "{}",
        v[0].line
    );
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
        (PULL_BACK_ENV, "no"),
    ]))
    .unwrap();
    assert!(!p.enabled);
    assert_eq!(p.interval, Duration::from_secs(15));
    assert_eq!((p.min_ratio, p.min_gain_s), (3.0, 120.0));
    assert!(!p.pull_back);
    assert!(Policy::default().pull_back, "pull-back is on by default");
    for bad in [
        (ENABLE_ENV, "maybe"),
        (INTERVAL_ENV, "0"),
        (MIN_RATIO_ENV, "0.5"),
        (MIN_GAIN_ENV, "-1"),
        (MIN_GAIN_ENV, "NaN"),
        (PULL_BACK_ENV, "sometimes"),
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
        broad_test: false,
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
fn policy_file_knobs_apply_and_the_environment_wins() {
    let file = ReroutePolicy {
        enabled: Some(false),
        interval_s: Some(60.0),
        min_ratio: Some(1.5),
        min_gain_s: None,
        pull_back: Some(false),
    };
    let p = Policy::from_sources(&file, |_| None).unwrap();
    assert!(!p.enabled);
    assert!(!p.pull_back, "from the file");
    assert_eq!(p.interval, Duration::from_secs(60));
    assert_eq!(p.min_ratio, 1.5);
    assert_eq!(p.min_gain_s, DEFAULT_MIN_GAIN_S);
    let env = |name: &str| match name {
        ENABLE_ENV => Some("on".to_string()),
        MIN_RATIO_ENV => Some("3".to_string()),
        PULL_BACK_ENV => Some("on".to_string()),
        _ => None,
    };
    let p = Policy::from_sources(&file, env).unwrap();
    assert!(p.enabled, "env wins");
    assert!(p.pull_back, "env wins over the file");
    assert_eq!(p.min_ratio, 3.0);
    assert_eq!(p.interval, Duration::from_secs(60), "file value kept");
    let bad = ReroutePolicy {
        min_ratio: Some(0.5),
        ..Default::default()
    };
    let e = Policy::from_sources(&bad, |_| None).unwrap_err();
    assert!(e.contains("reroute.min_ratio"), "{e}");
}

#[test]
fn a_moved_job_keeps_its_broad_test_escape() {
    let dir = std::env::temp_dir().join(format!("reroute-unit-broad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let argv = words("cargo test -j 2 -p x some_filter");
    let original = Recipe {
        argv: argv.clone(),
        resource: Resource::Compile,
        broad_test: true,
    }
    .unchecked_spec("agent".into(), dir.clone())
    .unwrap();
    let to = moved_spec(&original, &argv, "nixos", &dir, Path::new("/usr/bin/tasks"))
        .expect("the recorded escape lets the forward spec pass the submission policy");
    let recipe = Recipe::from_spec_unvalidated(&to).unwrap();
    assert!(recipe.broad_test, "{recipe:?}");
    assert_eq!(recipe.resource, Resource::Offload);
    let _ = std::fs::remove_dir_all(&dir);
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

// ---- pull-back ----------------------------------------------------------------------------------

const PSI: &str = "memory PSI full avg10 above 0.5%";

/// Forward #7 (peer job 77): placement-chosen, never moved, waiting on the peer.
fn forwarded() -> Forwarded {
    Forwarded {
        id: 7,
        phase: Phase::Building,
        peer: "nixos".into(),
        argv: words(TEST),
        resource: Resource::Compile,
        source: CHECKOUT.into(),
        owner: "agent".into(),
        cancel_requested: false,
        moves: 0,
        pinned: None,
        requested: Some("anywhere".into()),
        remote_job_id: Some(77),
        remote_phase: Some("waiting_build".into()),
        remote_state: Some("attached".into()),
    }
}

/// An idle laptop that ran TEST in `run_s`, and a worker holding job 77 in `waiting_for`.
fn pull_world(run_s: f64, waiting_for: &str) -> (Snapshot, Vec<Peer>) {
    let local = machine(
        "kurukkan",
        None,
        vec![passed(900, TEST, run_s, "warm", CHECKOUT)],
        2,
    );
    let mut remote = waiting(77, TEST, run_s, 1, "warm");
    remote["queue"]["waiting_for"] = json!(waiting_for);
    let worker = machine("nixos", None, vec![remote], 8);
    (local, vec![peer("nixos", worker)])
}

fn pull(f: &[Forwarded], local: &Snapshot, peers: &[Peer], policy: &Policy) -> PullVerdict {
    decide_pull_back(f, local, 2, peers, policy)
        .into_iter()
        .next()
        .unwrap()
}

#[test]
fn a_blocked_forward_comes_back_when_this_machine_is_free() {
    let (local, peers) = pull_world(300.0, PSI);
    let v = pull(&[forwarded()], &local, &peers, &Policy::default());
    let PullAction::Pull {
        peer,
        argv,
        blocker,
    } = &v.action
    else {
        panic!("{v:?}");
    };
    assert_eq!((peer.as_str(), blocker.as_str()), ("nixos", PSI));
    assert_eq!(
        argv,
        &placement::apply_jobs(&words(TEST), 2),
        "Cargo -j is this machine's cap"
    );
    assert!(v.line.starts_with("pulled back nixos"), "{}", v.line);
    assert!(v.there_s >= 2.0 * v.here_s && v.there_s - v.here_s >= 300.0);
    // Off: stays.
    let off = Policy {
        pull_back: false,
        ..Policy::default()
    };
    assert!(!pull(&[forwarded()], &local, &peers, &off).pulls());
}

#[test]
fn the_peer_jobs_cap_is_replaced_by_this_machines() {
    let (local, peers) = pull_world(300.0, PSI);
    let mut f = forwarded();
    f.argv = words("cargo test --offline --locked -j 8 -p video");
    let v = pull(&[f], &local, &peers, &Policy::default());
    let PullAction::Pull { argv, .. } = &v.action else {
        panic!("{v:?}");
    };
    assert_eq!(argv, &placement::apply_jobs(&words(TEST), 2));
    assert!(!argv.iter().any(|a| a == "8"), "{argv:?}");
}

#[test]
fn a_forward_stays_when_it_started_or_is_not_blocked_by_pressure() {
    let (local, peers) = pull_world(300.0, PSI);
    let policy = Policy::default();
    // Started per the link record.
    for phase in ["building", "running", "passed"] {
        let mut f = forwarded();
        f.remote_phase = Some(phase.into());
        let v = pull(&[f], &local, &peers, &policy);
        assert!(!v.pulls() && v.line.contains("started"), "{phase}: {v:?}");
    }
    // Started per the peer's own snapshot (the link is a poll behind).
    let local2 = local.clone();
    let mut remote = waiting(77, TEST, 300.0, 1, "warm");
    remote["phase"] = json!("building");
    let started = vec![peer("nixos", machine("nixos", None, vec![remote], 8))];
    let v = pull(&[forwarded()], &local2, &started, &policy);
    assert!(!v.pulls() && v.line.contains("started"), "{v:?}");
    // Waiting behind a busy slot or a queue, not a pressure gate: it will run soon enough.
    for text in ["compile slot held by #3", "behind #4 in the compile queue"] {
        let (local, peers) = pull_world(300.0, text);
        let v = pull(&[forwarded()], &local, &peers, &policy);
        assert!(
            !v.pulls() && v.line.contains("not blocked"),
            "{text}: {v:?}"
        );
    }
    for text in [
        "memory PSI some avg10 above 1%",
        "CPU PSI some avg10 above 40%",
        "I/O PSI some avg10 above 20%",
        "available memory below 2.0 GiB",
    ] {
        assert!(pressure_blocker(text), "{text}");
    }
    for text in [
        "compile slot held by #3",
        "worker pickup",
        "admission check",
    ] {
        assert!(!pressure_blocker(text), "{text}");
    }
    // Not submitted yet, or the remote state is uncertain.
    let mut f = forwarded();
    f.remote_job_id = None;
    assert!(!pull(&[f], &local, &peers, &policy).pulls());
    let mut f = forwarded();
    f.remote_state = Some("uncertain".into());
    assert!(!pull(&[f], &local, &peers, &policy).pulls());
}

#[test]
fn only_a_placement_chosen_unpinned_forward_comes_back() {
    let (local, peers) = pull_world(300.0, PSI);
    let policy = Policy::default();
    for (requested, pinned, why) in [
        (None, None, "named peer"),
        (Some("nixos"), None, "named peer"),
        (Some("local"), None, "local"),
        (Some("anywhere"), Some("--pin at submit"), "pinned"),
    ] {
        let mut f = forwarded();
        f.requested = requested.map(str::to_owned);
        f.pinned = pinned.map(str::to_owned);
        let v = pull(&[f], &local, &peers, &policy);
        assert!(!v.pulls(), "{why}: {v:?}");
    }
    let mut f = forwarded();
    f.cancel_requested = true;
    assert!(!pull(&[f], &local, &peers, &policy).pulls());
    let mut f = forwarded();
    f.resource = Resource::Run;
    assert!(!pull(&[f], &local, &peers, &policy).pulls());
}

#[test]
fn a_forward_stays_when_this_machine_is_pressured_or_busy_too() {
    let (mut local, peers) = pull_world(300.0, PSI);
    let policy = Policy::default();
    local.machine.blocker = Some("CPU PSI some avg10 above 40%".into());
    let v = pull(&[forwarded()], &local, &peers, &policy);
    assert!(!v.pulls() && v.line.contains("blocked here too"), "{v:?}");
    // The compile slot is held here.
    let mut busy = machine(
        "kurukkan",
        Some((9, 1200.0)),
        vec![passed(900, TEST, 300.0, "warm", CHECKOUT)],
        2,
    );
    let v = pull(&[forwarded()], &busy, &peers, &policy);
    assert!(!v.pulls() && v.line.contains("slot held by #9"), "{v:?}");
    // A compile job already waits here for the free slot.
    busy = machine(
        "kurukkan",
        None,
        vec![waiting(12, TEST, 60.0, 1, "warm")],
        2,
    );
    let v = pull(&[forwarded()], &busy, &peers, &policy);
    assert!(!v.pulls() && v.line.contains("#12"), "{v:?}");
}

#[test]
fn the_ratio_and_gain_rule_applies_and_one_job_comes_back_per_round() {
    // A long job: ~700s here vs ~1200s there (cold guess + gate) is under 2x.
    let (local, peers) = pull_world(700.0, PSI);
    let v = pull(&[forwarded()], &local, &peers, &Policy::default());
    assert!(!v.pulls() && v.line.contains("needs >="), "{v:?}");
    let loose = Policy {
        min_ratio: 1.0,
        min_gain_s: 0.0,
        ..Policy::default()
    };
    assert!(pull(&[forwarded()], &local, &peers, &loose).pulls());
    // Two forwards: the first takes the slot.
    let (local, peers) = pull_world(300.0, PSI);
    let mut second = forwarded();
    second.id = 8;
    let out = decide_pull_back(
        &[forwarded(), second],
        &local,
        2,
        &peers,
        &Policy::default(),
    );
    assert!(out[0].pulls());
    assert!(
        !out[1].pulls() && out[1].line.contains("goes to #7"),
        "{:?}",
        out[1]
    );
}

#[test]
fn a_job_never_flaps_back_and_forth() {
    let (local, peers) = pull_world(300.0, PSI);
    let mut f = forwarded();
    f.moves = MAX_MOVES;
    let v = pull(&[f], &local, &peers, &Policy::default());
    assert!(!v.pulls() && v.line.contains("already moved once"), "{v:?}");
}

#[test]
fn the_pull_back_claim_restores_the_local_spec_once() {
    let mut l = ledger("pull-back");
    let forward = moved(&l);
    // The bridge runs: swap in the forward spec as a placement-chosen job has it.
    l.store
        .0
        .execute(
            "UPDATE jobs SET spec=?2 WHERE id=?1",
            rusqlite::params![l.id, serde_json::to_string(&forward).unwrap()],
        )
        .unwrap();
    let argv = l.spec_argv();
    let pulled = pulled_spec(&forward, &argv).unwrap();
    assert_eq!(pulled, l.spec, "the local command comes back unchanged");
    let mut rec = record();
    rec.state = STATE_PULLED_BACK.into();
    // Preparing is not a running bridge.
    assert_eq!(
        l.store
            .claim_pull_back(l.id, &forward, &pulled, &rec)
            .unwrap(),
        Claim::Refused("phase is now Preparing".into())
    );
    let e = l.store.execution(l.id).unwrap().unwrap();
    l.store.update_execution(l.id, Phase::Building, e).unwrap();
    // A changed spec is refused; the matching one is claimed.
    assert_eq!(
        l.store
            .claim_pull_back(l.id, &l.spec.clone(), &pulled, &rec)
            .unwrap(),
        Claim::Refused("its spec changed".into())
    );
    assert_eq!(
        l.store
            .claim_pull_back(l.id, &forward, &pulled, &rec)
            .unwrap(),
        Claim::Claimed
    );
    let job = l.store.job(l.id).unwrap();
    assert_eq!(job.spec, pulled);
    let r = job.execution.unwrap().reroute.unwrap();
    assert_eq!((r.moves, r.state.as_str()), (1, STATE_PULLED_BACK));
    assert_eq!(r.original_spec.as_ref(), Some(&forward));
    assert!(
        r.summary().starts_with("pulled back kurukkan"),
        "{}",
        r.summary()
    );
    // The count is spent: neither a pull-back nor a move may claim it again.
    assert_eq!(
        l.store
            .claim_pull_back(l.id, &pulled, &forward, &rec)
            .unwrap(),
        Claim::Refused("already moved once".into())
    );
    assert_eq!(
        l.store
            .claim_move(&l.dir, l.id, &pulled, &forward, &record())
            .unwrap(),
        Claim::Refused("phase is now Building".into())
    );
}
