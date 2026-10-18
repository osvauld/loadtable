use super::*;
use crate::execution::ProcessIdentity;
use crate::local_command::{Recipe, Resource};
use std::os::unix::fs::PermissionsExt;

struct Fixture(PathBuf);
impl Fixture {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "task-manager-reconcile-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("sidecars")).unwrap();
        Self(path)
    }
    fn store(&self) -> Store {
        Store::open(&self.0.join("tasks.sqlite")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn argv(client: &str) -> Vec<String> {
    [
        client,
        "--worker",
        "agent@worker.invalid",
        "--who",
        "fixture-agent",
        "--",
        "cargo",
        "test",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// An offload job that was live when its worker went away, now `interrupted`.
fn interrupted_offload(store: &mut Store, client: &str) -> Job {
    let spec = Recipe {
        argv: argv(client),
        resource: Resource::Offload,
        broad_test: false,
    }
    .spec("fixture-agent".into(), PathBuf::from("/"))
    .unwrap();
    let id = store.submit(&spec).unwrap().id;
    store.approve_command(id).unwrap();
    store
        .claim(ProcessIdentity {
            pid: 1,
            start_ticks: 1,
        })
        .unwrap()
        .unwrap();
    assert_eq!(store.reconcile_interrupted().unwrap(), 1);
    let job = store.job(id).unwrap();
    assert_eq!(job.phase, Phase::Interrupted);
    job
}

fn sidecar(stage: &str, remote: Option<i64>) -> Sidecar {
    Sidecar {
        stage: stage.into(),
        worker: Some("agent@worker.invalid".into()),
        remote_job_id: remote,
        remote_phase: None,
        exit: None,
    }
}

fn report(state: &str, phase: Option<&str>, exit: Option<i32>, log: &str) -> ClientReport {
    ClientReport {
        log: log.into(),
        remote_job_id: Some(152),
        remote_phase: phase.map(str::to_owned),
        exit_code: exit,
        state: state.into(),
        detail: "fixture".into(),
    }
}

const FRESH: Duration = Duration::from_secs(60);

#[test]
fn passed_remote_half_is_recorded_without_touching_the_local_phase() {
    let f = Fixture::new("passed");
    let mut store = f.store();
    let job = interrupted_offload(&mut store, "tasks-offload");
    let (r, log) = reconcile_job(&job, Some(sidecar("running", Some(152))), FRESH, |a| {
        assert!(a.iter().any(|x| x == "--worker"));
        Ok(report(
            "terminal",
            Some("passed"),
            Some(0),
            "test result: ok\n",
        ))
    })
    .unwrap();
    assert_eq!(
        (r.state.as_str(), r.remote_job_id, r.exit_code),
        ("terminal", Some(152), Some(0))
    );
    assert_eq!(log.as_deref(), Some("test result: ok\n"));
    assert!(store.write_remote_result(job.id, &r).unwrap());
    let after = store.job(job.id).unwrap();
    assert_eq!(after.phase, Phase::Interrupted, "never re-graded");
    assert_eq!(after.execution.unwrap().remote_result.unwrap(), r);
    // Settled: a later record never replaces it, and the job is no longer a candidate.
    let mut other = r.clone();
    other.remote_phase = Some("failed".into());
    assert!(!store.write_remote_result(job.id, &other).unwrap());
    let job = store.job(job.id).unwrap();
    assert!(reconcile_job(&job, None, FRESH, |_| unreachable!()).is_none());
}

#[test]
fn failed_or_live_or_unreachable_remote_halves_are_never_success() {
    let f = Fixture::new("states");
    let mut store = f.store();
    let job = interrupted_offload(&mut store, "tasks-offload");
    let side = || Some(sidecar("running", Some(152)));
    let (r, _) = reconcile_job(&job, side(), FRESH, |_| {
        Ok(report("terminal", Some("failed"), Some(101), "panicked\n"))
    })
    .unwrap();
    assert_eq!((r.exit_code, r.settled()), (Some(101), true));
    // An interrupted remote job whose earlier stage exited 0 still has no exit code.
    let (r, _) = reconcile_job(&job, side(), FRESH, |_| {
        Ok(report("terminal", Some("interrupted"), Some(0), ""))
    })
    .unwrap();
    assert_eq!(r.exit_code, None);
    let (r, log) = reconcile_job(&job, side(), FRESH, |_| {
        Ok(report("running", Some("running"), None, "x"))
    })
    .unwrap();
    assert_eq!(
        (r.state.as_str(), r.settled(), log),
        ("running", false, None)
    );
    // Unsettled records may be replaced by the next check.
    assert!(store.write_remote_result(job.id, &r).unwrap());
    let (r, _) = reconcile_job(&job, side(), FRESH, |_| Err("ssh: no route".into())).unwrap();
    assert_eq!((r.state.as_str(), r.settled()), ("unreachable", false));
    assert!(store.write_remote_result(job.id, &r).unwrap());
    let (r, _) = reconcile_job(&job, side(), FRESH, |_| {
        Err("UNSUPPORTED: older client".into())
    })
    .unwrap();
    assert_eq!((r.state.as_str(), r.remote_job_id), ("unknown", Some(152)));
    assert!(r.detail.contains("tasks logs 152"), "{}", r.detail);
}

#[test]
fn missing_or_idless_sidecars_and_old_jobs_settle_without_asking_the_worker() {
    let f = Fixture::new("nosidecar");
    let mut store = f.store();
    let job = interrupted_offload(&mut store, "tasks-offload");
    let ask = |_: &[String]| -> std::result::Result<ClientReport, String> {
        panic!("must not contact the worker")
    };
    let (r, _) = reconcile_job(&job, None, FRESH, ask).unwrap();
    assert_eq!((r.state.as_str(), r.remote_job_id), ("unknown", None));
    let (r, _) = reconcile_job(&job, Some(sidecar("submitting", None)), FRESH, ask).unwrap();
    assert!(r.detail.contains("submitting"), "{}", r.detail);
    let (r, _) = reconcile_job(&job, Some(sidecar("running", Some(9))), MAX_AGE * 2, ask).unwrap();
    assert_eq!((r.state.as_str(), r.remote_job_id), ("unknown", Some(9)));
    // The client finished and wrote its final record; only the local completion was lost.
    let mut done = sidecar("done", Some(9));
    done.remote_phase = Some("passed".into());
    done.exit = Some(0);
    let (r, _) = reconcile_job(&job, Some(done), FRESH, ask).unwrap();
    assert_eq!(
        (r.state.as_str(), r.remote_phase.as_deref(), r.exit_code),
        ("terminal", Some("passed"), Some(0))
    );
}

#[test]
fn only_interrupted_ssh_offloads_are_candidates() {
    let mut store = Store::memory().unwrap();
    let spec = Recipe {
        argv: argv("tasks-offload"),
        resource: Resource::Offload,
        broad_test: false,
    }
    .spec("fixture-agent".into(), PathBuf::from("/"))
    .unwrap();
    let id = store.submit(&spec).unwrap().id;
    store.approve_command(id).unwrap();
    let approved = store.job(id).unwrap();
    assert!(reconcile_job(&approved, None, FRESH, |_| unreachable!()).is_none());
    let forward = Recipe {
        argv: crate::peer_link::ForwardArgs {
            state: "/state".into(),
            peer: "worker".into(),
            who: "fixture-agent".into(),
            resource: crate::peer_link::ForwardResource::Compile,
            fetch: vec![],
            argv: vec!["cargo".into(), "check".into()],
        }
        .to_argv("/bin/tasks"),
        resource: Resource::Offload,
        broad_test: false,
    }
    .spec("fixture-agent".into(), PathBuf::from("/"))
    .unwrap();
    let id = store.submit(&forward).unwrap().id;
    store.approve_command(id).unwrap();
    store
        .claim(ProcessIdentity {
            pid: 1,
            start_ticks: 1,
        })
        .unwrap();
    store
        .claim(ProcessIdentity {
            pid: 1,
            start_ticks: 1,
        })
        .unwrap();
    store.reconcile_interrupted().unwrap();
    let forwarded = store.job(id).unwrap();
    assert_eq!(forwarded.phase, Phase::Interrupted);
    // Peer forwards are the peer runtime's; its detail still names the peer.
    assert!(reconcile_job(&forwarded, None, FRESH, |_| unreachable!()).is_none());
    let detail = forwarded.execution.unwrap().detail;
    assert!(detail.contains("Forwarded to peer worker"), "{detail}");
}

#[test]
fn client_output_is_log_then_one_json_line() {
    let out = "line 1\nline 2\n{\"reconcile\": {\"remote_job_id\": 152, \"remote_phase\": \"passed\", \"exit_code\": 0, \"state\": \"terminal\", \"detail\": \"d\"}}\n";
    let r = parse_client_output(out).unwrap();
    assert_eq!(r.log, "line 1\nline 2\n");
    assert_eq!(
        (r.remote_job_id, r.remote_phase.as_deref(), r.state.as_str()),
        (Some(152), Some("passed"), "terminal")
    );
    assert!(parse_client_output("usage: error\n").is_none());
    assert!(parse_client_output("").is_none());
}

#[test]
fn round_runs_the_jobs_own_client_once_and_appends_the_remote_log() {
    let f = Fixture::new("round");
    // A stand-in client with the real name: echoes its argv shape and a reconcile reply.
    let bin = f.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let client = bin.join("tasks-offload");
    std::fs::write(
        &client,
        "#!/bin/sh\n[ \"$1\" = --reconcile-local-job ] || exit 9\necho 'remote: test result: FAILED (panicked at x.rs:1)'\necho '{\"reconcile\": {\"remote_job_id\": 152, \"remote_phase\": \"failed\", \"exit_code\": 101, \"state\": \"terminal\", \"detail\": \"fixture\"}}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&client, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut store = f.store();
    let job = interrupted_offload(&mut store, client.to_str().unwrap());
    std::fs::create_dir_all(f.0.join("jobs").join(job.id.to_string())).unwrap();
    std::fs::write(
        f.0.join("sidecars").join(format!("{}.json", job.id)),
        format!(
            "{{\"version\": 1, \"local_job_id\": {}, \"stage\": \"running\", \"worker\": \"agent@worker.invalid\", \"remote_job_id\": 152}}",
            job.id
        ),
    )
    .unwrap();
    let mut attempts = HashMap::new();
    assert_eq!(
        round_in(&f.0, &f.0.join("sidecars"), &mut attempts).unwrap(),
        1
    );
    let after = store.job(job.id).unwrap();
    assert_eq!(after.phase, Phase::Interrupted);
    let r = after.execution.unwrap().remote_result.unwrap();
    assert_eq!(
        (r.state.as_str(), r.remote_phase.as_deref(), r.exit_code),
        ("terminal", Some("failed"), Some(101))
    );
    let log =
        std::fs::read_to_string(f.0.join("jobs").join(job.id.to_string()).join("job.log")).unwrap();
    assert!(log.contains("panicked at x.rs:1"), "{log}");
    assert!(log.contains("stays interrupted"), "{log}");
    // Settled: the next round neither asks again nor appends again.
    attempts.clear();
    assert_eq!(
        round_in(&f.0, &f.0.join("sidecars"), &mut attempts).unwrap(),
        0
    );
    let again =
        std::fs::read_to_string(f.0.join("jobs").join(job.id.to_string()).join("job.log")).unwrap();
    assert_eq!(log, again);
}

#[test]
fn sidecar_must_match_the_job_and_version() {
    let f = Fixture::new("sidecar");
    let path = f.0.join("s.json");
    std::fs::write(
        &path,
        r#"{"version": 1, "local_job_id": 7, "stage": "running", "remote_job_id": 3}"#,
    )
    .unwrap();
    assert_eq!(read_sidecar(&path, 7).unwrap().remote_job_id, Some(3));
    assert!(read_sidecar(&path, 8).is_none());
    std::fs::write(&path, r#"{"version": 2, "local_job_id": 7}"#).unwrap();
    assert!(read_sidecar(&path, 7).is_none());
}
