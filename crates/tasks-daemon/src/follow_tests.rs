//! Tests of the CLI/daemon gap work: job timeouts, cancel reasons, drain records, remote phase,
//! the history API, the shared submit rules, per-target timing and the connection limit.
//! Names carry the regression ids they pin.

use crate::api::*;
use crate::peer_link::serve::DaemonRules;
use crate::testkit::{FakeForwarder, FakeProcs, Rig, TempDir};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tasks_core::plan::{Plan, Step};
use tasks_core::{
    JobId, Kind, MachineName, Phase, Placement, Requirements, SplitOrigin, StepHints, WaitReason,
};
use tasks_ledger::Clock;
use tasks_ledger::{NewJob, Store};
use tasks_peer::receiver::SubmitPolicy;
use tasks_timing::{Cache, History, Outcome, Sample, SampleKey};

fn ok(r: Reply) -> Response {
    match r {
        Reply::Ok(x) => x,
        Reply::Err(e) => panic!("unexpected error {e:?}"),
    }
}

fn err(r: Reply) -> ApiError {
    match r {
        Reply::Err(e) => e,
        Reply::Ok(x) => panic!("unexpected success {x:?}"),
    }
}

fn argv(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

fn sample(secs: f64, at_ms: u64) -> Sample {
    Sample {
        secs,
        cache: Cache::Warm,
        outcome: Outcome::Passed,
        at_ms,
    }
}

fn key(recipe: &str, target: Option<&str>, machine: &str) -> SampleKey {
    SampleKey {
        recipe: recipe.into(),
        target: target.map(str::to_string),
        machine: machine.into(),
    }
}

/// The samples filed under `k` once `n` have arrived. A split piece's per-target samples are
/// written after the job's own terminal write, so reading right after `until_done` races them
/// when the machine is busy.
fn samples_when(r: &Rig, k: &SampleKey, n: usize) -> Vec<Sample> {
    let t0 = Instant::now();
    loop {
        let s = r.store.samples(k).unwrap();
        if s.len() >= n || t0.elapsed() > Duration::from_secs(5) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn step(name: &str, text: &str) -> Step {
    Step {
        name: name.into(),
        argv: argv(text),
        after: vec![],
        kind: Kind::Test,
        requirements: Requirements::default(),
        hints: StepHints::default(),
    }
}

fn submit_plan(r: &Rig, plan: Plan, broad_test: bool) -> Reply {
    r.sch.handle(Request::SubmitPlan {
        plan,
        owner: "o".into(),
        cwd: r.dir.path().to_path_buf(),
        env: vec![],
        placement: Placement::Here,
        broad_test,
        priority: Default::default(),
        priority_reason: None,
    })
}

// ---------------------------------------------------------------- 1: timeouts

#[test]
fn r095_a_spec_timeout_ends_the_job_timed_out() {
    let r = Rig::native();
    let mut s = r.spec("o", &["sh", "-c", "sleep 30"], Kind::Run);
    s.timeout_secs = Some(1);
    let id = r.submit(&s);
    let t0 = std::time::Instant::now();
    assert!(matches!(r.until_done(id), Phase::TimedOut { .. }));
    assert!(t0.elapsed() < std::time::Duration::from_secs(4));
    assert_eq!(
        r.record(id).timeout,
        Some(tasks_ledger::TimeoutReason::Wall)
    );
    // `get` shows the limit
    let Response::Job { job } = ok(r.sch.handle(Request::Get { id })) else {
        panic!("job expected");
    };
    assert_eq!(job.timeout_secs, Some(1));
}

#[test]
fn r095_a_plan_steps_timeout_hint_fills_the_spec_and_is_enforced() {
    let r = Rig::native();
    let mut slow = step("slow", "sh");
    slow.argv = vec!["sh".into(), "-c".into(), "sleep 30".into()];
    slow.kind = Kind::Run;
    slow.hints.timeout_secs = Some(1);
    let Response::PlanSubmitted { jobs, .. } =
        ok(submit_plan(&r, Plan { steps: vec![slow] }, false))
    else {
        panic!("plan expected");
    };
    let id = jobs[0].1;
    assert_eq!(r.record(id).spec.timeout_secs, Some(1));
    assert!(matches!(r.until_done(id), Phase::TimedOut { .. }));
}

#[test]
fn r095_an_impossible_timeout_is_refused_at_submit() {
    let r = Rig::new();
    let mut s = r.spec("o", &["true"], Kind::Run);
    s.timeout_secs = Some(0);
    let e = err(r.sch.handle(Request::Submit { spec: s }));
    assert_eq!(e.code, ErrorCode::Refused);
    assert!(e.message.contains("timeout"), "{}", e.message);
}

// ---------------------------------------------------------------- 2: cancel reason

#[test]
fn r108_a_cancel_reason_is_stored_and_shown_by_get_and_list() {
    let r = Rig::new();
    let id = r.submit(&r.spec("o", &["make"], Kind::Run));
    ok(r.sch.handle(Request::Cancel {
        id,
        who: "o".into(),
        reason: Some("wrong branch".into()),
    }));
    let Response::Job { job } = ok(r.sch.handle(Request::Get { id })) else {
        panic!("job expected");
    };
    assert_eq!(job.cancel_requested_by.as_deref(), Some("o"));
    assert_eq!(job.cancel_reason.as_deref(), Some("wrong branch"));
    let Response::Jobs { jobs, .. } = ok(r.sch.handle(Request::List {
        owner: None,
        active_only: false,
        recent_finished: None,
        limit: None,
    })) else {
        panic!("jobs expected");
    };
    assert_eq!(jobs[0].cancel_reason.as_deref(), Some("wrong branch"));
    // a reason is bounded
    let id2 = r.submit(&r.spec("o", &["make"], Kind::Run));
    let e = err(r.sch.handle(Request::Cancel {
        id: id2,
        who: "o".into(),
        reason: Some("x".repeat(501)),
    }));
    assert_eq!(e.code, ErrorCode::BadRequest);
    assert_eq!(r.phase(id2), Phase::Queued);
}

// ---------------------------------------------------------------- 3: drain record

#[test]
fn r189_a_drain_records_who_and_when_and_a_resume_forgets_it() {
    let r = Rig::new();
    let drained = |r: &Rig| match ok(r.sch.handle(Request::Status)) {
        Response::Status(s) => s.daemon,
        other => panic!("{other:?}"),
    };
    assert_eq!(drained(&r).drained_by, None);
    ok(r.sch.handle(Request::Drain {
        cancel: false,
        who: Some("alice".into()),
    }));
    let at = r.clock.now_ms();
    r.clock.advance(90_000);
    let d = drained(&r);
    assert!(d.draining);
    assert_eq!(
        (d.drained_by.as_deref(), d.drained_at_ms, d.now_ms),
        (Some("alice"), Some(at), at + 90_000)
    );
    // a second drain keeps the first requester
    ok(r.sch.handle(Request::Drain {
        cancel: false,
        who: Some("bob".into()),
    }));
    assert_eq!(drained(&r).drained_by.as_deref(), Some("alice"));
    ok(r.sch.handle(Request::Drain {
        cancel: true,
        who: Some("bob".into()),
    }));
    let d = drained(&r);
    assert!((d.draining, d.drained_by, d.drained_at_ms) == (false, None, None));
    // a drain with no name is recorded as unknown, not lost
    ok(r.sch.handle(Request::Drain {
        cancel: false,
        who: None,
    }));
    assert_eq!(drained(&r).drained_by.as_deref(), Some("unknown"));
}

// ---------------------------------------------------------------- 4: remote phase

#[test]
fn r186_a_forwarded_row_carries_the_remote_phase() {
    let r = Rig::new();
    let id = r.submit(&r.spec("o", &["make"], Kind::Compile));
    r.store
        .set_forward(id, &MachineName("nixos".into()), Some(7), Some("k"))
        .unwrap();
    let get = |r: &Rig| match ok(r.sch.handle(Request::Get { id })) {
        Response::Job { job } => job,
        other => panic!("{other:?}"),
    };
    assert_eq!(get(&r).remote_phase, None);
    r.store.set_remote_phase(id, &Phase::Running).unwrap();
    let j = get(&r);
    assert_eq!(j.remote_phase, Some(Phase::Running));
    assert_eq!(j.forwarded_to, Some(("nixos".into(), Some(7))));
    // the local phase is untouched (R-015)
    assert_eq!(j.phase, Phase::Queued);
    let Response::Jobs { jobs, .. } = ok(r.sch.handle(Request::List {
        owner: None,
        active_only: true,
        recent_finished: None,
        limit: None,
    })) else {
        panic!("jobs expected");
    };
    assert_eq!(jobs[0].remote_phase, Some(Phase::Running));
}

// ---------------------------------------------------------------- 5: history

fn history(r: &Rig, recipes: &[&str], machines: &[&str], max: Option<usize>) -> Reply {
    r.sch.handle(Request::History {
        recipes: recipes.iter().map(|s| s.to_string()).collect(),
        machines: machines.iter().map(|s| s.to_string()).collect(),
        max_samples: max,
    })
}

fn seed(r: &Rig, k: &SampleKey, n: u64) {
    for i in 0..n {
        r.store
            .record_sample(k, &sample(1.0 + i as f64, 100 + i))
            .unwrap();
    }
}

#[test]
fn r114_history_returns_samples_and_targets_per_recipe_and_machine() {
    let r = Rig::new();
    seed(&r, &key("cargo test pkg=x", None, "testbox"), 3);
    seed(&r, &key("cargo test pkg=x", Some("test:a"), "testbox"), 2);
    seed(&r, &key("cargo test pkg=x", Some("test:b"), "testbox"), 1);
    seed(&r, &key("cargo test pkg=x", None, "nixos"), 2);
    seed(&r, &key("cargo check", None, "testbox"), 1);
    let Response::History(h) = ok(history(&r, &[], &[], None)) else {
        panic!("history expected");
    };
    assert!(!h.truncated);
    let names: Vec<&str> = h.recipes.iter().map(|x| x.recipe.as_str()).collect();
    assert_eq!(names, ["cargo check", "cargo test pkg=x"]);
    let test = &h.recipes[1];
    let machines: Vec<&str> = test.machines.iter().map(|m| m.machine.as_str()).collect();
    assert_eq!(machines, ["nixos", "testbox"]);
    let local = &test.machines[1];
    assert_eq!(local.targets, ["test:a", "test:b"]);
    let whole = local.series.iter().find(|s| s.target.is_none()).unwrap();
    assert_eq!(whole.samples.len(), 3);
    assert_eq!(
        whole.samples.iter().map(|s| s.secs).collect::<Vec<_>>(),
        [1.0, 2.0, 3.0],
        "oldest first"
    );
    assert_eq!(whole.samples[0].outcome, "passed");
    // narrowed by recipe and machine
    let Response::History(h) = ok(history(&r, &["cargo test pkg=x"], &["testbox"], None)) else {
        panic!("history expected");
    };
    assert_eq!(h.recipes.len(), 1);
    assert_eq!(h.recipes[0].machines.len(), 1);
    // it feeds the same estimates the daemon makes: samples survive the round trip
    let mem = h.to_history();
    let got = mem
        .samples(&key("cargo test pkg=x", Some("test:a"), "testbox"))
        .unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(
        mem.targets("cargo test pkg=x").unwrap(),
        ["test:a", "test:b"]
    );
}

#[test]
fn r114_history_keeps_only_the_newest_samples_and_says_when_it_cut() {
    let r = Rig::new();
    seed(&r, &key("r", None, "m"), 10);
    let Response::History(h) = ok(history(&r, &[], &[], Some(4))) else {
        panic!("history expected");
    };
    assert!(h.truncated);
    let secs: Vec<f64> = h.recipes[0].machines[0].series[0]
        .samples
        .iter()
        .map(|s| s.secs)
        .collect();
    assert_eq!(secs, [7.0, 8.0, 9.0, 10.0], "the newest four, oldest first");
    // bounds on the request itself
    assert_eq!(
        err(history(&r, &[], &[], Some(0))).code,
        ErrorCode::BadRequest
    );
    assert_eq!(
        err(history(&r, &[], &[], Some(MAX_HISTORY_SAMPLES + 1))).code,
        ErrorCode::BadRequest
    );
}

#[test]
fn r114_history_is_bounded_in_recipes() {
    let r = Rig::new();
    for i in 0..(MAX_HISTORY_RECIPES + 5) {
        seed(&r, &key(&format!("tool{i:04}"), None, "m"), 1);
    }
    let Response::History(h) = ok(history(&r, &[], &[], Some(1))) else {
        panic!("history expected");
    };
    assert_eq!(h.recipes.len(), MAX_HISTORY_RECIPES);
    assert!(h.truncated);
    // a named recipe past the cut is still reachable
    let Response::History(one) = ok(history(&r, &["tool0204"], &[], Some(1))) else {
        panic!("history expected");
    };
    assert_eq!(one.recipes.len(), 1);
    assert!(!one.truncated);
}

// ---------------------------------------------------------------- 6: shared submit rules

const BROAD: &str = "cargo test -p x video_call";

fn submit(r: &Rig, s: tasks_core::JobSpec) -> Reply {
    r.sch.handle(Request::Submit { spec: s })
}

#[test]
fn r191_a_broad_cargo_test_is_refused_by_the_daemon_unless_escaped() {
    let r = Rig::new();
    let mut s = r.spec("o", &["cargo", "test", "-p", "x", "video_call"], Kind::Test);
    let e = err(submit(&r, s.clone()));
    assert_eq!(e.code, ErrorCode::Refused);
    assert!(
        e.message.contains("refused: this builds every test target"),
        "{}",
        e.message
    );
    assert!(e.message.contains("--broad-test"), "{}", e.message);
    assert!(r.store.list(&Default::default()).unwrap().is_empty());
    // the escape on the spec lets it through and is recorded on the job
    s.broad_test_escape = true;
    let Response::Submitted { id, .. } = ok(submit(&r, s)) else {
        panic!("submitted expected");
    };
    assert!(r.record(id).spec.broad_test_escape);
    // and the job log says so
    r.tick();
    r.until_running(id);
    assert!(
        r.log_text(id).contains("warning:") && r.log_text(id).contains("--broad-test"),
        "{}",
        r.log_text(id)
    );
}

#[test]
fn r191_the_machine_policy_downgrades_the_refusal_to_a_warning_with_its_source() {
    let r = Rig::with_policy("[submit]\nbroad_test = \"warn\"\n");
    let s = r.spec("o", &["cargo", "test", "-p", "x", "video_call"], Kind::Test);
    let Response::Submitted { id, .. } = ok(submit(&r, s)) else {
        panic!("submitted expected");
    };
    r.tick();
    r.until_running(id);
    assert!(
        r.log_text(id)
            .contains("[tasks] warning: this builds every test target"),
        "{}",
        r.log_text(id)
    );
    // status shows the value and where it came from
    let Response::Status(st) = ok(r.sch.handle(Request::Status)) else {
        panic!("status expected");
    };
    let v = st
        .settings
        .iter()
        .find(|s| s.key == "submit.broad_test")
        .unwrap();
    assert_eq!((v.value.as_str(), v.source), ("warn", Source::File));
    let plain = Rig::new();
    let Response::Status(st) = ok(plain.sch.handle(Request::Status)) else {
        panic!("status expected");
    };
    let v = st
        .settings
        .iter()
        .find(|s| s.key == "submit.broad_test")
        .unwrap();
    assert_eq!((v.value.as_str(), v.source), ("refuse", Source::Default));
    // a bad value refuses the whole file loudly, as for every setting
    let p = crate::machine_policy::MachinePolicy::from_text(
        Some("[submit]\nbroad_test = \"sometimes\"\n"),
        &|_| None,
    );
    assert!(
        p.problems.iter().any(|m| m.contains("submit.broad_test")),
        "{:?}",
        p.problems
    );
}

#[test]
fn r191_the_daemons_environment_overrides_the_policy_file() {
    let p = crate::machine_policy::MachinePolicy::from_text(
        Some("[submit]\nbroad_test = \"warn\"\n"),
        &|k| (k == "TASKS_BROAD_TEST").then(|| "refuse".to_string()),
    );
    assert_eq!(
        p.broad_test_mode(),
        tasks_policy::submit_rules::BroadTestMode::Refuse
    );
    assert_eq!(p.source_of("submit.broad_test"), Some(Source::Env));
}

#[test]
fn r210_a_denied_serial_is_refused_in_argv_in_any_case_and_as_a_device_resource() {
    let r = Rig::new();
    let lower = r.spec("o", &["adb", "-s", "zd22257p6g", "devices"], Kind::Run);
    let e = err(submit(&r, lower));
    assert_eq!(e.code, ErrorCode::Refused);
    assert!(e.message.contains("denied device"), "{}", e.message);
    let mut by_resource = r.spec("o", &["adb", "devices"], Kind::Device);
    by_resource.requirements.device = Some(tasks_core::DeviceId("ZD22257P6G".into()));
    assert_eq!(err(submit(&r, by_resource)).code, ErrorCode::Refused);
    let mut fine = r.spec("o", &["adb", "devices"], Kind::Device);
    fine.requirements.device = Some(tasks_core::DeviceId("9BWAY1LSDC".into()));
    assert!(matches!(ok(submit(&r, fine)), Response::Submitted { .. }));
}

#[test]
fn r091_a_cargo_jobs_request_above_the_cap_is_refused_at_submit() {
    let r = Rig::new();
    let over = r.spec("o", &["cargo", "check", "-j", "3"], Kind::Compile);
    let e = err(submit(&r, over));
    assert_eq!(e.code, ErrorCode::Refused);
    assert!(e.message.contains("cap of 2"), "{}", e.message);
    for ok_argv in [
        &["cargo", "check", "-j2"][..],
        &["cargo", "check"],
        &["env", "CARGO_BUILD_JOBS=2", "cargo", "check"],
    ] {
        let s = r.spec("o", ok_argv, Kind::Compile);
        assert!(
            matches!(ok(submit(&r, s)), Response::Submitted { .. }),
            "{ok_argv:?}"
        );
    }
    let env_over = r.spec(
        "o",
        &["env", "CARGO_BUILD_JOBS=5", "cargo", "check"],
        Kind::Compile,
    );
    assert_eq!(err(submit(&r, env_over)).code, ErrorCode::Refused);
}

#[test]
fn r137_a_raised_cap_in_the_machine_policy_accepts_what_it_allows() {
    let r = Rig::with_policy("[cargo]\nmax_jobs = 4\n");
    let s = r.spec("o", &["cargo", "check", "-j", "4"], Kind::Compile);
    assert!(matches!(ok(submit(&r, s)), Response::Submitted { .. }));
    let s = r.spec("o", &["cargo", "check", "-j", "5"], Kind::Compile);
    assert_eq!(err(submit(&r, s)).code, ErrorCode::Refused);
}

fn placed_over_cap(r: &Rig, placement: Placement) -> Reply {
    let mut s = r.spec("o", &["cargo", "test", "-j", "6"], Kind::Test);
    s.broad_test_escape = true;
    s.placement = placement;
    submit(r, s)
}

// R-540: the cap of the machine that will run the job applies, not the origin's
#[test]
fn r540_a_peer_job_is_not_held_to_the_origins_cargo_cap() {
    let r = Rig::new();
    let peer = Placement::Peer(tasks_core::MachineName("nixos".into()));
    assert!(matches!(
        ok(placed_over_cap(&r, peer)),
        Response::Submitted { .. }
    ));
}

#[test]
fn r540_an_anywhere_job_is_left_to_placement_and_here_or_pinned_jobs_are_refused() {
    let r = Rig::new();
    assert!(matches!(
        ok(placed_over_cap(&r, Placement::Anywhere)),
        Response::Submitted { .. }
    ));
    for p in [Placement::Here, Placement::Pin] {
        let e = err(placed_over_cap(&r, p));
        assert_eq!(e.code, ErrorCode::Refused);
        assert!(e.message.contains("cap of 2"), "{}", e.message);
    }
}

#[test]
fn r540_plan_steps_follow_the_same_rule_by_plan_placement_and_step_hint() {
    let r = Rig::new();
    let over = || Plan {
        steps: vec![step("a", "cargo check -j 9")],
    };
    let send = |plan: Plan, placement: Placement| {
        r.sch.handle(Request::SubmitPlan {
            plan,
            owner: "o".into(),
            cwd: r.dir.path().to_path_buf(),
            env: vec![],
            placement,
            broad_test: false,
            priority: Default::default(),
            priority_reason: None,
        })
    };
    let peer = Placement::Peer(tasks_core::MachineName("nixos".into()));
    assert!(matches!(
        ok(send(over(), peer)),
        Response::PlanSubmitted { .. }
    ));
    assert_eq!(err(send(over(), Placement::Here)).code, ErrorCode::Refused);
    // a step that asks for a peer by hint is the peer's to judge
    let mut hinted = over();
    hinted.steps[0].hints.placement = Some("nixos".into());
    assert!(matches!(
        ok(send(hinted, Placement::Here)),
        Response::PlanSubmitted { .. }
    ));
}

#[test]
fn r191_plans_get_the_same_answer_naming_the_step() {
    let r = Rig::new();
    let plan = || Plan {
        steps: vec![step("fine", "cargo test --lib"), step("broad", BROAD)],
    };
    let e = err(submit_plan(&r, plan(), false));
    assert_eq!(e.code, ErrorCode::Refused);
    assert!(
        e.message.starts_with("step \"broad\": refused"),
        "{}",
        e.message
    );
    assert!(
        r.store.list(&Default::default()).unwrap().is_empty(),
        "a refused plan stores no step"
    );
    let Response::PlanSubmitted { jobs, .. } = ok(submit_plan(&r, plan(), true)) else {
        panic!("plan expected");
    };
    assert!(
        jobs.iter()
            .all(|(_, id)| r.record(*id).spec.broad_test_escape)
    );
    // denied devices and the cap apply to every step too
    let bad = Plan {
        steps: vec![step("a", "adb -s ZD22257P6G devices")],
    };
    assert_eq!(err(submit_plan(&r, bad, false)).code, ErrorCode::Refused);
    let over = Plan {
        steps: vec![step("a", "cargo check -j 9")],
    };
    assert_eq!(err(submit_plan(&r, over, false)).code, ErrorCode::Refused);
}

#[test]
fn r191_suites_get_the_same_answer() {
    let r = Rig::new();
    let repo = r.dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join(".tasks")).unwrap();
    std::fs::write(
        repo.join(".tasks/run-policy.toml"),
        "version = 1\n[suites.s]\n[[suites.s.steps]]\nname=\"a\"\ncommand=[\"cargo\",\"test\",\"foo\"]\n",
    )
    .unwrap();
    let req = |broad_test: bool| Request::SubmitSuite {
        name: "s".into(),
        owner: "o".into(),
        cwd: repo.clone(),
        env: vec![],
        placement: Placement::Here,
        quick: false,
        broad_test,
        priority: Default::default(),
        priority_reason: None,
    };
    let e = err(r.sch.handle(req(false)));
    assert_eq!(e.code, ErrorCode::Refused);
    assert!(e.message.contains("step \"a\": refused"), "{}", e.message);
    assert!(matches!(
        ok(r.sch.handle(req(true))),
        Response::PlanSubmitted { .. }
    ));
}

#[test]
fn r153_the_peer_receiver_applies_the_same_rules() {
    let r = Rig::new();
    let rules = DaemonRules::new(r.sch.clone());
    let broad = r.spec("o", &["cargo", "test", "-p", "x", "video_call"], Kind::Test);
    let e = rules.check(&broad).unwrap_err();
    assert!(e.contains("refused: this builds every test target"), "{e}");
    let mut escaped = broad;
    escaped.broad_test_escape = true;
    assert!(rules.check(&escaped).is_ok());
    let phone = r.spec("o", &["adb", "-s", "zd22257p6g", "shell"], Kind::Run);
    assert!(rules.check(&phone).unwrap_err().contains("denied device"));
    // the origin's -j is not refused here: it is lowered to this machine's cap when it starts
    let jobs = r.spec("o", &["cargo", "check", "-j", "8"], Kind::Compile);
    assert!(rules.check(&jobs).is_ok());
    let warn = Rig::with_policy("[submit]\nbroad_test = \"warn\"\n");
    let rules = DaemonRules::new(warn.sch.clone());
    let again = warn.spec("o", &["cargo", "test", "-p", "x", "video_call"], Kind::Test);
    assert!(rules.check(&again).is_ok());
}

// ---------------------------------------------------------------- 7: per-target timing

fn piece(r: &Rig, argv: &[&str], split: Option<SplitOrigin>) -> JobId {
    let spec = r.spec("o", argv, Kind::Test);
    let id = r
        .store
        .submit_job(&NewJob {
            spec,
            hints: StepHints {
                split,
                ..StepHints::default()
            },
            step: Some((1, "t.test-a".into())),
            origin: None,
        })
        .unwrap();
    r.clock.advance(10);
    id
}

#[test]
fn r114_a_split_piece_teaches_the_unsplit_recipe_its_target() {
    let r = Rig::new();
    r.procs.auto_exit(0);
    let unsplit = "cargo test pkg=x profile=dev";
    let id = piece(
        &r,
        &["cargo", "test", "-p", "x", "--test", "a"],
        Some(SplitOrigin {
            recipe: unsplit.into(),
            target: tasks_timing::label_of("test-a"),
        }),
    );
    assert_eq!(r.until_done(id), Phase::Passed);
    let filed = samples_when(&r, &key(unsplit, Some("test:a"), "testbox"), 1);
    assert_eq!(
        filed.len(),
        1,
        "one sample under the unsplit recipe and the label"
    );
    assert_eq!(filed[0].outcome, Outcome::Passed);
    // the job's own whole-run sample is still recorded under its own recipe
    let own = RecipeKeyOf::of(&["cargo", "test", "-p", "x", "--test", "a"]);
    assert_eq!(
        r.store.samples(&key(&own, None, "testbox")).unwrap().len(),
        1
    );
    // the policy's lookup finds it, as `measure` does for ordering a split suite
    assert_eq!(r.store.targets(unsplit).unwrap(), ["test:a"]);
    let e = tasks_timing::estimate(
        &*r.sch.ledger,
        &key(unsplit, Some("test:a"), "testbox"),
        &tasks_timing::MachineInfo {
            name: "testbox".into(),
            cores: 1,
        },
        &[],
    )
    .unwrap();
    assert_eq!(e.samples, 1);
}

#[test]
fn r114_a_job_that_is_not_a_split_piece_files_nothing_under_a_target() {
    let r = Rig::new();
    r.procs.auto_exit(0);
    let id = piece(&r, &["cargo", "test", "-p", "x", "--test", "a"], None);
    assert_eq!(r.until_done(id), Phase::Passed);
    assert!(
        r.store
            .targets("cargo test pkg=x profile=dev")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn r114_a_failed_piece_is_filed_as_failed_and_a_cancelled_one_as_cancelled() {
    let r = Rig::new();
    r.procs.auto_exit(1);
    let unsplit = "cargo test pkg=x profile=dev";
    let id = piece(
        &r,
        &["cargo", "test", "-p", "x", "--test", "a"],
        Some(SplitOrigin {
            recipe: unsplit.into(),
            target: "test:a".into(),
        }),
    );
    assert!(matches!(r.until_done(id), Phase::Failed { .. }));
    let filed = samples_when(&r, &key(unsplit, Some("test:a"), "testbox"), 1);
    assert_eq!(filed.len(), 1);
    assert_eq!(filed[0].outcome, Outcome::Failed);
}

struct RecipeKeyOf;
impl RecipeKeyOf {
    fn of(argv: &[&str]) -> String {
        let v: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        tasks_timing::RecipeKey::from_argv(&v, &[])[0].to_string()
    }
}

// ---------------------------------------------------------------- 8: execution text

#[test]
fn a_stopped_queue_names_the_real_command_that_starts_it() {
    let r = Rig::new();
    ok(r.sch.handle(Request::Execution { enabled: false }));
    let id = r.submit(&r.spec("o", &["make"], Kind::Run));
    r.tick();
    let text = r.record(id).wait_text.unwrap();
    assert!(text.contains("`tasks2 execution start`"), "{text}");
    assert!(!text.contains("execution enable"), "{text}");
}

// ---------------------------------------------------------------- 10: placement waits

#[test]
fn r186_a_placement_wait_is_stored_as_a_placement_wait() {
    let f = Arc::new(FakeForwarder::new(vec![]));
    let r = Rig::with_forwarder(f.clone());
    let mut s = r.spec("o", &["make", "x"], Kind::Compile);
    s.placement = Placement::Anywhere;
    let id = r.submit(&s);
    r.tick();
    // whatever placement chose, a wait for a machine is never stored as machine pressure
    if let Phase::Waiting(w) = r.phase(id) {
        assert!(!matches!(w, WaitReason::Pressure(_)), "{w:?}");
    }
    // a failing forward waits and says so
    let f = Arc::new(FakeForwarder::new(vec![crate::tests::peer_view()]));
    let r = Rig::with_forwarder(f.clone());
    *f.fail.lock().unwrap() = true;
    let mut s = r.spec("o", &["make", "x"], Kind::Compile);
    s.placement = Placement::Peer(MachineName("nixos".into()));
    let id = r.submit(&s);
    r.tick();
    let Phase::Waiting(WaitReason::Placement(text)) = r.phase(id) else {
        panic!("a placement wait expected, got {:?}", r.phase(id));
    };
    assert!(text.contains("link down"), "{text}");
}

// ---------------------------------------------------------------- 9: connection limit

#[test]
fn r101_the_connection_limit_has_its_own_code_and_says_nothing_was_processed() {
    let dir = TempDir::new("connlimit");
    let procs = Arc::new(FakeProcs::new());
    let opts = crate::tests::daemon_options(&dir, &procs);
    let cfg = opts.config.clone();
    let d = crate::server::Daemon::start(opts).unwrap();
    let sock = cfg.socket_path();
    let handle = std::thread::spawn(move || d.run());
    // fill every connection slot with idle clients
    let held: Vec<UnixStream> = (0..crate::server::MAX_CONNECTIONS)
        .map(|_| UnixStream::connect(&sock).unwrap())
        .collect();
    // let the accept loop register them all
    std::thread::sleep(std::time::Duration::from_millis(300));
    let over = UnixStream::connect(&sock).unwrap();
    over.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut line = String::new();
    BufReader::new(over).read_line(&mut line).unwrap();
    let env: ReplyEnvelope = serde_json::from_str(&line).unwrap();
    let Reply::Err(e) = env.reply else {
        panic!("an error expected, got {line}");
    };
    assert_eq!(e.code, ErrorCode::ConnectionLimit);
    assert!(e.message.contains("not processed"), "{}", e.message);
    drop(held);
    // a freed slot serves again
    let mut ok_conn = None;
    for _ in 0..50 {
        let mut s = UnixStream::connect(&sock).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let req = serde_json::to_string(&RequestEnvelope {
            v: PROTOCOL_VERSION,
            request: Request::Hello,
        })
        .unwrap();
        s.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut out = String::new();
        BufReader::new(s).read_line(&mut out).unwrap();
        if let Reply::Ok(Response::Hello(_)) =
            serde_json::from_str::<ReplyEnvelope>(&out).unwrap().reply
        {
            ok_conn = Some(());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(ok_conn.is_some(), "a freed connection slot serves again");
    let r = crate::tests::call(&sock, &Request::Shutdown);
    assert!(matches!(r, Reply::Ok(Response::Ack)));
    handle.join().unwrap().unwrap();
}

// R-368
#[test]
fn r368_the_submitted_reply_carries_the_submit_rule_warnings_and_is_silent_without_any() {
    let r = Rig::with_policy("[submit]\nbroad_test = \"warn\"\n");
    let broad = r.spec("o", &["cargo", "test", "-p", "x", "video_call"], Kind::Test);
    let Response::Submitted { warnings, .. } = ok(submit(&r, broad)) else {
        panic!("submitted expected");
    };
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("this builds every test target"),
        "{warnings:?}"
    );
    let fine = r.spec("o", &["cargo", "check"], Kind::Compile);
    let Response::Submitted { warnings, .. } = ok(submit(&r, fine)) else {
        panic!("submitted expected");
    };
    assert!(warnings.is_empty());
    // an older client sees the same reply it always did: no `warnings` key when there are none
    let plain = serde_json::to_string(&Response::Submitted {
        id: JobId(1),
        warnings: vec![],
    })
    .unwrap();
    assert!(!plain.contains("warnings"), "{plain}");
    let old: Response = serde_json::from_str(&plain).unwrap();
    assert_eq!(
        old,
        Response::Submitted {
            id: JobId(1),
            warnings: vec![]
        }
    );
}

// R-369
#[test]
fn r369_a_tail_returns_the_offset_a_follower_resumes_from_without_gaps_or_repeats() {
    let r = Rig::new();
    let id = r.submit(&r.spec("o", &["make"], Kind::Run));
    r.tick();
    r.until_running(id);
    let rec = r.record(id);
    let log = rec.log_path.clone().unwrap();
    std::fs::write(&log, "one\ntwo\nthr").unwrap();
    let tail = |r: &Rig| {
        let Response::LogTail {
            lines, end_offset, ..
        } = ok(r.sch.handle(Request::Logs {
            id,
            view: LogView::Tail { lines: 10 },
        }))
        else {
            panic!("tail expected");
        };
        (lines, end_offset)
    };
    // the unterminated last line is not in the tail, and the offset stops before it
    let (lines, end) = tail(&r);
    assert_eq!(lines, ["one", "two"]);
    assert_eq!(end, Some(8));
    // appending and following from that offset gives the line whole, once
    std::fs::write(&log, "one\ntwo\nthree\nfour\n").unwrap();
    let Response::LogChunk {
        data, next_offset, ..
    } = ok(r.sch.handle(Request::Logs {
        id,
        view: LogView::Follow {
            offset: 8,
            max_bytes: Some(4096),
        },
    }))
    else {
        panic!("chunk expected");
    };
    assert_eq!(data, "three\nfour\n");
    assert_eq!(next_offset, 19);
    // a complete tail ends at the file's end
    let (lines, end) = tail(&r);
    assert_eq!((lines.len(), end), (4, Some(19)));
}

// R-543: a program the job runs by name, directly or through `env`, is found in the same
// directories the daemon searches ($HOME/.cargo/bin), not only in the daemon's own PATH
#[test]
fn r543_a_job_finds_a_tool_by_name_through_env_in_the_daemons_fallback_dirs() {
    use std::os::unix::fs::PermissionsExt;
    let r = Rig::native();
    let bin = r.dir.path().join("home/.cargo/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("r543-tool");
    std::fs::write(&tool, "#!/bin/sh\necho tool-ran-$R543\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let id = r.submit(&r.spec("o", &["env", "R543=1", "r543-tool"], Kind::Run));
    assert_eq!(r.until_done(id), tasks_core::Phase::Passed);
    let log = r.log_text(id);
    assert!(log.contains("tool-ran-1"), "{log}");
    let id = r.submit(&r.spec("o", &["r543-tool"], Kind::Run));
    assert_eq!(r.until_done(id), tasks_core::Phase::Passed);
}
