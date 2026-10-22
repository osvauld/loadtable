use super::*;

fn step(name: &str, after: &[&str]) -> Step {
    Step {
        name: name.into(),
        argv: vec!["true".into()],
        after: after.iter().map(|s| s.to_string()).collect(),
        run: false,
        broad_test: false,
    }
}

fn plan(steps: Vec<Step>) -> Plan {
    Plan { steps }
}

fn err(steps: Vec<Step>) -> String {
    plan(steps).validate().unwrap_err()
}

#[test]
fn a_plan_parses_with_defaults_and_rejects_unknown_fields() {
    let p = Plan::parse(
        r#"{"steps":[{"name":"build","argv":["cargo","test","--no-run"]},
                      {"name":"peer","argv":["cargo","test","--test","peer"],"after":["build"],"run":true}]}"#,
    )
    .unwrap();
    assert_eq!(p.steps.len(), 2);
    assert!(!p.steps[0].run && p.steps[1].run);
    assert_eq!(p.steps[1].after, ["build"]);
    assert!(Plan::parse(r#"{"steps":[{"name":"a","argv":["x"],"nope":1}]}"#).is_err());
    assert!(
        Plan::parse(r#"{"steps":[]}"#)
            .unwrap_err()
            .contains("at least one")
    );
}

#[test]
fn validation_names_the_problem() {
    assert!(err(vec![step("a", &[]), step("a", &[])]).contains("listed twice"));
    assert!(err(vec![step("bad name", &[])]).contains("must be"));
    assert!(err(vec![step("", &[])]).contains("must be"));
    assert!(err(vec![step("a", &["a"])]).contains("waits for itself"));
    assert!(err(vec![step("a", &["ghost"])]).contains("not a step"));
    assert!(err(vec![step("a", &[]), step("b", &["a", "a"])]).contains("twice in after"));
    assert!(err(vec![step("a", &["b"]), step("b", &["a"])]).contains("loop"));
    assert!(
        err(vec![
            step("a", &["c"]),
            step("b", &["a"]),
            step("c", &["b"])
        ])
        .contains("loop")
    );
    let mut empty = step("a", &[]);
    empty.argv.clear();
    assert!(err(vec![empty]).contains("no command"));
    let mut nul = step("a", &[]);
    nul.argv = vec!["x\0y".into()];
    assert!(err(vec![nul]).contains("NUL"));
    let many: Vec<Step> = (0..=MAX_STEPS)
        .map(|i| step(&format!("s{i}"), &[]))
        .collect();
    assert!(err(many).contains("at most"));
}

#[test]
fn independent_steps_start_together_and_dependents_wait() {
    let mut s = Scheduler::new(
        plan(vec![
            step("build", &[]),
            step("peer", &["build"]),
            step("lifecycle", &["build"]),
            step("report", &["peer", "lifecycle"]),
        ]),
        OnFailure::Stop,
    );
    assert_eq!(s.start_ready(), [0]);
    assert!(s.start_ready().is_empty(), "build is still running");
    s.finish(0, true, Report::default());
    assert_eq!(s.start_ready(), [1, 2], "both targets start together");
    s.finish(1, true, Report::default());
    assert!(
        s.start_ready().is_empty(),
        "report still waits for lifecycle"
    );
    s.finish(2, true, Report::default());
    assert_eq!(s.start_ready(), [3]);
    s.finish(3, true, Report::default());
    assert!(s.start_ready().is_empty() && s.done() && s.passed());
    assert_eq!(s.progress(), (4, 4));
}

#[test]
fn stop_mode_starts_nothing_new_after_a_failure_but_lets_running_steps_finish() {
    let mut s = Scheduler::new(
        plan(vec![
            step("a", &[]),
            step("b", &[]),
            step("c", &["a"]),
            step("d", &[]),
        ]),
        OnFailure::Stop,
    );
    assert_eq!(s.start_ready(), [0, 1, 3]);
    s.finish(
        0,
        false,
        Report {
            job: Some(7),
            seconds: Some(3.0),
        },
    );
    assert!(s.start_ready().is_empty());
    assert_eq!(
        s.state(1),
        State::Running,
        "a running step is not cancelled"
    );
    assert_eq!(s.state(2), State::Skipped);
    s.finish(1, true, Report::default());
    s.finish(3, true, Report::default());
    assert!(s.done() && !s.passed());
    let text = s.summary();
    assert!(text.contains("failed: a (job 7; tasks logs 7)"), "{text}");
    assert!(text.contains("not run: c"), "{text}");
    assert!(text.contains("steps: 2/4 passed (4 finished)"), "{text}");
}

#[test]
fn continue_mode_skips_only_the_dependents_of_a_failure() {
    let mut s = Scheduler::new(
        plan(vec![
            step("a", &[]),
            step("b", &[]),
            step("c", &["a"]),
            step("d", &["c"]),
            step("e", &["b"]),
        ]),
        OnFailure::Continue,
    );
    assert_eq!(s.start_ready(), [0, 1]);
    s.finish(0, false, Report::default());
    assert_eq!(s.start_ready(), Vec::<usize>::new());
    assert_eq!((s.state(2), s.state(3)), (State::Skipped, State::Skipped));
    s.finish(1, true, Report::default());
    assert_eq!(s.start_ready(), [4], "e does not need a");
    s.finish(4, true, Report::default());
    assert!(s.done() && !s.passed());
}

#[test]
fn notes_warn_about_cargo_hidden_in_a_shell() {
    let mut shell = step("tests", &[]);
    shell.argv = vec![
        "bash".into(),
        "-c".into(),
        "cargo test --test a | grep result".into(),
    ];
    let mut plain = step("ok", &[]);
    plain.argv = vec!["cargo".into(), "test".into()];
    let notes = plan(vec![shell, plain]).notes();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("\"tests\"") && notes[0].contains("one step per target"));
}

#[test]
fn job_ids_come_from_the_run_clients_lines() {
    assert_eq!(
        job_id_from_line(
            "tasks: if this client stops or times out, job 4194 is still live; query with `tasks logs 4194`"
        ),
        Some(4194)
    );
    assert_eq!(
        job_id_from_line("job 4195 failed (exit 101) owner=x"),
        Some(4195)
    );
    assert_eq!(
        job_id_from_line("tasks peer: job 1403 on nixos failed"),
        None
    );
    assert_eq!(job_id_from_line("job done"), None);
    assert_eq!(job_id_from_line("running tests"), None);
}

#[test]
fn durations_read_naturally() {
    assert_eq!(seconds_text(4.2), "4s");
    assert_eq!(seconds_text(128.0), "2m08s");
    assert_eq!(seconds_text(3348.0), "55m48s");
    assert_eq!(seconds_text(3661.0), "1h01m01s");
}

#[test]
fn summary_lines_up_names_and_shows_dependencies() {
    let mut s = Scheduler::new(
        plan(vec![step("build", &[]), step("lifecycle", &["build"])]),
        OnFailure::Stop,
    );
    s.start_ready();
    s.finish(
        0,
        true,
        Report {
            job: Some(12),
            seconds: Some(61.0),
        },
    );
    let text = s.summary();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("STEP       STATE"), "{text}");
    assert!(lines[1].contains("build") && lines[1].contains("1m01s") && lines[1].contains("12"));
    assert!(lines[2].contains("lifecycle") && lines[2].trim_end().ends_with("build"));
}
