use super::*;
use serde_json::json;

fn job(id: i64, owner: &str, phase: &str, submitted: i64, finished: Option<i64>) -> Job {
    let configuration = json!({"argv": ["sh", "-c", "true"], "resource": "run"}).to_string();
    serde_json::from_value(json!({
        "id": id, "phase": phase, "submitted_at": submitted,
        "execution": {"approved_at": submitted, "started_at": submitted, "finished_at": finished,
            "cancel_requested": false, "worker": null, "supervisor": null, "snapshot": null,
            "stages": [], "local_command": true, "detail": ""},
        "spec": {"suite": "local.command", "owner": owner, "source": "/synthetic/checkout",
            "revision": "live-workspace", "configuration": configuration, "cost": "cpu_simulation"}
    }))
    .unwrap()
}

struct State(std::path::PathBuf);
impl State {
    fn new(label: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("tasks-logsearch-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn log(&self, id: i64, text: &str) {
        let dir = self.0.join(format!("jobs/{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("job.log"), text).unwrap();
    }
}
impl Drop for State {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn local(_: &Job) -> (String, Option<i64>) {
    ("laptop".into(), None)
}

fn run(state: &State, jobs: &[Job], q: Query) -> Report {
    search(&state.0, jobs, &q, "laptop", 10_000, &local, &|_| true).unwrap()
}

fn q(pattern: &str) -> Query {
    Query {
        pattern: pattern.into(),
        ..Default::default()
    }
}

#[test]
fn time_parsing_accepts_unix_seconds_and_relative_durations() {
    let now = 1_800_000_000;
    assert_eq!(parse_time("1792216000", now), Ok(1_792_216_000));
    assert_eq!(parse_time("2h", now), Ok(now - 7200));
    assert_eq!(parse_time("90s", now), Ok(now - 90));
    assert_eq!(parse_time("15m", now), Ok(now - 900));
    assert_eq!(parse_time("1.5h", now), Ok(now - 5400));
    assert_eq!(parse_time("3 days ago", now), Ok(now - 3 * 86400));
    assert_eq!(parse_time("1w", now), Ok(now - 604_800));
    assert_eq!(parse_time("now", now), Ok(now));
    assert!(parse_time("2 fortnights", now).is_err());
    assert!(parse_time("yesterday", now).is_err());
    assert!(parse_time("", now).is_err());
}

#[test]
fn substring_and_regex_subset_match_as_documented() {
    let p = |pat: &str, re: bool, ic: bool| Pattern::new(pat, re, ic).unwrap();
    assert!(p("error[E0308]", false, false).is_match("x error[E0308]: mismatched"));
    assert!(!p("ERROR", false, false).is_match("error: x"));
    assert!(p("ERROR", false, true).is_match("error: x"));
    // Regex subset.
    let r = p(r"^error\[E\d+\]", true, false);
    assert!(r.is_match("error[E0308]: mismatched types"));
    assert!(!r.is_match("  error[E0308]"));
    assert!(!r.is_match("error[E]"));
    assert!(p(r"test .* FAILED$", true, false).is_match("test sync::a ... FAILED"));
    assert!(!p(r"test .* FAILED$", true, false).is_match("test sync::a ... FAILED (x)"));
    assert!(p("panicked|SIGSEGV", true, false).is_match("signal: 11, SIGSEGV"));
    assert!(p("colou?r", true, false).is_match("color"));
    assert!(p("colou?r", true, false).is_match("colour"));
    assert!(p("[a-c]+z", true, false).is_match("xxabcabz"));
    assert!(!p("[^a-c]z", true, false).is_match("az"));
    assert!(p(r"\w+@\w+", true, false).is_match("agent@host"));
    assert!(p("^$", true, false).is_match(""));
    assert!(p("FAIL", true, true).is_match("test failed"));
    assert!(p("[A-Z]x", true, true).is_match("qX"));
    // Linear time: no backtracking blow-up on a pathological pattern.
    let long = "a".repeat(20_000);
    let started = std::time::Instant::now();
    assert!(!p("a*a*a*a*a*a*a*b", true, false).is_match(&long));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    // Unsupported syntax is a clear error, not a silent literal.
    // One capture group (progress lines).
    let g = p(r"^\s*Compiling (\S+)", true, false);
    assert_eq!(
        g.captures("   Compiling serde v1.0"),
        Some(Some("serde".to_string()))
    );
    assert_eq!(g.captures("Finished"), None);
    assert_eq!(p("x", true, false).captures("x"), Some(None));
    assert!(p("a(b)c", true, false).is_match("abc"));
    for bad in [
        "(a|b)c", "(a)(b)", "((a))", "(a)*", "(a", "a{2}", "*a", "a**", r"\q", "[abc", "", "a^b",
    ] {
        assert!(Pattern::new(bad, true, false).is_err(), "{bad}");
    }
    assert!(
        Pattern::new("(a|b)c", false, false).is_ok(),
        "substring takes any text"
    );
}

#[test]
fn filters_select_by_owner_ids_phase_and_time_window_newest_first() {
    let jobs = vec![
        job(1, "alpha@laptop", "passed", 1000, Some(1100)),
        job(2, "beta", "failed", 2000, Some(2100)),
        job(3, "alpha", "running", 3000, None),
        job(4, "alpha", "queued", 4000, None),
    ];
    let ids = |q: &Query| {
        select(&jobs, q, 10_000)
            .iter()
            .map(|j| j.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&q("x")), [4, 3, 2, 1]);
    let owner = Query {
        owner: Some("alpha".into()),
        ..q("x")
    };
    assert_eq!(ids(&owner), [4, 3, 1], "agent part before @ matches too");
    let exact = Query {
        owner: Some("alpha@laptop".into()),
        ..q("x")
    };
    assert_eq!(ids(&exact), [1]);
    let phases = Query {
        phases: vec!["failed".into(), "running".into()],
        ..q("x")
    };
    assert_eq!(ids(&phases), [3, 2]);
    let some = Query {
        job_ids: vec![2, 4],
        exclude_job_ids: vec![4],
        ..q("x")
    };
    assert_eq!(ids(&some), [2]);
    // Overlap with [since, until]: job 1 ended at 1100, job 3 is still running.
    let window = Query {
        since: Some(1500),
        until: Some(3500),
        ..q("x")
    };
    assert_eq!(ids(&window), [3, 2]);
    let late = Query {
        since: Some(9000),
        ..q("x")
    };
    assert_eq!(ids(&late), [4, 3], "live jobs extend to now");
}

#[test]
fn search_reports_lines_context_and_respects_every_bound() {
    let state = State::new("bounds");
    let jobs = vec![
        job(1, "alpha", "passed", 1000, Some(1100)),
        job(2, "beta", "failed", 2000, Some(2100)),
        job(3, "alpha", "passed", 3000, Some(3100)),
    ];
    state.log(1, "start\nneedle one\nend\n");
    state.log(2, "a\nb\nneedle two\r\nc\nd\n");
    state.log(3, "nothing here\n");
    let r = run(
        &state,
        &jobs,
        Query {
            context: 1,
            ..q("needle")
        },
    );
    assert_eq!(r.machine, "laptop");
    assert_eq!(r.jobs_scanned, 3);
    assert!(!r.truncated);
    assert_eq!(r.matches.len(), 2);
    let m = &r.matches[0];
    assert_eq!(
        (m.job_id, m.owner.as_str(), m.machine.as_str()),
        (2, "beta", "laptop")
    );
    assert_eq!((m.line_number, m.line.as_str()), (3, "needle two"));
    assert_eq!(
        (m.before.clone(), m.after.clone()),
        (vec!["b".to_string()], vec!["c".to_string()])
    );
    assert_eq!(m.phase, "failed");
    assert_eq!(r.matches[1].job_id, 1, "newest job first");
    // max_matches.
    let r = run(
        &state,
        &jobs,
        Query {
            max_matches: 1,
            ..q("needle")
        },
    );
    assert_eq!(r.matches.len(), 1);
    assert_eq!(r.truncated_reason.as_deref(), Some("max_matches"));
    // max_jobs: only the newest log is scanned.
    let r = run(
        &state,
        &jobs,
        Query {
            max_jobs: 1,
            ..q("needle")
        },
    );
    assert_eq!((r.jobs_scanned, r.matches.len()), (1, 0));
    assert_eq!(r.truncated_reason.as_deref(), Some("max_jobs"));
    assert_eq!(r.jobs_matching_filters, 3);
    // max_bytes: job 3's 13 bytes, then 5 of job 2.
    let r = run(
        &state,
        &jobs,
        Query {
            max_bytes: 18,
            ..q("needle")
        },
    );
    assert!(r.bytes_scanned <= 18, "{r:?}");
    assert!(r.matches.is_empty());
    assert_eq!(r.truncated_reason.as_deref(), Some("max_bytes"));
    // Regex mode and filters together.
    let r = run(
        &state,
        &jobs,
        Query {
            regex: true,
            owner: Some("alpha".into()),
            ..q(r"^needle \w+$")
        },
    );
    assert_eq!(r.matches.iter().map(|m| m.job_id).collect::<Vec<_>>(), [1]);
    // Output bound and long-line cut.
    let long = format!("needle {}\n", "x".repeat(5000));
    state.log(3, &long.repeat(10));
    let r = run(
        &state,
        &jobs,
        Query {
            max_output_bytes: 2048,
            ..q("needle")
        },
    );
    assert_eq!(r.truncated_reason.as_deref(), Some("max_output_bytes"));
    assert!(!r.matches.is_empty() && r.matches.len() < 10);
    assert!(r.matches[0].line.len() <= MAX_LINE_BYTES + "…".len());
    // A bad regex is an error, a missing log is skipped.
    assert!(
        search(
            &state.0,
            &jobs,
            &Query {
                regex: true,
                ..q("(x")
            },
            "laptop",
            0,
            &local,
            &|_| true
        )
        .is_err()
    );
    let r = run(&state, &[job(9, "a", "passed", 1, Some(2))], q("needle"));
    assert_eq!((r.jobs_scanned, r.matches.len()), (1, 0));
}

#[test]
fn machine_names_come_from_the_callback_and_filter_jobs() {
    let state = State::new("machine");
    let jobs = vec![
        job(1, "alpha", "passed", 1000, Some(1100)),
        job(2, "alpha", "passed", 2000, Some(2100)),
    ];
    state.log(1, "needle local\n");
    state.log(2, "needle forwarded\n");
    let machine_of = |j: &Job| {
        if j.id == 2 {
            ("worker".to_string(), Some(77))
        } else {
            ("laptop".to_string(), None)
        }
    };
    let only_worker = |m: &str| m == "worker";
    let r = search(
        &state.0,
        &jobs,
        &q("needle"),
        "laptop",
        0,
        &machine_of,
        &only_worker,
    )
    .unwrap();
    assert_eq!(r.matches.len(), 1);
    assert_eq!(r.matches[0].machine, "worker");
    assert_eq!(r.matches[0].remote_job_id, Some(77));
    let json = serde_json::to_value(&r.matches[0]).unwrap();
    assert!(
        json.get("before").is_none(),
        "empty context is omitted: {json}"
    );
}
