//! The job lifecycle, API and wiring of the audit log. Names carry the regression ids they pin.

use crate::api::*;
use crate::audited::{AuditLinks, AuditSink};
use crate::peer_link::PeerLink;
use crate::peer_link::serve::{DaemonRules, DaemonRunner};
use crate::testkit::{PeerWorld, Rig, StubTrust, TempDir, peer_view, rig_identity};
use std::sync::Arc;
use tasks_core::{JobId, Kind, MachineName, Phase};
use tasks_ledger::{
    AuditLedger, AuditLog, AuditRow, Finish, ManualClock, MemoryAudit, StartInfo, TimeoutReason,
};
use tasks_peer::PeerId;
use tasks_peer::receiver::{JobRunner, SubmitPolicy};
use tasks_peer::transport::{LinkInfo, PathKind};

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

/// The DID this machine's audit rows carry: the rig identity's.
fn me() -> String {
    use tasks_identity::Signer;
    rig_identity().did().to_string()
}

fn rows(r: &Rig) -> Vec<AuditRow> {
    r.audit_store.scan(0, 10_000).unwrap()
}

fn of_job(r: &Rig, id: JobId) -> Vec<AuditRow> {
    rows(r)
        .into_iter()
        .filter(|x| x.job == Some(id.0))
        .collect()
}

fn acts(rows: &[AuditRow]) -> Vec<&str> {
    rows.iter().map(|r| r.action.as_str()).collect()
}

fn stub_with(peers: &[(&str, &str)]) -> Arc<StubTrust> {
    let t = Arc::new(StubTrust::default());
    t.set_peers(
        peers
            .iter()
            .map(|(n, d)| peer_view(n, d, &["submit"], &["submit"]))
            .collect(),
    );
    t
}

// R-290
#[test]
fn r290_a_job_leaves_submitted_started_and_ended_rows_without_its_arguments() {
    let r = Rig::new();
    r.procs.auto_exit(0);
    let spec = r.spec("agent-1", &["make", "--password=hunter2", "all"], Kind::Run);
    let Response::Submitted { id, .. } = ok(r.sch.handle(Request::Submit { spec })) else {
        panic!("not submitted");
    };
    r.tick();
    assert_eq!(r.until_done(id), Phase::Passed);
    let mine = of_job(&r, id);
    assert_eq!(acts(&mine), ["job.submitted", "job.started", "job.ended"]);
    assert!(mine.iter().all(|x| x.owner == "agent-1"));
    assert_eq!(mine[0].actor, me());
    let d = mine[0].details_json();
    assert_eq!(
        (d["program"].as_str(), d["kind"].as_str()),
        (Some("make"), Some("run"))
    );
    assert_eq!(d["placement"], "here");
    let end = mine[2].details_json();
    assert_eq!(
        (end["phase"].as_str(), end["exit"].as_i64()),
        (Some("passed"), Some(0))
    );
    let dump = format!("{:?}", rows(&r));
    assert!(
        !dump.contains("hunter2"),
        "the command line reached the audit log"
    );
}

// R-290
#[test]
fn r290_a_failed_job_and_a_job_that_never_started_are_recorded_with_their_phase() {
    let r = Rig::new();
    r.procs.auto_exit(3);
    let id = r.submit(&r.spec("o", &["make", "x"], Kind::Run));
    r.tick();
    assert!(matches!(r.until_done(id), Phase::Failed { .. }));
    let end = of_job(&r, id).pop().unwrap();
    assert_eq!(end.action, "job.ended");
    assert_eq!(end.details_json()["phase"], "failed");
    assert_eq!(end.details_json()["exit"], 3);
    // an end that the ledger refuses (a finished job is immutable) adds no row
    let before = rows(&r).len();
    assert!(
        r.ledger
            .finish(id, &Finish::from_phase(Phase::Passed))
            .is_err()
    );
    assert_eq!(rows(&r).len(), before);
}

// R-279
#[test]
fn r279_a_cancel_records_who_asked_and_why_and_a_repeat_adds_nothing() {
    let r = Rig::new();
    ok(r.sch.handle(Request::Execution { enabled: false }));
    let id = r.submit(&r.spec("agent-1", &["make", "x"], Kind::Run));
    // somebody else cannot cancel it, and a refusal is not a cancel
    assert_eq!(
        err(r.sch.handle(Request::Cancel {
            id,
            who: "agent-2".into(),
            reason: Some("mine now".into()),
        }))
        .code,
        ErrorCode::NotOwner
    );
    assert!(of_job(&r, id).iter().all(|x| x.action != "job.cancelled"));
    for _ in 0..2 {
        ok(r.sch.handle(Request::Cancel {
            id,
            who: "agent-1".into(),
            reason: Some("wrong branch".into()),
        }));
    }
    let cancels: Vec<AuditRow> = of_job(&r, id)
        .into_iter()
        .filter(|x| x.action == "job.cancelled")
        .collect();
    assert_eq!(cancels.len(), 1, "{:?}", acts(&of_job(&r, id)));
    let c = &cancels[0];
    assert_eq!(c.owner, "agent-1");
    let d = c.details_json();
    assert_eq!(
        (d["by"].as_str(), d["reason"].as_str(), d["result"].as_str()),
        (Some("agent-1"), Some("wrong branch"), Some("cancelled"))
    );
}

// R-279
#[test]
fn r279_cancelling_a_running_job_records_the_request_and_then_the_end() {
    let r = Rig::new();
    let id = r.submit(&r.spec("agent-1", &["make", "x"], Kind::Run));
    r.tick();
    r.until_running(id);
    ok(r.sch.handle(Request::Cancel {
        id,
        who: "agent-1".into(),
        reason: None,
    }));
    assert_eq!(r.until_done(id), Phase::Cancelled);
    let mine = of_job(&r, id);
    assert_eq!(
        acts(&mine),
        ["job.submitted", "job.started", "job.cancelled", "job.ended"]
    );
    assert_eq!(mine[2].details_json()["result"], "requested");
    assert!(mine[2].details_json().get("reason").is_none());
    assert_eq!(mine[3].details_json()["phase"], "cancelled");
}

// R-279
#[test]
fn r279_a_dependency_failure_cancels_with_that_as_the_requester() {
    let r = Rig::new();
    r.procs.auto_exit(1);
    let first = r.submit(&r.spec("o", &["make", "a"], Kind::Run));
    let mut dependent = r.spec("o", &["make", "b"], Kind::Run);
    dependent.after = vec![first];
    let second = r.submit(&dependent);
    r.tick();
    r.until_done(first);
    r.tick();
    let c = of_job(&r, second)
        .into_iter()
        .find(|x| x.action == "job.cancelled")
        .unwrap();
    assert_eq!(c.details_json()["by"], "dependency failed");
}

// R-280
#[test]
fn r280_a_job_from_a_peer_is_recorded_under_the_peers_did_with_the_owner_as_given() {
    let r = Rig::new();
    r.audit
        .set_trust(stub_with(&[("laptop", "did:key:zLaptop")]));
    let run = DaemonRunner::new(r.sch.clone());
    let peer = PeerId::new("laptop-node").unwrap();
    let key = tasks_peer::SubmitKey::new("laptop:41").unwrap();
    let spec = r.spec("origin-owner", &["make", "x"], Kind::Run);
    let (remote, dup) = run.submit(&peer, &key, spec.clone()).unwrap();
    assert!(!dup);
    // the same key again is the same job and adds no row
    run.submit(&peer, &key, spec).unwrap();
    let id = JobId(i64::try_from(remote.0).unwrap());
    let got: Vec<AuditRow> = of_job(&r, id);
    assert_eq!(acts(&got), ["job.received"]);
    let row = &got[0];
    assert_eq!(row.actor, "did:key:zLaptop");
    assert_eq!(row.peer.as_deref(), Some("did:key:zLaptop"));
    assert_eq!(row.owner, "origin-owner");
    assert_eq!(row.details_json()["origin_node"], "laptop-node");
    // what happens to it here is this machine's act, still naming the peer it is for
    r.procs.auto_exit(0);
    r.tick();
    r.until_done(id);
    let started = of_job(&r, id)
        .into_iter()
        .find(|x| x.action == "job.started")
        .unwrap();
    assert_eq!(started.actor, me());
    assert_eq!(started.peer.as_deref(), Some("did:key:zLaptop"));
    // and the peer can be found by its name or its DID
    for who in ["laptop", "did:key:zLaptop"] {
        let Response::Audit { rows, .. } = ok(r.sch.handle(Request::Audit {
            since_ms: None,
            until_ms: None,
            peer: Some(who.into()),
            job: None,
            action: None,
            limit: None,
        })) else {
            panic!()
        };
        assert!(rows.len() >= 2, "{who}: {rows:?}");
        assert!(
            rows.iter()
                .any(|x| x.peer_name.as_deref() == Some("laptop"))
        );
    }
}

// R-280
#[test]
fn r280_a_peer_that_is_not_paired_leaves_the_node_id_and_this_machine_as_actor() {
    let r = Rig::new();
    let run = DaemonRunner::new(r.sch.clone());
    let peer = PeerId::new("stranger-node").unwrap();
    let key = tasks_peer::SubmitKey::new("x:1").unwrap();
    let (remote, _) = run
        .submit(&peer, &key, r.spec("o", &["make", "x"], Kind::Run))
        .unwrap();
    let row = of_job(&r, JobId(i64::try_from(remote.0).unwrap())).remove(0);
    assert_eq!(row.action, "job.received");
    assert_eq!(
        (row.actor.as_str(), row.peer.as_deref()),
        (me().as_str(), None)
    );
    assert_eq!(row.details_json()["origin_node"], "stranger-node");
}

// R-279
#[test]
fn r279_a_peer_cancelling_its_job_is_the_actor_of_the_cancel() {
    let r = Rig::new();
    r.audit
        .set_trust(stub_with(&[("laptop", "did:key:zLaptop")]));
    let run = DaemonRunner::new(r.sch.clone());
    let peer = PeerId::new("laptop-node").unwrap();
    let key = tasks_peer::SubmitKey::new("laptop:7").unwrap();
    ok(r.sch.handle(Request::Execution { enabled: false }));
    let (remote, _) = run
        .submit(&peer, &key, r.spec("o", &["make", "x"], Kind::Run))
        .unwrap();
    assert!(run.cancel(&peer, remote).is_some());
    let c = of_job(&r, JobId(i64::try_from(remote.0).unwrap()))
        .into_iter()
        .find(|x| x.action == "job.cancelled")
        .unwrap();
    assert_eq!(c.actor, "did:key:zLaptop");
    assert_eq!(c.details_json()["by"], "peer laptop-node");
}

// R-280
#[test]
fn r280_forwarding_a_job_records_the_peers_did_once() {
    let r = Rig::new();
    r.audit.set_trust(stub_with(&[("nixos", "did:key:zNixos")]));
    let id = r.submit(&r.spec("agent-1", &["cargo", "test"], Kind::Test));
    let nixos = MachineName("nixos".into());
    r.ledger
        .set_forward(id, &nixos, None, Some("testbox:1"))
        .unwrap();
    // learning the remote id later is not a second forward
    r.ledger.set_forward(id, &nixos, Some(5), None).unwrap();
    let fwd: Vec<AuditRow> = of_job(&r, id)
        .into_iter()
        .filter(|x| x.action == "job.forwarded")
        .collect();
    assert_eq!(fwd.len(), 1);
    assert_eq!(fwd[0].peer.as_deref(), Some("did:key:zNixos"));
    assert_eq!(fwd[0].owner, "agent-1");
    assert_eq!(fwd[0].details_json()["peer_name"], "nixos");
}

// R-287
#[test]
fn r287_a_job_refused_by_a_submit_rule_is_recorded_with_the_rule_id() {
    let r = Rig::new();
    let broad = r.spec("agent-1", &["cargo", "test", "some_filter"], Kind::Test);
    let e = err(r.sch.handle(Request::Submit { spec: broad }));
    assert_eq!(e.code, ErrorCode::Refused);
    let phone = r.spec(
        "agent-1",
        &["adb", "-s", "zd22257p6g", "devices"],
        Kind::Run,
    );
    assert_eq!(
        err(r.sch.handle(Request::Submit { spec: phone })).code,
        ErrorCode::Refused
    );
    let refused: Vec<AuditRow> = rows(&r)
        .into_iter()
        .filter(|x| x.action == "job.refused")
        .collect();
    assert_eq!(refused.len(), 2);
    assert_eq!(
        refused[0].details_json()["rules"],
        serde_json::json!(["R-191"])
    );
    assert_eq!(
        refused[1].details_json()["rules"],
        serde_json::json!(["R-210"])
    );
    assert!(refused.iter().all(|x| x.owner == "agent-1"));
    assert_eq!(refused[0].details_json()["via"], "local");
    // nothing was queued, so nothing else was recorded about a job
    assert!(rows(&r).iter().all(|x| x.job.is_none()));
    // the rule ids come from the rules themselves
    assert_eq!(tasks_policy::submit_rules::Rule::CargoJobs.id(), "R-091");
    // the denied serial itself is not in the log
    assert!(
        !format!("{:?}", rows(&r))
            .to_lowercase()
            .contains("zd22257p6g")
    );
}

// R-287
#[test]
fn r287_a_peers_refused_job_names_the_peer_as_actor() {
    let r = Rig::new();
    r.audit
        .set_trust(stub_with(&[("laptop", "did:key:zLaptop")]));
    let rules = DaemonRules::new(r.sch.clone());
    let peer = PeerId::new("laptop-node").unwrap();
    let spec = r.spec(
        "origin-owner",
        &["adb", "-s", "zd22257p6g", "devices"],
        Kind::Run,
    );
    assert!(rules.check_from(&peer, &spec).is_err());
    let row = rows(&r).pop().unwrap();
    assert_eq!(row.action, "job.refused");
    assert_eq!(row.actor, "did:key:zLaptop");
    assert_eq!(row.details_json()["via"], "peer");
    // an allowed spec records nothing
    let before = rows(&r).len();
    assert!(
        rules
            .check_from(&peer, &r.spec("o", &["make", "x"], Kind::Run))
            .is_ok()
    );
    assert_eq!(rows(&r).len(), before);
}

// R-288
#[test]
fn r288_drain_and_resume_record_who_and_a_repeat_adds_nothing() {
    let r = Rig::new();
    for (cancel, who) in [
        (false, Some("agent-1")),
        (false, Some("agent-2")),
        (true, Some("agent-3")),
        (true, None),
    ] {
        ok(r.sch.handle(Request::Drain {
            cancel,
            who: who.map(String::from),
        }));
    }
    let q: Vec<AuditRow> = rows(&r)
        .into_iter()
        .filter(|x| x.action.starts_with("queue."))
        .collect();
    assert_eq!(acts(&q), ["queue.drain", "queue.resume"]);
    assert_eq!(
        (q[0].owner.as_str(), q[1].owner.as_str()),
        ("agent-1", "agent-3")
    );
    ok(r.sch.handle(Request::Drain {
        cancel: false,
        who: None,
    }));
    assert_eq!(rows(&r).last().unwrap().owner, "unknown");
}

// R-289
#[test]
fn r289_a_timeout_is_recorded_with_which_limit_and_then_the_end() {
    let r = Rig::new();
    let id = r.submit(&r.spec("o", &["make", "x"], Kind::Run));
    r.ledger
        .start(
            id,
            &StartInfo {
                machine: MachineName("testbox".into()),
                lease: tasks_ledger::LeaseRecord {
                    pgid: 1,
                    leader_start: 1,
                },
                log_path: None,
            },
        )
        .unwrap();
    r.clock.advance(5_000);
    let mut fin = Finish::from_phase(Phase::TimedOut { exit: None });
    fin.timeout = Some(TimeoutReason::Idle);
    r.ledger.finish(id, &fin).unwrap();
    let mine = of_job(&r, id);
    assert_eq!(
        acts(&mine),
        ["job.submitted", "job.started", "job.timeout", "job.ended"]
    );
    assert_eq!(mine[2].details_json()["limit"], "idle");
    let end = mine[3].details_json();
    assert_eq!(
        (end["phase"].as_str(), end["wall_ms"].as_i64()),
        (Some("timed_out"), Some(5_000))
    );
}

// R-282
#[test]
fn r282_the_audit_request_filters_limits_and_names_the_machine_and_its_peers() {
    let r = Rig::new();
    r.audit.set_trust(stub_with(&[("nixos", "did:key:zNixos")]));
    let a = r.submit(&r.spec("agent-1", &["make", "a"], Kind::Run));
    let b = r.submit(&r.spec("agent-2", &["make", "b"], Kind::Run));
    r.ledger
        .set_forward(b, &MachineName("nixos".into()), None, Some("k"))
        .unwrap();
    let q = |job: Option<i64>, act: Option<&str>, peer: Option<&str>, limit: Option<usize>| {
        r.sch.handle(Request::Audit {
            since_ms: None,
            until_ms: None,
            peer: peer.map(String::from),
            job,
            action: act.map(String::from),
            limit,
        })
    };
    let Response::Audit {
        rows: all,
        truncated,
    } = ok(q(None, None, None, None))
    else {
        panic!()
    };
    assert_eq!((all.len(), truncated), (3, false));
    // newest first, own rows carry this machine's name
    assert_eq!(all[0].action, "job.forwarded");
    assert_eq!(all[0].peer_name.as_deref(), Some("nixos"));
    assert_eq!(all[2].actor_name.as_deref(), Some("testbox"));
    let Response::Audit { rows: one, .. } = ok(q(Some(a.0), None, None, None)) else {
        panic!()
    };
    assert_eq!(one.len(), 1);
    let Response::Audit { rows: fam, .. } = ok(q(None, Some("job"), Some("nixos"), None)) else {
        panic!()
    };
    assert_eq!(
        fam.iter().map(|x| x.action.as_str()).collect::<Vec<_>>(),
        ["job.forwarded"]
    );
    let Response::Audit {
        rows: cut,
        truncated,
    } = ok(q(None, None, None, Some(2)))
    else {
        panic!()
    };
    assert_eq!((cut.len(), truncated), (2, true));
    for bad in [0, MAX_AUDIT_ROWS + 1] {
        assert_eq!(
            err(q(None, None, None, Some(bad))).code,
            ErrorCode::BadRequest
        );
    }
    // an older client's request without the new optional fields still parses
    let line = br#"{"v":1,"request":{"op":"audit"}}"#;
    assert!(matches!(
        parse_request(line),
        Ok(Request::Audit { limit: None, .. })
    ));
    assert!(matches!(
        parse_request(br#"{"v":1,"request":{"op":"audit_verify"}}"#),
        Ok(Request::AuditVerify)
    ));
}

// R-282
#[test]
fn r282_verify_reports_a_clean_chain_a_signed_checkpoint_and_the_first_break() {
    let r = Rig::new();
    for i in 0..4 {
        r.submit(&r.spec("o", &["make", &format!("t{i}")], Kind::Run));
    }
    let Response::AuditVerify(v) = ok(r.sch.handle(Request::AuditVerify)) else {
        panic!()
    };
    assert!(v.ok, "{v:?}");
    assert_eq!(
        (v.rows, v.first_seq, v.last_seq, v.signed_through),
        (4, Some(1), Some(4), None)
    );
    let Response::AuditCheckpoint { seq } = ok(r.sch.handle(Request::AuditCheckpoint)) else {
        panic!()
    };
    assert_eq!(seq, Some(5));
    let Response::AuditVerify(v) = ok(r.sch.handle(Request::AuditVerify)) else {
        panic!()
    };
    assert!(v.ok);
    assert_eq!((v.checkpoints, v.signed_through), (1, Some(5)));
    r.audit_store
        .tamper(|rows| rows[1].owner = "mallory".into());
    let Response::AuditVerify(v) = ok(r.sch.handle(Request::AuditVerify)) else {
        panic!()
    };
    assert!(!v.ok);
    let p = v.problem.unwrap();
    assert_eq!((p.seq, p.kind.as_str()), (2, "edited"));
}

// R-292
#[test]
fn r292_old_rows_are_pruned_by_the_policys_retention_and_the_chain_still_verifies() {
    let r = Rig::with_policy("[audit]\nretention_days = 1\ncheckpoint_every = 0\n");
    // the old jobs must stay queued, so the only rows are the ones the test writes
    ok(r.sch.handle(Request::Execution { enabled: false }));
    for i in 0..5 {
        r.submit(&r.spec("o", &["make", &format!("t{i}")], Kind::Run));
    }
    r.clock.advance(3 * 86_400_000);
    let fresh = r.submit(&r.spec("o", &["make", "fresh"], Kind::Run));
    // the first tick runs the hourly maintenance, which prunes
    r.sch.tick();
    let left = rows(&r);
    assert!(
        left.iter().any(|x| x.action == "audit.pruned"),
        "{:?}",
        acts(&left)
    );
    assert!(left.iter().any(|x| x.job == Some(fresh.0)));
    assert!(left.iter().all(|x| x.job.is_none_or(|j| j >= fresh.0)));
    let Response::AuditVerify(v) = ok(r.sch.handle(Request::AuditVerify)) else {
        panic!()
    };
    assert!(v.ok, "{v:?}");
    assert!(v.pruned_before.is_some());
}

// R-315
#[test]
fn r315_sync_traffic_refusals_and_retention_are_audited_with_tree_ids_and_bytes() {
    use tasks_peer::sync::{Deleted, SyncEvent, SyncObserver};
    let clock = Arc::new(ManualClock::new(1_000_000));
    let store = Arc::new(MemoryAudit::new());
    let sink = AuditSink::new(Arc::new(AuditLog::new(store.clone(), clock, "did:key:zMe")));
    sink.set_trust(stub_with(&[("laptop", "did:key:zLaptop")]));
    let obs = crate::audited::AuditSync::new(sink);
    let peer = PeerId::new("laptop-node").unwrap();
    let checkout = tasks_peer::CheckoutId::derive("laptop", std::path::Path::new("/w"));
    let id = |c: char| tasks_peer::GitId::parse(&c.to_string().repeat(40)).unwrap();
    obs.event(&SyncEvent::Sent {
        peer: peer.clone(),
        checkout: checkout.clone(),
        job: Some(7),
        tree: id('a'),
        base: Some(id('b')),
        dirty: 3,
        files: 120,
        bytes: 4096,
    });
    obs.event(&SyncEvent::Received {
        peer: peer.clone(),
        checkout: checkout.clone(),
        tree: id('a'),
        bytes: 4096,
        written: 3,
        removed: 1,
    });
    obs.event(&SyncEvent::Refused {
        peer: peer.clone(),
        checkout: checkout.clone(),
        job: Some(8),
        files: vec![("models/net.bin".into(), "lfs_pointer")],
    });
    obs.event(&SyncEvent::Deleted {
        kind: Deleted::WorkingCopy,
        peer: Some(peer),
        target: checkout.to_string(),
        bytes: 123_456,
    });
    obs.event(&SyncEvent::Deleted {
        kind: Deleted::OriginObjects,
        peer: None,
        target: "repo".into(),
        bytes: 9,
    });
    let rows = store.scan(0, 100).unwrap();
    assert_eq!(
        acts(&rows),
        [
            "data.sync_sent",
            "data.sync_received",
            "data.sync_refused",
            "data.retention",
            "data.retention"
        ]
    );
    let sent = rows[0].details_json();
    assert_eq!(rows[0].peer.as_deref(), Some("did:key:zLaptop"));
    assert_eq!(rows[0].job, Some(7));
    assert_eq!(sent["tree"], "a".repeat(40));
    assert_eq!(sent["base"], "b".repeat(40));
    assert_eq!(
        (sent["bytes"].as_u64(), sent["dirty_files"].as_u64()),
        (Some(4096), Some(3))
    );
    assert_eq!(rows[1].details_json()["written"], 3);
    assert_eq!(
        rows[2].details_json()["files"][0],
        "models/net.bin: lfs_pointer"
    );
    let del = rows[3].details_json();
    assert_eq!(
        (del["what"].as_str(), del["bytes_freed"].as_u64()),
        (Some("working_copy"), Some(123_456))
    );
    assert_eq!(rows[3].peer.as_deref(), Some("did:key:zLaptop"));
    assert_eq!(rows[4].details_json()["what"], "origin_objects");
    assert_eq!(rows[4].peer, None);
}

// R-293
#[test]
fn r293_link_probes_that_find_nothing_new_add_no_rows_and_state_changes_add_one_each() {
    let w = PeerWorld::new();
    let clock = Arc::new(ManualClock::new(1_000_000));
    let store = Arc::new(MemoryAudit::new());
    let log =
        AuditLog::new(store.clone(), clock.clone(), "did:key:zMe").with_signer(rig_identity());
    log.set_checkpoint_every(0);
    let sink = AuditSink::new(Arc::new(log));
    let mut opts = w.options();
    opts.peers[0].did = "did:key:zNixos".into();
    opts.observer = Some(Arc::new(AuditLinks::new(sink.clone(), clock.clone())));
    let link = PeerLink::new(w.net.transport(&w.origin), opts);
    let n = || store.scan(0, 1000).unwrap();
    let last = || n().pop().unwrap();

    // 200 probes of a healthy peer, five seconds apart: one row, the link coming up
    for _ in 0..200 {
        clock.advance(5_000);
        link.probe_all();
    }
    assert_eq!(acts(&n()), ["conn.up"]);
    assert_eq!(last().peer.as_deref(), Some("did:key:zNixos"));
    assert_eq!(last().details_json()["peer_name"], "nixos");

    // going from an unknown path to a known one is not a path change; direct to relay is
    w.net.set_link_info(Some(LinkInfo {
        path: PathKind::Direct,
        rtt: None,
    }));
    clock.advance(5_000);
    link.probe_all();
    assert_eq!(n().len(), 1);
    w.net.set_link_info(Some(LinkInfo {
        path: PathKind::Relay,
        rtt: None,
    }));
    for _ in 0..20 {
        clock.advance(5_000);
        link.probe_all();
    }
    assert_eq!(acts(&n()), ["conn.up", "conn.path"]);
    let d = last().details_json();
    assert_eq!(
        (d["from"].as_str(), d["to"].as_str()),
        (Some("direct"), Some("relay"))
    );

    // down once however many probes fail, up once when it answers again
    w.net.set_down(&w.nixos, true);
    for _ in 0..30 {
        clock.advance(5_000);
        link.probe_all();
    }
    assert_eq!(acts(&n()), ["conn.up", "conn.path", "conn.down"]);
    w.net.set_down(&w.nixos, false);
    for _ in 0..30 {
        clock.advance(5_000);
        link.probe_all();
    }
    assert_eq!(acts(&n()), ["conn.up", "conn.path", "conn.down", "conn.up"]);
}

// R-293
#[test]
fn r293_a_link_that_flaps_every_probe_is_capped_per_minute() {
    let w = PeerWorld::new();
    let clock = Arc::new(ManualClock::new(1_000_000));
    let store = Arc::new(MemoryAudit::new());
    let sink = AuditSink::new(Arc::new(AuditLog::new(store.clone(), clock.clone(), "me")));
    let mut opts = w.options();
    opts.observer = Some(Arc::new(AuditLinks::new(sink, clock.clone())));
    let link = PeerLink::new(w.net.transport(&w.origin), opts);
    for i in 0..40 {
        w.net.set_down(&w.nixos, i % 2 == 0);
        clock.advance(1_000);
        link.probe_all();
    }
    let n = store.scan(0, 1000).unwrap().len();
    assert!(n <= 6, "{n} rows for 40 flaps inside a minute");
    // after the window the next change is recorded and says how many were dropped
    clock.advance(61_000);
    w.net.set_down(&w.nixos, false);
    link.probe_all();
    w.net.set_down(&w.nixos, true);
    link.probe_all();
    let all = store.scan(0, 1000).unwrap();
    assert!(all.len() > n);
    assert!(
        all.last().unwrap().details_json()["suppressed"]
            .as_u64()
            .unwrap()
            > 0
    );
}

// R-294
#[test]
fn r294_a_real_daemon_audits_to_the_ledger_signs_checkpoints_and_keeps_rows_across_a_restart() {
    use crate::server::Daemon;
    let dir = TempDir::new("audit-daemon");
    let procs = Arc::new(crate::testkit::FakeProcs::new());
    procs.auto_exit(0);
    let spec_of = |d: &Daemon| {
        let cwd = d.scheduler().config().state_dir.clone();
        tasks_core::JobSpec {
            owner: tasks_core::Owner::new("agent-1").unwrap(),
            argv: vec!["make".into(), "x".into()],
            cwd,
            env: vec![],
            kind: Kind::Run,
            requirements: tasks_core::Requirements::default(),
            placement: tasks_core::Placement::Here,
            after: vec![],
            timeout_secs: None,
            broad_test_escape: false,
            script_escape: false,
            priority: tasks_core::Priority::Normal,
            priority_reason: None,
            fetch: Vec::new(),
            isolate: false,
            light: false,
            retry_alone: false,
        }
    };
    let first_rows = {
        let d = Daemon::start(crate::tests::daemon_options(&dir, &procs)).unwrap();
        let sch = d.scheduler().clone();
        let Response::Submitted { id, .. } = ok(sch.handle(Request::Submit { spec: spec_of(&d) }))
        else {
            panic!()
        };
        let t0 = std::time::Instant::now();
        loop {
            sch.tick();
            if let Response::Job { job } = ok(sch.handle(Request::Get { id }))
                && job.phase.is_terminal()
            {
                break;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let Response::AuditCheckpoint { seq } = ok(sch.handle(Request::AuditCheckpoint)) else {
            panic!()
        };
        assert!(seq.is_some());
        let Response::AuditVerify(v) = ok(sch.handle(Request::AuditVerify)) else {
            panic!()
        };
        assert!(v.ok, "{v:?}");
        assert_eq!(v.checkpoints, 1);
        v.rows
    };
    // a second daemon on the same state keeps the chain and appends to it
    let d = Daemon::start(crate::tests::daemon_options(&dir, &procs)).unwrap();
    let sch = d.scheduler().clone();
    ok(sch.handle(Request::Drain {
        cancel: false,
        who: Some("agent-1".into()),
    }));
    let Response::AuditVerify(v) = ok(sch.handle(Request::AuditVerify)) else {
        panic!()
    };
    assert!(v.ok, "{v:?}");
    assert!(
        v.rows > first_rows,
        "{} rows before, {} after",
        first_rows,
        v.rows
    );
    assert_eq!(v.checkpoints, 1);
    let Response::Audit { rows, .. } = ok(sch.handle(Request::Audit {
        since_ms: None,
        until_ms: None,
        peer: None,
        job: None,
        action: Some("queue".into()),
        limit: None,
    })) else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].owner, "agent-1");
    // the rows are signed by this machine's identity, whose DID is the actor
    let Response::Identity(me) = ok(sch.handle(Request::Identity)) else {
        panic!()
    };
    assert_eq!(rows[0].actor, me.did);
}

// R-696
#[test]
fn r696_a_probe_that_fails_for_a_new_reason_writes_one_daemon_log_line() {
    use std::sync::Mutex;
    let w = PeerWorld::new();
    let clock = Arc::new(ManualClock::new(1_000_000));
    let sink = AuditSink::new(Arc::new(AuditLog::new(
        Arc::new(MemoryAudit::new()),
        clock.clone(),
        "me",
    )));
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = lines.clone();
    let observer = AuditLinks::new(sink, clock.clone())
        .with_say(Box::new(move |l| seen.lock().unwrap().push(l.to_string())));
    let mut opts = w.options();
    opts.observer = Some(Arc::new(observer));
    let link = PeerLink::new(w.net.transport(&w.origin), opts);
    let count = || lines.lock().unwrap().len();

    // healthy: nothing to say
    for _ in 0..5 {
        clock.advance(5_000);
        link.probe_all();
    }
    assert_eq!(count(), 0);

    // down: one line with the peer and the reason, however many probes fail the same way
    w.net.set_down(&w.nixos, true);
    for _ in 0..30 {
        clock.advance(5_000);
        link.probe_all();
    }
    assert_eq!(count(), 1);
    let first = lines.lock().unwrap()[0].clone();
    assert!(first.starts_with("peer nixos unreachable: "), "{first}");

    // a different reason is one more line
    w.net.set_down(&w.nixos, false);
    w.net.drop_replies_of("status_request", true);
    for _ in 0..10 {
        clock.advance(5_000);
        link.probe_all();
    }
    assert_eq!(count(), 2);
    assert!(
        lines.lock().unwrap()[1].contains("reply lost"),
        "{:?}",
        lines.lock().unwrap()
    );
}

// R-696
#[test]
fn r696_a_peer_that_flips_between_reasons_cannot_flood_the_log() {
    use std::sync::Mutex;
    let w = PeerWorld::new();
    let clock = Arc::new(ManualClock::new(1_000_000));
    let sink = AuditSink::new(Arc::new(AuditLog::new(
        Arc::new(MemoryAudit::new()),
        clock.clone(),
        "me",
    )));
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = lines.clone();
    let observer = AuditLinks::new(sink, clock.clone())
        .with_say(Box::new(move |l| seen.lock().unwrap().push(l.to_string())));
    let mut opts = w.options();
    opts.observer = Some(Arc::new(observer));
    let link = PeerLink::new(w.net.transport(&w.origin), opts);
    for i in 0..40 {
        w.net.set_down(&w.nixos, i % 2 == 0);
        w.net.drop_replies_of("status_request", i % 2 == 1);
        clock.advance(1_000);
        link.probe_all();
    }
    let n = lines.lock().unwrap().len();
    assert!(n <= 6, "{n} lines for 40 changes inside a minute");
    // the next line after the window says how many were left out
    clock.advance(61_000);
    w.net.set_down(&w.nixos, true);
    w.net.drop_replies_of("status_request", false);
    link.probe_all();
    let all = lines.lock().unwrap();
    assert!(all.len() > n);
    assert!(
        all.last().unwrap().contains("similar line(s) left out"),
        "{:?}",
        all.last()
    );
}
