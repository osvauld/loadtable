use tinline_task_manager::{Cost, Estimate, JobSpec, Phase, Store};

fn spec() -> JobSpec {
    JobSpec {
        suite: "sim.groups.smoke".into(),
        owner: "agent-a".into(),
        source: "/fixture/repo".into(),
        revision: "fixture-commit".into(),
        configuration: "release/linux-x86_64".into(),
        cost: Cost::CpuSimulation,
    }
}

#[test]
fn history_learns_first_sample_then_weights_recent_runs() {
    let mut e = Estimate::default();
    assert_eq!(e.seconds(), None);
    e.record(100.0).unwrap();
    assert_eq!(e.seconds(), Some(100.0));
    e.record(50.0).unwrap();
    assert_eq!(e.seconds(), Some(90.0));
    assert_eq!(e.samples(), 2);
}

#[test]
fn invalid_samples_do_not_mutate_estimate() {
    let mut e = Estimate::default();
    e.record(3.0).unwrap();
    for invalid in [-1.0, f64::NAN, f64::INFINITY] {
        let before = e.clone();
        assert!(e.record(invalid).is_err());
        assert_eq!(e, before);
    }
}

#[test]
fn queue_is_fifo_and_survives_reopening() {
    let path = std::env::temp_dir().join(format!(
        "task-manager-contract-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut store = Store::open(&path).unwrap();
    let first = store.submit(&spec()).unwrap();
    let second = store.submit(&spec()).unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    let jobs = store.jobs().unwrap();
    assert_eq!(
        jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
        vec![first.id, second.id]
    );
    assert!(jobs.iter().all(|j| j.phase == Phase::Queued));
    drop(store);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn only_queued_jobs_can_be_cancelled_in_control_plane_milestone() {
    let mut store = Store::memory().unwrap();
    let job = store.submit(&spec()).unwrap();
    store.cancel(job.id).unwrap();
    assert_eq!(store.jobs().unwrap()[0].phase, Phase::Cancelled);
    assert!(store.cancel(job.id).is_err());
    assert!(store.cancel(job.id + 1).is_err());
}

#[test]
fn runtime_history_is_separate_by_suite_configuration_and_phase() {
    let mut store = Store::memory().unwrap();
    store
        .record_runtime("sim.groups", "release", "run", 100.0)
        .unwrap();
    store
        .record_runtime("sim.groups", "release", "run", 50.0)
        .unwrap();
    store
        .record_runtime("sim.groups", "release", "build", 12.0)
        .unwrap();
    assert_eq!(
        store
            .estimate("sim.groups", "release", "run")
            .unwrap()
            .seconds(),
        Some(90.0)
    );
    assert_eq!(
        store
            .estimate("sim.groups", "release", "build")
            .unwrap()
            .seconds(),
        Some(12.0)
    );
    assert_eq!(
        store
            .estimate("sim.groups", "debug", "run")
            .unwrap()
            .seconds(),
        None
    );
}

#[test]
fn malformed_submission_does_not_create_job() {
    let mut store = Store::memory().unwrap();
    let mut bad = spec();
    bad.revision.clear();
    assert!(store.submit(&bad).is_err());
    assert!(store.jobs().unwrap().is_empty());
}

#[test]
fn bad_runtime_does_not_change_durable_history() {
    let mut store = Store::memory().unwrap();
    store.record_runtime("suite", "config", "run", 5.0).unwrap();
    assert!(
        store
            .record_runtime("suite", "config", "run", -2.0)
            .is_err()
    );
    assert_eq!(
        store.estimate("suite", "config", "run").unwrap().seconds(),
        Some(5.0)
    );
}
