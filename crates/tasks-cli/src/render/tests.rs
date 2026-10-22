use super::*;
use crate::testutil::{NOW, argv, job};
use std::path::PathBuf;
use tasks_core::JobId;
use tasks_daemon::api::GrepLine;

fn long_path() -> String {
    format!("/opt/tool{}", "x".repeat(60))
}

// R-184
#[test]
fn r184_long_env_values_are_abbreviated_for_display_only() {
    let p = format!("PATH={}", long_path());
    let shown = abbreviate_arg(&p);
    assert!(shown.starts_with("PATH=/opt/toolxxx"), "{shown}");
    assert!(shown.chars().count() < p.chars().count());
    assert!(shown.contains("chars)"));
    // short values, non-env words and flags with '=' are untouched
    assert_eq!(abbreviate_arg("FOO=bar"), "FOO=bar");
    assert_eq!(
        abbreviate_arg(&format!("--cfg={}", "y".repeat(80))),
        format!("--cfg={}", "y".repeat(80))
    );
    // the job's own argv is not changed by display
    let argv = vec!["env".to_string(), p.clone(), "cargo".to_string()];
    let line = command_display(&argv);
    assert!(!line.contains(&p));
    assert_eq!(argv[1], p);
}

#[test]
fn r184_quoting_keeps_arguments_distinguishable() {
    let a = vec!["echo".to_string(), "a b".to_string(), "c".to_string()];
    let b = vec!["echo".to_string(), "a".to_string(), "b c".to_string()];
    assert_ne!(command_display(&a), command_display(&b));
    assert_eq!(command_display(&a), "echo 'a b' c");
    assert_eq!(quote_arg(""), "''");
    assert_eq!(quote_arg("it's"), "'it'\\''s'");
    assert_eq!(quote_arg("-p"), "-p");
}

#[test]
fn r184_truncation_is_character_safe() {
    let s = "ééééééééééé";
    assert_eq!(truncate_end(s, 5), "éééé…");
    assert_eq!(truncate_start(s, 5), "…éééé");
    assert_eq!(truncate_end("abc", 5), "abc");
    assert_eq!(truncate_end("日本語日本語", 4), "日本語…");
    let value = format!("K={}", "é".repeat(90));
    let shown = abbreviate_arg(&value);
    assert!(shown.starts_with("K=ééé"));
}

// R-183
#[test]
fn r183_submit_line_keeps_the_parsed_prefix_and_shows_cwd_and_command() {
    let line = submit_line(42, Path::new("/home/u/wt"), &argv("cargo test -p x"));
    assert!(
        line.starts_with("tasks: submitted job 42; cwd /home/u/wt;"),
        "{line}"
    );
    assert!(line.contains("argv cargo test -p x;"), "{line}");
    assert!(
        line.ends_with("Ctrl-C detaches, tasks2 cancel 42 cancels"),
        "{line}"
    );
    let long = vec![
        "env".to_string(),
        format!("PATH={}", long_path()),
        "cargo".to_string(),
    ];
    assert!(!submit_line(1, Path::new("/w"), &long).contains(&"x".repeat(50)));
}

// R-104
#[test]
fn r104_the_detach_message_says_the_job_is_live_and_not_to_resubmit() {
    let m = still_live(9);
    assert!(m.contains("job 9 is still live"));
    assert!(m.contains("do not resubmit"));
    assert!(m.contains("tasks2 cancel 9"));
}

// R-009
#[test]
fn r009_only_passed_shows_exit_zero() {
    let passed = job(1, "a", "/w", Phase::Passed, "true");
    assert_eq!(exit_label(&passed), "0");
    let mut interrupted = job(2, "a", "/w", Phase::Interrupted, "true");
    interrupted.exit = Some(0);
    assert_eq!(exit_label(&interrupted), "-");
    let failed = job(3, "a", "/w", Phase::Failed { exit: Some(101) }, "false");
    assert_eq!(exit_label(&failed), "101");
    assert_eq!(exit_label(&job(4, "a", "/w", Phase::Running, "x")), "-");
}

#[test]
fn run_exit_codes_pass_the_jobs_code_through() {
    let j = |p| job(1, "a", "/w", p, "x");
    assert_eq!(job_exit_code(&j(Phase::Passed)), 0);
    assert_eq!(job_exit_code(&j(Phase::Failed { exit: Some(101) })), 101);
    assert_eq!(job_exit_code(&j(Phase::Failed { exit: None })), 1);
    assert_eq!(job_exit_code(&j(Phase::TimedOut { exit: None })), 124);
    assert_eq!(job_exit_code(&j(Phase::TimedOut { exit: Some(143) })), 143);
    assert_eq!(job_exit_code(&j(Phase::Cancelled)), 125);
    assert_eq!(job_exit_code(&j(Phase::Interrupted)), 125);
    let mut weird = j(Phase::Failed { exit: Some(0) });
    weird.exit = Some(0);
    assert_eq!(
        job_exit_code(&weird),
        1,
        "a failed job never reports success"
    );
}

// R-185
#[test]
fn r185_table_has_one_line_per_job_with_the_requested_columns() {
    let jobs = vec![
        job(1, "alice", "/home/u/wt", Phase::Passed, "cargo test"),
        job(
            22,
            "bob",
            "/home/u/other",
            Phase::Running,
            "cargo check -p x",
        ),
    ];
    let t = job_table(&jobs, NOW, Some("/home/u"), &[], TableOptions::default());
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines.len(), 4, "{t}"); // header, 2 jobs, legend
    assert!(lines[0].contains("ID") && lines[0].contains("PHASE") && lines[0].contains("CWD"));
    assert!(lines[1].contains("passed") && lines[1].contains("~/wt") && lines[1].contains(" 0 "));
    assert!(lines[2].contains("running") && lines[2].contains("bob"));
    assert!(lines[3].starts_with("tasks: AGE ="));
    let brief = job_table(
        &jobs,
        NOW,
        Some("/home/u"),
        &[],
        TableOptions { brief: true },
    );
    assert_eq!(brief.lines().count(), 3, "{brief}");
    assert!(!brief.contains("~/wt"));
    assert_eq!(
        job_table(&[], NOW, None, &[], TableOptions::default()),
        "(no matching jobs)\n"
    );
}

#[test]
fn r185_a_table_without_waiting_jobs_has_no_note_lines() {
    let jobs = vec![job(1, "a", "/w", Phase::Running, "cargo test")];
    let t = job_table(&jobs, NOW, None, &[], TableOptions::default());
    assert_eq!(t.lines().count(), 3);
    assert!(!t.contains("waits for"));
}

// R-186
#[test]
fn r186_active_table_names_the_remote_job_and_what_a_queued_job_waits_for() {
    let mut fwd = job(10, "lead", "/w", Phase::Running, "cargo test");
    fwd.forwarded_to = Some(("nixos".into(), Some(1474)));
    fwd.summary = Some("building".into());
    let mut queued = job(
        11,
        "me",
        "/w",
        Phase::Waiting(tasks_core::WaitReason::Slot(tasks_core::SlotKind::Compile)),
        "cargo build",
    );
    queued.wait_text = Some("compile slot held by job 10".into());
    let waiter = WaiterView {
        id: JobId(11),
        owner: "me".into(),
        command: "cargo build".into(),
        wait_text: "compile slot held by job 10 (about 3m left)".into(),
        position: 2,
        start_estimate_secs: Some(190.0),
        rough: true,
    };
    let t = job_table(
        &[fwd, queued],
        NOW,
        None,
        &[waiter],
        TableOptions::default(),
    );
    assert!(t.contains("running → nixos #1474 (building)"), "{t}");
    assert!(t.contains("waits for: compile slot held by job 10 (about 3m left); position 2; starts in ~3m (rough, no history)"), "{t}");
}

#[test]
fn r186_a_reconciled_remote_result_is_shown_next_to_the_local_phase() {
    let mut j = job(5, "a", "/w", Phase::Interrupted, "cargo test");
    j.forwarded_to = Some(("nixos".into(), Some(77)));
    j.summary = Some("remote passed".into());
    assert_eq!(phase_label(&j), "interrupted → nixos #77");
    assert!(job_detail(&j, NOW, None).contains("forwarded: to nixos as job #77"));
}

#[test]
fn job_detail_lists_the_facts() {
    let mut j = job(
        3,
        "a",
        "/w",
        Phase::Failed { exit: Some(2) },
        "cargo test -p x",
    );
    j.plan = Some((4, "build".into()));
    j.after = vec![JobId(1), JobId(2)];
    j.log_path = Some(PathBuf::from("/s/logs/3.log"));
    let d = job_detail(&j, NOW, None);
    for want in [
        "job 3: failed",
        "owner:     a",
        "command:   cargo test -p x",
        "exit:      2",
        "after:     1, 2",
        "plan:      4 step build",
        "log:       /s/logs/3.log",
    ] {
        assert!(d.contains(want), "{want} in\n{d}");
    }
}

// R-112
#[test]
fn r112_grep_prints_line_numbers_and_separates_context_groups() {
    let g = |lines: &[(u64, &str, bool)]| GrepGroup {
        lines: lines
            .iter()
            .map(|(n, t, m)| GrepLine {
                n: *n,
                text: (*t).into(),
                matched: *m,
            })
            .collect(),
    };
    let out = grep_groups(&[
        g(&[(3, "before", false), (4, "FAILED a", true)]),
        g(&[(20, "FAILED b", true), (21, "after", false)]),
    ]);
    assert_eq!(out, "3-before\n4:FAILED a\n--\n20:FAILED b\n21-after\n");
}

#[test]
fn log_search_names_each_job_and_says_when_a_bound_cut_it() {
    let r = JobGrep {
        id: JobId(7),
        owner: "me".into(),
        phase: "failed".into(),
        groups: vec![GrepGroup {
            lines: vec![GrepLine {
                n: 1,
                text: "boom".into(),
                matched: true,
            }],
        }],
    };
    let t = log_search(&[r], 1, true);
    assert!(t.contains("== job 7 (me, failed)\n1:boom\n"));
    assert!(t.contains("1 matching line(s) in 1 job(s); the search hit a bound"));
    assert!(log_search(&[], 0, false).starts_with("(no matches)"));
}

fn snapshot() -> StatusSnapshot {
    crate::testutil::status_snapshot()
}

// R-050
#[test]
fn r050_status_lists_a_busy_slot_without_a_job_as_external_work() {
    let t = status(&snapshot());
    assert!(t.contains("compile  0/2 free"), "{t}");
    assert!(t.contains("slot 0: job 5"));
    assert!(
        t.contains(
            "slot 1: external work, not a ledger job: pid 9 cargo (from job 3, which has ended)"
        ),
        "{t}"
    );
    assert!(!t.contains("no live jobs"));
    assert!(t.contains("waiting (3):") && t.contains("... and 2 more"));
    assert!(
        t.contains("job 5 lead [compile 0] tasks-core for 1m15s 3/10 targets, ~3m left"),
        "{t}"
    );
    assert!(t.contains("a new compile job would wait: memory stalls 12% over 10%"));
    assert!(t.contains("alert [warn] disk low"));
    let mut idle = snapshot();
    idle.slots.clear();
    idle.running.clear();
    idle.waiting.clear();
    assert!(status(&idle).contains("no live jobs"));
}

// R-189
#[test]
fn r189_a_held_queue_says_it_is_held_and_how_to_resume() {
    let mut s = snapshot();
    s.daemon.draining = true;
    let t = status(&s);
    assert!(t.contains("queue: DRAINING"), "{t}");
    assert!(t.contains("`tasks2 resume`"));
    s.daemon.draining = false;
    s.daemon.execution_enabled = false;
    let t = status(&s);
    assert!(
        t.contains("queue: STOPPED") && t.contains("tasks2 execution start"),
        "{t}"
    );
    assert!(!t.contains("worker not running"));
}

// R-045, R-231, R-219
#[test]
fn r231_admission_reports_the_daemons_lock_dir_and_flags_a_disagreeing_shell() {
    let s = snapshot();
    let same = admission(&s, Some(Path::new("/state/slots")));
    assert!(
        same.starts_with("lock dir: /state/slots (the daemon's)\n"),
        "{same}"
    );
    assert!(!same.contains("would resolve"));
    let other = admission(&s, Some(Path::new("/tmp/claude-1000")));
    assert!(
        other.contains("this shell would resolve the lock dir to /tmp/claude-1000"),
        "{other}"
    );
    assert!(admission(&s, None).contains("lock dir: /state/slots"));
}

#[test]
fn r219_admission_shows_every_policy_value_with_its_source() {
    let t = admission(&snapshot(), None);
    assert!(t.contains("slots.compile   = 2 (file)"), "{t}");
    assert!(t.contains("cargo.max_jobs  = 2 (default)"), "{t}");
    assert!(t.contains("cancel_grace_ms = 500 (env)"), "{t}");
    assert!(t.contains("policy problem: slots.compile: 9 is above 8"));
    assert!(t.contains("compile admission: blocked: memory stalls"));
    assert!(
        t.contains("cache 100.0 GiB free, state ? free; memory available 8.0 GiB"),
        "{t}"
    );
    assert!(t.contains("machine policy: /cfg/machine-policy.toml (loaded)"));
}

#[test]
fn estimate_and_stats_render() {
    let e = EstimateView {
        recipe: "cargo test pkg=x".into(),
        median_secs: 116.0,
        p90_secs: 200.0,
        samples: 5,
        text: "about 2m on this machine".into(),
    };
    let t = estimate(&e);
    assert!(
        t.starts_with("estimate: median 1m56s, p90 3m20s, 5 sample(s)\n"),
        "{t}"
    );
    let s = stats(&[StatRow {
        recipe: "cargo check".into(),
        runs: 3,
        passed: 2,
        median_secs: 10.0,
        p90_secs: 30.0,
    }]);
    assert!(
        s.contains("RUNS") && s.contains("    3      2      10s      30s  cargo check"),
        "{s}"
    );
    assert_eq!(stats(&[]), "(no finished jobs)\n");
    assert_eq!(fmt_bytes(512 << 20), "512 MiB");
}
