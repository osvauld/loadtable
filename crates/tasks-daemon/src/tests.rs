//! Scheduler, API and process tests. Names carry the regression ids they pin.

use crate::api::*;
use crate::holders::Holder;
use crate::platform::Platform;
use crate::server::{Daemon, DaemonOptions};
use crate::startup::reconcile;
use crate::testkit::{FakeForwarder, FakeHolders, FakeMachine, FakeProcs, Rig, TempDir};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tasks_core::plan::Plan;
use tasks_core::{Arch, JobId, Kind, Machine, MachineName, Os, Phase, Placement, SlotKind, Tool};
use tasks_ledger::{
    CancelOutcome, ChangeToken, Finish, JobRecord, LeaseRecord, LedgerError, ListFilter,
    ManualClock, MemoryStore, NewJob, PlanContext, PlanSubmitted, PruneReport, RetentionPolicy,
    StartInfo, Store,
};
use tasks_placement::{Checkout, Link, MachineView, Pressure};
use tasks_platform::{FlockSlots, Level, Signal, SlotLocks};
use tasks_timing::{History, RecipeKey, Sample, SampleKey, TimingError};

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

// ---------------------------------------------------------------- the basic flow

// R-116, R-231, R-009
#[test]
fn r116_a_job_runs_and_the_placement_line_is_written_once_at_the_top_of_its_log() {
    let r = Rig::new();
    r.procs.output(b"hello\n");
    r.procs.auto_exit(0);
    let id = r.submit(&r.spec("o", &["make", "all"], Kind::Run));
    let rep = r.tick();
    assert_eq!(rep.started, vec![id]);
    assert_eq!(r.until_done(id), Phase::Passed);
    let log = r.log_text(id);
    assert!(
        log.starts_with("[tasks] placement: testbox (default: stays on this machine)"),
        "{log}"
    );
    assert_eq!(log.matches("[tasks] placement:").count(), 1);
    assert!(log.contains("hello"));
    let rec = r.record(id);
    assert_eq!(rec.shown_exit(), Some(0));
    assert!(rec.lease.is_some() && rec.log_path.is_some());
    // R-231: the slot lock lives in the daemon's lock dir
    assert!(r.cfg.lock_dir.join("run-0.lock").exists());
    // the timing sample was written with the outcome
    let recipe = RecipeKey::from_argv(&["make".to_string(), "all".to_string()], &[])[0].to_string();
    let key = SampleKey {
        recipe,
        target: None,
        machine: "testbox".into(),
    };
    assert_eq!(r.store.samples(&key).unwrap().len(), 1);
    let est = ok(r.sch.handle(Request::Estimate {
        argv: vec!["make".into(), "all".into()],
        kind: None,
    }));
    assert!(matches!(est, Response::Estimate(e) if e.samples == 1));
}

#[test]
fn a_failing_exit_code_is_recorded_and_probes_need_no_slot() {
    let r = Rig::new();
    r.procs.auto_exit(3);
    let id = r.submit(&r.spec("o", &["make", "x"], Kind::Run));
    r.tick();
    assert_eq!(r.until_done(id), Phase::Failed { exit: Some(3) });
    let p = r.submit(&r.spec("o", &["true"], Kind::Probe));
    r.tick();
    r.until_done(p);
    assert!(r.record(p).phase.is_terminal());
}

// ---------------------------------------------------------------- R-230 and gates

// R-230: a waiter held by the checkout gate never blocks the slot queue for others.
#[test]
fn r230_a_same_checkout_waiter_does_not_block_other_checkouts() {
    let r = Rig::new();
    let a = r.submit(&r.spec_in("o", &["make", "a"], Kind::Compile, "x"));
    assert_eq!(r.tick().started, vec![a]);
    r.until_running(a);
    let b = r.submit(&r.spec_in("o", &["make", "b"], Kind::Compile, "x"));
    let c = r.submit(&r.spec_in("o", &["make", "c"], Kind::Compile, "y"));
    let rep = r.tick();
    assert_eq!(
        rep.started,
        vec![c],
        "the other checkout takes the free slot"
    );
    assert!(matches!(r.phase(b), Phase::Waiting(_)));
    assert!(r.record(b).wait_text.is_some());
    // when the checkout frees up, the waiter goes
    r.procs.exit(r.procs.pgid(0), 0);
    r.until("b running", || {
        r.tick();
        r.phase(b) == Phase::Running
    });
}

// R-230: one order for every rule, so two waiters never wait on each other.
#[test]
fn r230_two_waiters_on_one_checkout_use_queue_order_and_never_deadlock() {
    let r = Rig::new();
    let x1 = r.submit(&r.spec_in("o", &["make", "1"], Kind::Compile, "x"));
    let x2 = r.submit(&r.spec_in("o", &["make", "2"], Kind::Compile, "x"));
    let c = r.submit(&r.spec_in("o", &["make", "3"], Kind::Compile, "y"));
    let rep = r.tick();
    assert_eq!(rep.started, vec![x1, c]);
    assert!(matches!(r.phase(x2), Phase::Waiting(_)));
}

// R-005: a ledger error on one job never stops the others from being admitted.
#[test]
fn r005_a_transient_ledger_error_on_one_job_does_not_stop_the_others() {
    let r = Rig::new();
    let flaky = Arc::new(Flaky {
        inner: r.store.clone(),
        bad: std::sync::Mutex::new(None),
    });
    let sch = crate::Scheduler::new(crate::SchedulerParts {
        cfg: r.cfg.clone(),
        policy: crate::machine_policy::MachinePolicy::defaults(),
        ledger: flaky.clone(),
        platform: Platform {
            memory: r.machine.clone(),
            pressure: r.machine.clone(),
            procs: r.procs.clone(),
            disk: r.machine.clone(),
            locks: Arc::new(FlockSlots::new(&r.cfg.lock_dir)),
            holders: r.holders.clone(),
            cgroups: r.cgroups.clone(),
            table: r.table.clone(),
            power: r.power.clone(),
            sleep: r.power.clone(),
        },
        clock: r.clock.clone(),
        forwarder: Arc::new(crate::forwarder::NoForwarder),
        parent_env: vec![],
        execution_enabled: true,
        runner_poll: Duration::from_millis(2),
        transfer_gate: None,
    });
    let bad = r.submit(&r.spec("o", &["make", "bad"], Kind::Run));
    let good = r.submit(&r.spec("o", &["make", "good"], Kind::Run));
    *flaky.bad.lock().unwrap() = Some(bad);
    let rep = sch.tick();
    assert_eq!(rep.started, vec![good]);
    assert_eq!(r.phase(bad), Phase::Queued);
    let alerts = sch.status().alerts;
    assert!(alerts.iter().any(|a| a.id == "ledger-error"), "{alerts:?}");
}

// R-063: cancelling during the disk wait creates nothing.
#[test]
fn r063_cancel_during_the_disk_wait_creates_nothing() {
    let r = Rig::new();
    r.machine.set_disk(1 << 30);
    let id = r.submit(&r.spec("o", &["make", "x"], Kind::Compile));
    r.tick();
    let rec = r.record(id);
    assert!(
        matches!(rec.phase, Phase::Waiting(tasks_core::WaitReason::Disk)),
        "{rec:?}"
    );
    assert!(rec.wait_text.unwrap().contains("disk"));
    let reply = ok(r.sch.handle(Request::Cancel {
        id,
        who: "o".into(),
        reason: None,
    }));
    assert!(matches!(reply, Response::Cancelled { result, .. } if result == "cancelled"));
    assert_eq!(r.phase(id), Phase::Cancelled);
    assert!(!r.cfg.log_path(id).exists());
    assert_eq!(r.procs.spawn_count(), 0);
}

// R-018
#[test]
fn r018_drain_stops_claims_until_it_is_cancelled() {
    let r = Rig::new();
    ok(r.sch.handle(Request::Drain {
        cancel: false,
        who: None,
    }));
    let id = r.submit(&r.spec("o", &["make", "x"], Kind::Run));
    assert!(r.tick().started.is_empty());
    assert!(matches!(r.phase(id), Phase::Waiting(_)));
    assert_eq!(r.procs.spawn_count(), 0);
    ok(r.sch.handle(Request::Drain {
        cancel: true,
        who: None,
    }));
    assert_eq!(r.tick().started, vec![id]);
}

// R-017, R-098
#[test]
fn r017_r098_execution_intent_persists_and_compiling_never_enables_it() {
    let r = Rig::new();
    ok(r.sch.handle(Request::Execution { enabled: false }));
    assert_eq!(
        std::fs::read_to_string(r.cfg.intent_path()).unwrap().trim(),
        "stopped"
    );
    let id = r.submit(&r.spec("o", &["make", "x"], Kind::Compile));
    assert!(r.tick().started.is_empty());
    assert!(matches!(r.phase(id), Phase::Waiting(_)));
    let st = r.sch.status();
    assert!(
        !st.daemon.execution_enabled,
        "a compile job did not enable execution"
    );
    ok(r.sch.handle(Request::Execution { enabled: true }));
    assert_eq!(
        std::fs::read_to_string(r.cfg.intent_path()).unwrap().trim(),
        "enabled"
    );
    assert_eq!(r.tick().started, vec![id]);
}

// ---------------------------------------------------------------- cancel

// R-107, R-108
#[test]
fn r107_r108_cancel_is_owner_checked_and_stops_a_running_group() {
    let r = Rig::new();
    let id = r.submit(&r.spec("owner", &["make", "x"], Kind::Run));
    r.tick();
    r.until_running(id);
    let e = err(r.sch.handle(Request::Cancel {
        id,
        who: "intruder".into(),
        reason: None,
    }));
    assert_eq!(e.code, ErrorCode::NotOwner);
    assert_eq!(r.phase(id), Phase::Running);
    let reply = ok(r.sch.handle(Request::Cancel {
        id,
        who: "owner".into(),
        reason: None,
    }));
    assert!(matches!(reply, Response::Cancelled { result, .. } if result == "requested"));
    assert_eq!(r.until_done(id), Phase::Cancelled);
    assert!(r.procs.signals().contains(&(r.procs.pgid(0), Signal::Term)));
    assert_eq!(
        err(r.sch.handle(Request::Cancel {
            id: JobId(999),
            who: "x".into(),
            reason: None,
        }))
        .code,
        ErrorCode::NotFound
    );
}

// R-178 through the scheduler: dependents of a failed step are cancelled.
#[test]
fn dependents_of_failed_jobs_are_cancelled_and_plans_submit_in_one_call() {
    let r = Rig::new();
    r.procs.auto_exit(1);
    let plan = Plan::parse(
        r#"{"steps":[{"name":"a","argv":["make","a"]},{"name":"b","argv":["make","b"],"after":["a"]}]}"#,
    )
    .unwrap();
    let Response::PlanSubmitted { jobs, .. } = ok(r.sch.handle(Request::SubmitPlan {
        plan,
        owner: "o".into(),
        cwd: r.dir.path().to_path_buf(),
        env: vec![],
        placement: Placement::Here,
        broad_test: false,
        priority: Default::default(),
        priority_reason: None,
    })) else {
        panic!("plan expected");
    };
    let (a, b) = (jobs[0].1, jobs[1].1);
    r.tick();
    assert_eq!(r.until_done(a), Phase::Failed { exit: Some(1) });
    r.tick();
    assert_eq!(r.phase(b), Phase::Cancelled);
}

// ---------------------------------------------------------------- not started

#[test]
fn a_job_that_cannot_start_ends_not_started_with_the_reason() {
    let r = Rig::new();
    let mut s = r.spec("o", &["make", "x"], Kind::Run);
    s.requirements.tools.insert(Tool("no-such-tool-zz".into()));
    let id = r.submit(&s);
    let rep = r.tick();
    assert_eq!(rep.not_started, vec![id]);
    let Phase::NotStarted { reason } = r.phase(id) else {
        panic!("not started expected");
    };
    assert!(reason.contains("no-such-tool-zz"), "{reason}");

    r.procs.fail_spawn("boom");
    let id = r.submit(&r.spec("o", &["make", "y"], Kind::Run));
    r.tick();
    let Phase::NotStarted { reason } = r.until_done(id) else {
        panic!("not started expected");
    };
    assert!(reason.contains("boom"), "{reason}");
}

// ---------------------------------------------------------------- cargo -j

// R-092, R-093 at spawn, shown in the log
#[test]
fn r092_r093_cargo_jobs_are_capped_and_lowered_when_busy_and_shown_in_the_log() {
    let r = Rig::with_policy("[cargo]\nmax_jobs = 8\njobs_when_busy = 2\n");
    let a = r.submit(&r.spec_in("o", &["cargo", "build", "-j16"], Kind::Compile, "x"));
    r.tick();
    r.until_running(a);
    let b = r.submit(&r.spec_in("o", &["cargo", "build", "-j8"], Kind::Compile, "y"));
    r.tick();
    r.until_running(b);
    let spawned = r.procs.spawned();
    assert!(
        spawned[0].argv.contains(&"-j8".to_string()),
        "{:?}",
        spawned[0].argv
    );
    assert!(
        spawned[1].argv.contains(&"-j2".to_string()),
        "{:?}",
        spawned[1].argv
    );
    assert!(r.log_text(a).contains("cargo -j 16 lowered to 8"));
    assert!(r.log_text(b).contains("cargo -j 8 lowered to 2"));
}

// ---------------------------------------------------------------- watchdogs

#[test]
fn the_log_size_watchdog_kills_a_job_past_the_hard_cap_with_a_named_reason() {
    let r = Rig::with_policy("[logs]\nsoft_cap_mib = 1\nhard_cap_mib = 1\n");
    let mut big = "x".repeat(1_200_000).into_bytes();
    big.push(b'\n');
    r.procs.output(&big);
    let id = r.submit(&r.spec("o", &["make", "x"], Kind::Run));
    r.tick();
    r.until_running(id);
    let phase = r.until_done(id);
    assert!(matches!(phase, Phase::Failed { .. }), "{phase:?}");
    assert!(r.record(id).summary.unwrap().contains("hard cap"));
}

// R-095, R-096 through the scheduler
#[test]
fn r095_a_quiet_job_with_a_terminal_failure_line_and_flat_cpu_is_ended_as_stuck() {
    let r = Rig::with_policy("[stuck]\nseconds = 30\n");
    r.procs
        .output(b"java.lang.OutOfMemoryError: heap\nFAILURE: Build failed with an exception.\n");
    let id = r.submit(&r.spec("o", &["make", "x"], Kind::Run));
    r.tick();
    r.until_running(id);
    r.tick();
    r.clock.advance(31_000);
    r.tick();
    let phase = r.until_done(id);
    assert!(matches!(phase, Phase::Failed { .. }), "{phase:?}");
    assert!(r.record(id).summary.unwrap().contains("stuck"));
}

#[test]
fn r095_moving_cpu_or_ordinary_test_failures_never_end_a_job() {
    let r = Rig::with_policy("[stuck]\nseconds = 30\n");
    r.procs
        .output(b"thread 'x' panicked at src/lib.rs:1\ntest result: FAILED. 0 passed; 1 failed\n");
    let id = r.submit(&r.spec("o", &["make", "x"], Kind::Run));
    r.tick();
    r.until_running(id);
    for i in 1..=3 {
        r.clock.advance(31_000);
        r.tick();
        r.procs.set_cpu(r.procs.pgid(0), f64::from(i) * 10.0);
    }
    assert_eq!(r.phase(id), Phase::Running);
    r.procs.exit(r.procs.pgid(0), 0);
    assert_eq!(r.until_done(id), Phase::Passed);
}

// ---------------------------------------------------------------- alerts and holders

fn ended_job_with_group(r: &Rig, pgid: i32) -> JobId {
    let id = r.submit(&r.spec("o", &["make", "old"], Kind::Run));
    r.store
        .start(
            id,
            &StartInfo {
                machine: MachineName("testbox".into()),
                lease: LeaseRecord {
                    pgid,
                    leader_start: 7,
                },
                log_path: None,
            },
        )
        .unwrap();
    r.store
        .finish(id, &Finish::from_phase(Phase::Failed { exit: Some(1) }))
        .unwrap();
    id
}

// R-047, R-048, R-049, R-050, R-221, R-222
#[test]
fn r222_a_slot_held_with_no_live_job_alerts_after_the_grace_naming_the_holder() {
    let r = Rig::new();
    let old = ended_job_with_group(&r, 4000);
    r.procs.survivor(4000, 7);
    *r.holders.0.lock().unwrap() = vec![Holder {
        pid: 4242,
        command: "sleep 99".into(),
        origin_job: Some(old),
    }];
    let _held = FlockSlots::new(&r.cfg.lock_dir)
        .try_acquire(SlotKind::Compile, 0)
        .unwrap()
        .unwrap();
    r.tick();
    assert!(
        r.sch
            .status()
            .alerts
            .iter()
            .all(|a| !a.id.starts_with("slot-held"))
    );
    // R-049: the audit stopped the orphan of the ended job
    assert!(r.procs.signals().contains(&(4000, Signal::Term)));
    // R-050: the busy slot is listed as external work, not "no live jobs"
    let st = r.sch.status();
    let compile = &st.slots[0];
    assert_eq!(compile.free, 1);
    assert!(compile.busy[0].external);
    let who = compile.busy[0].holder.clone().unwrap();
    assert!(
        who.contains("pid 4242") && who.contains("which has ended"),
        "{who}"
    );
    r.clock.advance(121_000);
    r.tick();
    let alerts = r.sch.status().alerts;
    let a = alerts
        .iter()
        .find(|a| a.id == "slot-held-compile-0")
        .expect("alert");
    assert!(a.message.contains("pid 4242"), "{}", a.message);
    // R-221: it clears by itself
    drop(_held);
    r.tick();
    assert!(
        r.sch
            .status()
            .alerts
            .iter()
            .all(|a| a.id != "slot-held-compile-0")
    );
}

// R-049: an untracked holder is named and never signalled.
#[test]
fn r049_untracked_holders_are_named_and_never_signalled() {
    let r = Rig::new();
    *r.holders.0.lock().unwrap() = vec![Holder {
        pid: 4343,
        command: "mystery".into(),
        origin_job: None,
    }];
    let _held = FlockSlots::new(&r.cfg.lock_dir)
        .try_acquire(SlotKind::Run, 1)
        .unwrap()
        .unwrap();
    let lines = r.sch.audit_holders(&r.sch.policy());
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("UNTRACKED") && lines[0].contains("run-1"),
        "{lines:?}"
    );
    assert!(r.procs.signals().is_empty());
}

// R-220
#[test]
fn r220_disk_alerts_warn_below_twice_the_reserve_and_go_critical_below_it() {
    let r = Rig::new();
    let level = |r: &Rig| {
        r.sch
            .status()
            .alerts
            .into_iter()
            .find(|a| a.id == "disk-cache")
            .map(|a| a.level)
    };
    r.tick();
    assert_eq!(level(&r), None, "ample space raises nothing");
    r.machine.set_disk(30 << 30);
    r.tick();
    assert_eq!(level(&r).as_deref(), Some("warn"));
    r.machine.set_disk(10 << 30);
    r.tick();
    assert_eq!(level(&r).as_deref(), Some("critical"));
    r.machine.set_disk(100 << 30);
    r.tick();
    assert_eq!(level(&r), None);
}

// R-642
#[test]
fn r642_the_daemon_reports_the_memory_stall_limit_where_it_measures_stalls() {
    let r = Rig::new();
    // a machine without PSI (the fake, like macOS) has no stall figure and so no limit
    assert_eq!(r.sch.status().pressure.memory_stall_limit, None);
    r.machine.set_memory_some(Some(3.0));
    let limit = r.sch.status().pressure.memory_stall_limit;
    assert_eq!(limit, Some(r.sch.policy().psi().memory_some_warn));
    assert!(limit.is_some_and(|l| l > 0.0));
}

// R-222
#[test]
fn r222_memory_pressure_must_last_before_it_alerts() {
    let r = Rig::new();
    r.machine.set_memory_level(Level::Warn);
    r.tick();
    assert!(
        r.sch
            .status()
            .alerts
            .iter()
            .all(|a| a.id != "memory-pressure")
    );
    r.clock.advance(61_000);
    r.tick();
    assert!(
        r.sch
            .status()
            .alerts
            .iter()
            .any(|a| a.id == "memory-pressure")
    );
    r.machine.set_memory_level(Level::Normal);
    r.tick();
    assert!(
        r.sch
            .status()
            .alerts
            .iter()
            .all(|a| a.id != "memory-pressure")
    );
}

// ---------------------------------------------------------------- status, list, logs

// R-129, R-050: queued rows show position, reason and start estimate.
#[test]
fn r129_status_shows_waiters_with_full_text_position_and_start_estimate() {
    let r = Rig::with_policy("[slots]\ncompile = 1\n");
    let a = r.submit(&r.spec_in("o", &["make", "a"], Kind::Compile, "a"));
    r.tick();
    r.until_running(a);
    let b = r.submit(&r.spec_in("p", &["make", "b"], Kind::Compile, "b"));
    let c = r.submit(&r.spec_in("q", &["make", "c"], Kind::Compile, "c"));
    r.tick();
    let st = r.sch.status();
    assert_eq!(st.running.len(), 1);
    assert_eq!(st.running[0].id, a);
    assert_eq!(st.slots[0].busy[0].job, Some(a));
    assert_eq!(st.slots[0].free, 0);
    let ids: Vec<(JobId, usize)> = st.waiting.iter().map(|w| (w.id, w.position)).collect();
    assert_eq!(ids, vec![(b, 1), (c, 2)]);
    assert!(
        st.waiting[0].wait_text.contains(&format!("#{}", a.0)),
        "{}",
        st.waiting[0].wait_text
    );
    let s0 = st.waiting[0].start_estimate_secs.unwrap();
    let s1 = st.waiting[1].start_estimate_secs.unwrap();
    assert!(s1 > s0, "{s0} {s1}");
    assert!(st.waiting[0].rough, "no history yet");
}

// R-027: replies stay small and list is bounded.
#[test]
fn r027_hot_replies_are_small_and_list_is_bounded() {
    let r = Rig::new();
    let st = serde_json::to_string(&r.sch.status()).unwrap();
    assert!(st.len() < 8 * 1024, "{}", st.len());
    for i in 0..3 {
        r.submit(&r.spec("o", &["make", &i.to_string()], Kind::Run));
    }
    let Response::Jobs { jobs, truncated } = ok(r.sch.handle(Request::List {
        owner: None,
        active_only: false,
        recent_finished: None,
        limit: Some(2),
    })) else {
        panic!("jobs expected");
    };
    assert_eq!((jobs.len(), truncated), (2, true));
    for bad in [0, MAX_LIST_ROWS + 1] {
        assert_eq!(
            err(r.sch.handle(Request::List {
                owner: None,
                active_only: false,
                recent_finished: None,
                limit: Some(bad)
            }))
            .code,
            ErrorCode::BadRequest
        );
    }
}

fn logs_rig() -> (Rig, JobId, JobId) {
    let r = Rig::new();
    r.procs
        .output(b"alpha\nERROR one\nbeta\ngamma\nERROR two\n");
    r.procs.auto_exit(0);
    let a = r.submit(&r.spec("alice", &["make", "a"], Kind::Run));
    r.tick();
    r.until_done(a);
    let b = r.submit(&r.spec("bob", &["make", "b"], Kind::Run));
    r.tick();
    r.until_done(b);
    (r, a, b)
}

// R-112
#[test]
fn r112_log_requests_grep_tail_and_follow() {
    let (r, a, _) = logs_rig();
    let Response::LogGrep {
        groups, matches, ..
    } = ok(r.sch.handle(Request::Logs {
        id: a,
        view: LogView::Grep {
            pattern: "ERROR".into(),
            context: 1,
            regex: false,
            max_matches: None,
        },
    }))
    else {
        panic!("grep expected");
    };
    assert_eq!(matches, 2);
    assert!(!groups.is_empty() && groups.iter().any(|g| g.lines.iter().any(|l| l.matched)));
    let e = err(r.sch.handle(Request::Logs {
        id: a,
        view: LogView::Grep {
            pattern: "a(".into(),
            context: 0,
            regex: true,
            max_matches: None,
        },
    }));
    assert_eq!(e.code, ErrorCode::BadRequest);
    let Response::LogTail { lines, running, .. } = ok(r.sch.handle(Request::Logs {
        id: a,
        view: LogView::Tail { lines: 2 },
    })) else {
        panic!("tail expected");
    };
    assert_eq!(lines.last().map(String::as_str), Some("ERROR two"));
    assert!(!running);
    let Response::LogChunk {
        data,
        next_offset,
        done,
    } = ok(r.sch.handle(Request::Logs {
        id: a,
        view: LogView::Follow {
            offset: 0,
            max_bytes: None,
        },
    }))
    else {
        panic!("chunk expected");
    };
    assert!(data.contains("alpha") && done);
    assert_eq!(next_offset as usize, data.len());
}

// R-114
#[test]
fn r114_cross_job_search_filters_by_owner_phase_ids_and_time_newest_first() {
    let (r, a, b) = logs_rig();
    let search = |q: LogSearchQuery| match ok(r.sch.handle(Request::LogSearch { query: q })) {
        Response::LogSearch {
            results,
            matches,
            truncated,
        } => (results, matches, truncated),
        other => panic!("{other:?}"),
    };
    let base = || LogSearchQuery {
        pattern: "ERROR".into(),
        ..LogSearchQuery::default()
    };
    let (res, matches, _) = search(base());
    assert_eq!(
        res.iter().map(|g| g.id).collect::<Vec<_>>(),
        vec![b, a],
        "newest first"
    );
    assert_eq!(matches, 4);
    let (res, ..) = search(LogSearchQuery {
        owner: Some("alice".into()),
        ..base()
    });
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].id, a);
    let (res, ..) = search(LogSearchQuery {
        phase: Some("failed".into()),
        ..base()
    });
    assert!(res.is_empty());
    let (res, ..) = search(LogSearchQuery {
        ids: vec![b],
        ..base()
    });
    assert_eq!(res.len(), 1);
    let (res, ..) = search(LogSearchQuery {
        since: Some("1h".into()),
        ..base()
    });
    assert_eq!(res.len(), 2);
    let (res, ..) = search(LogSearchQuery {
        until: Some("1h".into()),
        ..base()
    });
    assert!(res.is_empty());
    let (_, matches, truncated) = search(LogSearchQuery {
        max_matches: Some(1),
        ..base()
    });
    assert_eq!((matches, truncated), (1, true));
    let (res, ..) = search(LogSearchQuery {
        regex: true,
        pattern: "ERROR t.o$".into(),
        ..base()
    });
    assert_eq!(res.len(), 2);
    let e = err(r.sch.handle(Request::LogSearch {
        query: LogSearchQuery {
            since: Some("soon".into()),
            ..base()
        },
    }));
    assert_eq!(e.code, ErrorCode::BadRequest);
}

#[test]
fn suites_expand_from_the_repo_policy_and_unknown_ones_are_refused() {
    let r = Rig::new();
    let repo = r.dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join(".tasks")).unwrap();
    std::fs::write(
        repo.join(".tasks/run-policy.toml"),
        "version = 1\n[suites.s]\n[[suites.s.steps]]\nname=\"a\"\ncommand=[\"true\"]\n",
    )
    .unwrap();
    let req = |name: &str| Request::SubmitSuite {
        name: name.into(),
        owner: "o".into(),
        cwd: repo.clone(),
        env: vec![],
        placement: Placement::Here,
        quick: false,
        executables: Default::default(),
        broad_test: false,
        priority: Default::default(),
        priority_reason: None,
    };
    let Response::PlanSubmitted { jobs, .. } = ok(r.sch.handle(req("s"))) else {
        panic!("plan expected");
    };
    assert_eq!(jobs.len(), 1);
    assert_eq!(err(r.sch.handle(req("nope"))).code, ErrorCode::Refused);
}

// ---------------------------------------------------------------- forwarding

pub(crate) fn peer_view() -> MachineView {
    MachineView {
        machine: Machine {
            name: MachineName("nixos".into()),
            os: Os::current(),
            arch: Arch::current(),
            cores: 8,
            memory_bytes: 16 << 30,
            tools: Default::default(),
            devices: Default::default(),
        },
        is_local: false,
        link: Link::Up { age_secs: 1 },
        free_slots: [(SlotKind::Compile, 2)].into(),
        queue_depth: 0,
        expected_wait_secs: None,
        pressure: Pressure::default(),
        estimate: None,
        checkout: Checkout {
            synced: true,
            sync_bytes: 0,
        },
        marks: vec![],
    }
}

#[test]
fn a_peer_decision_forwards_the_job_and_records_it_instead_of_running_it() {
    let f = Arc::new(FakeForwarder::new(vec![peer_view()]));
    let r = Rig::with_forwarder(f.clone());
    let mut s = r.spec("o", &["make", "x"], Kind::Compile);
    s.placement = Placement::Peer(MachineName("nixos".into()));
    let id = r.submit(&s);
    assert_eq!(r.tick().forwarded, vec![id]);
    let fw = r.record(id).forward.unwrap();
    // registered only: the peer has not been asked yet, but the key is already persisted
    assert_eq!((fw.peer.0.as_str(), fw.remote_job), ("nixos", None));
    assert_eq!(
        fw.remote_key.as_deref(),
        Some(format!("testbox:{}", id.0).as_str())
    );
    assert_eq!(r.procs.spawn_count(), 0);
    r.tick();
    assert_eq!(f.started.lock().unwrap().clone(), vec![id]);
    assert_eq!(
        r.procs.spawn_count(),
        0,
        "a forwarded job is not scheduled locally"
    );
    // a failing forward waits and retries rather than losing the job
    *f.fail.lock().unwrap() = true;
    let id2 = r.submit(&s);
    r.tick();
    assert!(r.record(id2).wait_text.unwrap().contains("link down"));
    // an unknown peer is refused, named
    let r2 = Rig::new();
    let mut s = r2.spec("o", &["make", "x"], Kind::Compile);
    s.placement = Placement::Peer(MachineName("ghost".into()));
    let id = r2.submit(&s);
    r2.tick();
    let Phase::NotStarted { reason } = r2.phase(id) else {
        panic!("not started expected");
    };
    assert!(reason.contains("ghost"), "{reason}");
}

// ---------------------------------------------------------------- startup (R-008)

#[test]
fn r008_startup_terminates_surviving_groups_and_marks_every_running_job_interrupted() {
    let r = Rig::new();
    let mk = |pgid: i32| {
        let id = r.submit(&r.spec("o", &["make", "x"], Kind::Run));
        r.store
            .start(
                id,
                &StartInfo {
                    machine: MachineName("testbox".into()),
                    lease: LeaseRecord {
                        pgid,
                        leader_start: 7,
                    },
                    log_path: None,
                },
            )
            .unwrap();
        id
    };
    let alive = mk(5000);
    let gone = mk(5001);
    let queued = r.submit(&r.spec("o", &["make", "q"], Kind::Run));
    r.procs.survivor(5000, 7);
    let rep = reconcile(&*r.store, &*r.procs, Duration::from_secs(1), &|_| {}).unwrap();
    assert_eq!(rep.terminated, vec![alive]);
    assert_eq!(rep.interrupted, vec![alive, gone]);
    assert_eq!(r.phase(alive), Phase::Interrupted);
    assert_eq!(r.phase(gone), Phase::Interrupted);
    assert_eq!(
        r.phase(queued),
        Phase::Queued,
        "nothing is re-run or re-queued"
    );
    assert_eq!(r.procs.signals(), vec![(5000, Signal::Term)]);
    assert_eq!(r.procs.spawn_count(), 0);
}

// ---------------------------------------------------------------- real processes

// R-009, R-116 with real process groups.
#[test]
fn a_real_process_runs_and_its_exit_code_and_output_are_recorded() {
    let r = Rig::native();
    let id = r.submit(&r.spec("o", &["sh", "-c", "printf hello; exit 3"], Kind::Run));
    r.tick();
    assert_eq!(r.until_done(id), Phase::Failed { exit: Some(3) });
    assert!(r.log_text(id).contains("hello"));
    assert!(r.log_text(id).starts_with("[tasks] placement:"));
}

// R-107 with a real process group.
#[test]
fn cancelling_a_real_running_job_stops_its_group_quickly() {
    let r = Rig::native();
    let id = r.submit(&r.spec("o", &["sh", "-c", "sleep 30"], Kind::Run));
    r.tick();
    r.until_running(id);
    let t0 = Instant::now();
    ok(r.sch.handle(Request::Cancel {
        id,
        who: "o".into(),
        reason: None,
    }));
    assert_eq!(r.until_done(id), Phase::Cancelled);
    assert!(t0.elapsed() < Duration::from_secs(3));
}

// ---------------------------------------------------------------- the socket server

pub(crate) fn daemon_options(dir: &TempDir, procs: &Arc<FakeProcs>) -> DaemonOptions {
    let cfg = crate::config::DaemonConfig {
        state_dir: dir.path().join("state"),
        config_dir: dir.path().join("config"),
        lock_dir: dir.path().join("state/slots"),
        cache_dir: dir.path().join("cache"),
        machine: "testbox".into(),
    };
    let machine = Arc::new(FakeMachine::new());
    let power = Arc::new(tasks_platform::fake::FakePower::default());
    let platform = Platform {
        memory: machine.clone(),
        pressure: machine.clone(),
        procs: procs.clone(),
        disk: machine,
        locks: Arc::new(FlockSlots::new(&cfg.lock_dir)),
        holders: Arc::new(FakeHolders::default()),
        cgroups: Arc::new(tasks_platform::fake::FakeCgroups::unavailable("test")),
        table: Arc::new(tasks_platform::fake::FakePlatform::default()),
        power: power.clone(),
        sleep: power,
    };
    let mut o = DaemonOptions::new(cfg);
    o.env = vec![("PATH".into(), "/usr/bin:/bin".into())];
    o.platform = Some(platform);
    o
}

pub(crate) fn call(sock: &Path, req: &Request) -> Reply {
    let line = serde_json::to_string(&RequestEnvelope {
        v: PROTOCOL_VERSION,
        request: req.clone(),
    })
    .unwrap();
    raw_call(sock, line.as_bytes())
}

fn raw_call(sock: &Path, line: &[u8]) -> Reply {
    let mut s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(line).unwrap();
    s.write_all(b"\n").unwrap();
    let mut out = String::new();
    BufReader::new(s).read_line(&mut out).unwrap();
    serde_json::from_str::<ReplyEnvelope>(&out).unwrap().reply
}

fn wait_phase(sock: &Path, id: JobId, want: impl Fn(&Phase) -> bool) {
    let t0 = Instant::now();
    loop {
        // the submit of a vanished client may not have landed yet: NotFound just retries
        if let Reply::Ok(Response::Job { job }) = call(sock, &Request::Get { id })
            && want(&job.phase)
        {
            return;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "timed out waiting for the job"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_socket_serves_requests_refuses_bad_ones_and_survives_a_closed_client() {
    let dir = TempDir::new("sock");
    let procs = Arc::new(FakeProcs::new());
    let opts = daemon_options(&dir, &procs);
    let cfg = opts.config.clone();
    let d = Daemon::start(opts).unwrap();
    // one daemon per state dir: a second start is refused with the holder's pid
    match Daemon::start(daemon_options(&dir, &procs)) {
        Err(crate::DaemonError::AlreadyRunning { pid, .. }) => {
            assert_eq!(pid, Some(std::process::id()));
        }
        other => panic!("expected a refusal, got {:?}", other.err()),
    }
    let sock = cfg.socket_path();
    let handle = std::thread::spawn(move || d.run());

    // R-231: clients learn the lock dir from the daemon
    let Response::Hello(h) = ok(call(&sock, &Request::Hello)) else {
        panic!("hello expected");
    };
    assert_eq!(h.lock_dir, cfg.lock_dir);
    assert_eq!(h.protocol, PROTOCOL_VERSION);
    // named refusals
    let bad = raw_call(&sock, br#"{"v":99,"request":{"op":"status"}}"#);
    assert_eq!(err(bad).code, ErrorCode::UnsupportedVersion);
    assert_eq!(err(raw_call(&sock, b"{nope")).code, ErrorCode::BadRequest);
    let huge = vec![b'x'; MAX_REQUEST_BYTES + 10];
    assert_eq!(err(raw_call(&sock, &huge)).code, ErrorCode::TooLarge);

    // R-104, R-105, R-106: a client that submits and vanishes changes nothing
    let spec = tasks_core::JobSpec {
        owner: tasks_core::Owner::new("o").unwrap(),
        argv: vec!["make".into(), "x".into()],
        cwd: dir.path().to_path_buf(),
        env: vec![],
        kind: Kind::Run,
        requirements: Default::default(),
        placement: Placement::Here,
        after: vec![],
        timeout_secs: None,
        broad_test_escape: false,
        priority: tasks_core::Priority::Normal,
        priority_reason: None,
        fetch: Vec::new(),
    };
    {
        let mut s = UnixStream::connect(&sock).unwrap();
        let line = serde_json::to_string(&RequestEnvelope {
            v: PROTOCOL_VERSION,
            request: Request::Submit { spec },
        })
        .unwrap();
        writeln!(s, "{line}").unwrap();
    } // dropped without reading the reply
    wait_phase(&sock, JobId(1), |p| *p == Phase::Running);
    // shutdown is refused while a job runs
    assert_eq!(err(call(&sock, &Request::Shutdown)).code, ErrorCode::Busy);
    procs.exit(procs.pgid(0), 0);
    wait_phase(&sock, JobId(1), |p| *p == Phase::Passed);
    let Response::Status(st) = ok(call(&sock, &Request::Status)) else {
        panic!("status expected");
    };
    assert_eq!(st.daemon.lock_dir, cfg.lock_dir);
    ok(call(&sock, &Request::Shutdown));
    handle.join().unwrap().unwrap();
    assert!(!sock.exists(), "the socket is removed on shutdown");
}

#[test]
fn startup_reconciles_the_ledger_and_honours_the_persisted_execution_intent() {
    let dir = TempDir::new("startup");
    let procs = Arc::new(FakeProcs::new());
    let opts = daemon_options(&dir, &procs);
    let cfg = opts.config.clone();
    cfg.create_dirs().unwrap();
    // a previous daemon left a running job and a stopped execution intent
    let old = {
        let store = tasks_ledger::SqliteStore::open(&cfg.db_path()).unwrap();
        let spec = tasks_core::JobSpec {
            owner: tasks_core::Owner::new("o").unwrap(),
            argv: vec!["make".into()],
            cwd: dir.path().to_path_buf(),
            env: vec![],
            kind: Kind::Run,
            requirements: Default::default(),
            placement: Placement::Here,
            after: vec![],
            timeout_secs: None,
            broad_test_escape: false,
            priority: tasks_core::Priority::Normal,
            priority_reason: None,
            fetch: Vec::new(),
        };
        let id = store.submit(&spec).unwrap();
        store
            .start(
                id,
                &StartInfo {
                    machine: MachineName("testbox".into()),
                    lease: LeaseRecord {
                        pgid: 9999,
                        leader_start: 1,
                    },
                    log_path: None,
                },
            )
            .unwrap();
        id
    };
    std::fs::write(cfg.intent_path(), "stopped\n").unwrap();
    let d = Daemon::start(opts).unwrap();
    let sch = d.scheduler().clone();
    assert_eq!(d.startup.interrupted, vec![old]);
    let rec = sch.ledger.get(old).unwrap().unwrap();
    assert_eq!(rec.phase, Phase::Interrupted, "never re-run");
    assert!(
        !sch.status().daemon.execution_enabled,
        "a stop sticks across restarts"
    );
}

// keep imports used by the Flaky wrapper below honest
#[allow(dead_code)]
fn _types(_: ManualClock, _: ChangeToken) {}

// ---------------------------------------------------------------- a store that fails on demand

struct Flaky {
    inner: Arc<MemoryStore>,
    bad: std::sync::Mutex<Option<JobId>>,
}

impl Store for Flaky {
    fn submit_job(&self, job: &NewJob) -> tasks_ledger::Result<JobId> {
        self.inner.submit_job(job)
    }
    fn submit_plan(&self, ctx: &PlanContext, plan: &Plan) -> tasks_ledger::Result<PlanSubmitted> {
        self.inner.submit_plan(ctx, plan)
    }
    fn get(&self, id: JobId) -> tasks_ledger::Result<Option<JobRecord>> {
        if *self.bad.lock().unwrap() == Some(id) {
            return Err(LedgerError::Unavailable {
                waited: Duration::from_secs(1),
            });
        }
        self.inner.get(id)
    }
    fn set_phase(&self, id: JobId, next: Phase) -> tasks_ledger::Result<()> {
        self.inner.set_phase(id, next)
    }
    fn list(&self, filter: &ListFilter) -> tasks_ledger::Result<Vec<JobRecord>> {
        self.inner.list(filter)
    }
    fn ready(&self) -> tasks_ledger::Result<Vec<JobId>> {
        self.inner.ready()
    }
    fn blocked_by_failure(&self) -> tasks_ledger::Result<Vec<JobId>> {
        self.inner.blocked_by_failure()
    }
    fn set_wait(
        &self,
        id: JobId,
        reason: tasks_core::WaitReason,
        text: &str,
    ) -> tasks_ledger::Result<()> {
        self.inner.set_wait(id, reason, text)
    }
    fn start(&self, id: JobId, info: &StartInfo) -> tasks_ledger::Result<()> {
        self.inner.start(id, info)
    }
    fn finish_recording(
        &self,
        id: JobId,
        outcome: &Finish,
        sample: Option<(&SampleKey, &Sample)>,
    ) -> tasks_ledger::Result<()> {
        self.inner.finish_recording(id, outcome, sample)
    }
    fn request_cancel_with(
        &self,
        id: JobId,
        by: &str,
        reason: Option<&str>,
    ) -> tasks_ledger::Result<CancelOutcome> {
        self.inner.request_cancel_with(id, by, reason)
    }
    fn set_forward(
        &self,
        id: JobId,
        peer: &MachineName,
        remote_job: Option<i64>,
        remote_key: Option<&str>,
    ) -> tasks_ledger::Result<()> {
        self.inner.set_forward(id, peer, remote_job, remote_key)
    }
    fn set_priority(
        &self,
        id: JobId,
        priority: tasks_core::Priority,
        reason: Option<&str>,
    ) -> tasks_ledger::Result<tasks_core::Priority> {
        self.inner.set_priority(id, priority, reason)
    }
    fn clear_forward(&self, id: JobId) -> tasks_ledger::Result<()> {
        self.inner.clear_forward(id)
    }
    fn find_origin(&self, peer: &str, key: &str) -> tasks_ledger::Result<Option<JobId>> {
        self.inner.find_origin(peer, key)
    }
    fn set_remote_phase(&self, id: JobId, phase: &Phase) -> tasks_ledger::Result<()> {
        self.inner.set_remote_phase(id, phase)
    }
    fn set_log_path(&self, id: JobId, path: &Path) -> tasks_ledger::Result<()> {
        self.inner.set_log_path(id, path)
    }
    fn set_figures(&self, id: JobId, figures: &tasks_core::JobFigures) -> tasks_ledger::Result<()> {
        self.inner.set_figures(id, figures)
    }
    fn find_recent_spec(
        &self,
        owner: &str,
        cwd: &Path,
        argv: &[String],
        within: Duration,
    ) -> tasks_ledger::Result<Vec<JobRecord>> {
        self.inner.find_recent_spec(owner, cwd, argv, within)
    }
    fn record_sample(&self, key: &SampleKey, sample: &Sample) -> tasks_ledger::Result<()> {
        self.inner.record_sample(key, sample)
    }
    fn sample_keys(
        &self,
        recipes: &[String],
        limit: usize,
    ) -> tasks_ledger::Result<Vec<SampleKey>> {
        self.inner.sample_keys(recipes, limit)
    }
    fn prune(&self, policy: &RetentionPolicy) -> tasks_ledger::Result<PruneReport> {
        self.inner.prune(policy)
    }
    fn change_token(&self) -> tasks_ledger::Result<ChangeToken> {
        self.inner.change_token()
    }
}

impl History for Flaky {
    fn samples(&self, key: &SampleKey) -> Result<Vec<Sample>, TimingError> {
        self.inner.samples(key)
    }
    fn targets(&self, recipe: &str) -> Result<Vec<String>, TimingError> {
        self.inner.targets(recipe)
    }
}
