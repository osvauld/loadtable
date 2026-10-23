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

// R-542
#[test]
fn r542_the_summary_line_has_the_documented_shape() {
    let mut j = job(
        11,
        "mac-trial",
        "/w",
        Phase::Failed { exit: Some(101) },
        "x",
    );
    j.forwarded_to = Some(("mac".into(), Some(3)));
    assert_eq!(
        summary_line(&j),
        "job 11 failed (exit 101) owner=mac-trial on mac"
    );
    let p = job(9, "a", "/w", Phase::Passed, "uname");
    assert_eq!(summary_line(&p), "job 9 passed (exit 0) owner=a");
    for (phase, word) in [
        (Phase::Cancelled, "cancelled"),
        (Phase::Interrupted, "interrupted"),
        (Phase::TimedOut { exit: None }, "failed"),
    ] {
        let l = summary_line(&job(1, "a", "/w", phase, "x"));
        assert!(l.starts_with(&format!("job 1 {word} (exit ")), "{l}");
        assert!(!l.contains('\n'));
    }
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
        class: String::new(),
        class_note: String::new(),
        on_peer: None,
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

#[test]
fn r189_a_drained_queue_says_who_stopped_it_how_long_ago_and_how_to_resume() {
    let mut s = snapshot();
    s.daemon.draining = true;
    s.daemon.drained_by = Some("alice".into());
    s.daemon.drained_at_ms = Some(s.daemon.now_ms - 125_000);
    let t = status(&s);
    assert!(
        t.contains("queue: DRAINING: stopped by alice, 2m05s ago, resume with `tasks2 resume`"),
        "{t}"
    );
    // a daemon that did not record it keeps the plain line
    s.daemon.drained_by = None;
    assert!(status(&s).contains("queue: DRAINING: no new job starts"));
}

#[test]
fn r186_a_forwarded_row_shows_the_remote_phase_beside_the_local_one() {
    let mut j = job(
        10,
        "lead",
        "/w",
        Phase::Waiting(tasks_core::WaitReason::Placement("syncing".into())),
        "cargo test",
    );
    j.forwarded_to = Some(("nixos".into(), Some(1474)));
    j.remote_phase = Some(Phase::Waiting(tasks_core::WaitReason::Slot(
        tasks_core::SlotKind::Compile,
    )));
    assert_eq!(phase_label(&j), "waiting → nixos #1474 (remote waiting)");
    j.phase = Phase::Running;
    j.remote_phase = Some(Phase::Running);
    j.summary = Some("building".into());
    assert_eq!(
        phase_label(&j),
        "running → nixos #1474 (remote running, building)"
    );
    let t = job_table(&[j.clone()], NOW, None, &[], TableOptions::default());
    assert!(
        t.contains("running → nixos #1474 (remote running, building)"),
        "{t}"
    );
    assert!(job_detail(&j, NOW, None).contains("remote:    running"));
    // a finished local row keeps the remote result without the stale summary
    j.phase = Phase::Interrupted;
    j.remote_phase = Some(Phase::Passed);
    assert_eq!(phase_label(&j), "interrupted → nixos #1474 (remote passed)");
}

#[test]
fn r108_a_cancelled_job_shows_why_in_the_table_and_in_detail() {
    let mut j = job(3, "a", "/w", Phase::Cancelled, "cargo test");
    j.cancel_requested_by = Some("a".into());
    j.cancel_reason = Some("wrong branch".into());
    j.timeout_secs = Some(90);
    assert_eq!(phase_label(&j), "cancelled (wrong branch)");
    let d = job_detail(&j, NOW, None);
    assert!(d.contains("cancel requested by a: wrong branch"), "{d}");
    assert!(d.contains("timeout:   1m30s"), "{d}");
    j.cancel_reason = None;
    assert_eq!(phase_label(&j), "cancelled");
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
        machine: None,
        target: None,
        runs: 3,
        passed: 2,
        median_secs: 10.0,
        p90_secs: 30.0,
    }]);
    assert!(
        s.contains("RUNS") && s.contains("    3      2      10s      30s  cargo check"),
        "{s}"
    );
    assert_eq!(stats(&[]), "(no timing samples)\n");
    assert_eq!(fmt_bytes(512 << 20), "512 MiB");
}

// R-326, R-335
#[test]
fn r326_status_says_how_exact_the_costs_are_and_shows_cost_and_trips() {
    use tasks_core::{Attribution, JobFigures};
    use tasks_daemon::api::{TransferView, TripView};
    let mut s = crate::testutil::status_snapshot();
    s.attribution = "attribution: process tree (no cgroup delegation: not under systemd)".into();
    let mut f = JobFigures::empty(Attribution::ProcessTree);
    f.io_write_bytes = Some(3 << 30);
    s.running[0].figures = Some(f.clone());
    s.transfer = Some(TransferView {
        active: 1,
        waiting: Some("Pack: I/O stalls: pressure 52% > 40% for transfer".into()),
        runs: 3,
        inproc_read_bytes: 1 << 20,
        inproc_write_bytes: 2 << 20,
        figures: None,
    });
    s.trips = vec![TripView {
        seq: 9,
        at_ms: 1_792_239_733_000,
        resource: "io".into(),
        source: "gate".into(),
        summary: "io gate: I/O stalls; busiest outside jobs: firefox (pid 7) moved 25 GiB".into(),
        suppressed: 0,
    }];
    let text = status(&s);
    assert!(
        text.contains("attribution: process tree (no cgroup delegation: not under systemd)"),
        "{text}"
    );
    assert!(text.contains("cost: wrote 3 GiB (process tree)"), "{text}");
    assert!(
        text.contains("transfers: 1 active, 3 run(s)")
            && text.contains("waiting: Pack: I/O stalls"),
        "{text}"
    );
    assert!(
        text.contains("busiest outside jobs: firefox (pid 7) moved 25 GiB"),
        "{text}"
    );
    // an older daemon sends none of it
    s.attribution.clear();
    s.transfer = None;
    s.trips.clear();
    assert!(!status(&s).contains("attribution"));
    // and a job's cost shows in `get`
    let mut j = job(1, "o", "/w", Phase::Passed, "make");
    j.figures = Some(f);
    assert!(job_detail(&j, 0, None).contains("cost:      wrote 3 GiB (process tree)"));
}

// R-353
#[test]
fn r353_a_finished_forwarded_job_does_not_show_an_unfinished_remote_phase() {
    let mut j = job(1, "o", "/w", Phase::Passed, "uname -a");
    j.forwarded_to = Some(("nixos".into(), Some(1)));
    // a row written before the final phase was stored: the last one mirrored was `queued`
    j.remote_phase = Some(Phase::Queued);
    let detail = job_detail(&j, NOW, None);
    assert!(!detail.contains("remote:"), "{detail}");
    assert!(
        !phase_label(&j).contains("remote queued"),
        "{}",
        phase_label(&j)
    );
    // the peer's own final word is shown
    j.remote_phase = Some(Phase::Passed);
    assert!(job_detail(&j, NOW, None).contains("remote:    passed"));
    assert!(
        phase_label(&j).contains("remote passed"),
        "{}",
        phase_label(&j)
    );
    // while the job lives, what the peer says is what is shown
    let mut live = job(2, "o", "/w", Phase::Running, "cargo test");
    live.forwarded_to = Some(("nixos".into(), Some(2)));
    live.remote_phase = Some(Phase::Queued);
    assert!(job_detail(&live, NOW, None).contains("remote:    queued"));
}

// R-354
#[test]
fn r354_placement_and_kind_are_words_not_debug_text() {
    let mut j = job(1, "o", "/w", Phase::Passed, "uname -a");
    j.placement = tasks_core::Placement::Peer(tasks_core::MachineName("nixos".into()));
    j.kind = tasks_core::Kind::Run;
    let d = job_detail(&j, NOW, None);
    assert!(d.contains("placement: peer nixos (asked), kind run"), "{d}");
    for leak in ["Peer(", "MachineName", "Kind::", "Run\n"] {
        assert!(!d.contains(leak), "{leak} leaked: {d}");
    }
    for (p, want) in [
        (tasks_core::Placement::Here, "placement: here,"),
        (tasks_core::Placement::Pin, "placement: here (pinned),"),
        (
            tasks_core::Placement::Anywhere,
            "placement: anywhere (chosen by the scheduler),",
        ),
    ] {
        j.placement = p;
        assert!(job_detail(&j, NOW, None).contains(want));
    }
    // the words other renderers use for phases and kinds carry no braces either
    assert_eq!(tasks_core::Phase::Failed { exit: Some(2) }.word(), "failed");
    assert_eq!(tasks_core::Kind::Compile.to_string(), "compile");
}

// R-356
#[test]
fn r356_status_lists_each_peer_with_state_path_latencies_roles_and_its_last_report() {
    use tasks_daemon::api::{PeerMachineView, PeerSlotView, PeerView};
    let mut s = crate::testutil::status_snapshot();
    s.peers = vec![
        PeerView {
            name: "nixos".into(),
            node_id: "n".into(),
            state: "up".into(),
            last_seen_ago_secs: Some(2),
            detail: String::new(),
            did: "did:key:z".into(),
            roles_out: vec!["submit".into(), "observe".into()],
            roles_in: vec!["observe".into()],
            grant_expires_at: None,
            path: "direct".into(),
            rtt_ms: Some(3),
            reply_ms: Some(87),
            machine: Some(PeerMachineView {
                cores: 16,
                memory_bytes: 64 << 30,
                slots: vec![PeerSlotView {
                    kind: "compile".into(),
                    total: 4,
                    used: 1,
                }],
                waiting: 2,
                running: 1,
                cpu_pct: Some(35),
                memory_pct: None,
                memory_level: None,
                io_pct: Some(1),
                pressure_summary: String::new(),
                cpu_busy_pct: Some(41),
                memory_available_bytes: None,
                swap_total: Some(8 << 30),
                swap_used: Some(1 << 30),
                swap_in_per_s: Some(0),
                swap_out_per_s: Some(2048),
                jobs: Vec::new(),
                cores_in_use_milli: None,
                power: Some("on battery 80%".into()),
            }),
        },
        PeerView {
            name: "mini".into(),
            node_id: "m".into(),
            state: "offline".into(),
            last_seen_ago_secs: None,
            detail: "connect timed out".into(),
            did: String::new(),
            roles_out: vec![],
            roles_in: vec![],
            grant_expires_at: None,
            path: String::new(),
            rtt_ms: None,
            reply_ms: None,
            machine: None,
        },
    ];
    let t = status(&s);
    assert!(t.contains("peers:\n"), "{t}");
    assert!(
        t.contains("nixos up (direct; path RTT 3 ms, reply time 87 ms, last seen"),
        "{t}"
    );
    assert!(
        t.contains("roles: we may submit, observe; it may observe"),
        "{t}"
    );
    assert!(t.contains("16 cores") && t.contains("compile 1/4"), "{t}");
    assert!(t.contains("2 waiting, 1 running"), "{t}");
    assert!(t.contains("cpu busy 41%"), "{t}");
    assert!(
        t.contains("swap 1.0 GiB of 8.0 GiB used (in 0 B/s, out 2.0 KiB/s)"),
        "{t}"
    );
    // a peer that never answered is listed with why, and no invented figures
    assert!(
        t.contains("mini offline (unknown; path RTT -, reply time -)"),
        "{t}"
    );
    assert!(t.contains("connect timed out"), "{t}");
    // no peers, no section
    s.peers.clear();
    assert!(!status(&s).contains("peers:"));
}

// R-358
#[test]
fn r358_status_shows_cpu_busy_and_swap_with_rates_and_says_so_when_there_is_no_swap() {
    let mut s = crate::testutil::status_snapshot();
    s.pressure.cpu_busy_pct = Some(23.4);
    s.pressure.swap_total = Some(8 << 30);
    s.pressure.swap_used = Some(512 << 20);
    s.pressure.swap_in_per_s = Some(4096);
    s.pressure.swap_out_per_s = Some(0);
    let t = status(&s);
    assert!(
        t.contains("cpu busy 23%, swap 512.0 MiB of 8.0 GiB used (in 4.0 KiB/s, out 0 B/s)"),
        "{t}"
    );
    s.pressure.swap_total = Some(0);
    assert!(status(&s).contains("cpu busy 23%, no swap"));
    // an older daemon reports none of it: no line, no zeros
    s.pressure.swap_total = None;
    s.pressure.cpu_busy_pct = None;
    let t = status(&s);
    assert!(!t.contains("swap") && !t.contains("cpu busy"), "{t}");
}

// R-504
#[test]
fn r504_get_lists_each_fetch_result_and_a_passed_job_with_a_failed_one_exits_one() {
    use tasks_core::{FetchResult, FetchStatus};
    let mut j = job(1, "a", "/w", Phase::Passed, "x");
    j.fetch = vec![
        FetchResult {
            path: "a.apk".into(),
            status: FetchStatus::Fetched {
                bytes: 2048,
                sha256: "ff".into(),
                unchanged: false,
            },
        },
        FetchResult {
            path: "b.apk".into(),
            status: FetchStatus::Missing,
        },
    ];
    assert_eq!(job_exit_code(&j), 1);
    let text = job_detail(&j, NOW, None);
    assert!(
        text.contains("fetch:     fetched a.apk (2 KiB, sha256 ff)"),
        "{text}"
    );
    assert!(
        text.contains("fetch failed: b.apk does not exist on the peer"),
        "{text}"
    );
    j.fetch.truncate(1);
    assert_eq!(job_exit_code(&j), 0);
}

// R-609, R-611: status shows the power source, the stay-awake hold and a peer's power
#[test]
fn r609_status_shows_the_power_source_and_whether_the_machine_is_kept_awake() {
    use tasks_daemon::api::PowerView;
    let mut s = snapshot();
    let mut pw = PowerView {
        on_battery: false,
        battery_pct: None,
        awake_jobs: 2,
        holding: true,
        linger_secs: None,
        guard_error: None,
        battery_held: 0,
        peer_jobs_on_battery: false,
    };
    s.power = Some(pw.clone());
    let t = status(&s);
    assert!(t.contains("power: on AC\nkeeping awake: 2 jobs\n"), "{t}");
    pw.holding = false;
    pw.awake_jobs = 0;
    pw.on_battery = true;
    pw.battery_pct = Some(80);
    pw.battery_held = 1;
    s.power = Some(pw.clone());
    let t = status(&s);
    assert!(
        t.contains("power: on battery 80%; 1 job from peers wait for AC"),
        "{t}"
    );
    assert!(t.contains("\nsleep allowed\n"), "{t}");
    pw.holding = true;
    pw.linger_secs = Some(80);
    s.power = Some(pw);
    assert!(status(&s).contains("keeping awake: no jobs, releases in 1m20s"));
    // an older daemon sends none
    s.power = None;
    assert!(!status(&s).contains("keeping awake"));
}

#[test]
fn r611_a_peers_power_is_shown_with_its_last_report() {
    let t = status(&{
        let mut s = snapshot();
        s.peers = vec![tasks_daemon::api::PeerView {
            name: "mac".into(),
            node_id: "n".into(),
            state: "offline".into(),
            last_seen_ago_secs: Some(78),
            detail: String::new(),
            did: String::new(),
            roles_out: vec![],
            roles_in: vec![],
            grant_expires_at: None,
            path: String::new(),
            rtt_ms: None,
            reply_ms: None,
            machine: Some(tasks_daemon::api::PeerMachineView {
                cores: 8,
                memory_bytes: 8 << 30,
                slots: vec![],
                waiting: 0,
                running: 0,
                cpu_pct: None,
                memory_pct: None,
                io_pct: None,
                pressure_summary: String::new(),
                cpu_busy_pct: None,
                memory_available_bytes: None,
                swap_total: None,
                swap_used: None,
                swap_in_per_s: None,
                swap_out_per_s: None,
                power: Some("on battery 80%".into()),
                memory_level: None,
                jobs: vec![],
                cores_in_use_milli: None,
            }),
        }];
        s
    });
    assert!(t.contains("0 running; on battery 80%"), "{t}");
}

// R-523
#[test]
fn r523_a_peer_without_psi_shows_its_memory_level_not_a_question_mark() {
    assert_eq!(peer_memory_text(None, Some("warn")), "tight");
    assert_eq!(peer_memory_text(None, None), "?");
}

#[test]
fn r641_memory_reads_ok_tight_or_critical_in_status_for_this_machine_and_peers() {
    assert_eq!(peer_memory_text(Some(4), Some("normal")), "ok (stalled 4%)");
    assert_eq!(peer_memory_text(None, Some("critical")), "critical");
    assert_eq!(peer_memory_text(Some(12), None), "stalled 12%");
    let mut p = crate::testutil::status_snapshot().pressure;
    p.memory = "warn".into();
    p.memory_some = Some(12.0);
    p.memory_full = Some(0.0);
    p.memory_stall_limit = Some(10.0);
    assert_eq!(
        memory_line(&p),
        "memory tight (stalled 12%, fully stalled 0%, tight from 10%)"
    );
    // macOS: a level and no percentages
    p.memory = "normal".into();
    p.memory_some = None;
    assert_eq!(memory_line(&p), "memory ok");
}
