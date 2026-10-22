//! Shared behavioural suite run against every `Store` implementation.

use crate::{LedgerError, ListFilter, Store};
use std::path::PathBuf;
use tasks_core::{JobId, JobSpec, Kind, Owner, Phase, Placement, Requirements, WaitReason};

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
    }
}

fn run_to(s: &dyn Store, id: JobId, end: Phase) {
    s.set_phase(id, Phase::Running).unwrap();
    s.set_phase(id, end).unwrap();
}

/// Exercises every `Store` method.
pub(crate) fn run(s: &dyn Store) {
    // submit / get
    let a = s.submit(&spec("alice", vec![])).unwrap();
    let b = s.submit(&spec("bob", vec![a])).unwrap();
    let c = s.submit(&spec("alice", vec![a, b])).unwrap();
    assert!(a < b && b < c);
    let rec = s.get(a).unwrap().unwrap();
    assert_eq!(rec.spec, spec("alice", vec![]));
    assert_eq!(rec.phase, Phase::Queued);
    assert!(rec.started_at_ms.is_none() && rec.finished_at_ms.is_none());
    assert!(s.get(JobId(999)).unwrap().is_none());

    // validation and unknown deps
    let mut bad = spec("alice", vec![]);
    bad.argv.clear();
    assert!(matches!(s.submit(&bad), Err(LedgerError::Invalid(_))));
    assert!(matches!(
        s.submit(&spec("alice", vec![JobId(999)])),
        Err(LedgerError::UnknownDependency(_))
    ));

    // ready
    assert_eq!(s.ready().unwrap(), vec![a]);
    assert!(s.blocked_by_failure().unwrap().is_empty());

    // transitions and timestamps
    s.set_phase(a, Phase::Waiting(WaitReason::Disk)).unwrap();
    s.set_phase(a, Phase::Waiting(WaitReason::Dependency(JobId(1))))
        .unwrap();
    s.set_phase(a, Phase::Running).unwrap();
    assert!(s.get(a).unwrap().unwrap().started_at_ms.is_some());
    assert!(s.ready().unwrap().is_empty());
    s.set_phase(a, Phase::Passed).unwrap();
    let done = s.get(a).unwrap().unwrap();
    assert_eq!(done.phase, Phase::Passed);
    assert!(done.finished_at_ms.is_some());
    assert_eq!(s.ready().unwrap(), vec![b]);

    // illegal transitions
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

    // failure blocks dependents
    run_to(s, b, Phase::Failed { exit: Some(2) });
    assert!(s.ready().unwrap().is_empty());
    assert_eq!(s.blocked_by_failure().unwrap(), vec![c]);

    // list filters
    assert_eq!(s.list(&ListFilter::default()).unwrap().len(), 3);
    let alice = ListFilter {
        owner: Some("alice".into()),
        ..ListFilter::default()
    };
    assert_eq!(s.list(&alice).unwrap().len(), 2);
    let active = ListFilter {
        active_only: true,
        ..ListFilter::default()
    };
    let ids: Vec<JobId> = s.list(&active).unwrap().iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![c]);
    let limited = ListFilter {
        limit: Some(1),
        ..ListFilter::default()
    };
    assert_eq!(s.list(&limited).unwrap().len(), 1);

    // cancel from queued
    s.set_phase(c, Phase::Cancelled).unwrap();
    assert!(s.blocked_by_failure().unwrap().is_empty());
}
