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
use tasks_timing::{Cache, Outcome, RecipeKey, Sample, SampleKey};

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
