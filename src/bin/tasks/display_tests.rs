use super::*;
use tinline_task_manager::execution::{Execution, StageTiming};
use tinline_task_manager::{Cost, JobSpec};

const NOW: i64 = 1_000_000;

fn job(id: i64, owner: &str, cwd: &str, phase: Phase, argv: &[&str], resource: Resource) -> Job {
    let recipe = Recipe {
        broad_test: false,
        argv: argv.iter().map(|s| s.to_string()).collect(),
        resource,
    };
    Job {
        id,
        spec: JobSpec {
            suite: local_command::SUITE.into(),
            owner: owner.into(),
            source: cwd.into(),
            revision: "live-workspace".into(),
            configuration: serde_json::to_string(&recipe).unwrap(),
            cost: Cost::CpuSimulation,
        },
        phase,
        submitted_at: NOW - 300,
        execution: None,
    }
}

fn finished(mut j: Job, stages: &[(f64, f64, i32)]) -> Job {
    j.execution = Some(Execution {
        approved_at: NOW - 299,
        started_at: Some(NOW - 298),
        finished_at: Some(NOW - 100),
        cancel_requested: false,
        worker: None,
        supervisor: None,
        snapshot: None,
        stages: stages
            .iter()
            .map(|(wait, run, code)| StageTiming {
                stage: "build".into(),
                wait_seconds: *wait,
                seconds: Some(*run),
                exit_code: Some(*code),
                signal: None,
            })
            .collect(),
        build_artifact: None,
        android_trial: false,
        local_command: true,
        detail: String::new(),
        metrics: None,
        peer: None,
        reroute: None,
        remote_result: None,
        priority: None,
    });
    j
}

fn long_path() -> String {
    (0..60)
        .map(|i| format!("/opt/tool{i}/bin"))
        .collect::<Vec<_>>()
        .join(":")
}

#[test]
fn long_env_values_are_abbreviated_for_display_only() {
    let path = format!("PATH={}", long_path());
    let shown = abbreviate_arg(&path);
    assert!(
        shown.starts_with("PATH=/opt/tool0/bin:/opt/tool…(+"),
        "{shown}"
    );
    assert!(shown.len() <= 5 + ENV_VALUE_MAX + 20, "{shown}");
    assert!(
        shown.ends_with(&format!("…(+{})", path.len() - 5 - 24)),
        "{shown}"
    );
    // Short values, non-assignments and option values with '=' stay untouched.
    for same in [
        "VPX_DIR=/x",
        "--features=a,b",
        "cargo",
        "a=b=c",
        &format!("--x={}", long_path()),
    ] {
        assert_eq!(abbreviate_arg(same), same);
    }
    let j = job(
        1,
        "a",
        "/w",
        Phase::Queued,
        &["env", &path, "cargo", "test"],
        Resource::Compile,
    );
    let summary = command_summary(&j);
    assert!(summary.starts_with("env PATH=/opt/tool0"), "{summary}");
    assert!(summary.ends_with(" cargo test"), "{summary}");
    assert!(summary.len() < 120, "{summary}");
    // The stored argv is unchanged.
    let stored: Recipe = serde_json::from_str(&j.spec.configuration).unwrap();
    assert_eq!(stored.argv[1], path);
}

#[test]
fn quoting_keeps_arguments_distinguishable() {
    let j = job(
        1,
        "a",
        "/w",
        Phase::Queued,
        &["sh", "-c", "cargo test && echo 'ok'"],
        Resource::Run,
    );
    assert_eq!(
        command_summary(&j),
        r"sh -c 'cargo test && echo '\''ok'\'''"
    );
    assert_eq!(resource_label(&j), "run");
}

#[test]
fn truncation_is_character_safe() {
    assert_eq!(truncate_end("abcdef", 4), "abc…");
    assert_eq!(truncate_start("/a/b/c/d", 4), "…c/d");
    assert_eq!(truncate_end("ééé", 3), "ééé");
    assert_eq!(truncate_start("éééé", 2), "…é");
    assert_eq!(home_relative("/home/u/x", Some("/home/u")), "~/x");
    assert_eq!(home_relative("/home/u2/x", Some("/home/u")), "/home/u2/x");
    assert_eq!(home_relative("/home/u", Some("/home/u")), "~");
    assert_eq!(duration(5.4), "5s");
    assert_eq!(duration(65.0), "1m05s");
    assert_eq!(duration(3725.0), "1h02m");
}

#[test]
fn select_filters_by_owner_activity_and_limit() {
    let jobs = vec![
        job(1, "a", "/w", Phase::Passed, &["true"], Resource::Run),
        job(2, "b", "/w", Phase::Running, &["true"], Resource::Run),
        job(
            3,
            "a",
            "/w",
            Phase::WaitingBuild,
            &["true"],
            Resource::Compile,
        ),
        job(4, "a", "/w", Phase::Interrupted, &["true"], Resource::Run),
        job(5, "a", "/w", Phase::Queued, &["true"], Resource::Run),
    ];
    let ids = |v: Vec<Job>| v.iter().map(|j| j.id).collect::<Vec<_>>();
    assert_eq!(
        ids(select(jobs.clone(), Some("a"), false, None)),
        [1, 3, 4, 5]
    );
    assert_eq!(ids(select(jobs.clone(), None, true, None)), [2, 3, 5]);
    assert_eq!(ids(select(jobs.clone(), Some("a"), true, Some(1))), [5]);
    assert_eq!(ids(select(jobs.clone(), None, false, Some(2))), [4, 5]);
    assert!(select(jobs, Some("nobody"), false, None).is_empty());
}

#[test]
fn table_has_one_line_per_job_with_the_requested_columns() {
    let home = "/home/u";
    let cwd = "/home/u/projects/some/deeply/nested/checkout/worktree-name";
    let path = format!("PATH={}", long_path());
    let jobs = vec![
        finished(
            job(
                7,
                "agent-a",
                cwd,
                Phase::Failed,
                &["env", &path, "cargo", "test", "-j", "2"],
                Resource::Compile,
            ),
            &[(12.0, 30.0, 0), (1.0, 4.0, 101)],
        ),
        job(
            8,
            "agent-b",
            "/srv/x",
            Phase::WaitingRun,
            &["python3", "t.py"],
            Resource::Run,
        ),
    ];
    let table = job_table(&jobs, NOW, Some(home));
    let lines: Vec<_> = table.lines().collect();
    assert_eq!(lines.len(), 3, "{table}");
    for header in [
        "ID", "PHASE", "EXIT", "OWNER", "AGE", "WAIT", "RUN", "CWD", "COMMAND",
    ] {
        assert!(lines[0].contains(header), "{header}");
    }
    let failed = lines[1];
    assert!(failed.trim_start().starts_with("7 failed"), "{failed}");
    assert!(failed.contains(" 101 "), "{failed}");
    assert!(failed.contains("3m20s"), "age to finish: {failed}");
    assert!(
        failed.contains("13s") && failed.contains("34s"),
        "wait/run: {failed}"
    );
    assert!(
        failed.contains("…eeply/nested/checkout/worktree-name "),
        "{failed}"
    );
    assert!(
        !failed.contains("/opt/tool30"),
        "PATH must be abbreviated: {failed}"
    );
    assert!(failed.chars().count() < 220, "{failed}");
    let waiting = lines[2];
    assert!(
        waiting.contains("waiting_run") && waiting.contains("5m00s"),
        "{waiting}"
    );
    assert!(
        waiting.contains("python3 t.py") && waiting.contains("/srv/x"),
        "{waiting}"
    );
    assert!(job_table(&[], NOW, None).contains("(no matching jobs)"));
}

#[test]
fn submit_line_keeps_the_parsed_prefix_and_shows_cwd() {
    let path = format!("PATH={}", long_path());
    let j = job(
        42,
        "a",
        "/home/u/wt",
        Phase::Queued,
        &["env", &path, "cargo", "test"],
        Resource::Compile,
    );
    let line = submit_line(42, "/home/u/wt", Some(&j));
    assert!(
        line.starts_with("tasks: submitted job 42; cwd /home/u/wt;"),
        "{line}"
    );
    assert!(line.contains("argv env PATH=/opt"), "{line}");
    assert!(
        line.ends_with("Ctrl-C detaches, tasks cancel 42 cancels"),
        "{line}"
    );
    assert!(!line.contains("/opt/tool30"), "{line}");
    assert_eq!(
        submit_line(1, "/w", None),
        "tasks: submitted job 1; cwd /w; Ctrl-C detaches, tasks cancel 1 cancels"
    );
}

#[test]
fn shared_cwd_lists_only_other_owners_unfinished_jobs() {
    let jobs = vec![
        job(
            1,
            "other",
            "/w",
            Phase::Running,
            &["true"],
            Resource::Compile,
        ),
        job(
            2,
            "other",
            "/w",
            Phase::Passed,
            &["true"],
            Resource::Compile,
        ),
        job(
            3,
            "me",
            "/w",
            Phase::WaitingBuild,
            &["true"],
            Resource::Compile,
        ),
        job(
            4,
            "third",
            "/elsewhere",
            Phase::Running,
            &["true"],
            Resource::Compile,
        ),
        job(5, "me", "/w", Phase::Queued, &["true"], Resource::Compile),
        job(
            6,
            "third",
            "/w",
            Phase::Queued,
            &["true"],
            Resource::Compile,
        ),
    ];
    let ids: Vec<_> = shared_cwd(&jobs, 5).iter().map(|j| j.id).collect();
    assert_eq!(ids, [1, 6]);
    let warning = shared_cwd_warning(&jobs, 5).unwrap();
    assert!(
        warning.contains("1 (other, running)") && warning.contains("6 (third, queued)"),
        "{warning}"
    );
    assert!(
        warning.contains("/w") && warning.contains("not blocking"),
        "{warning}"
    );
    assert!(shared_cwd_warning(&jobs, 4).is_none());
    assert!(shared_cwd_warning(&jobs, 99).is_none());
}

#[test]
fn holders_mark_legacy_clients_and_link_durable_jobs() {
    assert_eq!(slot_name("p2p-build.lock"), "compile");
    assert_eq!(slot_name("p2p-run-2.lock"), "run-2");
    assert_eq!(slot_name("p2p-device-9BWAY.lock"), "device 9BWAY");
    let jobs = vec![job(
        9,
        "agent-a",
        "/w",
        Phase::Running,
        &["true"],
        Resource::Run,
    )];
    let holders = [
        Holder {
            slot: "compile".into(),
            who: "callui-android".into(),
            pid: Some(11),
            since: (NOW - 90) as f64,
        },
        Holder {
            slot: "run-0".into(),
            who: "agent-a:job-9".into(),
            pid: Some(12),
            since: (NOW - 5) as f64,
        },
    ];
    let waiting = [Holder {
        slot: "waiting".into(),
        who: "agent-a:job-10".into(),
        pid: Some(13),
        since: NOW as f64,
    }];
    let text = holders_section(&holders, &waiting, &jobs, NOW);
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    assert!(
        lines[1].contains("compile") && lines[1].contains("callui-android"),
        "{text}"
    );
    assert!(
        lines[1].contains("1m30s") && lines[1].contains("not in ledger (legacy/external"),
        "{text}"
    );
    assert!(
        lines[2].contains("run-0") && lines[2].ends_with("job 9"),
        "{text}"
    );
    assert!(
        lines[3].contains("waits")
            && lines[3].ends_with("native job not in this ledger (other state dir)"),
        "{text}"
    );
    assert!(holders_section(&[], &[], &jobs, NOW).contains("no lock is held"));
}

#[test]
fn execution_enabled_is_explained_for_people() {
    let j = finished(
        job(22, "lead", "/w", Phase::Passed, &["true"], Resource::Run),
        &[(0.0, 1.0, 0)],
    );
    let line = job_status_line(&j, false);
    assert!(
        line.starts_with("tasks: job 22 passed (exit 0) owner=lead;"),
        "{line}"
    );
    assert!(line.contains("does not mean the job did not run"), "{line}");
    assert!(job_status_line(&j, true).contains("worker process running"));
    let approved = job(23, "lead", "/w", Phase::Approved, &["true"], Resource::Run);
    assert!(job_status_line(&approved, false).starts_with("tasks: job 23 approved owner=lead;"));
}

#[test]
fn offload_jobs_and_slots_are_labelled() {
    let offload = job(
        3,
        "agent-o",
        "/w",
        Phase::Running,
        &["tasks-offload", "--", "true"],
        Resource::Offload,
    );
    assert_eq!(resource_label(&offload), "offload");
    assert_eq!(slot_name("p2p-offload-1.lock"), "offload-1");
}

#[test]
fn interrupted_job_with_a_successful_earlier_stage_never_shows_exit_0() {
    // Job 498 of 2026-10-17: build exited 0, the run stage was cut by the reboot.
    let argv = ["cargo", "test", "-j", "2"];
    let j = finished(
        job(498, "a", "/w", Phase::Interrupted, &argv, Resource::Compile),
        &[(1.0, 42.0, 0)],
    );
    assert_eq!(exit_label(&j), "-");
    let table = job_table(std::slice::from_ref(&j), NOW, None);
    let row = table.lines().nth(1).unwrap();
    assert!(row.contains("interrupted   -"), "{row}");
    assert!(job_status_line(&j, false).contains("interrupted (exit -)"));
    let cancelled = finished(
        job(2, "a", "/w", Phase::Cancelled, &argv, Resource::Compile),
        &[(1.0, 2.0, 0)],
    );
    assert_eq!(exit_label(&cancelled), "-");
    let failed = finished(
        job(3, "a", "/w", Phase::Failed, &argv, Resource::Compile),
        &[(1.0, 2.0, 0), (0.0, 1.0, 101)],
    );
    assert_eq!(exit_label(&failed), "101");
    let passed = finished(
        job(4, "a", "/w", Phase::Passed, &argv, Resource::Compile),
        &[(1.0, 2.0, 0)],
    );
    assert_eq!(exit_label(&passed), "0");
}

#[test]
fn reconciled_remote_result_is_shown_next_to_the_local_phase() {
    let argv = [
        "tasks-offload",
        "--worker",
        "w@h",
        "--who",
        "a",
        "--",
        "true",
    ];
    let mut j = finished(
        job(507, "a", "/w", Phase::Interrupted, &argv, Resource::Offload),
        &[],
    );
    j.execution.as_mut().unwrap().remote_result =
        Some(tinline_task_manager::execution::RemoteResult {
            transport: "ssh".into(),
            worker: "w@h".into(),
            remote_job_id: Some(152),
            remote_phase: Some("passed".into()),
            exit_code: Some(0),
            state: "terminal".into(),
            checked_at: NOW,
            detail: String::new(),
        });
    let line = job_status_line(&j, false);
    assert!(line.contains("job 507 interrupted (exit -)"), "{line}");
    assert!(
        line.contains("is interrupted here; its remote job 152 on w@h passed (exit 0) [terminal]"),
        "{line}"
    );
}

#[test]
fn start_and_stop_worker_print_one_line_not_the_ledger() {
    let argv = ["cargo", "check"];
    let jobs = vec![
        job(1, "a", "/w", Phase::Approved, &argv, Resource::Compile),
        job(2, "a", "/w", Phase::Running, &argv, Resource::Compile),
    ];
    let start = worker_switch_line(true, true, &jobs);
    assert!(
        start.contains("execution enabled; worker running"),
        "{start}"
    );
    assert!(
        start.contains("1 approved job(s) waiting, 1 running"),
        "{start}"
    );
    assert!(!start.contains('\n') && !start.contains('{'));
    let stop = worker_switch_line(false, true, &jobs);
    assert!(stop.contains("execution stopped"), "{stop}");
    assert!(stop.contains("until tasks start-worker"), "{stop}");
}

fn link(local: i64, remote: Option<i64>, phase: Option<&str>, stage: &str) -> OutboundLink {
    OutboundLink {
        version: 1,
        local_job_id: local,
        peer: "nixos".into(),
        peer_endpoint: "e".into(),
        key: "k".into(),
        owner: "o".into(),
        checkout: None,
        sub: String::new(),
        stage: stage.into(),
        remote_job_id: remote,
        remote_phase: phase.map(String::from),
        remote_state: None,
        exit: None,
        sync_s: None,
        run_s: None,
        fetch_s: None,
        fetch_dest: None,
        blobs_sent: None,
        cancel_sent: false,
        ledger_linked: false,
        started_at: 0.0,
        updated_at: 0.0,
        error: None,
    }
}

#[test]
fn active_table_names_the_remote_job_and_what_a_queued_job_waits_for() {
    let jobs = vec![
        job(
            1,
            "a",
            "/w",
            Phase::Running,
            &["tasks", "x"],
            Resource::Offload,
        ),
        job(
            2,
            "a",
            "/w",
            Phase::Running,
            &["tasks", "x"],
            Resource::Offload,
        ),
        job(
            3,
            "a",
            "/w",
            Phase::WaitingRun,
            &["tasks", "x"],
            Resource::Offload,
        ),
        job(4, "a", "/w", Phase::Building, &["true"], Resource::Compile),
        job(5, "a", "/w", Phase::Passed, &["true"], Resource::Run),
    ];
    let links = [
        link(1, Some(1346), Some("building"), "running"),
        link(2, None, None, "syncing"),
        link(5, Some(9), Some("passed"), "done"),
    ];
    let waiting = HashMap::from([
        (3, "this checkout in use by #1".to_string()),
        (5, "stale".to_string()),
    ]);
    let notes = phase_notes(&jobs, &links, &waiting);
    assert_eq!(notes[&1], "running → nixos #1346 (building)");
    assert_eq!(notes[&2], "running → nixos (syncing)");
    assert_eq!(notes[&3], "waiting_run: this checkout in use by #1");
    assert!(!notes.contains_key(&4) && !notes.contains_key(&5));
    let table = job_table_with(&jobs, NOW, None, &notes);
    let lines: Vec<_> = table.lines().collect();
    assert!(
        lines[1].contains("running → nixos #1346 (building)"),
        "{table}"
    );
    // The wider PHASE column keeps every later column aligned.
    // The header is ASCII, so the byte offset of EXIT is also its character column.
    let exit = lines[0].find("EXIT").expect("EXIT header");
    assert!(lines[0].starts_with("   ID PHASE"), "{table}");
    for line in &lines[1..] {
        let field: String = line.chars().skip(exit).take(4).collect();
        assert!(field.starts_with(['-', '0']), "{field:?} in {table}");
    }
}

#[test]
fn table_without_notes_is_unchanged() {
    let jobs = vec![job(1, "a", "/w", Phase::Running, &["true"], Resource::Run)];
    assert_eq!(
        job_table(&jobs, NOW, None),
        job_table_with(&jobs, NOW, None, &HashMap::new())
    );
    assert!(job_table(&jobs, NOW, None).starts_with("   ID PHASE         EXIT"));
}
