//! Shared behavioural suite run against every `Store` implementation. Each case is named after
//! the regression row (`r0xx`) it covers.

use crate::{
    CancelOutcome, Finish, LeaseRecord, LedgerError, ListFilter, ManualClock, NewJob, Origin,
    PlanContext, RetentionPolicy, StartInfo, Store, TimeoutReason,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tasks_core::plan::{Plan, Step};
use tasks_core::{
    JobId, JobSpec, Kind, MachineName, Owner, Phase, Placement, Requirements, StepHints, WaitReason,
};
use tasks_timing::{Cache, History, Outcome, Sample, SampleKey};

/// A store that is also a timing history.
pub(crate) trait Full: Store + History {}
impl<T: Store + History> Full for T {}

/// A fresh store plus the clock it reads and anything that must outlive it.
pub(crate) struct Env {
    pub store: Box<dyn Full>,
    pub clock: Arc<ManualClock>,
    pub _guard: Box<dyn std::any::Any>,
}

type Case = (&'static str, fn(&Env));

const CASES: &[Case] = &[
    ("lifecycle_and_dependencies", lifecycle_and_dependencies),
    ("r011_malformed_submission_creates_no_row", r011_malformed),
    ("r012_queue_is_fifo", r012_fifo),
    ("r008_restart_marks_running_jobs_interrupted", r008_restart),
    (
        "r006_cancel_wins_and_terminal_rows_are_immutable",
        r006_cancel_wins,
    ),
    ("r007_wait_text_never_drops_a_cancel", r007_no_lost_update),
    ("r335_figures_are_kept_with_the_job", r335_figures),
    ("r009_only_passed_reports_exit_zero", r009_exit_zero),
    (
        "start_and_finish_record_the_full_outcome",
        start_finish_record,
    ),
    ("wait_text_sits_beside_the_coarse_reason", wait_text),
    ("r12_cancel_of_unstarted_job_never_starts", cancel_unstarted),
    (
        "r002_cancel_is_visible_at_once_and_unknown_is_an_error",
        r002_cancel_visible,
    ),
    (
        "r099_find_recent_spec_returns_all_exact_matches",
        r099_recent_spec,
    ),
    ("r177_plan_is_atomic_with_durable_dependencies", r177_plan),
    (
        "r177_a_job_waits_for_all_of_its_dependencies",
        r177_all_deps,
    ),
    ("r178_blocked_rule_and_cascade_cancel", r178_blocked),
    ("r179_bad_dependency_lists_are_refused", r179_bad_deps),
    ("timing_samples_round_trip", samples_round_trip),
    ("r013_completion_and_sample_are_atomic", r013_atomic),
    (
        "r015_remote_state_never_changes_the_local_phase",
        r015_remote,
    ),
    (
        "r152_forward_key_is_kept_and_an_origin_is_unique",
        r152_forward_key_and_origin,
    ),
    (
        "r095_a_plan_steps_timeout_hint_fills_the_spec_and_the_escape_is_carried",
        r095_plan_timeout_fills_spec,
    ),
    ("sample_keys_enumerate_history", sample_keys_enumerate),
    (
        "r108_a_cancel_keeps_its_requester_and_reason",
        r108_cancel_reason,
    ),
    (
        "r186_placement_waits_are_their_own_reason",
        r186_placement_wait,
    ),
    (
        "r339_set_priority_changes_a_queued_job_and_refuses_a_started_one",
        r339_set_priority,
    ),
    (
        "r340_a_plans_background_hint_and_the_submitters_class_reach_the_specs",
        r340_plan_priority,
    ),
    (
        "r342_a_finished_job_leaves_its_peak_memory_beside_its_timing_sample",
        r342_peak_memory,
    ),
    ("r022_r026_list_filters", list_filters),
    ("retention_prunes_only_old_finished_work", retention),
    ("r025_change_token_moves_on_commit", change_token),
];

/// Runs every case on a fresh store from `make`.
pub(crate) fn run_all(make: &dyn Fn() -> Env) {
    for (name, case) in CASES {
        eprintln!("suite case {name}");
        case(&make());
    }
}

fn spec(owner: &str, after: Vec<JobId>) -> JobSpec {
    JobSpec {
        owner: Owner::new(owner).unwrap(),
        argv: vec!["true".into()],
        cwd: PathBuf::from("/tmp"),
        env: vec![],
        kind: Kind::Test,
        requirements: Requirements::default(),
        placement: Placement::Anywhere,
        after,
        timeout_secs: None,
        broad_test_escape: false,
        priority: tasks_core::Priority::Normal,
        priority_reason: None,
        fetch: Vec::new(),
    }
}

fn info() -> StartInfo {
    StartInfo {
        machine: MachineName("nixos".into()),
        lease: LeaseRecord {
            pgid: 4242,
            leader_start: 99,
        },
        log_path: Some(PathBuf::from("/logs/1.txt")),
    }
}

fn run_to(s: &dyn Store, id: JobId, end: Phase) {
    s.start(id, &info()).unwrap();
    s.finish(id, &Finish::from_phase(end)).unwrap();
}

fn step(name: &str, after: &[&str]) -> Step {
    Step {
        name: name.into(),
        argv: vec!["true".into()],
        after: after.iter().map(|s| s.to_string()).collect(),
        kind: Kind::Compile,
        requirements: Requirements::default(),
        hints: StepHints::default(),
    }
}

fn ctx() -> PlanContext {
    PlanContext {
        owner: Owner::new("planner").unwrap(),
        cwd: PathBuf::from("/work"),
        env: vec![("A".into(), "1".into())],
        placement: Placement::Here,
        broad_test_escape: false,
        priority: tasks_core::Priority::Normal,
        priority_reason: None,
    }
}

fn key(target: Option<&str>) -> SampleKey {
    SampleKey {
        recipe: "cargo test -p x".into(),
        target: target.map(str::to_string),
        machine: "nixos".into(),
    }
}

fn sample(secs: f64, at_ms: u64) -> Sample {
    Sample {
        secs,
        cache: Cache::Warm,
        outcome: Outcome::Passed,
        at_ms,
    }
}

fn ids(v: &[crate::JobRecord]) -> Vec<JobId> {
    v.iter().map(|r| r.id).collect()
}

fn lifecycle_and_dependencies(e: &Env) {
    let s = &*e.store;
    let a = s.submit(&spec("alice", vec![])).unwrap();
    let b = s.submit(&spec("bob", vec![a])).unwrap();
    let c = s.submit(&spec("alice", vec![a, b])).unwrap();
    assert!(a < b && b < c);
    let rec = s.get(a).unwrap().unwrap();
    assert_eq!(rec.spec, spec("alice", vec![]));
    assert_eq!(rec.phase, Phase::Queued);
    assert_eq!(rec.submitted_at_ms, 1_000_000);
    assert!(rec.started_at_ms.is_none() && rec.finished_at_ms.is_none());
    assert!(rec.hints.is_empty() && rec.plan_id.is_none() && rec.step_name.is_none());
    assert!(s.get(JobId(999)).unwrap().is_none());
    assert_eq!(s.ready().unwrap(), vec![a]);
    assert!(s.blocked_by_failure().unwrap().is_empty());

    s.set_phase(a, Phase::Waiting(WaitReason::Disk)).unwrap();
    s.set_phase(a, Phase::Waiting(WaitReason::Dependency(JobId(1))))
        .unwrap();
    e.clock.advance(10);
    s.set_phase(a, Phase::Running).unwrap();
    assert_eq!(s.get(a).unwrap().unwrap().started_at_ms, Some(1_000_010));
    assert!(s.ready().unwrap().is_empty());
    e.clock.advance(5);
    s.set_phase(a, Phase::Passed).unwrap();
    let done = s.get(a).unwrap().unwrap();
    assert_eq!(
        (done.phase.clone(), done.finished_at_ms),
        (Phase::Passed, Some(1_000_015))
    );
    assert_eq!(done.shown_exit(), Some(0));
    assert_eq!(s.ready().unwrap(), vec![b]);

    for next in [Phase::Running, Phase::Queued, Phase::Cancelled] {
        assert!(matches!(
            s.set_phase(a, next),
            Err(LedgerError::IllegalTransition { .. })
        ));
    }
    assert!(matches!(
        s.set_phase(c, Phase::Passed),
        Err(LedgerError::IllegalTransition { .. })
    ));
    assert!(matches!(
        s.set_phase(JobId(999), Phase::Running),
        Err(LedgerError::NotFound(_))
    ));
    assert_eq!(s.get(c).unwrap().unwrap().phase, Phase::Queued);

    run_to(s, b, Phase::Failed { exit: Some(2) });
    assert!(s.ready().unwrap().is_empty());
    assert_eq!(s.blocked_by_failure().unwrap(), vec![c]);
    s.set_phase(c, Phase::Cancelled).unwrap();
    assert!(s.blocked_by_failure().unwrap().is_empty());
}

// R-011: a malformed submission creates no job row.
fn r011_malformed(e: &Env) {
    let s = &*e.store;
    let mut bad = spec("alice", vec![]);
    bad.argv.clear();
    assert!(matches!(s.submit(&bad), Err(LedgerError::Invalid(_))));
    assert!(matches!(
        s.submit(&spec("alice", vec![JobId(999)])),
        Err(LedgerError::UnknownDependency(_))
    ));
    let mut denied = spec("alice", vec![]);
    denied.argv = vec!["adb".into(), "-s".into(), "ZD22257P6G".into()];
    assert!(s.submit(&denied).is_err());
    assert!(s.list(&ListFilter::default()).unwrap().is_empty());
    // The next id is still the first one: nothing was consumed.
    assert_eq!(s.submit(&spec("alice", vec![])).unwrap(), JobId(1));
}

// R-012: FIFO order (reopen survival is tested on the SQLite store).
fn r012_fifo(e: &Env) {
    let s = &*e.store;
    let made: Vec<JobId> = (0..5)
        .map(|_| s.submit(&spec("a", vec![])).unwrap())
        .collect();
    assert_eq!(ids(&s.list(&ListFilter::default()).unwrap()), made);
    assert_eq!(s.ready().unwrap(), made);
}

// R-008: running jobs become interrupted, never re-queued or re-run.
fn r008_restart(e: &Env) {
    let s = &*e.store;
    let run = s.submit(&spec("a", vec![])).unwrap();
    let queued = s.submit(&spec("a", vec![])).unwrap();
    let waiting = s.submit(&spec("a", vec![])).unwrap();
    s.set_wait(waiting, WaitReason::Disk, "disk low").unwrap();
    s.start(run, &info()).unwrap();
    let found = s.find_running().unwrap();
    assert_eq!(ids(&found), vec![run]);
    assert_eq!(
        found[0].lease,
        Some(LeaseRecord {
            pgid: 4242,
            leader_start: 99
        })
    );

    assert_eq!(s.interrupt_running("daemon restart").unwrap(), vec![run]);
    let r = s.get(run).unwrap().unwrap();
    assert_eq!(r.phase, Phase::Interrupted);
    assert_eq!(r.shown_exit(), None);
    assert_eq!(r.summary.as_deref(), Some("daemon restart"));
    assert!(s.find_running().unwrap().is_empty());
    assert_eq!(s.get(queued).unwrap().unwrap().phase, Phase::Queued);
    assert!(matches!(
        s.get(waiting).unwrap().unwrap().phase,
        Phase::Waiting(_)
    ));
    assert!(s.interrupt_running("again").unwrap().is_empty());
    assert!(matches!(
        s.start(run, &info()),
        Err(LedgerError::IllegalTransition { .. })
    ));
}

// R-006: a cancel committed before the worker's terminal write wins; terminal rows are immutable.
fn r006_cancel_wins(e: &Env) {
    let s = &*e.store;
    let id = s.submit(&spec("a", vec![])).unwrap();
    s.start(id, &info()).unwrap();
    assert_eq!(
        s.request_cancel(id, "alice (pid 1)").unwrap(),
        CancelOutcome::Requested
    );
    // A stale worker finishes as passed after the cancel committed.
    s.finish(id, &Finish::from_phase(Phase::Passed)).unwrap();
    let r = s.get(id).unwrap().unwrap();
    assert_eq!(r.phase, Phase::Cancelled);
    assert_eq!(r.cancel.as_ref().unwrap().by, "alice (pid 1)");
    for late in [
        Phase::Passed,
        Phase::Failed { exit: Some(1) },
        Phase::Interrupted,
    ] {
        assert!(matches!(
            s.finish(id, &Finish::from_phase(late)),
            Err(LedgerError::IllegalTransition { .. })
        ));
    }
    assert_eq!(s.get(id).unwrap().unwrap(), r);
    assert_eq!(
        s.request_cancel(id, "x").unwrap(),
        CancelOutcome::AlreadyFinished(Phase::Cancelled)
    );

    // A timeout that really happened is not rewritten by a later cancel flag.
    let t = s.submit(&spec("a", vec![])).unwrap();
    s.start(t, &info()).unwrap();
    s.request_cancel(t, "u").unwrap();
    let mut f = Finish::from_phase(Phase::TimedOut { exit: Some(143) });
    f.timeout = Some(TimeoutReason::Wall);
    s.finish(t, &f).unwrap();
    let r = s.get(t).unwrap().unwrap();
    assert_eq!(
        (r.phase, r.timeout, r.exit_code),
        (
            Phase::TimedOut { exit: Some(143) },
            Some(TimeoutReason::Wall),
            Some(143)
        )
    );
}

// R-007: a stale update elsewhere never drops a field written meanwhile.
fn r007_no_lost_update(e: &Env) {
    let s = &*e.store;
    let id = s.submit(&spec("a", vec![])).unwrap();
    s.start(id, &info()).unwrap();
    s.set_forward(id, &MachineName("nixos".into()), Some(77), None)
        .unwrap();
    s.request_cancel(id, "u").unwrap();
    s.set_log_path(id, Path::new("/logs/new.txt")).unwrap();
    s.set_remote_phase(id, &Phase::Running).unwrap();
    let r = s.get(id).unwrap().unwrap();
    assert!(r.cancel_requested());
    assert_eq!(r.forward.unwrap().remote_job, Some(77));
    assert_eq!(r.log_path, Some(PathBuf::from("/logs/new.txt")));
    assert_eq!(r.remote_phase, Some(Phase::Running));
    assert_eq!(r.lease.unwrap().pgid, 4242);
}

// R-335: figures set while running survive the job's end and a later phase change.
fn r335_figures(e: &Env) {
    let s = &*e.store;
    let id = s.submit(&spec("a", vec![])).unwrap();
    assert_eq!(s.get(id).unwrap().unwrap().figures, None);
    s.start(id, &info()).unwrap();
    let mut f = tasks_core::JobFigures::empty(tasks_core::Attribution::Cgroup);
    f.io_write_bytes = Some(5 << 30);
    f.mem_peak_bytes = Some(1 << 30);
    s.set_figures(id, &f).unwrap();
    assert_eq!(s.get(id).unwrap().unwrap().figures, Some(f.clone()));
    f.io_write_bytes = Some(6 << 30);
    s.set_figures(id, &f).unwrap();
    s.finish(id, &Finish::from_phase(Phase::Passed)).unwrap();
    let r = s.get(id).unwrap().unwrap();
    assert_eq!(r.figures, Some(f));
    assert_eq!(r.phase, Phase::Passed);
    assert!(matches!(
        s.set_figures(
            JobId(999),
            &tasks_core::JobFigures::empty(tasks_core::Attribution::ProcessTree)
        ),
        Err(LedgerError::NotFound(_))
    ));
}

// R-009: only a passed job shows exit 0; an interrupted one shows none.
fn r009_exit_zero(e: &Env) {
    let s = &*e.store;
    let mk = |end: Finish| {
        let id = s.submit(&spec("a", vec![])).unwrap();
        s.start(id, &info()).unwrap();
        s.finish(id, &end).unwrap();
        s.get(id).unwrap().unwrap()
    };
    assert_eq!(mk(Finish::from_phase(Phase::Passed)).shown_exit(), Some(0));
    let mut f = Finish::from_phase(Phase::Interrupted);
    f.exit_code = Some(0);
    let r = mk(f);
    assert_eq!((r.exit_code, r.shown_exit()), (None, None));
    let mut f = Finish::from_phase(Phase::Cancelled);
    f.exit_code = Some(0);
    assert_eq!(mk(f).shown_exit(), None);
    assert_eq!(
        mk(Finish::from_phase(Phase::Failed { exit: Some(0) })).shown_exit(),
        None
    );
    assert_eq!(
        mk(Finish::from_phase(Phase::Failed { exit: Some(3) })).shown_exit(),
        Some(3)
    );
}

fn start_finish_record(e: &Env) {
    let s = &*e.store;
    let id = s.submit(&spec("a", vec![])).unwrap();
    s.set_wait(id, WaitReason::Disk, "waiting for disk")
        .unwrap();
    e.clock.advance(100);
    s.start(id, &info()).unwrap();
    let r = s.get(id).unwrap().unwrap();
    assert_eq!(r.phase, Phase::Running);
    assert_eq!(r.machine, Some(MachineName("nixos".into())));
    assert_eq!(
        r.lease,
        Some(LeaseRecord {
            pgid: 4242,
            leader_start: 99
        })
    );
    assert_eq!(r.log_path, Some(PathBuf::from("/logs/1.txt")));
    assert_eq!(r.started_at_ms, Some(1_000_100));
    assert!(r.wait_text.is_none());
    assert!(matches!(
        s.start(id, &info()),
        Err(LedgerError::IllegalTransition { .. })
    ));

    e.clock.advance(50);
    let mut f = Finish::from_phase(Phase::TimedOut { exit: Some(143) });
    f.signal = Some(15);
    f.timeout = Some(TimeoutReason::Idle);
    f.summary = Some("command (idle timeout) exit 143".into());
    s.finish(id, &f).unwrap();
    let r = s.get(id).unwrap().unwrap();
    assert_eq!(r.phase, Phase::TimedOut { exit: Some(143) });
    assert_eq!(
        (r.exit_code, r.signal, r.timeout),
        (Some(143), Some(15), Some(TimeoutReason::Idle))
    );
    assert_eq!(
        r.summary.as_deref(),
        Some("command (idle timeout) exit 143")
    );
    assert_eq!(r.finished_at_ms, Some(1_000_150));
    assert_eq!(
        r.lease,
        Some(LeaseRecord {
            pgid: 4242,
            leader_start: 99
        })
    );

    // A non-terminal "finish" and finishing an unstarted job are refused.
    let q = s.submit(&spec("a", vec![])).unwrap();
    assert!(s.finish(q, &Finish::from_phase(Phase::Running)).is_err());
    assert!(s.finish(q, &Finish::from_phase(Phase::Passed)).is_err());
    assert!(matches!(
        s.finish(JobId(99), &Finish::from_phase(Phase::Passed)),
        Err(LedgerError::NotFound(_))
    ));
    assert!(matches!(
        s.start(JobId(99), &info()),
        Err(LedgerError::NotFound(_))
    ));
}

fn wait_text(e: &Env) {
    let s = &*e.store;
    let id = s.submit(&spec("a", vec![])).unwrap();
    let text = "waiting for compile slot: 1/1 busy (job 7 holds it), 2 ahead";
    s.set_wait(id, WaitReason::Slot(tasks_core::SlotKind::Compile), text)
        .unwrap();
    let r = s.get(id).unwrap().unwrap();
    assert_eq!(
        r.phase,
        Phase::Waiting(WaitReason::Slot(tasks_core::SlotKind::Compile))
    );
    assert_eq!(r.wait_text.as_deref(), Some(text));
    s.set_wait(id, WaitReason::Disk, "disk low").unwrap();
    assert_eq!(
        s.get(id).unwrap().unwrap().wait_text.as_deref(),
        Some("disk low")
    );
    s.start(id, &info()).unwrap();
    assert!(matches!(
        s.set_wait(id, WaitReason::Disk, "x"),
        Err(LedgerError::IllegalTransition { .. })
    ));
    assert!(matches!(
        s.set_wait(JobId(50), WaitReason::Disk, "x"),
        Err(LedgerError::NotFound(_))
    ));
}

// Keys come back sorted, whole-job series first, narrowed by recipe and bounded.
fn sample_keys_enumerate(e: &Env) {
    let s = &*e.store;
    let mk = |recipe: &str, machine: &str, target: Option<&str>| SampleKey {
        recipe: recipe.into(),
        target: target.map(str::to_string),
        machine: machine.into(),
    };
    for k in [
        mk("b", "m1", Some("test:y")),
        mk("b", "m1", None),
        mk("a", "m2", None),
        mk("a", "m1", Some("test:x")),
        mk("a", "m1", Some("test:x")),
    ] {
        s.record_sample(&k, &sample(1.0, 5)).unwrap();
    }
    let all = s.sample_keys(&[], 100).unwrap();
    assert_eq!(
        all,
        vec![
            mk("a", "m1", Some("test:x")),
            mk("a", "m2", None),
            mk("b", "m1", None),
            mk("b", "m1", Some("test:y")),
        ]
    );
    assert_eq!(s.sample_keys(&[], 2).unwrap(), all[..2].to_vec());
    assert_eq!(
        s.sample_keys(&["b".to_string()], 100).unwrap(),
        all[2..].to_vec()
    );
    assert!(
        s.sample_keys(&["none".to_string()], 100)
            .unwrap()
            .is_empty()
    );
}

// The reason given with the first cancel request is kept with the job, running or not.
fn r108_cancel_reason(e: &Env) {
    let s = &*e.store;
    let queued = s.submit(&spec("a", vec![])).unwrap();
    assert_eq!(
        s.request_cancel_with(queued, "a", Some("wrong branch"))
            .unwrap(),
        CancelOutcome::Cancelled
    );
    let r = s.get(queued).unwrap().unwrap();
    assert_eq!(
        r.cancel.as_ref().and_then(|c| c.reason.as_deref()),
        Some("wrong branch")
    );
    assert!(r.summary.unwrap().contains("wrong branch"));
    let running = s.submit(&spec("a", vec![])).unwrap();
    s.start(running, &info()).unwrap();
    assert_eq!(
        s.request_cancel_with(running, "a", Some("first")).unwrap(),
        CancelOutcome::Requested
    );
    s.request_cancel_with(running, "b", Some("second")).unwrap();
    let c = s.get(running).unwrap().unwrap().cancel.unwrap();
    assert_eq!((c.by.as_str(), c.reason.as_deref()), ("a", Some("first")));
    // a plain cancel has no reason
    let plain = s.submit(&spec("a", vec![])).unwrap();
    s.request_cancel(plain, "a").unwrap();
    assert_eq!(s.get(plain).unwrap().unwrap().cancel.unwrap().reason, None);
}

// The placement wait is stored as itself, not as pressure, and survives a reread.
fn r186_placement_wait(e: &Env) {
    let s = &*e.store;
    let id = s.submit(&spec("a", vec![])).unwrap();
    let text = "waiting for nixos: checkout sync";
    s.set_wait(id, WaitReason::Placement(text.into()), text)
        .unwrap();
    let r = s.get(id).unwrap().unwrap();
    assert_eq!(r.phase, Phase::Waiting(WaitReason::Placement(text.into())));
    assert_eq!(r.wait_text.as_deref(), Some(text));
    assert_eq!(ids(&s.list(&ListFilter::default()).unwrap()), vec![id]);
    assert_eq!(s.ready().unwrap(), vec![id]);
}

// A step's `timeout_secs` hint becomes the spec's limit; a plain submit keeps its own.
fn r095_plan_timeout_fills_spec(e: &Env) {
    let s = &*e.store;
    let mut slow = step("slow", &[]);
    slow.hints.timeout_secs = Some(90);
    let plain = step("plain", &[]);
    let plan = Plan {
        steps: vec![slow, plain],
    };
    let mut c = ctx();
    c.broad_test_escape = true;
    let done = s.submit_plan(&c, &plan).unwrap();
    let get = |name: &str| {
        let id = done.jobs.iter().find(|(n, _)| n == name).unwrap().1;
        s.get(id).unwrap().unwrap()
    };
    assert_eq!(get("slow").spec.timeout_secs, Some(90));
    assert_eq!(get("plain").spec.timeout_secs, None);
    assert!(get("slow").spec.broad_test_escape && get("plain").spec.broad_test_escape);
    let mut own = spec("a", vec![]);
    own.timeout_secs = Some(5);
    let id = s.submit(&own).unwrap();
    assert_eq!(s.get(id).unwrap().unwrap().spec.timeout_secs, Some(5));
}

// Requirements 6.12: cancelling a waiting job must not take admission or start the command.
fn cancel_unstarted(e: &Env) {
    let s = &*e.store;
    let id = s.submit(&spec("a", vec![])).unwrap();
    s.set_wait(id, WaitReason::Disk, "disk").unwrap();
    e.clock.advance(7);
    assert_eq!(
        s.request_cancel(id, "alice (pid 5)").unwrap(),
        CancelOutcome::Cancelled
    );
    let r = s.get(id).unwrap().unwrap();
    assert_eq!(r.phase, Phase::Cancelled);
    assert!(r.started_at_ms.is_none() && r.lease.is_none() && r.wait_text.is_none());
    assert_eq!(r.finished_at_ms, Some(1_000_007));
    let c = r.cancel.unwrap();
    assert_eq!((c.by.as_str(), c.at_ms), ("alice (pid 5)", 1_000_007));
    assert!(matches!(
        s.start(id, &info()),
        Err(LedgerError::IllegalTransition { .. })
    ));
    assert!(s.ready().unwrap().is_empty());
    assert!(matches!(
        s.request_cancel(JobId(40), "x"),
        Err(LedgerError::NotFound(_))
    ));
}

// R-001 / R-002: a committed cancel is seen at once; a missing job is an error, never "cancelled".
fn r002_cancel_visible(e: &Env) {
    let s = &*e.store;
    let id = s.submit(&spec("a", vec![])).unwrap();
    s.start(id, &info()).unwrap();
    assert!(!s.cancel_requested(id).unwrap());
    s.request_cancel(id, "u").unwrap();
    assert!(s.cancel_requested(id).unwrap());
    assert!(matches!(
        s.cancel_requested(JobId(77)),
        Err(LedgerError::NotFound(_))
    ));
}

// R-099: every exact (owner, cwd, argv) match inside the window; ambiguity is the caller's.
fn r099_recent_spec(e: &Env) {
    let s = &*e.store;
    let first = s.submit(&spec("alice", vec![])).unwrap();
    e.clock.advance(60_000);
    let second = s.submit(&spec("alice", vec![])).unwrap();
    let mut other_argv = spec("alice", vec![]);
    other_argv.argv = vec!["true".into(), "-v".into()];
    s.submit(&other_argv).unwrap();
    s.submit(&spec("bob", vec![])).unwrap();
    let mut other_cwd = spec("alice", vec![]);
    other_cwd.cwd = "/elsewhere".into();
    s.submit(&other_cwd).unwrap();

    let argv = vec!["true".to_string()];
    let cwd = Path::new("/tmp");
    let all = s
        .find_recent_spec("alice", cwd, &argv, Duration::from_secs(3600))
        .unwrap();
    assert_eq!(ids(&all), vec![first, second]);
    let recent = s
        .find_recent_spec("alice", cwd, &argv, Duration::from_secs(30))
        .unwrap();
    assert_eq!(ids(&recent), vec![second]);
    assert!(
        s.find_recent_spec(
            "alice",
            Path::new("/nope"),
            &argv,
            Duration::from_secs(3600)
        )
        .unwrap()
        .is_empty()
    );
    assert!(
        s.find_recent_spec("carol", cwd, &argv, Duration::from_secs(3600))
            .unwrap()
            .is_empty()
    );
    assert!(
        s.find_recent_spec(
            "alice",
            cwd,
            &["true".into(), "x".into()],
            Duration::from_secs(3600)
        )
        .unwrap()
        .is_empty()
    );
    e.clock.advance(3_600_000);
    assert!(
        s.find_recent_spec("alice", cwd, &argv, Duration::from_secs(60))
            .unwrap()
            .is_empty()
    );
}

// R-177 / R-180: plans store all steps with durable deps, hints and names, or nothing.
fn r177_plan(e: &Env) {
    let s = &*e.store;
    let mut test = step("test", &["build", "lint"]);
    test.hints = StepHints {
        timeout_secs: Some(600),
        long: true,
        tools: vec!["cargo".into()],
        fetch: vec!["out/report.html".into()],
        ..StepHints::default()
    };
    test.kind = Kind::Test;
    let plan = Plan {
        steps: vec![
            test.clone(),
            step("build", &[]),
            step("lint", &[]),
            step("pkg", &["test"]),
        ],
    };
    let done = s.submit_plan(&ctx(), &plan).unwrap();
    let names: Vec<&str> = done.jobs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["test", "build", "lint", "pkg"]);
    let id = |n: &str| done.jobs.iter().find(|(m, _)| m == n).unwrap().1;
    // Dependencies are submitted first, so ids ascend along edges.
    assert!(id("build") < id("test") && id("lint") < id("test") && id("test") < id("pkg"));

    let t = s.get(id("test")).unwrap().unwrap();
    assert_eq!(t.plan_id, Some(done.plan_id));
    assert_eq!(t.step_name.as_deref(), Some("test"));
    assert_eq!(t.hints, test.hints);
    // R-518: the step's fetch list becomes the job's
    assert_eq!(t.spec.fetch, ["out/report.html"]);
    assert_eq!(t.spec.after, vec![id("build"), id("lint")]);
    assert_eq!(
        (t.spec.kind, t.spec.cwd.as_path()),
        (Kind::Test, Path::new("/work"))
    );
    assert_eq!(t.spec.env, vec![("A".to_string(), "1".to_string())]);
    assert_eq!(t.spec.owner.as_str(), "planner");

    assert_eq!(s.ready().unwrap(), vec![id("build"), id("lint")]);
    run_to(&*e.store, id("build"), Phase::Passed);
    assert_eq!(s.ready().unwrap(), vec![id("lint")]);
    run_to(&*e.store, id("lint"), Phase::Passed);
    assert_eq!(s.ready().unwrap(), vec![id("test")]);

    // Atomic: a later step that fails validation leaves no earlier step behind.
    let before = s.list(&ListFilter::default()).unwrap().len();
    let mut bad = step("b", &["a"]);
    bad.argv = vec!["adb".into(), "ZD22257P6G".into()];
    let broken = Plan {
        steps: vec![step("a", &[]), bad],
    };
    assert!(matches!(
        s.submit_plan(&ctx(), &broken),
        Err(LedgerError::Invalid(_))
    ));
    let cyc = Plan {
        steps: vec![step("a", &["b"]), step("b", &["a"])],
    };
    assert!(matches!(
        s.submit_plan(&ctx(), &cyc),
        Err(LedgerError::Plan(_))
    ));
    assert!(matches!(
        s.submit_plan(&ctx(), &Plan { steps: vec![] }),
        Err(LedgerError::Plan(_))
    ));
    assert_eq!(s.list(&ListFilter::default()).unwrap().len(), before);

    // A second plan gets its own id.
    let again = s
        .submit_plan(
            &ctx(),
            &Plan {
                steps: vec![step("only", &[])],
            },
        )
        .unwrap();
    assert_ne!(again.plan_id, done.plan_id);
}

// R-177: not ready until every dependency passed.
fn r177_all_deps(e: &Env) {
    let s = &*e.store;
    let a = s.submit(&spec("a", vec![])).unwrap();
    let b = s.submit(&spec("a", vec![])).unwrap();
    let c = s.submit(&spec("a", vec![a, b])).unwrap();
    assert_eq!(s.ready().unwrap(), vec![a, b]);
    run_to(s, a, Phase::Passed);
    assert!(!s.ready().unwrap().contains(&c));
    s.start(b, &info()).unwrap();
    assert!(!s.ready().unwrap().contains(&c));
    s.finish(b, &Finish::from_phase(Phase::Passed)).unwrap();
    assert_eq!(s.ready().unwrap(), vec![c]);
    // A waiting job is still a candidate once its dependencies have passed.
    s.set_wait(c, WaitReason::Disk, "disk").unwrap();
    assert_eq!(s.ready().unwrap(), vec![c]);
}

// R-178: one rule: any terminal dependency other than Passed blocks; only unstarted jobs count.
fn r178_blocked(e: &Env) {
    let s = &*e.store;
    for end in [
        Phase::Failed { exit: Some(1) },
        Phase::TimedOut { exit: None },
        Phase::Cancelled,
        Phase::Interrupted,
    ] {
        let dep = s.submit(&spec("a", vec![])).unwrap();
        let waiter = s.submit(&spec("a", vec![dep])).unwrap();
        let running_dependent_ok = s.submit(&spec("a", vec![])).unwrap();
        run_to(s, dep, end.clone());
        assert!(s.blocked_by_failure().unwrap().contains(&waiter), "{end:?}");
        assert!(
            !s.blocked_by_failure()
                .unwrap()
                .contains(&running_dependent_ok)
        );
        assert!(!s.ready().unwrap().contains(&waiter));
        s.request_cancel(waiter, "t").unwrap();
        s.request_cancel(running_dependent_ok, "t").unwrap();
    }

    // A job that never started (e.g. sync failed) ends from Queued and blocks the same way.
    let dep = s.submit(&spec("b", vec![])).unwrap();
    let waiter = s.submit(&spec("b", vec![dep])).unwrap();
    let never = Phase::NotStarted {
        reason: "checkout sync failed".into(),
    };
    s.finish(dep, &Finish::from_phase(never.clone())).unwrap();
    assert_eq!(s.get(dep).unwrap().unwrap().phase, never);
    assert!(s.blocked_by_failure().unwrap().contains(&waiter));
    s.request_cancel(waiter, "t").unwrap();

    // Transitive cascade, and a passed dependency never blocks.
    let root = s.submit(&spec("c", vec![])).unwrap();
    let mid = s.submit(&spec("c", vec![root])).unwrap();
    let leaf = s.submit(&spec("c", vec![mid])).unwrap();
    let diamond = s.submit(&spec("c", vec![mid, root])).unwrap();
    let fine = s.submit(&spec("c", vec![])).unwrap();
    let after_fine = s.submit(&spec("c", vec![fine])).unwrap();
    s.set_wait(leaf, WaitReason::Dependency(mid), "waiting for job mid")
        .unwrap();
    run_to(s, fine, Phase::Passed);
    run_to(s, root, Phase::Failed { exit: Some(1) });
    let mut cancelled = s.cancel_blocked().unwrap();
    cancelled.sort();
    assert_eq!(cancelled, vec![mid, leaf, diamond]);
    for id in [mid, leaf, diamond] {
        let r = s.get(id).unwrap().unwrap();
        assert_eq!(r.phase, Phase::Cancelled);
        assert_eq!(r.cancel.unwrap().by, "dependency failed");
    }
    assert_eq!(s.get(after_fine).unwrap().unwrap().phase, Phase::Queued);
    assert_eq!(s.ready().unwrap(), vec![after_fine]);
    assert!(s.cancel_blocked().unwrap().is_empty());
}

// R-179: unknown, terminal-not-passed and cyclic dependency lists are refused.
fn r179_bad_deps(e: &Env) {
    let s = &*e.store;
    assert!(matches!(
        s.submit(&spec("a", vec![JobId(5)])),
        Err(LedgerError::UnknownDependency(_))
    ));
    let dead = s.submit(&spec("a", vec![])).unwrap();
    run_to(s, dead, Phase::Failed { exit: Some(1) });
    assert!(matches!(
        s.submit(&spec("a", vec![dead])),
        Err(LedgerError::DependencyNotPassable(_))
    ));
    let ok = s.submit(&spec("a", vec![])).unwrap();
    run_to(s, ok, Phase::Passed);
    assert!(s.submit(&spec("a", vec![ok])).is_ok());
    let n = s.list(&ListFilter::default()).unwrap().len();
    let selfdep = Plan {
        steps: vec![step("a", &["a"])],
    };
    assert!(matches!(
        s.submit_plan(&ctx(), &selfdep),
        Err(LedgerError::Plan(_))
    ));
    let unknown = Plan {
        steps: vec![step("a", &["zzz"])],
    };
    assert!(matches!(
        s.submit_plan(&ctx(), &unknown),
        Err(LedgerError::Plan(_))
    ));
    assert_eq!(s.list(&ListFilter::default()).unwrap().len(), n);
}

fn samples_round_trip(e: &Env) {
    let s = &*e.store;
    let whole = key(None);
    let lifecycle = key(Some("test:lifecycle"));
    let alpha = key(Some("test:alpha"));
    s.record_sample(&whole, &sample(12.5, 100)).unwrap();
    s.record_sample(
        &whole,
        &Sample {
            cache: Cache::Cold,
            outcome: Outcome::Cancelled,
            ..sample(3.0, 200)
        },
    )
    .unwrap();
    s.record_sample(&lifecycle, &sample(190.0, 300)).unwrap();
    s.record_sample(&alpha, &sample(1.0, 400)).unwrap();
    let mut other_machine = lifecycle.clone();
    other_machine.machine = "laptop".into();
    s.record_sample(&other_machine, &sample(400.0, 500))
        .unwrap();
    let mut other_recipe = lifecycle.clone();
    other_recipe.recipe = "gradle test".into();
    other_recipe.target = Some("test:zzz".into());
    s.record_sample(&other_recipe, &sample(5.0, 600)).unwrap();

    let got = s.samples(&whole).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0], sample(12.5, 100));
    assert_eq!(
        (got[1].cache, got[1].outcome),
        (Cache::Cold, Outcome::Cancelled)
    );
    assert_eq!(s.samples(&lifecycle).unwrap(), vec![sample(190.0, 300)]);
    assert_eq!(s.samples(&other_machine).unwrap(), vec![sample(400.0, 500)]);
    assert!(s.samples(&key(Some("test:none"))).unwrap().is_empty());
    assert_eq!(
        s.targets("cargo test -p x").unwrap(),
        ["test:alpha", "test:lifecycle"]
    );
    assert_eq!(s.targets("gradle test").unwrap(), ["test:zzz"]);
    assert!(s.targets("nothing").unwrap().is_empty());

    for secs in [f64::NAN, f64::INFINITY, -0.5] {
        assert!(matches!(
            s.record_sample(&whole, &sample(secs, 1)),
            Err(LedgerError::InvalidSample(_))
        ));
    }
    assert_eq!(s.samples(&whole).unwrap().len(), 2);
}

// R-013: completion and its sample are one transaction; a bad sample changes nothing.
fn r013_atomic(e: &Env) {
    let s = &*e.store;
    let id = s.submit(&spec("a", vec![])).unwrap();
    s.start(id, &info()).unwrap();
    let k = key(None);
    let bad = sample(f64::NAN, 1);
    assert!(matches!(
        s.finish_with_sample(id, &Finish::from_phase(Phase::Passed), &k, &bad),
        Err(LedgerError::InvalidSample(_))
    ));
    assert_eq!(s.get(id).unwrap().unwrap().phase, Phase::Running);
    assert!(s.samples(&k).unwrap().is_empty());

    s.finish_with_sample(id, &Finish::from_phase(Phase::Passed), &k, &sample(2.0, 5))
        .unwrap();
    assert_eq!(s.get(id).unwrap().unwrap().phase, Phase::Passed);
    assert_eq!(s.samples(&k).unwrap().len(), 1);

    // An illegal finish records no sample either.
    assert!(
        s.finish_with_sample(
            id,
            &Finish::from_phase(Phase::Failed { exit: Some(1) }),
            &k,
            &sample(9.0, 6)
        )
        .is_err()
    );
    assert_eq!(s.samples(&k).unwrap().len(), 1);

    // A cancelled run is kept, marked cancelled, for the timing layer to ignore.
    let c = s.submit(&spec("a", vec![])).unwrap();
    s.start(c, &info()).unwrap();
    let cancelled = Sample {
        outcome: Outcome::Cancelled,
        ..sample(4.0, 7)
    };
    s.finish_with_sample(c, &Finish::from_phase(Phase::Cancelled), &k, &cancelled)
        .unwrap();
    assert_eq!(s.samples(&k).unwrap().len(), 2);
}

// R-015: remote state is additive and never changes the local phase.
fn r015_remote(e: &Env) {
    let s = &*e.store;
    let id = s.submit(&spec("a", vec![])).unwrap();
    run_to(s, id, Phase::Interrupted);
    let peer = MachineName("nixos".into());
    s.set_forward(id, &peer, None, None).unwrap();
    assert_eq!(
        s.get(id).unwrap().unwrap().forward.unwrap().remote_job,
        None
    );
    s.set_forward(id, &peer, Some(12), None).unwrap();
    s.set_remote_phase(id, &Phase::Passed).unwrap();
    let r = s.get(id).unwrap().unwrap();
    assert_eq!(r.phase, Phase::Interrupted);
    assert_eq!(r.shown_exit(), None);
    let f = r.forward.unwrap();
    assert_eq!((f.peer, f.remote_job), (peer, Some(12)));
    assert_eq!(r.remote_phase, Some(Phase::Passed));
    assert!(matches!(
        s.set_forward(JobId(80), &MachineName("x".into()), None, None),
        Err(LedgerError::NotFound(_))
    ));
}

// R-152: the (peer, key) pair of a forward is recorded before the submit and survives later
// updates; a received job is unique per (peer, key).
fn r152_forward_key_and_origin(e: &Env) {
    let s = &*e.store;
    let id = s.submit(&spec("a", vec![])).unwrap();
    let peer = MachineName("nixos".into());
    s.set_forward(id, &peer, None, Some("laptop:1")).unwrap();
    let f = s.get(id).unwrap().unwrap().forward.unwrap();
    assert_eq!(f.remote_key.as_deref(), Some("laptop:1"));
    // learning the remote id keeps the key
    s.set_forward(id, &peer, Some(9), None).unwrap();
    let f = s.get(id).unwrap().unwrap().forward.unwrap();
    assert_eq!(
        (f.remote_job, f.remote_key.as_deref()),
        (Some(9), Some("laptop:1"))
    );
    s.clear_forward(id).unwrap();
    assert!(s.get(id).unwrap().unwrap().forward.is_none());

    let mut job = NewJob::new(spec("remote-owner", vec![]));
    job.origin = Some(Origin {
        peer: "peer-a".into(),
        key: "k1".into(),
    });
    let first = s.submit_job(&job).unwrap();
    assert_eq!(s.find_origin("peer-a", "k1").unwrap(), Some(first));
    assert_eq!(s.find_origin("peer-b", "k1").unwrap(), None);
    assert!(matches!(
        s.submit_job(&job),
        Err(LedgerError::DuplicateOrigin { .. })
    ));
    let mut other = job.clone();
    other.origin = Some(Origin {
        peer: "peer-b".into(),
        key: "k1".into(),
    });
    assert!(s.submit_job(&other).is_ok());
    let r = s.get(first).unwrap().unwrap();
    assert_eq!(r.origin.unwrap().peer, "peer-a");
}

// R-022 to R-024 and R-026: id, recent and active filters.
fn list_filters(e: &Env) {
    let s = &*e.store;
    let mut made = vec![];
    for i in 0..6 {
        e.clock.advance(1);
        let id = s
            .submit(&spec(if i % 2 == 0 { "alice" } else { "bob" }, vec![]))
            .unwrap();
        made.push(id);
    }
    // 1,2,3 finish in order; 4 runs, 5 waits, 6 queued.
    for id in &made[..3] {
        e.clock.advance(1);
        run_to(s, *id, Phase::Passed);
    }
    s.start(made[3], &info()).unwrap();
    s.set_wait(made[4], WaitReason::Disk, "d").unwrap();

    let one = ListFilter {
        id: Some(made[1]),
        ..ListFilter::default()
    };
    assert_eq!(ids(&s.list(&one).unwrap()), vec![made[1]]);
    assert!(
        s.list(&ListFilter {
            id: Some(JobId(99)),
            ..ListFilter::default()
        })
        .unwrap()
        .is_empty()
    );

    let active = ListFilter {
        active_only: true,
        ..ListFilter::default()
    };
    assert_eq!(ids(&s.list(&active).unwrap()), made[3..].to_vec());

    let recent = ListFilter {
        recent_finished: Some(2),
        ..ListFilter::default()
    };
    let want: Vec<JobId> = vec![made[1], made[2], made[3], made[4], made[5]];
    assert_eq!(ids(&s.list(&recent).unwrap()), want);
    let none = ListFilter {
        recent_finished: Some(0),
        ..ListFilter::default()
    };
    assert_eq!(ids(&s.list(&none).unwrap()), made[3..].to_vec());

    let alice = ListFilter {
        owner: Some("alice".into()),
        ..ListFilter::default()
    };
    assert_eq!(
        ids(&s.list(&alice).unwrap()),
        vec![made[0], made[2], made[4]]
    );
    let limited = ListFilter {
        limit: Some(2),
        ..ListFilter::default()
    };
    assert_eq!(ids(&s.list(&limited).unwrap()), made[..2].to_vec());
}

fn retention(e: &Env) {
    let s = &*e.store;
    let mut done = vec![];
    for _ in 0..4 {
        e.clock.advance(1000);
        let id = s.submit(&spec("a", vec![])).unwrap();
        run_to(s, id, Phase::Passed);
        done.push(id);
    }
    let pinned_dep = done[0];
    let running = s.submit(&spec("a", vec![])).unwrap();
    s.start(running, &info()).unwrap();
    let queued = s.submit(&spec("a", vec![pinned_dep])).unwrap();
    let plan = s
        .submit_plan(
            &ctx(),
            &Plan {
                steps: vec![step("x", &[])],
            },
        )
        .unwrap();
    run_to(s, plan.jobs[0].1, Phase::Passed);
    for i in 0..5u64 {
        s.record_sample(
            &key(Some("test:t")),
            &sample(1.0, 1_000_000 - 4000 + i * 1000),
        )
        .unwrap();
    }
    s.record_sample(&key(None), &sample(1.0, 1)).unwrap();

    // Nothing to do without a policy.
    assert_eq!(
        s.prune(&RetentionPolicy::default()).unwrap(),
        crate::PruneReport::default()
    );

    e.clock.advance(10_000);
    let policy = RetentionPolicy {
        max_job_age: Some(Duration::from_secs(1)),
        max_finished_jobs: None,
        max_sample_age: Some(Duration::from_secs(16)),
        max_samples_per_key: Some(1),
    };
    let report = s.prune(&policy).unwrap();
    // done[0] is a dependency of a queued job; plan job is the newest id; running/queued stay.
    assert_eq!(report.jobs, 3);
    let left: Vec<JobId> = ids(&s.list(&ListFilter::default()).unwrap());
    assert_eq!(left, vec![done[0], running, queued, plan.jobs[0].1]);
    assert_eq!(s.get(running).unwrap().unwrap().phase, Phase::Running);
    assert_eq!(s.get(queued).unwrap().unwrap().phase, Phase::Queued);
    assert_eq!(s.ready().unwrap(), vec![queued]);
    // Samples: old ones go by age; the target keeps only its newest one by count.
    assert!(s.samples(&key(None)).unwrap().is_empty());
    let kept = s.samples(&key(Some("test:t"))).unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].at_ms, 1_000_000);
    assert_eq!(report.samples, 5);

    // Ids are never reused after pruning.
    let next = s.submit(&spec("a", vec![])).unwrap();
    assert!(next > plan.jobs[0].1);

    // Count policy keeps the newest finished jobs.
    run_to(s, queued, Phase::Passed);
    let count = RetentionPolicy {
        max_finished_jobs: Some(1),
        ..RetentionPolicy::default()
    };
    s.prune(&count).unwrap();
    let finished: Vec<JobId> = ids(&s.list(&ListFilter::default()).unwrap())
        .into_iter()
        .filter(|id| s.get(*id).unwrap().unwrap().phase.is_terminal())
        .collect();
    assert_eq!(finished, vec![queued]);
}

// R-025: a token that changes whenever the ledger commits.
fn change_token(e: &Env) {
    let s = &*e.store;
    let t0 = s.change_token().unwrap();
    assert_eq!(s.change_token().unwrap(), t0);
    let id = s.submit(&spec("a", vec![])).unwrap();
    let t1 = s.change_token().unwrap();
    assert_ne!(t1, t0);
    s.get(id).unwrap();
    s.list(&ListFilter::default()).unwrap();
    assert_eq!(s.change_token().unwrap(), t1);
    s.request_cancel(id, "u").unwrap();
    assert_ne!(s.change_token().unwrap(), t1);
}

// R-339: a queued job changes class (blocking needs its reason); a started one is refused.
fn r339_set_priority(e: &Env) {
    let s = &*e.store;
    let id = s.submit(&spec("a", vec![])).unwrap();
    assert!(matches!(
        s.set_priority(id, tasks_core::Priority::Blocking, None),
        Err(LedgerError::Invalid(
            tasks_core::CoreError::BlockingNeedsReason
        ))
    ));
    assert_eq!(
        s.get(id).unwrap().unwrap().spec.priority,
        tasks_core::Priority::Normal
    );
    let before = s
        .set_priority(id, tasks_core::Priority::Blocking, Some("  release gate "))
        .unwrap();
    assert_eq!(before, tasks_core::Priority::Normal);
    let r = s.get(id).unwrap().unwrap();
    assert_eq!(r.spec.priority, tasks_core::Priority::Blocking);
    assert_eq!(r.spec.priority_reason.as_deref(), Some("release gate"));
    // a waiting job changes too, and back to normal clears the reason
    s.set_wait(id, WaitReason::Disk, "disk").unwrap();
    let before = s
        .set_priority(id, tasks_core::Priority::Normal, None)
        .unwrap();
    assert_eq!(before, tasks_core::Priority::Blocking);
    let r = s.get(id).unwrap().unwrap();
    assert_eq!(
        (r.spec.priority, r.spec.priority_reason),
        (tasks_core::Priority::Normal, None)
    );
    // never preempts: a running or finished job is refused and keeps its class
    s.start(id, &info()).unwrap();
    assert!(matches!(
        s.set_priority(id, tasks_core::Priority::Blocking, Some("now")),
        Err(LedgerError::NotQueued { .. })
    ));
    s.finish(id, &Finish::from_phase(Phase::Passed)).unwrap();
    assert!(matches!(
        s.set_priority(id, tasks_core::Priority::Background, None),
        Err(LedgerError::NotQueued { .. })
    ));
    assert_eq!(
        s.get(id).unwrap().unwrap().spec.priority,
        tasks_core::Priority::Normal
    );
    assert!(matches!(
        s.set_priority(JobId(99), tasks_core::Priority::Background, None),
        Err(LedgerError::NotFound(_))
    ));
}

// R-340: the run policy's background hint fills the step's class; a class the submitter asked
// for covers every step.
fn r340_plan_priority(e: &Env) {
    let s = &*e.store;
    let mut warm = step("warm", &[]);
    warm.hints.priority = Some(tasks_core::Priority::Background);
    let plain = step("plain", &[]);
    let plan = Plan {
        steps: vec![warm, plain],
    };
    let done = s.submit_plan(&ctx(), &plan).unwrap();
    let class = |name: &str| {
        let id = done.jobs.iter().find(|(n, _)| n == name).unwrap().1;
        s.get(id).unwrap().unwrap().spec.priority
    };
    assert_eq!(class("warm"), tasks_core::Priority::Background);
    assert_eq!(class("plain"), tasks_core::Priority::Normal);
    let mut c = ctx();
    c.priority = tasks_core::Priority::Blocking;
    c.priority_reason = Some("agent waiting".into());
    let done = s.submit_plan(&c, &plan).unwrap();
    for (_, id) in &done.jobs {
        let r = s.get(*id).unwrap().unwrap();
        assert_eq!(r.spec.priority, tasks_core::Priority::Blocking);
        assert_eq!(r.spec.priority_reason.as_deref(), Some("agent waiting"));
    }
}

// R-342: a whole-job sample keeps the peak memory the job's figures ended with, newest first;
// a run with no figures, a peer's sample and an old row are unknown, never zero.
fn r342_peak_memory(e: &Env) {
    let s = &*e.store;
    let k = key(None);
    assert_eq!(s.recent_peaks(&k).unwrap(), Vec::<Option<u64>>::new());
    let run = |peak: Option<u64>, at: u64| {
        let id = s.submit(&spec("a", vec![])).unwrap();
        s.start(id, &info()).unwrap();
        if let Some(p) = peak {
            let mut f = tasks_core::JobFigures::empty(tasks_core::Attribution::Cgroup);
            f.mem_peak_bytes = Some(p);
            s.set_figures(id, &f).unwrap();
        }
        s.finish_recording(
            id,
            &Finish::from_phase(Phase::Passed),
            Some((&k, &sample(2.0, at))),
        )
        .unwrap();
    };
    run(Some(100), 1);
    run(None, 2);
    run(Some(300), 3);
    assert_eq!(
        s.recent_peaks(&k).unwrap(),
        vec![Some(300), None, Some(100)]
    );
    run(Some(50), 4);
    // only the latest three
    assert_eq!(s.recent_peaks(&k).unwrap(), vec![Some(50), Some(300), None]);
    // a per-target sample is not a whole-job run and a peer's sample knows no memory
    s.record_sample(&key(Some("test:x")), &sample(1.0, 5))
        .unwrap();
    assert!(
        s.recent_peaks(&key(Some("test:x")))
            .unwrap()
            .iter()
            .all(Option::is_none)
    );
    s.record_sample(&k, &sample(1.0, 6)).unwrap();
    assert_eq!(s.recent_peaks(&k).unwrap()[0], None);
    // another machine's series is separate
    let mut other = key(None);
    other.machine = "elsewhere".into();
    assert!(s.recent_peaks(&other).unwrap().is_empty());
}
