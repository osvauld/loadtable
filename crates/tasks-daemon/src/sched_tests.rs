//! Scheduling (design §10): classes, aging, owner turns, the short lane from history, and
//! forwarded jobs that wait on a peer. Names carry the regression ids they pin.

use crate::api::*;
use crate::forwarder::{ForwardPoll, ForwardProgress};
use crate::testkit::{FakeForwarder, Rig};
use std::sync::Arc;
use tasks_admission::SHORT_LANE_BASE;
use tasks_core::{
    JobFigures, JobId, Kind, MachineName, Phase, Placement, Priority, SlotKind, WaitReason,
};
use tasks_ledger::{AuditLedger, Clock, Finish, LeaseRecord, StartInfo, Store};
use tasks_placement::RunEstimate;
use tasks_timing::{Cache, History, Outcome, RecipeKey, Sample, SampleKey};

const MIN_MS: i64 = 60_000;

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

/// A one-compile-slot rig whose slot is held by a running job of owner `holder`.
fn busy_rig(holder: &str) -> (Rig, JobId) {
    let r = Rig::with_policy("[slots]\ncompile = 1\n");
    let a = r.submit(&r.spec_in(holder, &["make", "held"], Kind::Compile, "held"));
    r.tick();
    r.until_running(a);
    (r, a)
}

/// Submits a compile job of `owner` in its own checkout, in the given class.
fn queue(r: &Rig, owner: &str, name: &str, class: Priority) -> JobId {
    let mut s = r.spec_in(owner, &["make", name], Kind::Compile, name);
    s.priority = class;
    if class == Priority::Blocking {
        s.priority_reason = Some("an agent waits on it".into());
    }
    r.submit(&s)
}

/// Lets the running job end and returns which of `candidates` the freed slot went to.
fn who_starts_next(r: &Rig, ended: JobId, ended_pgid: usize, candidates: &[JobId]) -> JobId {
    r.procs.exit(r.procs.pgid(ended_pgid), 0);
    r.until_done(ended);
    r.until("a queued job starts", || {
        r.tick();
        candidates.iter().any(|c| r.phase(*c) == Phase::Running)
    });
    let running: Vec<JobId> = candidates
        .iter()
        .copied()
        .filter(|c| r.phase(*c) == Phase::Running)
        .collect();
    assert_eq!(running.len(), 1, "exactly one starts in a one-slot rig");
    running[0]
}

fn priority_rows(r: &Rig) -> Vec<tasks_ledger::AuditRow> {
    r.audit_store
        .scan(0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|x| x.action == "job.priority")
        .collect()
}

// ---------------------------------------------------------------- classes

// R-338
#[test]
fn r338_blocking_starts_before_normal_before_background() {
    let (r, a) = busy_rig("holder");
    let bg = queue(&r, "p", "bg", Priority::Background);
    let normal = queue(&r, "q", "normal", Priority::Normal);
    let blocking = queue(&r, "s", "blocking", Priority::Blocking);
    r.tick();
    let all = [bg, normal, blocking];
    let first = who_starts_next(&r, a, 0, &all);
    assert_eq!(first, blocking, "submitted last, served first");
    let second = who_starts_next(&r, first, 1, &[bg, normal]);
    assert_eq!(second, normal);
    let third = who_starts_next(&r, second, 2, &[bg]);
    assert_eq!(third, bg);
}

// R-338, R-347: the status lists waiters in the order admission will take them, and its start
// estimates follow that order.
#[test]
fn r347_status_positions_and_estimates_follow_the_queue_order() {
    let (r, _a) = busy_rig("holder");
    let bg = queue(&r, "p", "bg", Priority::Background);
    let normal = queue(&r, "q", "normal", Priority::Normal);
    let blocking = queue(&r, "s", "blocking", Priority::Blocking);
    r.tick();
    let st = r.sch.status();
    let order: Vec<(JobId, usize)> = st.waiting.iter().map(|w| (w.id, w.position)).collect();
    assert_eq!(order, vec![(blocking, 1), (normal, 2), (bg, 3)]);
    let starts: Vec<f64> = st
        .waiting
        .iter()
        .map(|w| w.start_estimate_secs.unwrap())
        .collect();
    assert!(starts[0] < starts[1] && starts[1] < starts[2], "{starts:?}");
    let classes: Vec<&str> = st.waiting.iter().map(|w| w.class.as_str()).collect();
    assert_eq!(classes, ["blocking", "normal", "background"]);
    assert!(
        st.waiting[0].class_note.contains("an agent waits on it"),
        "{}",
        st.waiting[0].class_note
    );
}

// ---------------------------------------------------------------- blocking: reason and audit

// R-339
#[test]
fn r339_blocking_needs_a_reason_and_is_audited_with_who_and_why() {
    let r = Rig::new();
    let mut s = r.spec("agent-7", &["make", "x"], Kind::Run);
    s.priority = Priority::Blocking;
    let e = err(r.sch.handle(Request::Submit { spec: s.clone() }));
    assert_eq!(e.code, ErrorCode::Refused);
    assert!(e.message.contains("needs a reason"), "{}", e.message);
    assert!(
        priority_rows(&r).is_empty(),
        "a refused claim is not a grant"
    );
    s.priority_reason = Some("release build blocks the user".into());
    let Response::Submitted { id, .. } = ok(r.sch.handle(Request::Submit { spec: s })) else {
        panic!("submitted expected");
    };
    let rows = priority_rows(&r);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!((row.owner.as_str(), row.job), ("agent-7", Some(id.0)));
    let d = row.details_json();
    assert_eq!(d["priority"], "blocking");
    assert_eq!(d["reason"], "release build blocks the user");
    assert_eq!(d["via"], "submit");
    // a plain submit writes no priority row
    r.submit(&r.spec("agent-7", &["make", "y"], Kind::Run));
    assert_eq!(priority_rows(&r).len(), 1);
}

// R-339: background is audited too, and a plan step's run-policy hint is a class of its own.
#[test]
fn r339_background_at_submit_is_audited_and_shown() {
    let r = Rig::new();
    let mut s = r.spec("agent-7", &["make", "sweep"], Kind::Run);
    s.priority = Priority::Background;
    let Response::Submitted { id, .. } = ok(r.sch.handle(Request::Submit { spec: s })) else {
        panic!("submitted expected");
    };
    let rows = priority_rows(&r);
    assert_eq!(rows[0].details_json()["priority"], "background");
    let Response::Job { job } = ok(r.sch.handle(Request::Get { id })) else {
        panic!("job expected");
    };
    assert_eq!(job.priority, Priority::Background);
}

// R-350
#[test]
fn r350_prioritize_changes_a_queued_job_audits_it_and_checks_the_owner_and_the_reason() {
    let (r, _a) = busy_rig("holder");
    let n1 = queue(&r, "p", "n1", Priority::Normal);
    let n2 = queue(&r, "q", "n2", Priority::Normal);
    r.tick();
    let asked = |who: &str, priority, reason: Option<&str>| {
        r.sch.handle(Request::Prioritize {
            id: n2,
            who: who.into(),
            priority,
            reason: reason.map(str::to_string),
        })
    };
    // not the owner
    assert_eq!(
        err(asked("p", Priority::Blocking, Some("mine"))).code,
        ErrorCode::NotOwner
    );
    // blocking without a reason
    let e = err(asked("q", Priority::Blocking, None));
    assert_eq!(e.code, ErrorCode::Refused);
    assert!(e.message.contains("reason"), "{}", e.message);
    assert!(priority_rows(&r).is_empty());
    // the owner, with a reason: it moves ahead of the older normal job
    let Response::Prioritized { from, to, .. } =
        ok(asked("q", Priority::Blocking, Some("user is waiting")))
    else {
        panic!("prioritized expected");
    };
    assert_eq!((from, to), (Priority::Normal, Priority::Blocking));
    let st = r.sch.status();
    assert_eq!(
        st.waiting.iter().map(|w| w.id).collect::<Vec<_>>(),
        vec![n2, n1]
    );
    let rows = priority_rows(&r);
    assert_eq!(rows.len(), 1);
    let d = rows[0].details_json();
    assert_eq!(rows[0].owner, "q");
    assert_eq!(rows[0].job, Some(n2.0));
    assert_eq!(
        (
            d["from"].as_str(),
            d["priority"].as_str(),
            d["via"].as_str()
        ),
        (Some("normal"), Some("blocking"), Some("prioritize"))
    );
    assert_eq!(d["reason"], "user is waiting");
    // and it can be lowered again
    ok(asked("q", Priority::Background, None));
    assert_eq!(priority_rows(&r).len(), 2);
}

// R-351
#[test]
fn r351_prioritize_never_preempts_a_running_job() {
    let (r, a) = busy_rig("holder");
    let e = err(r.sch.handle(Request::Prioritize {
        id: a,
        who: "holder".into(),
        priority: Priority::Background,
        reason: None,
    }));
    assert_eq!(e.code, ErrorCode::Refused);
    assert!(
        e.message.contains("only a queued or waiting job"),
        "{}",
        e.message
    );
    // nothing changed and nothing was signalled
    assert_eq!(r.phase(a), Phase::Running);
    assert!(r.procs.signals().is_empty());
    assert_eq!(r.record(a).spec.priority, Priority::Normal);
    // a blocking job queued behind it waits for the slot like any other
    let k = queue(&r, "s", "urgent", Priority::Blocking);
    r.tick();
    assert_eq!(r.phase(a), Phase::Running);
    assert!(matches!(r.phase(k), Phase::Waiting(_)), "{:?}", r.phase(k));
    assert!(r.procs.signals().is_empty());
    // a finished job is refused too
    r.procs.exit(r.procs.pgid(0), 0);
    r.until_done(a);
    let e = err(r.sch.handle(Request::Prioritize {
        id: a,
        who: "holder".into(),
        priority: Priority::Blocking,
        reason: Some("late".into()),
    }));
    assert_eq!(e.code, ErrorCode::Refused);
}

// ---------------------------------------------------------------- aging

// R-340
#[test]
fn r340_a_background_job_rises_to_normal_after_the_aging_wait_and_the_status_says_why() {
    let (r, _a) = busy_rig("holder");
    let t0 = r.clock.now_ms();
    let bg = queue(&r, "p", "bg", Priority::Background);
    r.clock.advance(5 * MIN_MS);
    let n1 = queue(&r, "q", "n1", Priority::Normal);
    // 19 minutes in: still background, behind the normal job
    r.clock.set(t0 + 19 * MIN_MS);
    r.tick();
    let st = r.sch.status();
    assert_eq!(
        st.waiting
            .iter()
            .map(|w| (w.id, w.class.as_str()))
            .collect::<Vec<_>>(),
        vec![(n1, "normal"), (bg, "background")]
    );
    let note = &st.waiting[1].class_note;
    assert!(note.contains("rises to normal after 20m"), "{note}");
    // 21 minutes in it rose at minute 20: it now queues among the normal jobs by when it rose,
    // so a normal job submitted at minute 21 is behind it
    r.clock.set(t0 + 21 * MIN_MS);
    let n2 = queue(&r, "s", "n2", Priority::Normal);
    r.tick();
    let st = r.sch.status();
    assert_eq!(
        st.waiting
            .iter()
            .map(|w| (w.id, w.class.as_str()))
            .collect::<Vec<_>>(),
        vec![(n1, "normal"), (bg, "normal"), (n2, "normal")]
    );
    let aged = st.waiting.iter().find(|w| w.id == bg).unwrap();
    assert!(
        aged.class_note
            .contains("submitted background, rose to normal after 20m"),
        "{}",
        aged.class_note
    );
    // the job itself still says what it was submitted as
    let Response::Job { job } = ok(r.sch.handle(Request::Get { id: bg })) else {
        panic!("job expected");
    };
    assert_eq!(job.priority, Priority::Background);
}

// R-340: the aging wait is machine policy.
#[test]
fn r340_the_aging_wait_is_a_machine_policy_key_with_a_default_of_twenty_minutes() {
    let r = Rig::new();
    let t = r.sch.policy().thresholds();
    assert_eq!(t.aging_ms, 20 * 60_000);
    let r = Rig::with_policy("[queue]\naging_mins = 5\n");
    assert_eq!(r.sch.policy().thresholds().aging_ms, 5 * 60_000);
    let off = Rig::with_policy("[queue]\naging_mins = \"off\"\n");
    assert_eq!(off.sch.policy().thresholds().aging_ms, 0);
}

// ---------------------------------------------------------------- owners take turns

// R-341
#[test]
fn r341_the_owner_that_started_least_recently_gets_the_freed_slot() {
    let (r, a) = busy_rig("busy");
    // busy has just run a job and has two more queued; quiet submitted one, later
    let b2 = queue(&r, "busy", "b2", Priority::Normal);
    let b3 = queue(&r, "busy", "b3", Priority::Normal);
    let q1 = queue(&r, "quiet", "q1", Priority::Normal);
    r.tick();
    let st = r.sch.status();
    assert_eq!(
        st.waiting.iter().map(|w| w.id).collect::<Vec<_>>(),
        vec![q1, b2, b3],
        "quiet's one job is first, not third"
    );
    assert_eq!(who_starts_next(&r, a, 0, &[b2, b3, q1]), q1);
}

// ---------------------------------------------------------------- the short lane

fn info() -> StartInfo {
    StartInfo {
        machine: MachineName("testbox".into()),
        lease: LeaseRecord {
            pgid: 1,
            leader_start: 1,
        },
        log_path: None,
    }
}

fn recipe_key(argv: &[&str]) -> SampleKey {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    SampleKey {
        recipe: RecipeKey::from_argv(&argv, &[])[0].to_string(),
        target: None,
        machine: "testbox".into(),
    }
}

/// Puts `runs` finished runs of the recipe into the ledger as the daemon would have left them:
/// a timing sample and the job's final figures.
fn seed_history(r: &Rig, argv: &[&str], runs: u64, secs: f64, peak: Option<u64>) {
    let key = recipe_key(argv);
    for n in 0..runs {
        let id = r
            .store
            .submit(&r.spec_in("history", argv, Kind::Compile, "history"))
            .unwrap();
        r.store.start(id, &info()).unwrap();
        if let Some(p) = peak {
            let mut f = JobFigures::empty(tasks_core::Attribution::Cgroup);
            f.mem_peak_bytes = Some(p);
            r.store.set_figures(id, &f).unwrap();
        }
        let s = Sample {
            secs,
            cache: Cache::Warm,
            outcome: Outcome::Passed,
            at_ms: 1_000 + n,
        };
        r.store
            .finish_recording(id, &Finish::from_phase(Phase::Passed), Some((&key, &s)))
            .unwrap();
    }
}

/// A rig whose CPU gate is shut, so that only the short lane can start a compile job.
fn gated_rig() -> Rig {
    // the CPU limit is stated: macOS defaults it to off (no PSI there),
    let r = Rig::with_policy("[slots]\ncompile = 1\n[pressure.compile]\nmax_cpu = 40\n");
    r.machine.state.lock().unwrap().pressure.cpu = Some(95.0);
    r
}

/// Submits the recipe and reports the slot index it was started in (None: it waited).
fn slot_of(r: &Rig, argv: &[&str]) -> (JobId, Option<u32>) {
    let id = r.submit(&r.spec_in("o", argv, Kind::Compile, "work"));
    r.tick();
    let st = r.sch.status();
    let slot = st
        .running
        .iter()
        .find(|j| j.id == id)
        .and_then(|j| j.slot.as_ref().map(|s| s.1));
    (id, slot)
}

const CHECK: [&str; 4] = ["cargo", "check", "-p", "tasks-core"];

// R-342
#[test]
fn r342_the_short_lane_takes_a_job_whose_history_says_quick_and_small() {
    let r = gated_rig();
    // no history: unknown is never short, the closed CPU gate holds it
    let (cold, slot) = slot_of(&r, &CHECK);
    assert_eq!(slot, None);
    assert!(
        r.record(cold).wait_text.unwrap().contains("CPU contended"),
        "{:?}",
        r.record(cold).wait_text
    );
    r.sch.handle(Request::Cancel {
        id: cold,
        who: "o".into(),
        reason: None,
    });
    // two quick, small runs
    seed_history(&r, &CHECK, 2, 8.0, Some(300 << 20));
    let (_, slot) = slot_of(&r, &CHECK);
    assert!(slot.is_some_and(|s| s >= SHORT_LANE_BASE), "{slot:?}");
}

#[test]
fn r342_one_run_of_history_is_too_little() {
    let r = gated_rig();
    seed_history(&r, &CHECK, 1, 8.0, Some(300 << 20));
    assert_eq!(slot_of(&r, &CHECK).1, None);
}

#[test]
fn r342_a_job_is_not_short_when_history_does_not_know_its_memory() {
    let r = gated_rig();
    // quick, but the runs left no figures
    seed_history(&r, &CHECK, 3, 8.0, None);
    assert_eq!(slot_of(&r, &CHECK).1, None);
}

#[test]
fn r342_a_job_is_not_short_when_history_says_it_is_big_or_slow() {
    let r = gated_rig();
    seed_history(&r, &CHECK, 3, 8.0, Some(2 << 30));
    assert_eq!(
        slot_of(&r, &CHECK).1,
        None,
        "peak 2 GiB is over the 1 GiB cap"
    );
    let slow = gated_rig();
    seed_history(&slow, &CHECK, 3, 120.0, Some(100 << 20));
    assert_eq!(
        slot_of(&slow, &CHECK).1,
        None,
        "120 s is over the 60 s limit"
    );
    // the cap is machine policy
    let roomy = Rig::with_policy(
        "[slots]\ncompile = 1\nshort_job_max_memory_mib = 4096\n[pressure.compile]\nmax_cpu = 40\n",
    );
    roomy.machine.state.lock().unwrap().pressure.cpu = Some(95.0);
    seed_history(&roomy, &CHECK, 3, 8.0, Some(2 << 30));
    assert!(
        slot_of(&roomy, &CHECK)
            .1
            .is_some_and(|s| s >= SHORT_LANE_BASE)
    );
}

// R-344: still the memory floor and the disk reserve.
#[test]
fn r344_a_short_job_still_waits_for_the_memory_floor() {
    let r = gated_rig();
    seed_history(&r, &CHECK, 2, 8.0, Some(100 << 20));
    r.machine.state.lock().unwrap().memory.available_bytes = 1 << 30;
    let (id, slot) = slot_of(&r, &CHECK);
    assert_eq!(slot, None);
    assert!(
        r.record(id).wait_text.unwrap().starts_with("memory:"),
        "{:?}",
        r.record(id).wait_text
    );
}

// R-345
#[test]
fn r345_an_overrunning_short_job_is_not_killed_and_the_next_run_is_not_short() {
    let r = gated_rig();
    seed_history(&r, &CHECK, 2, 8.0, Some(100 << 20));
    let (id, slot) = slot_of(&r, &CHECK);
    assert!(slot.is_some_and(|s| s >= SHORT_LANE_BASE));
    // an hour later it is still running: nothing killed it, nothing signalled it
    r.clock.advance(60 * MIN_MS);
    r.tick();
    assert_eq!(r.phase(id), Phase::Running);
    assert!(r.procs.signals().is_empty());
    // it finishes; the sample the runner writes at the end of such a run is this long
    r.procs.exit(r.procs.pgid(0), 0);
    r.until_done(id);
    let key = recipe_key(&CHECK);
    let at = u64::try_from(r.clock.now_ms()).unwrap() + 1;
    r.store
        .record_sample(
            &key,
            &Sample {
                secs: 3_600.0,
                cache: Cache::Warm,
                outcome: Outcome::Passed,
                at_ms: at,
            },
        )
        .unwrap();
    let next = r.submit(&r.spec_in("o", &CHECK, Kind::Compile, "work"));
    let facts = r.sch.facts(&r.record(next));
    assert_eq!(facts.short_secs, Some(3_600.0));
    r.tick();
    assert!(
        matches!(r.phase(next), Phase::Waiting(_)),
        "{:?}",
        r.phase(next)
    );
}

// ---------------------------------------------------------------- placement and forwarded jobs

fn fast_peer(blocked: bool) -> tasks_placement::MachineView {
    let mut p = crate::tests::peer_view();
    p.free_slots = [(SlotKind::Compile, 2)].into();
    p.estimate = Some(RunEstimate {
        median_secs: 10.0,
        p90_secs: 10.0,
        samples: 5,
    });
    p.pressure = tasks_placement::Pressure {
        blocked,
        summary: if blocked {
            "CPU contended: pressure 63% > 40% for compile".into()
        } else {
            String::new()
        },
    };
    p
}

fn anywhere(r: &Rig) -> tasks_core::JobSpec {
    let mut s = r.spec("o", &["cargo", "test"], Kind::Test);
    s.placement = Placement::Anywhere;
    s
}

// R-348
#[test]
fn r348_anywhere_does_not_send_a_job_to_a_peer_whose_compile_gate_is_closed() {
    let f = Arc::new(FakeForwarder::new(vec![fast_peer(true)]));
    let r = Rig::with_forwarder(f.clone());
    let id = r.submit(&anywhere(&r));
    let rep = r.tick();
    assert!(rep.forwarded.is_empty(), "{rep:?}");
    assert_eq!(rep.started, vec![id], "the idle local machine takes it");
    assert!(f.forwarded.lock().unwrap().is_empty());
}

#[test]
fn r348_the_same_peer_gets_the_job_when_its_gate_is_open() {
    let f = Arc::new(FakeForwarder::new(vec![fast_peer(false)]));
    let r = Rig::with_forwarder(f);
    let id = r.submit(&anywhere(&r));
    assert_eq!(r.tick().forwarded, vec![id]);
}

fn anywhere_over_cap(r: &Rig) -> tasks_core::JobSpec {
    let mut s = anywhere(r);
    s.argv = ["cargo", "test", "-j", "6"].map(String::from).to_vec();
    s
}

// R-541
#[test]
fn r541_an_anywhere_job_above_the_local_cap_is_forwarded_even_when_the_peer_is_not_faster() {
    let f = Arc::new(FakeForwarder::new(vec![fast_peer(false)]));
    let r = Rig::with_forwarder(f);
    let id = r.submit(&anywhere_over_cap(&r));
    assert_eq!(r.tick().forwarded, vec![id]);
}

#[test]
fn r541_with_no_peer_an_anywhere_job_above_the_cap_is_not_started_and_names_the_cap() {
    let r = Rig::with_forwarder(Arc::new(FakeForwarder::new(vec![])));
    let id = r.submit(&anywhere_over_cap(&r));
    let rep = r.tick();
    assert_eq!(rep.not_started, vec![id], "{rep:?}");
    let text = format!("{:?}", r.phase(id));
    assert!(text.contains("cap of 2"), "{text}");
}

fn nixos() -> MachineName {
    MachineName("nixos".into())
}

fn accepted(remote_phase: Phase) -> ForwardPoll {
    ForwardPoll::Active(ForwardProgress {
        mirror: "#7".into(),
        remote_job: Some(7),
        accepted: true,
        remote_phase: Some(remote_phase),
    })
}

// R-349
#[test]
fn r349_a_forwarded_job_the_peer_holds_back_is_listed_as_waiting_with_the_peers_reason() {
    let f = Arc::new(FakeForwarder::new(vec![]));
    let r = Rig::with_forwarder(f.clone());
    let id = r.submit(&anywhere(&r));
    r.store.set_forward(id, &nixos(), None, Some("k1")).unwrap();
    let why = "CPU contended: pressure 63% > 40% for compile";
    f.script(
        id,
        accepted(Phase::Waiting(WaitReason::Pressure(why.into()))),
    );
    r.until("accepted", || {
        r.tick();
        r.phase(id) == Phase::Running
    });
    // the local row stays Running (R-015) but nothing calls it running
    let st = r.sch.status();
    assert!(st.running.iter().all(|j| j.id != id), "{:?}", st.running);
    let w = st
        .waiting
        .iter()
        .find(|w| w.id == id)
        .expect("listed as waiting");
    assert_eq!(w.on_peer.as_deref(), Some("nixos"));
    assert!(
        w.wait_text.contains(why) && w.wait_text.contains("nixos"),
        "{}",
        w.wait_text
    );
    let Response::Jobs { jobs, .. } = ok(r.sch.handle(Request::List {
        owner: None,
        active_only: true,
        recent_finished: None,
        limit: None,
    })) else {
        panic!("jobs expected");
    };
    let row = jobs.iter().find(|j| j.id == id).unwrap();
    assert_eq!(
        row.remote_wait.as_deref().map(|t| t.contains(why)),
        Some(true)
    );
    // once the peer starts it, it is running
    f.script(id, accepted(Phase::Running));
    r.until("running on the peer", || {
        r.tick();
        r.sch.status().running.iter().any(|j| j.id == id)
    });
    let st = r.sch.status();
    assert!(st.waiting.iter().all(|w| w.id != id));
    let Response::Job { job } = ok(r.sch.handle(Request::Get { id })) else {
        panic!("job expected");
    };
    assert_eq!(job.remote_wait, None);
}

// R-353
#[test]
fn r353_a_forwarded_job_that_passed_stores_the_peers_final_phase_not_the_last_one_mirrored() {
    use crate::forwarder::ForwardEnd;
    let f = Arc::new(FakeForwarder::new(vec![]));
    let r = Rig::with_forwarder(f.clone());
    let id = r.submit(&anywhere(&r));
    r.store.set_forward(id, &nixos(), None, Some("k1")).unwrap();
    // the last phase mirrored while the job lived was `queued`
    f.script(id, accepted(Phase::Queued));
    r.until("accepted", || {
        r.tick();
        r.record(id).remote_phase == Some(Phase::Queued)
    });
    f.script(
        id,
        ForwardPoll::Done(ForwardEnd {
            phase: Phase::Passed,
            reason: "passed on nixos".into(),
            exit_code: Some(0),
            signal: None,
            remote_job: Some(1),
            nothing_submitted: false,
            samples: vec![],
            fetch: vec![],
        }),
    );
    assert_eq!(r.until_done(id), Phase::Passed);
    assert_eq!(r.record(id).remote_phase, Some(Phase::Passed));
    // and the view the CLI and UI read says so
    let Response::Job { job } = ok(r.sch.handle(Request::Get { id })) else {
        panic!("job expected");
    };
    assert_eq!(job.remote_phase, Some(Phase::Passed));
}

// R-370
#[test]
fn r370_a_class_changed_while_the_job_waits_is_what_the_peer_is_sent() {
    let f = Arc::new(FakeForwarder::new(vec![]));
    let r = Rig::with_forwarder(f.clone());
    let id = r.submit(&anywhere(&r));
    r.store.set_forward(id, &nixos(), None, Some("k1")).unwrap();
    r.store
        .set_priority(id, tasks_core::Priority::Background, None)
        .unwrap();
    f.script(id, accepted(Phase::Running));
    r.until("started", || {
        r.tick();
        r.phase(id) == Phase::Running
    });
    assert_eq!(
        f.classes.lock().unwrap().last().cloned(),
        Some((id, tasks_core::Priority::Background, None))
    );
}

// ---------------------------------------------------------------- honest estimates (R-660..R-679)

fn seed_run(r: &Rig, recipe: &str, secs: f64, outcome: Outcome, at_ms: u64) {
    let key = SampleKey {
        recipe: recipe.into(),
        target: None,
        machine: "testbox".into(),
    };
    r.store
        .record_sample(
            &key,
            &Sample {
                secs,
                cache: Cache::Cold,
                outcome,
                at_ms,
            },
        )
        .unwrap();
}

fn running_view(r: &Rig, id: JobId) -> RunningView {
    r.sch
        .status()
        .running
        .into_iter()
        .find(|j| j.id == id)
        .expect("running")
}

fn run_make(r: &Rig, arg: &str) -> JobId {
    let id = r.submit(&r.spec_in("o", &["make", arg], Kind::Compile, arg));
    r.tick();
    r.until_running(id);
    id
}

#[test]
fn r666_a_command_that_never_ran_shows_no_estimate_and_says_first_run() {
    let r = Rig::new();
    let v = running_view(&r, run_make(&r, "a"));
    assert_eq!(
        (v.eta_secs, v.no_eta_why.as_deref()),
        (None, Some("first run"))
    );
}

#[test]
fn r667_a_command_that_only_ever_failed_shows_no_estimate_and_says_so() {
    let r = Rig::new();
    seed_run(&r, "make b", 57.0, Outcome::Failed, 1);
    let v = running_view(&r, run_make(&r, "b"));
    assert_eq!(v.eta_secs, None, "an early failure is no basis for an ETA");
    assert_eq!(v.no_eta_why.as_deref(), Some("no passing run yet"));
}

#[test]
fn r668_a_passing_run_here_gives_an_estimate_and_it_is_not_rough() {
    let r = Rig::new();
    seed_run(&r, "make c", 400.0, Outcome::Passed, 1);
    let v = running_view(&r, run_make(&r, "c"));
    assert!(v.eta_secs.is_some_and(|e| e > 300.0), "{:?}", v.eta_secs);
    assert_eq!((v.no_eta_why, v.eta_rough_from), (None, None));
}

#[test]
fn r669_the_queue_still_plans_with_a_number_when_nothing_is_shown() {
    let r = Rig::with_policy("[slots]\ncompile = 1\n");
    let a = r.submit(&r.spec_in("o", &["make", "a"], Kind::Compile, "a"));
    r.tick();
    r.until_running(a);
    r.submit(&r.spec_in("p", &["make", "b"], Kind::Compile, "b"));
    r.tick();
    let st = r.sch.status();
    // planning figure for ordering and start times, marked rough; the running job shows none
    assert!(st.waiting[0].start_estimate_secs.is_some() && st.waiting[0].rough);
    assert_eq!(st.running[0].eta_secs, None);
}

#[test]
fn r670_a_forwarded_job_the_peer_holds_back_shows_the_peers_reason_not_starting() {
    let f = Arc::new(FakeForwarder::new(vec![fast_peer(true)]));
    let r = Rig::with_forwarder(f);
    let id = r.submit(&anywhere(&r));
    // forwarded to the peer but still waiting here, before the peer says anything
    r.store.set_forward(id, &nixos(), None, Some("k1")).unwrap();
    let st = r.sch.status();
    let w = st.waiting.iter().find(|w| w.id == id).expect("waiting");
    assert_eq!(w.on_peer.as_deref(), Some("nixos"));
    assert!(
        w.wait_text.starts_with("waiting on nixos: CPU contended"),
        "{}",
        w.wait_text
    );
    assert_eq!(w.start_estimate_secs, None, "no start time for a held job");
}

#[test]
fn r675_a_run_past_its_usual_time_is_over_the_usual_with_no_time_left() {
    let r = Rig::new();
    // usual 0s: any elapsed time is past it
    seed_run(&r, "make d", 0.0, Outcome::Passed, 1);
    let v = running_view(&r, run_make(&r, "d"));
    assert_eq!(v.eta_secs, None, "never 0s left");
    assert!(v.usual_secs.is_some() && v.over_by_secs.is_some(), "{v:?}");
}

#[test]
fn r799_an_interactive_job_shows_no_estimate_and_its_runs_teach_no_timings() {
    let key = SampleKey {
        recipe: "make gui".into(),
        target: None,
        machine: "testbox".into(),
    };
    // a passing 0s run would put a plain job over the usual at once (as R-675)
    let r = Rig::new();
    seed_run(&r, "make gui", 0.0, Outcome::Passed, 1);
    let mut s = r.spec_in("o", &["make", "gui"], Kind::Run, "gui");
    s.interactive = true;
    let id = r.submit(&s);
    r.tick();
    r.until_running(id);
    let v = running_view(&r, id);
    assert_eq!(
        (v.eta_secs, v.usual_secs, v.over_by_secs),
        (None, None, None)
    );
    assert_eq!(
        v.no_eta_why.as_deref(),
        Some("interactive: runs as long as it is used")
    );
    let tags = crate::status::job_view(&r.store.get(id).unwrap().unwrap()).tags;
    assert_eq!(tags, vec!["interactive".to_string()]);

    // its finished run files no sample; the job's sample is written with its result
    let r = Rig::new();
    r.procs.auto_exit(0);
    let mut s = r.spec("o", &["make", "gui"], Kind::Run);
    s.interactive = true;
    let id = r.submit(&s);
    r.tick();
    assert_eq!(r.until_done(id), Phase::Passed);
    assert!(r.store.samples(&key).unwrap().is_empty());
}

#[test]
fn r676_different_sh_chains_keep_separate_history() {
    let r = Rig::new();
    seed_run(&r, "build_core.sh", 30.0, Outcome::Passed, 1);
    let e = |script: &str| match ok(r.sch.handle(Request::Estimate {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        kind: None,
    })) {
        Response::Estimate(e) => e,
        other => panic!("{other:?}"),
    };
    assert!(e("build_core.sh").median_secs.is_some());
    let chain = e("build_core.sh && xcodegen && run_sim.sh");
    assert_eq!(chain.median_secs, None);
    assert_eq!(chain.why_not.as_deref(), Some("first run"));
}

#[test]
fn r662_the_estimate_request_says_which_case_applies() {
    let r = Rig::new();
    let ask = |a: &str| match ok(r.sch.handle(Request::Estimate {
        argv: vec!["make".into(), a.into()],
        kind: Some(Kind::Compile),
    })) {
        Response::Estimate(e) => e,
        other => panic!("{other:?}"),
    };
    // no built-in default even when the client names a kind
    let e = ask("x");
    assert_eq!(
        (e.median_secs, e.why_not.as_deref()),
        (None, Some("first run"))
    );
    seed_run(&r, "make y", 9.0, Outcome::Failed, 1);
    assert_eq!(ask("y").why_not.as_deref(), Some("no passing run yet"));
    seed_run(&r, "make z", 90.0, Outcome::Passed, 1);
    assert_eq!(ask("z").median_secs, Some(90.0));
}

#[test]
fn r665_a_finished_run_is_stamped_cold_until_this_machine_has_passed_it() {
    let r = Rig::new();
    r.procs.auto_exit(0);
    let mut stamps = Vec::new();
    for _ in 0..2 {
        let id = r.submit(&r.spec("o", &["make", "stamp"], Kind::Run));
        r.tick();
        r.until_done(id);
        let key = SampleKey {
            recipe: "make stamp".into(),
            target: None,
            machine: "testbox".into(),
        };
        let n = stamps.len() + 1;
        r.until("sample filed", || r.store.samples(&key).unwrap().len() == n);
        let mut got = r.store.samples(&key).unwrap();
        got.sort_by_key(|s| s.at_ms);
        stamps = got.iter().map(|s| s.cache).collect();
    }
    assert_eq!(stamps, vec![Cache::Cold, Cache::Warm]);
}

// R-709: a peer's test step that only reads the synced copy does not wait behind another job of
// the copy; jobs that may change it still go one at a time.
#[test]
fn r709_peer_steps_that_only_read_the_copy_start_together_and_other_jobs_still_queue() {
    let from_peer = |r: &Rig, key: &str, shared: bool| {
        let mut spec = r.spec("origin-owner", &["cargo", "test"], Kind::Test);
        spec.requirements.shared_checkout = shared;
        let mut job = tasks_ledger::NewJob::new(spec);
        job.origin = Some(tasks_ledger::Origin {
            peer: "laptop-node".into(),
            key: key.into(),
        });
        r.ledger.submit_job(&job).unwrap()
    };
    let r = Rig::with_policy("[slots]\ncompile = 3\n");
    let (a, b) = (from_peer(&r, "k1", true), from_peer(&r, "k2", true));
    let started = r.tick().started;
    assert!(started.contains(&a) && started.contains(&b), "{started:?}");

    let r = Rig::with_policy("[slots]\ncompile = 3\n");
    let (a, b) = (from_peer(&r, "k1", false), from_peer(&r, "k2", false));
    let started = r.tick().started;
    assert_eq!(
        started,
        vec![a],
        "the second waits for the copy: {started:?}"
    );
    assert!(r.record(b).wait_text.is_some());
}

// ---------------------------------------------------------------- isolate

/// Ticks until the job runs: the job that held the machine is gone from the scheduler a moment
/// after its terminal write, so one tick may still see it.
fn tick_until_running(r: &Rig, id: JobId) {
    r.until("the job starts", || {
        r.tick();
        r.phase(id) == Phase::Running
    });
}

fn isolated(r: &Rig, name: &str, kind: Kind) -> JobId {
    let mut s = r.spec_in("o", &["make", name], kind, name);
    s.isolate = true;
    r.submit(&s)
}

// R-712
#[test]
fn r712_an_isolate_job_waits_for_the_running_job_and_starts_when_the_machine_is_idle() {
    let r = Rig::new();
    let a = r.submit(&r.spec_in("o", &["make", "a"], Kind::Compile, "a"));
    r.tick();
    r.until_running(a);
    let b = isolated(&r, "b", Kind::Test);
    r.tick();
    assert!(matches!(r.phase(b), Phase::Waiting(_)), "{:?}", r.phase(b));
    let why = r.record(b).wait_text.unwrap();
    assert!(
        why.contains("runs alone (isolate)") && why.contains(&format!("#{}", a.0)),
        "{why}"
    );
    r.procs.exit(r.procs.pgid(0), 0);
    r.until_done(a);
    tick_until_running(&r, b);
}

// R-712, R-713
#[test]
fn r712_while_an_isolate_job_runs_nothing_else_starts_and_r713_the_job_behind_it_says_why() {
    let r = Rig::new();
    let a = isolated(&r, "a", Kind::Test);
    r.tick();
    r.until_running(a);
    let b = r.submit(&r.spec_in("o", &["make", "b"], Kind::Compile, "b"));
    r.tick();
    assert!(matches!(r.phase(b), Phase::Waiting(_)), "{:?}", r.phase(b));
    assert_eq!(
        r.record(b).wait_text.unwrap(),
        format!("job #{} runs alone (isolate)", a.0)
    );
    r.procs.exit(r.procs.pgid(0), 0);
    r.until_done(a);
    tick_until_running(&r, b);
}

// R-713
#[test]
fn r713_an_isolate_job_first_in_line_is_not_starved_by_the_jobs_behind_it() {
    let r = Rig::new();
    let a = r.submit(&r.spec_in("o", &["make", "a"], Kind::Compile, "a"));
    r.tick();
    r.until_running(a);
    let iso = isolated(&r, "iso", Kind::Test);
    let later = r.submit(&r.spec_in("o", &["make", "later"], Kind::Test, "later"));
    r.tick();
    assert!(matches!(r.phase(iso), Phase::Waiting(_)));
    assert!(
        matches!(r.phase(later), Phase::Waiting(_)),
        "a later job must not start while the isolate job is first in line"
    );
    assert_eq!(
        r.record(later).wait_text.unwrap(),
        format!("job #{} runs alone (isolate)", iso.0)
    );
    r.procs.exit(r.procs.pgid(0), 0);
    r.until_done(a);
    tick_until_running(&r, iso);
    assert!(matches!(r.phase(later), Phase::Waiting(_)));
}

// ---------------------------------------------------------------- light

fn light_job(r: &Rig, name: &str) -> JobId {
    let mut s = r.spec_in("o", &["cargo", "test", name], Kind::Test, name);
    s.light = true;
    r.submit(&s)
}

fn slot_index(r: &Rig, id: JobId) -> Option<u32> {
    r.sch
        .status()
        .running
        .iter()
        .find(|j| j.id == id)
        .and_then(|j| j.slot.as_ref().map(|s| s.1))
}

// R-714, R-715
#[test]
fn r714_a_light_job_runs_in_its_own_pool_through_a_closed_cpu_gate_and_a_full_pool_says_so() {
    let r = Rig::with_policy("[slots]\ncompile = 1\nlight = 1\n[pressure.compile]\nmax_cpu = 40\n");
    r.machine.state.lock().unwrap().pressure.cpu = Some(95.0);
    let ordinary = r.submit(&r.spec_in("o", &["make", "x"], Kind::Compile, "x"));
    let first = light_job(&r, "first");
    let second = light_job(&r, "second");
    r.tick();
    r.until_running(first);
    let slot = slot_index(&r, first).unwrap();
    assert!(slot >= tasks_admission::LIGHT_LANE_BASE, "{slot}");
    assert!(
        r.record(ordinary)
            .wait_text
            .unwrap()
            .contains("CPU contended"),
        "the ordinary job still meets the gate"
    );
    assert_eq!(
        r.record(second).wait_text.unwrap(),
        format!("all 1 light slots busy (#{}); you are 1st in line", first.0)
    );
    // it holds no compile slot: the status line still shows the compile slot free
    let st = r.sch.status();
    let compile = st.slots.iter().find(|g| g.kind == "compile").unwrap();
    assert_eq!(compile.free, compile.total);
}

// R-715
#[test]
fn r715_a_light_job_gets_the_tests_cpu_share_not_a_builds() {
    let r = Rig::delegated();
    r.procs.auto_exit(0);
    let mut s = r.spec_in("o", &["make", "soak"], Kind::Compile, "soak");
    s.light = true;
    let id = r.submit(&s);
    r.tick();
    assert_eq!(r.until_done(id), Phase::Passed);
    assert_eq!(
        r.cgroups.cpu_weight(tasks_platform::CgroupName::Job(id.0)),
        Some(200)
    );
}

// ---------------------------------------------------------------- retry alone

const FAILED_LOG: &[u8] = b"running 2 tests\n\
test glare::one_call ... FAILED\n\
test shards::clock_ahead ... FAILED\n\
\n\
failures:\n\
\n\
---- glare::one_call stdout ----\n\
panicked\n\
\n\
failures:\n\
    glare::one_call\n\
    shards::clock_ahead\n\
\n\
test result: FAILED. 0 passed; 2 failed\n";

fn retry_job(r: &Rig, original: JobId) -> Option<tasks_ledger::JobRecord> {
    r.store
        .list(&tasks_ledger::ListFilter::default())
        .unwrap()
        .into_iter()
        .find(|j| j.hints.retry_of == Some(original.0))
}

/// A `retry_alone` test job that fails with `FAILED_LOG`; the follow-up's run is `retry`.
fn failing_test_job(r: &Rig, retry: (&[u8], i32)) -> JobId {
    r.procs.script_spawns(&[(FAILED_LOG, 101), retry]);
    let mut s = r.spec(
        "o",
        &["cargo", "test", "-p", "p2pcore", "--test", "calls"],
        Kind::Test,
    );
    s.retry_alone = true;
    let id = r.submit(&s);
    r.tick();
    r.until_done(id);
    id
}

// R-716, R-717
#[test]
fn r716_a_failed_retry_alone_job_queues_one_isolated_follow_up_and_says_what_passed_alone() {
    let r = Rig::new();
    // the follow-up passes
    let id = failing_test_job(&r, (b"test result: ok. 2 passed\n", 0));
    r.until("the follow-up is queued", || retry_job(&r, id).is_some());
    let retry = retry_job(&r, id).unwrap();
    let argv = &retry.spec.argv;
    assert_eq!(
        argv[..7],
        ["cargo", "test", "-p", "p2pcore", "--test", "calls", "--"]
    );
    assert_eq!(
        argv[7..],
        [
            "--exact",
            "glare::one_call",
            "shards::clock_ahead",
            "--test-threads=1"
        ]
    );
    assert!(
        retry.spec.isolate && !retry.spec.retry_alone,
        "one retry at most"
    );
    assert_eq!(retry.hints.retry_of, Some(id.0));
    // the original keeps its failed exit and says a retry is under way (or already, what it
    // found: the follow-up is quick)
    let orig = r.record(id);
    assert_eq!(orig.phase, Phase::Failed { exit: Some(101) });
    let text = orig.summary.unwrap();
    assert!(
        text.contains("2 tests failed, retrying alone as job #") || text.contains("passed alone"),
        "{text}"
    );
    r.until("the outcome is settled", || {
        r.tick();
        r.record(id)
            .summary
            .is_some_and(|s| s.contains("passed alone"))
    });
    let orig = r.record(id);
    assert_eq!(orig.phase, Phase::Failed { exit: Some(101) });
    assert_eq!(
        orig.summary.unwrap(),
        format!(
            "2 tests failed under load and passed alone: glare::one_call, shards::clock_ahead (retry: job #{})",
            retry.id.0
        )
    );
    assert!(
        retry_job(&r, retry.id).is_none(),
        "never a retry of the retry"
    );
}

// R-717
#[test]
fn r717_a_test_that_fails_again_alone_is_named_as_failing_again() {
    let r = Rig::new();
    // the same log, the same exit: both fail again
    let id = failing_test_job(&r, (FAILED_LOG, 101));
    r.until("the follow-up is queued", || retry_job(&r, id).is_some());
    r.until("the outcome is settled", || {
        r.tick();
        r.record(id)
            .summary
            .is_some_and(|s| s.starts_with("failed again alone"))
    });
    let text = r.record(id).summary.unwrap();
    assert!(
        text.starts_with("failed again alone: glare::one_call, shards::clock_ahead (retry: job #"),
        "{text}"
    );
    assert_eq!(r.record(id).phase, Phase::Failed { exit: Some(101) });
}

// R-718
#[test]
fn r718_no_retry_without_the_flag_without_a_failures_list_or_for_a_command_that_is_not_cargo_test()
{
    // no flag
    let r = Rig::new();
    r.procs.output(FAILED_LOG);
    r.procs.auto_exit(101);
    let plain = r.submit(&r.spec("o", &["cargo", "test"], Kind::Test));
    r.tick();
    r.until_done(plain);
    // a build error has no failures list
    let mut s = r.spec_in("o", &["cargo", "test"], Kind::Test, "b");
    s.retry_alone = true;
    r.procs.output(b"error: could not compile `x`\n");
    let build = r.submit(&s);
    r.tick();
    r.until_done(build);
    // not cargo test
    r.procs.output(FAILED_LOG);
    let mut s = r.spec_in("o", &["make", "check"], Kind::Test, "m");
    s.retry_alone = true;
    let make = r.submit(&s);
    r.tick();
    r.until_done(make);
    std::thread::sleep(std::time::Duration::from_millis(100));
    for id in [plain, build, make] {
        assert!(retry_job(&r, id).is_none(), "job {}", id.0);
        assert!(
            !r.record(id)
                .summary
                .unwrap_or_default()
                .contains("retrying")
        );
    }
}

// R-718
#[test]
fn r718_a_cancelled_job_is_not_retried() {
    let r = Rig::new();
    r.procs.output(FAILED_LOG);
    let mut s = r.spec("o", &["cargo", "test"], Kind::Test);
    s.retry_alone = true;
    let id = r.submit(&s);
    r.tick();
    r.until_running(id);
    r.sch.handle(Request::Cancel {
        id,
        who: "o".into(),
        reason: None,
    });
    r.until_done(id);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(matches!(
        r.phase(id),
        Phase::Cancelled | Phase::Failed { .. }
    ));
    assert!(retry_job(&r, id).is_none());
}

// R-716
#[test]
fn r716_a_job_that_failed_on_a_peer_is_retried_by_its_origin_from_the_mirrored_log() {
    use crate::forwarder::ForwardEnd;
    let f = Arc::new(FakeForwarder::new(vec![]));
    let r = Rig::with_forwarder(f.clone());
    let mut spec = anywhere(&r);
    spec.argv = ["cargo", "test", "-p", "p2pcore", "--test", "calls"]
        .map(String::from)
        .to_vec();
    spec.kind = Kind::Test;
    spec.retry_alone = true;
    let id = r.submit(&spec);
    r.store.set_forward(id, &nixos(), None, Some("k1")).unwrap();
    f.script(id, accepted(Phase::Running));
    r.until("started", || {
        r.tick();
        r.phase(id) == Phase::Running
    });
    // the forward mirrors the peer's log into the local one
    std::fs::create_dir_all(r.cfg.log_path(id).parent().unwrap()).unwrap();
    std::fs::write(r.cfg.log_path(id), FAILED_LOG).unwrap();
    f.script(
        id,
        ForwardPoll::Done(ForwardEnd {
            phase: Phase::Failed { exit: Some(101) },
            reason: "exit code 101 on nixos".into(),
            exit_code: Some(101),
            signal: None,
            remote_job: Some(1),
            nothing_submitted: false,
            samples: vec![],
            fetch: vec![],
        }),
    );
    r.until_done(id);
    r.until("the follow-up is queued", || retry_job(&r, id).is_some());
    let retry = retry_job(&r, id).unwrap();
    assert_eq!(
        retry.spec.placement, spec.placement,
        "retried where it was sent"
    );
    assert!(retry.spec.isolate && !retry.spec.retry_alone);
    assert!(retry.spec.argv.ends_with(&["--test-threads=1".to_string()]));
    assert_eq!(r.record(id).phase, Phase::Failed { exit: Some(101) });
}

// R-718
#[test]
fn r718_a_job_a_peer_sent_here_is_left_to_its_origin_to_retry() {
    let r = Rig::new();
    r.procs.output(FAILED_LOG);
    r.procs.auto_exit(101);
    let mut spec = r.spec("o", &["cargo", "test"], Kind::Test);
    spec.retry_alone = true;
    let mut job = tasks_ledger::NewJob::new(spec);
    job.origin = Some(tasks_ledger::Origin {
        peer: "laptop".into(),
        key: "laptop:1".into(),
    });
    let id = r.store.submit_job(&job).unwrap();
    r.tick();
    r.until_done(id);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(retry_job(&r, id).is_none());
}

/// The slot a running job holds now, as status shows it.
fn held_slot(r: &Rig, id: JobId) -> Option<(String, u32)> {
    r.sch
        .status()
        .running
        .iter()
        .find(|j| j.id == id)
        .and_then(|j| j.slot.clone())
}

const TESTS_STARTED: &[u8] = b"   Compiling p2pcore v0.1.0\n    Finished `release` profile [optimized] target(s) in 40m\n     Running tests/calls.rs (target/release/deps/calls-0123abcd)\n";

// R-760
#[test]
fn r760_a_cargo_test_gives_its_compile_slot_back_once_its_tests_run() {
    let r = Rig::with_policy("[slots]\ncompile = 1\n");
    r.procs.output(TESTS_STARTED);
    let test = r.submit(&r.spec_in("o", &["cargo", "test", "-p", "p2pcore"], Kind::Test, "t"));
    r.tick();
    r.until_running(test);
    r.until("the tests to start", || {
        r.log_text(test).contains("Running tests/calls.rs")
    });
    let build = r.submit(&r.spec_in("o", &["cargo", "build"], Kind::Compile, "b"));
    r.tick();
    r.until("the hand-over", || {
        held_slot(&r, test).is_some_and(|s| s.0 == "run")
    });
    r.tick();
    r.until_running(build);
    assert_eq!(held_slot(&r, build), Some(("compile".to_string(), 0)));
    assert!(
        r.log_text(test)
            .contains("compile slot 0 handed back, now on run slot 0"),
        "{}",
        r.log_text(test)
    );
    let st = r.sch.status();
    let used = |k: &str| {
        st.slots
            .iter()
            .find(|g| g.kind == k)
            .map(|g| g.total - g.free)
    };
    assert_eq!((used("compile"), used("run")), (Some(1), Some(1)));
}

// R-760
#[test]
fn r760_a_build_or_a_test_still_compiling_keeps_its_compile_slot() {
    let r = Rig::with_policy("[slots]\ncompile = 2\n");
    r.procs.output(b"   Compiling p2pcore v0.1.0\n");
    let test = r.submit(&r.spec_in("o", &["cargo", "test", "-p", "p2pcore"], Kind::Test, "t"));
    let build = r.submit(&r.spec_in("o", &["cargo", "build"], Kind::Compile, "b"));
    r.tick();
    r.until_running(test);
    r.until_running(build);
    r.until("the log", || r.log_text(test).contains("Compiling"));
    r.tick();
    r.tick();
    assert_eq!(
        held_slot(&r, test).map(|s| s.0),
        Some("compile".to_string())
    );
    assert_eq!(
        held_slot(&r, build).map(|s| s.0),
        Some("compile".to_string())
    );
}
