use super::*;
use crate::execution::ProcessIdentity;
use crate::{Cost, JobSpec, catalog};

fn spec() -> JobSpec {
    JobSpec {
        suite: catalog::SUITE.into(),
        configuration: catalog::CONFIGURATION.into(),
        cost: Cost::Quick,
        source: "/fixture".into(),
        revision: "a".repeat(40),
        owner: "fixture-agent".into(),
    }
}

fn me() -> ProcessIdentity {
    ProcessIdentity {
        pid: 1,
        start_ticks: 1,
    }
}

fn approved(s: &mut Store) -> i64 {
    let id = s.submit(&spec()).unwrap().id;
    s.approve(id).unwrap();
    id
}

fn phase(s: &Store, id: i64) -> Phase {
    s.job(id).unwrap().phase
}

#[test]
fn a_waiting_job_is_not_claimed_until_its_dependencies_passed() {
    let mut s = Store::memory().unwrap();
    let first = approved(&mut s);
    let second = s.submit(&spec()).unwrap().id;
    s.set_after(second, &[first]).unwrap();
    s.approve(second).unwrap();
    assert_eq!(s.approved_waiting().unwrap(), 1, "only the first can start");

    assert_eq!(s.claim(me()).unwrap().unwrap().id, first);
    assert!(s.claim(me()).unwrap().is_none(), "second still waits");
    assert_eq!(
        s.approved_waiting().unwrap(),
        0,
        "a blocked job is not worker work"
    );
    assert_eq!(
        s.waiting_for(second).unwrap(),
        [(first, "preparing".into())]
    );

    s.finish(first, Phase::Passed, "ok").unwrap();
    assert_eq!(s.approved_waiting().unwrap(), 1);
    assert_eq!(s.claim(me()).unwrap().unwrap().id, second);
    assert!(s.waiting_for(second).unwrap().is_empty());
}

#[test]
fn a_job_waits_for_all_of_its_dependencies() {
    let mut s = Store::memory().unwrap();
    let (a, b) = (approved(&mut s), approved(&mut s));
    let last = s.submit(&spec()).unwrap().id;
    s.set_after(last, &[a, b]).unwrap();
    s.approve(last).unwrap();
    assert_eq!(s.claim(me()).unwrap().unwrap().id, a);
    assert_eq!(s.claim(me()).unwrap().unwrap().id, b);
    s.finish(a, Phase::Passed, "ok").unwrap();
    assert!(s.claim(me()).unwrap().is_none(), "b has not passed");
    s.finish(b, Phase::Passed, "ok").unwrap();
    assert_eq!(s.claim(me()).unwrap().unwrap().id, last);
}

#[test]
fn a_failed_dependency_cancels_what_waits_for_it_and_only_that() {
    let mut s = Store::memory().unwrap();
    let (bad, good) = (approved(&mut s), approved(&mut s));
    let after_bad = s.submit(&spec()).unwrap().id;
    let after_good = s.submit(&spec()).unwrap().id;
    let after_after_bad = s.submit(&spec()).unwrap().id;
    s.set_after(after_bad, &[bad]).unwrap();
    s.set_after(after_good, &[good]).unwrap();
    s.set_after(after_after_bad, &[after_bad]).unwrap();
    for id in [after_bad, after_good, after_after_bad] {
        s.approve(id).unwrap();
    }
    assert_eq!(s.claim(me()).unwrap().unwrap().id, bad);
    assert_eq!(s.claim(me()).unwrap().unwrap().id, good);
    s.finish(bad, Phase::Failed, "exit 1").unwrap();
    s.finish(good, Phase::Passed, "ok").unwrap();

    assert_eq!(s.claim(me()).unwrap().unwrap().id, after_good);
    assert_eq!(phase(&s, after_bad), Phase::Cancelled);
    // The next pass cancels the step that waited for the cancelled one.
    assert!(s.claim(me()).unwrap().is_none());
    assert_eq!(phase(&s, after_after_bad), Phase::Cancelled);
}

#[test]
fn bad_dependency_lists_are_refused() {
    let mut s = Store::memory().unwrap();
    let old = approved(&mut s);
    let id = s.submit(&spec()).unwrap().id;
    let newer = s.submit(&spec()).unwrap().id;
    for (after, why) in [
        (vec![], "empty"),
        (vec![id], "itself"),
        (vec![newer], "newer job (no cycles)"),
        (vec![old, old], "twice"),
        (vec![999], "unknown"),
        ((1..=65).collect(), "too many"),
    ] {
        assert!(s.set_after(id, &after).is_err(), "{why}");
    }
    assert!(
        s.waiting_for(id).unwrap().is_empty(),
        "a refused list changes nothing"
    );
    s.cancel(old).unwrap();
    assert!(
        s.set_after(id, &[old]).is_err(),
        "dependency already cancelled"
    );
    s.approve(newer).unwrap();
    assert!(
        s.set_after(newer, &[id]).is_err(),
        "only a queued job takes dependencies"
    );
}
