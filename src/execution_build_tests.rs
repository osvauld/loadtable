use super::*;
use crate::{Cost, JobSpec, snapshot::Snapshot};

// In-memory experimental approvals and synthetic observations ONLY.
// Normal catalog approval remains disabled; these do not prove an Android build.
fn claimed() -> (Store, i64, BuildArtifact) {
    let mut s = Store::memory().unwrap();
    let spec = JobSpec {
        suite: catalog::ANDROID_SUITE.into(),
        configuration: "debug/x86_64/rust-debug/offline".into(),
        cost: Cost::CpuSimulation,
        source: "/synthetic/repo".into(),
        revision: "a".repeat(40),
        owner: "synthetic".into(),
    };
    let j = s.submit(&spec).unwrap();
    s.approve_android_trial(j.id).unwrap();
    s.claim(ProcessIdentity {
        pid: 1,
        start_ticks: 1,
    })
    .unwrap();
    let mut e = s.execution(j.id).unwrap().unwrap();
    e.snapshot = Some(Snapshot {
        digest: "b".repeat(64),
        source: spec.source.clone(),
        revision: spec.revision.clone(),
        files: 1,
        bytes: 1,
    });
    e.stages = vec![StageTiming {
        stage: "build".into(),
        seconds: Some(2.0),
        wait_seconds: 1.0,
        exit_code: Some(0),
        signal: None,
    }];
    s.update_execution(j.id, Phase::Building, e).unwrap();
    let artifact = BuildArtifact {
        path: "apk/app-debug.apk".into(),
        sha256: "c".repeat(64),
        bytes: 123,
        source_digest: "b".repeat(64),
        configuration: spec.configuration,
        provenance_sha256: "d".repeat(64),
    };
    (s, j.id, artifact)
}
#[test]
fn build_only_completion_is_atomic_and_never_invents_test_runtime() {
    let (mut s, id, artifact) = claimed();
    s.complete_build_success(id, artifact).unwrap();
    let job = s.job(id).unwrap();
    assert_eq!(job.phase, Phase::Passed);
    let e = job.execution.unwrap();
    assert_eq!(e.stages.len(), 1);
    assert!(e.build_artifact.is_some());
    assert!(e.detail.contains("no tests"));
    assert_eq!(
        s.estimate(
            catalog::ANDROID_SUITE,
            "debug/x86_64/rust-debug/offline",
            "worker.build"
        )
        .unwrap()
        .samples(),
        1
    );
    assert_eq!(
        s.estimate(
            catalog::ANDROID_SUITE,
            "debug/x86_64/rust-debug/offline",
            "worker.run"
        )
        .unwrap()
        .samples(),
        0
    );
    assert!(s.complete_success(id).is_err());
}
#[test]
fn build_cancel_race_keeps_results_but_does_not_learn_success() {
    let (mut s, id, artifact) = claimed();
    s.cancel(id).unwrap();
    s.complete_build_success(id, artifact).unwrap();
    assert_eq!(s.job(id).unwrap().phase, Phase::Cancelled);
    assert!(s.estimates().unwrap().is_empty());
    assert!(s.execution(id).unwrap().unwrap().build_artifact.is_some());
}
#[test]
fn mismatched_receipts_and_bad_observations_roll_back_completion() {
    for case in 0..7 {
        let (mut s, id, mut artifact) = claimed();
        match case {
            0 => artifact.source_digest = "f".repeat(64),
            1 => artifact.configuration = "release".into(),
            2 => artifact.path = "../../foreign.apk".into(),
            3 => artifact.bytes = 0,
            4 => artifact.provenance_sha256 = "bad".into(),
            5 => {
                let mut e = s.execution(id).unwrap().unwrap();
                e.stages[0].signal = Some(9);
                s.update_execution(id, Phase::Building, e).unwrap();
            }
            _ => {
                let mut e = s.execution(id).unwrap().unwrap();
                e.stages[0].seconds = Some(-1.0);
                s.update_execution(id, Phase::Building, e).unwrap();
            }
        }
        assert!(s.complete_build_success(id, artifact).is_err());
        assert_eq!(s.job(id).unwrap().phase, Phase::Building);
        assert!(s.estimates().unwrap().is_empty());
    }
}
#[test]
fn test_completion_cannot_bypass_build_only_contract_and_legacy_wire_is_readable() {
    let (mut s, id, _) = claimed();
    let mut e = s.execution(id).unwrap().unwrap();
    let mut old = serde_json::to_value(&e).unwrap();
    old.as_object_mut().unwrap().remove("build_artifact");
    old.as_object_mut().unwrap().remove("android_trial");
    assert!(
        serde_json::from_value::<Execution>(old)
            .unwrap()
            .build_artifact
            .is_none()
    );
    e.stages.push(StageTiming {
        stage: "run".into(),
        seconds: Some(1.0),
        exit_code: Some(0),
        ..Default::default()
    });
    s.update_execution(id, Phase::Running, e).unwrap();
    assert!(s.complete_success(id).is_err());
    assert!(s.estimates().unwrap().is_empty());
}
#[test]
fn android_approval_is_still_gated_and_contract_is_typed() {
    let (mut s, id, _) = claimed();
    let spec = s.job(id).unwrap().spec;
    let proposal = s.submit(&spec).unwrap();
    assert!(s.approve(proposal.id).is_err());
    assert!(catalog::validate_claimed(&s.job(proposal.id).unwrap()).is_err());
    s.approve_android_trial(proposal.id).unwrap();
    assert!(s.execution(proposal.id).unwrap().unwrap().android_trial);
    assert!(catalog::validate_claimed(&s.job(proposal.id).unwrap()).is_ok());
    assert!(s.approve_android_trial(proposal.id).is_err());
    assert!(
        !catalog::catalog()
            .iter()
            .find(|s| s.name == catalog::ANDROID_SUITE)
            .unwrap()
            .executable
    );
    assert!(catalog::android_contract(&spec).is_ok());
    let mut bad = spec;
    bad.configuration = "debug/x86_64;adb install/rust-debug/offline".into();
    assert!(catalog::android_contract(&bad).is_err());
}
