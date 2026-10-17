use super::*;
use crate::execution::ProcessIdentity;

fn spec(owner: &str) -> JobSpec {
    JobSpec {
        suite: catalog::ANDROID_SUITE.into(),
        configuration: "debug/x86_64/rust-debug/offline".into(),
        cost: Cost::CpuSimulation,
        source: "/synthetic/repo".into(),
        revision: "a".repeat(40),
        owner: owner.into(),
    }
}

fn ask(store: &mut Store, action: Action) -> Response {
    handle(
        store,
        Request {
            version: PROTOCOL_VERSION,
            action,
        },
    )
}

#[test]
fn job_action_returns_only_that_job_with_its_execution() {
    let mut s = Store::memory().unwrap();
    let claimed = s.submit(&spec("synthetic-a")).unwrap();
    let queued = s.submit(&spec("synthetic-b")).unwrap();
    s.approve_android_trial(claimed.id).unwrap();
    s.claim(ProcessIdentity {
        pid: 1,
        start_ticks: 1,
    })
    .unwrap();

    let all = serde_json::to_value(s.jobs().unwrap()).unwrap();
    for (index, id) in [claimed.id, queued.id].into_iter().enumerate() {
        let r = ask(&mut s, Action::Job(id));
        assert_eq!(r.error, None);
        assert_eq!(r.jobs.len(), 1);
        assert!(r.estimates.is_empty());
        // Same job as the full listing, execution included.
        assert_eq!(serde_json::to_value(&r.jobs[0]).unwrap(), all[index]);
    }
    assert!(s.job(claimed.id).unwrap().execution.is_some());
    assert!(s.job(queued.id).unwrap().execution.is_none());
}

#[test]
fn job_action_for_a_missing_job_is_an_error_without_listing() {
    let mut s = Store::memory().unwrap();
    s.submit(&spec("synthetic")).unwrap();
    let r = ask(&mut s, Action::Job(999));
    assert_eq!(r.error.as_deref(), Some("job not found"));
    assert!(r.jobs.is_empty());
}

#[test]
fn older_daemons_reject_job_as_an_unknown_variant() {
    // `tasks run` falls back to List when the daemon's error says this.
    let error = serde_json::from_str::<Action>(r#"{"type":"not_an_action","data":1}"#).unwrap_err();
    assert!(error.to_string().contains("unknown variant"), "{error}");
    let line = serde_json::to_string(&Action::Job(7)).unwrap();
    assert_eq!(line, r#"{"type":"job","data":7}"#);
}
