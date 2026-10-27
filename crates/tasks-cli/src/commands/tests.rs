use super::*;
use crate::client::is_read_only;
use crate::fake::FakeClient;
use crate::testutil::{ClosedPipe, Harness, Pieces, api_err, argv, job};
use std::sync::{Arc, Mutex};
use tasks_daemon::api::{
    GrepGroup, GrepLine, HelloInfo, HistoryView, JobGrep, LogSearchQuery as Q, MachineHistory,
    RecipeHistory, Request, Response, SampleView, SeriesView,
};

fn chunk(data: &str, next: u64, done: bool) -> Response {
    Response::LogChunk {
        data: data.into(),
        next_offset: next,
        done,
    }
}

/// A daemon that accepts the submit as job `id`, serves `chunks` in order for follow reads and
/// answers `get` with `final_job`.
fn daemon(id: i64, chunks: Vec<Response>, final_job: JobView) -> Arc<FakeClient> {
    let chunks = Mutex::new(
        chunks
            .into_iter()
            .collect::<std::collections::VecDeque<_>>(),
    );
    Arc::new(FakeClient::from_fn(move |req| match req {
        Request::Submit { .. } => Ok(Response::Submitted {
            id: JobId(id),
            warnings: vec![],
        }),
        Request::List { .. } => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
        Request::Logs { .. } => Ok(chunks
            .lock()
            .map_err(|_| ClientError::BadReply("poisoned".into()))?
            .pop_front()
            .unwrap_or_else(|| chunk("", 0, true))),
        Request::Get { .. } => Ok(Response::Job {
            job: Box::new(final_job.clone()),
        }),
        other => Err(ClientError::BadReply(format!("unexpected {other:?}"))),
    }))
}

fn finished(id: i64, phase: Phase) -> JobView {
    job(id, "me", "/w/repo", phase, "cargo test")
}

fn submits(c: &FakeClient) -> usize {
    c.requests()
        .iter()
        .filter(|r| matches!(r, Request::Submit { .. }))
        .count()
}

// R-183, R-111
#[test]
fn r183_run_prints_the_submit_line_and_follows_the_log_to_stdout() {
    let c = daemon(
        7,
        vec![chunk("compiling\n", 10, false), chunk("done\n", 15, true)],
        finished(7, Phase::Passed),
    );
    let r = Harness::default().run(&c, &["run", "--", "cargo", "test", "-p", "x"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(r.out, "compiling\ndone\njob 7 passed (exit 0) owner=me\n");
    assert!(
        r.err
            .starts_with("loadtable: submitted job 7; cwd /w/repo; argv cargo test -p x;"),
        "{}",
        r.err
    );
    let Some(Request::Submit { spec }) = c
        .requests()
        .into_iter()
        .find(|r| matches!(r, Request::Submit { .. }))
    else {
        panic!("no submit")
    };
    assert_eq!(spec.owner.as_str(), "me");
    assert_eq!(spec.cwd, PathBuf::from("/w/repo"));
    assert_eq!(spec.argv, argv("cargo test -p x"));
    assert_eq!(spec.placement, Placement::Here);
    assert_eq!(spec.kind, Kind::Test);
}

#[test]
fn r111_the_follower_streams_appended_bytes_once_in_order_and_lines_wait_for_newlines() {
    let c = daemon(
        1,
        vec![
            chunk("he", 2, false),
            chunk("llo\nwor", 9, false),
            chunk("", 9, false),
            chunk("ld\ntail-without-newline", 32, true),
        ],
        finished(1, Phase::Passed),
    );
    let h = Harness::default();
    let mut out = Pieces::default();
    let mut err = Vec::new();
    let code = h.run_with(&c, &["run", "--", "x"], &mut out, &mut err);
    assert_eq!(code, 0);
    assert_eq!(
        out.0
            .iter()
            .filter(|p| *p != "\n")
            .cloned()
            .collect::<Vec<_>>(),
        [
            "hello\n",
            "world\n",
            "tail-without-newline",
            "job 1 passed (exit 0) owner=me"
        ]
    );
    let offsets: Vec<u64> = c
        .requests()
        .iter()
        .filter_map(|r| match r {
            Request::Logs {
                view: LogView::Follow { offset, .. },
                ..
            } => Some(*offset),
            _ => None,
        })
        .collect();
    assert_eq!(
        offsets,
        [0, 2, 9, 9],
        "each read starts where the last ended"
    );
}

#[test]
fn run_exits_with_the_jobs_exit_code() {
    for (phase, want) in [
        (Phase::Failed { exit: Some(101) }, 101),
        (Phase::Failed { exit: None }, 1),
        (Phase::TimedOut { exit: None }, 124),
        (Phase::Cancelled, 125),
    ] {
        let c = daemon(3, vec![chunk("", 0, true)], finished(3, phase));
        assert_eq!(Harness::default().run(&c, &["run", "--", "x"]).code, want);
    }
}

#[test]
fn detach_and_enqueue_print_the_id_and_do_not_follow() {
    for args in [
        &["run", "--detach", "--", "x"][..],
        &["enqueue", "--", "x"],
        &["submit", "--", "x"],
    ] {
        let c = daemon(12, vec![], finished(12, Phase::Passed));
        let r = Harness::default().run(&c, args);
        assert_eq!((r.code, r.out.as_str()), (0, "12\n"), "{args:?}");
        assert!(r.err.contains("loadtable: submitted job 12;"));
        assert!(
            c.requests()
                .iter()
                .all(|q| !matches!(q, Request::Logs { .. }))
        );
    }
}

#[test]
fn a_queued_job_prints_what_it_waits_for_while_following() {
    let mut waiting = finished(
        4,
        Phase::Waiting(tasks_core::WaitReason::Slot(tasks_core::SlotKind::Compile)),
    );
    waiting.wait_text = Some("compile slot held by job 2".into());
    let polls = Mutex::new(0);
    let c = Arc::new(FakeClient::from_fn(move |req| match req {
        Request::Submit { .. } => Ok(Response::Submitted {
            id: JobId(4),
            warnings: vec![],
        }),
        Request::List { .. } => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
        Request::Logs { .. } => {
            let mut n = polls
                .lock()
                .map_err(|_| ClientError::BadReply("p".into()))?;
            *n += 1;
            Err(api_err(ErrorCode::NotFound, "no log yet")).or_else(|e: ClientError| {
                if *n > 2 {
                    Ok(chunk("go\n", 3, true))
                } else {
                    Err(e)
                }
            })
        }
        Request::Get { .. } => Ok(Response::Job {
            job: Box::new(
                if *polls
                    .lock()
                    .map_err(|_| ClientError::BadReply("p".into()))?
                    > 2
                {
                    finished(4, Phase::Passed)
                } else {
                    waiting.clone()
                },
            ),
        }),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let r = Harness::default().run(&c, &["run", "--", "x"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(
        r.err
            .contains("loadtable: job 4 waiting: compile slot held by job 2"),
        "{}",
        r.err
    );
    assert_eq!(r.out, "go\njob 4 passed (exit 0) owner=me\n");
}

// R-104
#[test]
fn r104_a_signal_detaches_without_cancelling_and_says_not_to_resubmit() {
    let c = daemon(
        7,
        vec![chunk("a\n", 2, false); 20],
        finished(7, Phase::Running),
    );
    let h = Harness {
        signal_after: Some((3, 2)),
        ..Harness::default()
    }; // SIGINT after three polls
    let r = h.run(&c, &["run", "--", "x"]);
    assert_eq!(r.code, 130);
    assert!(r.err.contains("job 7 is still live"), "{}", r.err);
    assert!(r.err.contains("do not resubmit"));
    assert!(!r.out.is_empty());
    assert!(
        c.requests()
            .iter()
            .all(|q| !matches!(q, Request::Cancel { .. }))
    );
    assert_eq!(submits(&c), 1);
    for (sig, code) in [(15, 143), (1, 129)] {
        let h = Harness {
            signal_after: Some((0, sig)),
            ..Harness::default()
        };
        let r = h.run(
            &daemon(8, vec![], finished(8, Phase::Running)),
            &["run", "--", "x"],
        );
        assert_eq!(r.code, code);
        assert!(r.err.contains("job 8 is still live"));
    }
}

#[test]
fn r104_a_client_error_mid_follow_detaches_and_names_the_job() {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::Submit { .. } => Ok(Response::Submitted {
            id: JobId(9),
            warnings: vec![],
        }),
        Request::List { .. } => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
        _ => Err(ClientError::ReplyLost {
            detail: "the daemon closed the connection".into(),
        }),
    }));
    let r = Harness::default().run(&c, &["run", "--", "x"]);
    assert_eq!(r.code, 3);
    assert!(r.err.contains("no reply from the daemon"), "{}", r.err);
    assert!(
        r.err.contains("job 9 is still live") && r.err.contains("do not resubmit"),
        "{}",
        r.err
    );
    assert!(
        c.requests()
            .iter()
            .all(|q| !matches!(q, Request::Cancel { .. }))
    );
}

#[test]
fn a_final_state_that_cannot_be_read_is_not_called_live() {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::Submit { .. } => Ok(Response::Submitted {
            id: JobId(9),
            warnings: vec![],
        }),
        Request::List { .. } => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
        Request::Logs { .. } => Ok(chunk("x\n", 2, true)),
        _ => Err(ClientError::ReplyLost {
            detail: "gone".into(),
        }),
    }));
    let r = Harness::default().run(&c, &["run", "--", "x"]);
    assert_eq!(r.out, "x\n");
    assert!(
        r.err
            .contains("finished but its final state could not be read"),
        "{}",
        r.err
    );
    assert!(!r.err.contains("still live"));
}

// R-105, R-106
#[test]
fn r105_a_closed_stdout_sends_no_mutation_after_the_job_is_terminal() {
    let c = daemon(
        5,
        vec![chunk("line\n", 5, false), chunk("more\n", 10, true)],
        finished(5, Phase::Passed),
    );
    let h = Harness::default();
    let mut err = Vec::new();
    let code = h.run_with(&c, &["run", "--", "x"], &mut ClosedPipe, &mut err);
    assert_eq!(code, 141);
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("job 5 is still live"), "{err}");
    // the only mutation ever sent is the submit, and nothing but reads follows it
    let reqs = c.requests();
    let first = reqs
        .iter()
        .position(|r| matches!(r, Request::Submit { .. }))
        .unwrap();
    assert!(reqs.iter().skip(first + 1).all(is_read_only), "{reqs:?}");
    assert_eq!(submits(&c), 1);
}

#[test]
fn r105_the_whole_run_sends_one_mutation_at_most() {
    let c = daemon(
        5,
        vec![chunk("l\n", 2, true)],
        finished(5, Phase::Failed { exit: Some(2) }),
    );
    let r = Harness::default().run(&c, &["run", "--", "x"]);
    assert_eq!(r.code, 2);
    let mutations = c
        .requests()
        .into_iter()
        .filter(|q| !is_read_only(q))
        .count();
    assert_eq!(mutations, 1);
}

#[test]
fn r106_epipe_is_exit_141_not_a_panic() {
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let h = Harness::default();
    let mut err = Vec::new();
    // even the id of an enqueue goes to a closed stdout
    let code = h.run_with(&c, &["enqueue", "--", "x"], &mut ClosedPipe, &mut err);
    assert_eq!(code, 141);
    let c = Arc::new(FakeClient::from_fn(|_| {
        Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        })
    }));
    let code = h.run_with(&c, &["list"], &mut ClosedPipe, &mut Vec::new());
    assert_eq!(code, 141);
}

// R-099, R-100
fn lost_submit_daemon(listed: Vec<JobView>) -> Arc<FakeClient> {
    Arc::new(FakeClient::from_fn(move |req| match req {
        Request::Submit { .. } => Err(ClientError::ReplyLost {
            detail: "the daemon closed the connection".into(),
        }),
        Request::List {
            recent_finished: Some(_),
            ..
        } => Ok(Response::Jobs {
            jobs: listed.clone(),
            truncated: false,
        }),
        Request::List { .. } => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
        Request::Logs { .. } => Ok(chunk("ok\n", 3, true)),
        Request::Get { .. } => Ok(Response::Job {
            job: Box::new(finished(31, Phase::Passed)),
        }),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }))
}

fn recent(id: i64, cmd: &str) -> JobView {
    let mut j = job(id, "me", "/w/repo", Phase::Queued, cmd);
    j.submitted_at_ms = crate::testutil::NOW;
    j
}

#[test]
fn r099_a_lost_submit_reply_is_reconciled_without_resubmitting() {
    let c = lost_submit_daemon(vec![recent(31, "cargo check"), recent(32, "cargo test")]);
    let r = Harness::default().run(&c, &["run", "--", "cargo", "check"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(submits(&c), 1, "never resent");
    assert!(
        r.err.contains(
            "found the submitted job 31 in the ledger by exact spec match; not resubmitted"
        ),
        "{}",
        r.err
    );
    assert_eq!(r.out, "ok\njob 31 passed (exit 0) owner=me\n");
}

#[test]
fn r099_submit_candidates_need_an_exact_recent_spec_match() {
    let mut old = recent(31, "cargo check");
    old.submitted_at_ms = crate::testutil::NOW - 60_000;
    let mut other_cwd = recent(32, "cargo check");
    other_cwd.cwd = PathBuf::from("/elsewhere");
    let mut other_owner = recent(33, "cargo check");
    other_owner.owner = "someone".into();
    let c = lost_submit_daemon(vec![
        old,
        other_cwd,
        other_owner,
        recent(34, "cargo check -p x"),
    ]);
    let r = Harness::default().run(&c, &["enqueue", "--", "cargo", "check"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("no matching job is listed yet"), "{}", r.err);
    assert!(r.err.contains("Do NOT resubmit"));
    assert_eq!(submits(&c), 1);
}

#[test]
fn r099_ambiguity_says_where_to_look_and_not_to_resubmit() {
    let c = lost_submit_daemon(vec![recent(41, "cargo check"), recent(42, "cargo check")]);
    let r = Harness::default().run(&c, &["enqueue", "--", "cargo", "check"]);
    assert_eq!(r.code, 1);
    assert!(
        r.err.contains("several identical jobs match: 41, 42"),
        "{}",
        r.err
    );
    assert!(
        r.err.contains("loadtable list --who me") && r.err.contains("Do NOT resubmit"),
        "{}",
        r.err
    );
    assert_eq!(submits(&c), 1);
}

#[test]
fn r100_a_delivered_cancel_with_a_lost_reply_is_never_replayed() {
    let c = Arc::new(FakeClient::from_fn(|_| {
        Err(ClientError::ReplyLost {
            detail: "no reply within 60s".into(),
        })
    }));
    let r = Harness::default().run(&c, &["cancel", "9"]);
    assert_eq!(r.code, 1);
    assert!(
        r.err.contains("was not resent") && r.err.contains("loadtable get 9"),
        "{}",
        r.err
    );
    assert_eq!(c.requests().len(), 1);
}

// R-187
#[test]
fn r187_shared_cwd_lists_only_other_owners_unfinished_jobs() {
    let jobs = vec![
        job(1, "other", "/w/repo", Phase::Running, "x"),
        job(2, "other", "/w/repo", Phase::Passed, "x"),
        job(3, "other", "/elsewhere", Phase::Running, "x"),
        job(4, "me", "/w/repo", Phase::Running, "x"),
        job(5, "third", "/w/repo", Phase::Queued, "x"),
    ];
    let w = shared_cwd_warning(&jobs, "me", Path::new("/w/repo")).unwrap();
    assert!(w.contains("1 (other, running), 5 (third, queued)"), "{w}");
    assert!(w.contains("not blocking"));
    assert_eq!(
        shared_cwd_warning(&jobs[1..2], "me", Path::new("/w/repo")),
        None
    );
}

#[test]
fn r187_run_warns_but_still_submits() {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::List {
            active_only: true, ..
        } => Ok(Response::Jobs {
            jobs: vec![job(1, "other", "/w/repo", Phase::Running, "x")],
            truncated: false,
        }),
        Request::Submit { .. } => Ok(Response::Submitted {
            id: JobId(2),
            warnings: vec![],
        }),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let r = Harness::default().run(&c, &["enqueue", "--", "x"]);
    assert_eq!(r.code, 0);
    assert!(
        r.err
            .contains("other owners have unfinished jobs in this same cwd"),
        "{}",
        r.err
    );
    assert_eq!(submits(&c), 1);
}

// R-191, R-193, R-190
fn never_called() -> Arc<FakeClient> {
    Arc::new(FakeClient::from_fn(|_| {
        Err(ClientError::BadReply("must not be called".into()))
    }))
}

/// A daemon that answers every submit with `refused` and the given text.
fn refusing(text: &'static str) -> Arc<FakeClient> {
    Arc::new(FakeClient::from_fn(move |req| match req {
        Request::List { .. } => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
        Request::Submit { .. } => Err(api_err(ErrorCode::Refused, text)),
        other => Err(ClientError::BadReply(format!("unexpected {other:?}"))),
    }))
}

fn submitted_spec(c: &FakeClient) -> JobSpec {
    let Some(Request::Submit { spec }) = c
        .requests()
        .into_iter()
        .find(|r| matches!(r, Request::Submit { .. }))
    else {
        panic!("no submit")
    };
    spec
}

#[test]
fn r191_the_daemons_refusal_of_a_broad_test_is_shown_and_exits_2() {
    let text = "refused: this builds every test target of -p x; your filter 'video_call' matches: --lib. Re-run with those, or pass --broad-test (MCP: broad_test: true) to build every target deliberately.";
    let c = refusing(text);
    let r = Harness::default().run(&c, &["run", "--", "cargo", "test", "-p", "x", "video_call"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains(text), "{}", r.err);
    assert_eq!(submits(&c), 1, "the daemon is the one that decides");
    assert!(!submitted_spec(&c).broad_test_escape);
}

#[test]
fn r191_the_escape_flag_travels_on_the_spec_and_the_environment_is_not_read() {
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let r = Harness::default().run(
        &c,
        &["enqueue", "--broad-test", "--", "cargo", "test", "foo"],
    );
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(submitted_spec(&c).broad_test_escape);
    // LOADTABLE_BROAD_TEST is the daemon's policy now: setting it here changes nothing
    let mut h = Harness::default();
    h.env.insert("LOADTABLE_BROAD_TEST".into(), "warn".into());
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let r = h.run(&c, &["enqueue", "--", "cargo", "test", "foo"]);
    assert_eq!(r.code, 0);
    assert!(!submitted_spec(&c).broad_test_escape);
    assert!(!r.err.contains("LOADTABLE_BROAD_TEST"), "{}", r.err);
}

#[test]
fn r191_plans_and_suites_carry_the_escape_too() {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::SubmitPlan { .. } | Request::SubmitSuite { .. } => Ok(Response::PlanSubmitted {
            plan_id: 1,
            jobs: vec![("a".into(), JobId(1))],
        }),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let mut h = Harness::default();
    h.files.insert(
        PathBuf::from("plan.json"),
        r#"{"steps":[{"name":"a","argv":["cargo","test","foo"]}]}"#.into(),
    );
    assert_eq!(h.run(&c, &["steps", "plan.json", "--broad-test"]).code, 0);
    assert_eq!(h.run(&c, &["steps", "plan.json"]).code, 0);
    assert_eq!(h.run(&c, &["suite", "run", "core", "--broad-test"]).code, 0);
    let flags: Vec<bool> =
        c.requests()
            .iter()
            .map(|r| match r {
                Request::SubmitPlan { broad_test, .. }
                | Request::SubmitSuite { broad_test, .. } => *broad_test,
                other => panic!("{other:?}"),
            })
            .collect();
    assert_eq!(flags, [true, false, true]);
}

#[test]
fn r190_option_conflicts_are_reported_before_any_daemon_request() {
    let c = never_called();
    let r = Harness::default().run(&c, &["run", "--pin", "--anywhere", "--", "x"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("--pin and --anywhere conflict"), "{}", r.err);
    let r = Harness::default().run(
        &c,
        &[
            "run", "--device", "S1", "--peer", "nixos", "--", "adb", "devices",
        ],
    );
    assert_eq!(r.code, 2);
    assert!(
        r.err
            .contains("a device job runs on the machine the device is attached to"),
        "{}",
        r.err
    );
    let r = Harness::default().run(&c, &["run", "--who", "bad owner", "--", "x"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("is not valid"), "{}", r.err);
    assert!(c.requests().is_empty());
}

#[test]
fn r095_a_timeout_travels_on_the_spec() {
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let r = Harness::default().run(&c, &["enqueue", "--timeout", "30", "--", "x"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(submitted_spec(&c).timeout_secs, Some(30));
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    Harness::default().run(&c, &["enqueue", "--", "x"]);
    assert_eq!(submitted_spec(&c).timeout_secs, None);
    // an impossible limit is refused here, as the daemon would
    let r = Harness::default().run(&never_called(), &["enqueue", "--timeout", "0", "--", "x"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("timeout of 0 s"), "{}", r.err);
}

#[test]
fn r210_a_denied_device_is_the_daemons_to_refuse_and_the_device_travels_as_a_resource() {
    let c = refusing("refused: the command names the denied device ZD22257P6G");
    let r = Harness::default().run(
        &c,
        &["run", "--device", "ZD22257P6G", "--", "adb", "devices"],
    );
    assert_eq!(r.code, 2);
    assert!(r.err.contains("denied device"), "{}", r.err);
    assert_eq!(
        submitted_spec(&c).requirements.device,
        Some(DeviceId("ZD22257P6G".into()))
    );
}

#[test]
fn a_device_job_carries_the_serial_and_the_device_kind() {
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let r = Harness::default().run(
        &c,
        &[
            "enqueue",
            "--device",
            "9BWAY1LSDC",
            "--",
            "adb",
            "-s",
            "9BWAY1LSDC",
            "devices",
        ],
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let Some(Request::Submit { spec }) = c
        .requests()
        .into_iter()
        .find(|r| matches!(r, Request::Submit { .. }))
    else {
        panic!()
    };
    assert_eq!(spec.kind, Kind::Device);
    assert_eq!(
        spec.requirements.device,
        Some(DeviceId("9BWAY1LSDC".into()))
    );
}

#[test]
fn anywhere_for_a_build_says_it_stays_local() {
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let r = Harness::default().run(&c, &["enqueue", "--anywhere", "--", "cargo", "build"]);
    assert_eq!(r.code, 0);
    assert!(r.err.contains("--anywhere has no effect here"), "{}", r.err);
    let r = Harness::default().run(&c, &["enqueue", "--anywhere", "--", "cargo", "check"]);
    assert!(!r.err.contains("no effect"));
    let Some(Request::Submit { spec }) = c
        .requests()
        .into_iter()
        .last()
        .filter(|r| matches!(r, Request::Submit { .. }))
    else {
        panic!()
    };
    assert_eq!(spec.placement, Placement::Anywhere);
}

#[test]
fn the_owner_comes_from_who_then_the_environment() {
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let mut h = Harness::default();
    h.env.remove("LOADTABLE_WHO");
    h.env.insert("USER".into(), "abe".into());
    h.run(&c, &["enqueue", "--", "x"]);
    h.run(&c, &["enqueue", "--who", "agent-1", "--", "x"]);
    let owners: Vec<String> = c
        .requests()
        .iter()
        .filter_map(|r| match r {
            Request::Submit { spec } => Some(spec.owner.as_str().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(owners, ["abe", "agent-1"]);
    h.env.remove("USER");
    let r = h.run(&never_called(), &["enqueue", "--", "x"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("no job owner"));
}

// R-185, R-186
#[test]
fn r185_select_filters_by_owner_activity_and_limit() {
    let jobs = vec![
        job(1, "a", "/w", Phase::Passed, "x"),
        job(2, "b", "/w", Phase::Running, "x"),
        job(3, "a", "/w", Phase::Running, "x"),
        job(4, "a", "/w", Phase::Queued, "x"),
    ];
    let ids = |v: Vec<JobView>| v.iter().map(|j| j.id.0).collect::<Vec<_>>();
    assert_eq!(ids(select(jobs.clone(), Some("a"), false, None)), [1, 3, 4]);
    assert_eq!(ids(select(jobs.clone(), None, true, None)), [2, 3, 4]);
    assert_eq!(ids(select(jobs.clone(), Some("a"), true, Some(1))), [4]);
    assert_eq!(ids(select(jobs, None, false, Some(2))), [3, 4]);
}

#[test]
fn r185_list_asks_for_recent_jobs_and_prints_the_table_or_json() {
    let jobs = vec![
        job(1, "a", "/w", Phase::Passed, "x"),
        job(2, "a", "/w", Phase::Running, "y"),
    ];
    let reply = jobs.clone();
    let c = Arc::new(FakeClient::from_fn(move |_| {
        Ok(Response::Jobs {
            jobs: reply.clone(),
            truncated: true,
        })
    }));
    let r = Harness::default().run(&c, &["list", "--who", "a", "--limit", "5"]);
    assert_eq!(r.code, 0);
    assert!(r.out.contains("running") && r.out.contains("passed"));
    assert!(r.err.contains("cut the list at its row limit"));
    assert_eq!(
        c.requests().first(),
        Some(&Request::List {
            owner: Some("a".into()),
            active_only: false,
            recent_finished: Some(5),
            limit: None
        })
    );
    let r = Harness::default().run(&c, &["list", "--active", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&r.out).unwrap();
    assert_eq!(v["truncated"], true);
    assert_eq!(
        v["jobs"].as_array().map(Vec::len),
        Some(1),
        "--active drops the passed job"
    );
    assert_eq!(v["jobs"][0]["id"], 2);
    assert!(matches!(
        c.requests().last(),
        Some(Request::List {
            active_only: true,
            recent_finished: None,
            ..
        })
    ));
}

#[test]
fn r186_list_active_merges_queue_position_from_status() {
    let mut q = job(
        6,
        "me",
        "/w",
        Phase::Waiting(tasks_core::WaitReason::Slot(tasks_core::SlotKind::Compile)),
        "cargo build",
    );
    q.wait_text = Some("compile slot busy".into());
    let jobs = vec![q];
    let c = Arc::new(FakeClient::from_fn(move |req| match req {
        Request::List { .. } => Ok(Response::Jobs {
            jobs: jobs.clone(),
            truncated: false,
        }),
        Request::Status => Ok(Response::Status(Box::new(
            crate::testutil::status_snapshot(),
        ))),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let r = Harness::default().run(&c, &["list", "--active"]);
    assert!(
        r.out
            .contains("waits for: compile slots busy; position 1; starts in ~2m"),
        "{}",
        r.out
    );
    let r = Harness::default().run(&c, &["list", "--active", "--brief"]);
    assert!(!r.out.contains("position"));
}

#[test]
fn get_prints_one_job_or_its_json_and_a_missing_job_exits_4() {
    let j = job(5, "a", "/w", Phase::Passed, "cargo test");
    let reply = j.clone();
    let c = Arc::new(FakeClient::from_fn(move |req| match req {
        Request::Get { id } if id.0 == 5 => Ok(Response::Job {
            job: Box::new(reply.clone()),
        }),
        _ => Err(api_err(ErrorCode::NotFound, "no such job 6")),
    }));
    assert!(
        Harness::default()
            .run(&c, &["get", "5"])
            .out
            .starts_with("job 5: passed")
    );
    let v: serde_json::Value =
        serde_json::from_str(&Harness::default().run(&c, &["get", "5", "--json"]).out).unwrap();
    assert_eq!(v["id"], 5);
    let r = Harness::default().run(&c, &["get", "6"]);
    assert_eq!((r.code, r.err.as_str()), (4, "loadtable: no such job 6\n"));
}

// logs
#[test]
fn logs_tail_grep_and_follow() {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::Logs {
            view: LogView::Tail { lines },
            ..
        } => Ok(Response::LogTail {
            lines: vec![format!("last {lines}"), "end".into()],
            running: true,
            end_offset: None,
        }),
        Request::Logs {
            view:
                LogView::Grep {
                    pattern,
                    context,
                    regex,
                    ..
                },
            ..
        } => {
            assert_eq!((pattern.as_str(), *context, *regex), ("FAILED", 1, true));
            Ok(Response::LogGrep {
                groups: vec![GrepGroup {
                    lines: vec![GrepLine {
                        n: 8,
                        text: "FAILED x".into(),
                        matched: true,
                    }],
                }],
                matches: 1,
                truncated: false,
            })
        }
        Request::Logs { .. } => Ok(chunk("streamed\n", 9, true)),
        Request::Get { .. } => Ok(Response::Job {
            job: Box::new(job(3, "a", "/w", Phase::Passed, "x")),
        }),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let r = Harness::default().run(&c, &["logs", "3", "--tail", "2"]);
    assert_eq!(r.out, "last 2\nend\n");
    assert!(r.err.contains("still running"));
    assert_eq!(
        Harness::default().run(&c, &["logs", "3"]).out,
        "last 100\nend\n"
    );
    assert_eq!(
        Harness::default()
            .run(&c, &["logs", "3", "--grep", "FAILED", "-C", "1"])
            .out,
        "8:FAILED x\n"
    );
    assert_eq!(
        Harness::default().run(&c, &["logs", "3", "--follow"]).out,
        "streamed\n"
    );
    let r = Harness::default().run(&c, &["logs", "3", "--tail", "0"]);
    assert_eq!(r.code, 2);
    let r = Harness::default().run(&c, &["logs", "3", "--tail", "99999"]);
    assert!(r.err.contains("out of range"), "{}", r.err);
    let r = Harness::default().run(&c, &["logs", "3", "--grep", "x", "-C", "9999"]);
    assert_eq!(r.code, 2);
}

#[test]
fn log_search_sends_the_query_and_prints_matches_by_job() {
    let seen = Arc::new(Mutex::new(None));
    let seen2 = seen.clone();
    let c = Arc::new(FakeClient::from_fn(move |req| {
        if let Request::LogSearch { query } = req {
            *seen2
                .lock()
                .map_err(|_| ClientError::BadReply("p".into()))? = Some(query.clone());
        }
        Ok(Response::LogSearch {
            results: vec![JobGrep {
                id: JobId(2),
                owner: "a".into(),
                phase: "failed".into(),
                groups: vec![GrepGroup {
                    lines: vec![GrepLine {
                        n: 1,
                        text: "boom".into(),
                        matched: true,
                    }],
                }],
            }],
            matches: 1,
            truncated: false,
        })
    }));
    let r = Harness::default().run(
        &c,
        &[
            "log-search",
            "boom",
            "-F",
            "--who",
            "a",
            "--id",
            "2",
            "--phase",
            "failed",
            "--since",
            "2h",
            "-C",
            "1",
            "--max-matches",
            "10",
        ],
    );
    assert!(
        r.out.contains("== job 2 (a, failed)\n1:boom\n"),
        "{}",
        r.out
    );
    let q = seen.lock().unwrap().clone().unwrap();
    assert_eq!(
        q,
        Q {
            pattern: "boom".into(),
            regex: false,
            owner: Some("a".into()),
            ids: vec![JobId(2)],
            phase: Some("failed".into()),
            since: Some("2h".into()),
            until: None,
            context: 1,
            max_matches: Some(10),
            max_bytes: None
        }
    );
}

// status / control
#[test]
fn r231_admission_flags_a_shell_that_disagrees_with_the_daemon_about_the_lock_dir() {
    let c = Arc::new(FakeClient::from_fn(|_| {
        Ok(Response::Status(Box::new(
            crate::testutil::status_snapshot(),
        )))
    }));
    let h = Harness {
        lock_dir: Some(PathBuf::from("/tmp/claude-1000")),
        ..Harness::default()
    };
    let r = h.run(&c, &["admission"]);
    assert!(
        r.out.contains("lock dir: /state/slots (the daemon's)"),
        "{}",
        r.out
    );
    assert!(
        r.out
            .contains("would resolve the lock dir to /tmp/claude-1000")
    );
    let v: serde_json::Value = serde_json::from_str(&h.run(&c, &["status", "--json"]).out).unwrap();
    assert_eq!(v["daemon"]["lock_dir"], "/state/slots");
    assert!(h.run(&c, &["status"]).out.contains("daemon: loadtabled"));
}

#[test]
fn r108_cancel_sends_the_reason_to_the_daemon() {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::Cancel { id, .. } => Ok(Response::Cancelled {
            id: *id,
            result: "cancelled".into(),
        }),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let r = Harness::default().run(&c, &["cancel", "1", "--reason", "wrong branch"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("(reason: wrong branch)"), "{}", r.out);
    assert_eq!(
        c.requests(),
        [Request::Cancel {
            id: JobId(1),
            who: "me".into(),
            reason: Some("wrong branch".into())
        }]
    );
}

#[test]
fn cancel_reports_each_outcome_and_the_refusals() {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::Cancel { id, who, .. } => match id.0 {
            1 => Ok(Response::Cancelled {
                id: *id,
                result: "cancelled".into(),
            }),
            2 => Ok(Response::Cancelled {
                id: *id,
                result: "requested".into(),
            }),
            3 => Ok(Response::Cancelled {
                id: *id,
                result: "already_finished".into(),
            }),
            4 => Err(api_err(
                ErrorCode::NotOwner,
                &format!("job 4 belongs to someone else, not {who}"),
            )),
            _ => Err(api_err(ErrorCode::NotFound, "no such job")),
        },
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let h = Harness::default();
    assert!(
        h.run(&c, &["cancel", "1"])
            .out
            .contains("cancelled before it started")
    );
    let r = h.run(&c, &["cancel", "2", "--reason", "stale"]);
    assert!(
        r.out.contains("cancel requested") && r.out.contains("(reason: stale)"),
        "{}",
        r.out
    );
    assert!(
        h.run(&c, &["cancel", "3"])
            .out
            .contains("had already finished")
    );
    let r = h.run(&c, &["cancel", "4"]);
    assert_eq!(
        (r.code, r.err.contains("belongs to someone else, not me")),
        (2, true)
    );
    assert_eq!(h.run(&c, &["cancel", "5"]).code, 4);
    assert_eq!(
        c.requests().first(),
        Some(&Request::Cancel {
            id: JobId(1),
            who: "me".into(),
            reason: None,
        })
    );
}

#[test]
fn r188_execution_and_drain_print_one_line() {
    let c = Arc::new(FakeClient::from_fn(|_| Ok(Response::Ack)));
    let h = Harness::default();
    let r = h.run(&c, &["execution", "stop"]);
    assert_eq!(r.out.lines().count(), 1);
    assert!(r.out.contains("execution stopped"));
    assert!(
        h.run(&c, &["execution", "start"])
            .out
            .contains("execution started")
    );
    assert!(h.run(&c, &["drain"]).out.contains("`loadtable resume`"));
    assert!(h.run(&c, &["resume"]).out.contains("resumed"));
    assert_eq!(
        c.requests(),
        [
            Request::Execution { enabled: false },
            Request::Execution { enabled: true },
            Request::Drain {
                cancel: false,
                who: Some("me".into())
            },
            Request::Drain {
                cancel: true,
                who: Some("me".into())
            },
        ]
    );
}

#[test]
fn estimate_sends_the_command() {
    let c = Arc::new(FakeClient::from_fn(|_| {
        Ok(Response::Estimate(tasks_daemon::api::EstimateView {
            recipe: "r".into(),
            median_secs: Some(60.0),
            p90_secs: Some(90.0),
            samples: 3,
            text: "about a minute".into(),
            why_not: None,
            rough: false,
        }))
    }));
    let r = Harness::default().run(&c, &["estimate", "--", "cargo", "check"]);
    assert!(
        r.out.contains("median 1m00s") && r.out.contains("about a minute"),
        "{}",
        r.out
    );
    assert_eq!(
        c.requests(),
        [Request::Estimate {
            argv: argv("cargo check"),
            kind: None
        }]
    );
}

// R-573
#[test]
fn r573_metrics_prints_cores_memory_disk_and_the_top_five_groups() {
    use tasks_core::metrics::{JobMetricsView, MetricSample, ProcGroup};
    let sample = |t_ms, cpu, rd| MetricSample {
        t_ms,
        cpu_usec: Some(cpu),
        mem_bytes: Some(3 << 30),
        io_read_bytes: Some(rd),
        io_write_bytes: Some(0),
        io_some_us: Some(0),
    };
    let procs = (0..7)
        .map(|i| ProcGroup {
            name: format!("prog{i}"),
            count: 2,
            cpu_cores: Some(1.0),
            rss_bytes: 1 << 30,
            others: false,
        })
        .collect();
    let view = JobMetricsView {
        quality: Some(tasks_core::Attribution::Cgroup),
        samples: vec![sample(0, 0, 0), sample(2_000, 4_000_000, 20 << 20)],
        procs,
        usual_peak_mem_bytes: Some(4 << 30),
        note: None,
        source: None,
    };
    let c = Arc::new(FakeClient::from_fn(move |_| {
        Ok(Response::JobMetrics(view.clone()))
    }));
    let r = Harness::default().run(&c, &["metrics", "7"]);
    assert!(r.out.contains("cpu 2.0 cores"), "{}", r.out);
    assert!(r.out.contains("usual peak"), "{}", r.out);
    assert!(r.out.contains("disk read 10"), "{}", r.out);
    assert_eq!(r.out.matches("prog").count(), 5, "{}", r.out);
    assert_eq!(
        c.requests(),
        [Request::JobMetrics {
            id: tasks_core::JobId(7)
        }]
    );
    let j = Harness::default().run(&c, &["metrics", "7", "--json"]);
    assert!(j.out.contains("\"samples\""), "{}", j.out);
}

// R-628
#[test]
fn r628_metrics_of_a_forwarded_job_says_where_the_numbers_come_from() {
    use tasks_core::metrics::JobMetricsView;
    let view = JobMetricsView {
        quality: None,
        samples: Vec::new(),
        procs: Vec::new(),
        usual_peak_mem_bytes: None,
        note: Some("mac unreachable".into()),
        source: Some("on mac, remote job #9".into()),
    };
    let c = Arc::new(FakeClient::from_fn(move |_| {
        Ok(Response::JobMetrics(view.clone()))
    }));
    let r = Harness::default().run(&c, &["metrics", "7"]);
    assert!(
        r.out.contains("measured on mac, remote job #9"),
        "{}",
        r.out
    );
    assert!(r.out.contains("note: mac unreachable"), "{}", r.out);
}

#[test]
fn daemon_refusals_map_to_exit_codes() {
    for (code, want) in [
        (ErrorCode::Refused, 2),
        (ErrorCode::BadRequest, 2),
        (ErrorCode::NotFound, 4),
        (ErrorCode::LedgerUnavailable, 1),
        (ErrorCode::Internal, 1),
        (ErrorCode::UnsupportedVersion, 3),
    ] {
        let c = Arc::new(FakeClient::from_fn(move |_| Err(api_err(code, "no"))));
        assert_eq!(
            Harness::default().run(&c, &["status"]).code,
            want,
            "{code:?}"
        );
    }
    let c = Arc::new(FakeClient::from_fn(|_| {
        Err(ClientError::FrameTooLarge { cap: 10 })
    }));
    let r = Harness::default().run(&c, &["list"]);
    assert_eq!(r.code, 3);
    assert!(
        r.err
            .contains("larger than this client's frame cap of 10 bytes")
    );
}

// plans, suites, policy
#[test]
fn steps_submits_a_plan_and_lists_the_jobs() {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::SubmitPlan { plan, .. } => Ok(Response::PlanSubmitted {
            plan_id: 4,
            jobs: plan
                .steps
                .iter()
                .enumerate()
                .map(|(i, s)| (s.name.clone(), JobId(10 + i as i64)))
                .collect(),
        }),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let mut h = Harness::default();
    h.files.insert(PathBuf::from("plan.json"), r#"{"steps":[{"name":"build","argv":["cargo","check"]},{"name":"test","argv":["cargo","test","--lib"],"after":["build"]}]}"#.into());
    let r = h.run(&c, &["steps", "plan.json", "--pin"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(
        r.out,
        "loadtable: submitted plan 4 with 2 step(s)\n  build -> job 10\n  test -> job 11\n"
    );
    let Some(Request::SubmitPlan {
        owner,
        cwd,
        placement,
        ..
    }) = c.requests().into_iter().next()
    else {
        panic!()
    };
    assert_eq!(
        (owner.as_str(), cwd, placement),
        ("me", PathBuf::from("/w/repo"), Placement::Pin)
    );
}

#[test]
fn steps_refuses_a_bad_file_and_a_missing_file_and_shows_the_daemons_refusal_of_a_step() {
    let mut h = Harness::default();
    h.files.insert(
        PathBuf::from("bad.json"),
        r#"{"steps":[{"name":"a","argv":["x"],"after":["a"]}]}"#.into(),
    );
    h.files
        .insert(PathBuf::from("junk.json"), "not json".into());
    h.files.insert(
        PathBuf::from("broad.json"),
        r#"{"steps":[{"name":"t","argv":["cargo","test","foo"]}]}"#.into(),
    );
    let c = never_called();
    let r = h.run(&c, &["steps", "bad.json"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("depends on itself"), "{}", r.err);
    assert_eq!(h.run(&c, &["steps", "junk.json"]).code, 2);
    assert_eq!(h.run(&c, &["steps", "missing.json"]).code, 4);
    assert!(c.requests().is_empty(), "nothing is sent for a bad file");
    // the step rules are the daemon's: its refusal comes back naming the step
    let d = Arc::new(FakeClient::from_fn(|req| match req {
        Request::SubmitPlan {
            broad_test: false, ..
        } => Err(api_err(
            ErrorCode::Refused,
            "step \"t\": refused: this builds every test target",
        )),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let r = h.run(&d, &["steps", "broad.json"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("step \"t\": refused"), "{}", r.err);
}

#[test]
fn suite_run_sends_the_suite_and_suite_list_reads_the_policy() {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::SubmitSuite { name, .. } => Ok(Response::PlanSubmitted {
            plan_id: 2,
            jobs: vec![(format!("{name}-step"), JobId(5))],
        }),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let r = Harness::default().run(&c, &["suite", "run", "core", "--quick", "--peer", "nixos"]);
    assert!(
        r.out
            .contains("submitted plan 2 (suite core) with 1 step(s)"),
        "{}",
        r.out
    );
    let Some(Request::SubmitSuite {
        quick,
        placement,
        name,
        ..
    }) = c.requests().into_iter().next()
    else {
        panic!()
    };
    assert!(quick);
    assert_eq!(
        (name.as_str(), placement),
        (
            "core",
            Placement::Peer(tasks_core::MachineName("nixos".into()))
        )
    );

    let mut h = Harness::default();
    h.fs.files.insert(
        PathBuf::from("/w/repo/.loadtable/run-policy.toml"),
        POLICY.into(),
    );
    h.fs.files
        .insert(PathBuf::from("/w/repo/.git"), String::new());
    let r = h.run(&c, &["suite", "list"]);
    assert_eq!(r.out, "core (1 step(s)): Run before pushing.\n");
    let r = h.run(&c, &["policy", "show"]);
    assert!(
        r.out.contains("suite core") && r.out.contains("step tests: cargo test -p p2pcore"),
        "{}",
        r.out
    );
    let r = Harness::default().run(&c, &["policy", "show"]);
    assert_eq!(r.code, 4);
    assert!(
        r.err.contains("no .loadtable/run-policy.toml found"),
        "{}",
        r.err
    );
}

const POLICY: &str = r#"version = 1
[suites.core]
notes = "Run before pushing."
[[suites.core.steps]]
name = "tests"
command = ["cargo", "test", "-p", "p2pcore", "--lib"]
"#;

#[test]
fn r114_policy_suggest_asks_the_daemon_for_the_policys_recipes_on_this_machine() {
    let mut h = Harness::default();
    h.fs.files.insert(
        PathBuf::from("/w/repo/.loadtable/run-policy.toml"),
        POLICY.into(),
    );
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::Hello => Ok(Response::Hello(HelloInfo {
            protocol: 1,
            version: String::new(),
            pid: 1,
            state_dir: PathBuf::new(),
            lock_dir: PathBuf::new(),
            machine: "laptop".into(),
        })),
        Request::History { .. } => Ok(Response::History(HistoryView {
            recipes: vec![],
            truncated: false,
        })),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let r = h.run(&c, &["policy", "suggest"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let Some(Request::History {
        recipes,
        machines,
        max_samples,
    }) = c.requests().into_iter().nth(1)
    else {
        panic!("a history request expected");
    };
    assert_eq!(
        recipes,
        ["cargo test pkg=p2pcore profile=dev sel=lib"]
            .map(String::from)
            .to_vec()
    );
    assert_eq!(machines, ["laptop"]);
    assert_eq!(max_samples, Some(tasks_daemon::api::MAX_HISTORY_SAMPLES));
    // a daemon without the answer is an error, not an empty proposal
    let r = h.run(&never_called(), &["policy", "suggest"]);
    assert_ne!(r.code, 0);
}

#[test]
fn suggestions_render_from_a_history() {
    let loaded = tasks_policy::Loaded {
        file: PathBuf::from("/w/repo/.loadtable/run-policy.toml"),
        text: POLICY.into(),
        policy: tasks_policy::parse(POLICY, Path::new("p.toml")).unwrap(),
    };
    let h = tasks_timing::MemHistory::new();
    let text = suggest_report(&loaded, &h, &["laptop".to_string()]).unwrap();
    assert!(!text.is_empty());
}

const CARGO_JSON: &str = r#"{"reason":"compiler-artifact","package_id":"path+file:///w/crates/core#p2pcore@0.1.0","target":{"kind":["lib"],"name":"p2pcore"},"profile":{"test":true},"executable":"/t/deps/p2pcore-aaa"}
{"reason":"compiler-artifact","package_id":"path+file:///w/crates/core#p2pcore@0.1.0","target":{"kind":["test"],"name":"lifecycle"},"profile":{"test":true},"executable":"/t/deps/lifecycle-bbb"}
"#;

#[test]
fn split_turns_a_cargo_test_into_a_build_step_and_one_step_per_executable() {
    let mut h = Harness::default();
    h.files.insert(PathBuf::from("exe.json"), CARGO_JSON.into());
    let r = h.run(
        &never_called(),
        &[
            "split", "--exe", "exe.json", "--", "cargo", "test", "-p", "p2pcore",
        ],
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let plan = Plan::parse(&r.out).unwrap();
    plan.validate().unwrap();
    let names: Vec<&str> = plan.steps.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["build", "lib-p2pcore", "test-lifecycle"]);
    assert!(plan.steps[0].argv.contains(&"--no-run".to_string()));
    let r = h.run(
        &never_called(),
        &["split", "--exe", "exe.json", "--", "cargo", "build"],
    );
    assert_eq!(r.code, 2);
    let r = h.run(
        &never_called(),
        &["split", "--exe", "nope.json", "--", "cargo", "test"],
    );
    assert_eq!(r.code, 4);
}

#[test]
fn stats_groups_finished_jobs_by_recipe_and_leaves_out_cancelled_ones() {
    let mut a = job(1, "a", "/w", Phase::Passed, "cargo test -p x");
    a.started_at_ms = Some(0);
    a.finished_at_ms = Some(10_000);
    let mut b = job(
        2,
        "a",
        "/w",
        Phase::Failed { exit: Some(1) },
        "cargo test -j2 -p x",
    );
    b.started_at_ms = Some(0);
    b.finished_at_ms = Some(30_000);
    let c = job(3, "a", "/w", Phase::Cancelled, "cargo test -p x");
    let d = job(4, "a", "/w", Phase::Running, "cargo test -p x");
    let rows = stat_rows(&[a, b, c, d]);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].runs, rows[0].passed), (2, 1));
    assert_eq!((rows[0].median_secs, rows[0].p90_secs), (10.0, 30.0));
    // with --who the finished jobs are read: the history has no owners
    let reply = vec![job(1, "a", "/w", Phase::Passed, "echo hi")];
    let c = Arc::new(FakeClient::from_fn(move |_| {
        Ok(Response::Jobs {
            jobs: reply.clone(),
            truncated: false,
        })
    }));
    assert!(
        Harness::default()
            .run(&c, &["stats", "--who", "a"])
            .out
            .contains("RECIPE")
    );
}

fn series(target: Option<&str>, secs: &[(f64, &str)]) -> SeriesView {
    SeriesView {
        target: target.map(str::to_string),
        samples: secs
            .iter()
            .enumerate()
            .map(|(i, (s, o))| SampleView {
                secs: *s,
                cache: "warm".into(),
                outcome: (*o).into(),
                at_ms: i as u64,
            })
            .collect(),
    }
}

fn sample_history() -> HistoryView {
    HistoryView {
        recipes: vec![RecipeHistory {
            recipe: "cargo test pkg=x".into(),
            machines: vec![
                MachineHistory {
                    machine: "laptop".into(),
                    targets: vec!["test:a".into()],
                    series: vec![
                        series(
                            None,
                            &[(10.0, "passed"), (30.0, "failed"), (99.0, "cancelled")],
                        ),
                        series(Some("test:a"), &[(4.0, "passed")]),
                    ],
                },
                MachineHistory {
                    machine: "nixos".into(),
                    targets: vec![],
                    series: vec![series(None, &[(5.0, "passed")])],
                },
            ],
        }],
        truncated: false,
    }
}

#[test]
fn r114_stats_come_from_the_history_per_machine_and_leave_out_cancelled_samples() {
    let rows = history_rows(&sample_history(), None, false);
    assert_eq!(rows.len(), 2, "{rows:?}");
    let laptop = rows
        .iter()
        .find(|r| r.machine.as_deref() == Some("laptop"))
        .unwrap();
    assert_eq!((laptop.runs, laptop.passed), (2, 1));
    assert_eq!((laptop.median_secs, laptop.p90_secs), (10.0, 30.0));
    // targets only on request, and the recipe filter narrows
    let with = history_rows(&sample_history(), None, true);
    assert_eq!(with.len(), 3);
    assert!(with.iter().any(|r| r.target.as_deref() == Some("test:a")));
    assert!(history_rows(&sample_history(), Some("gradle"), true).is_empty());
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::History { .. } => Ok(Response::History(sample_history())),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let r = Harness::default().run(&c, &["stats", "--targets", "--machine", "laptop"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("@laptop  > test:a"), "{}", r.out);
    assert_eq!(
        c.requests(),
        [Request::History {
            recipes: vec![],
            machines: vec!["laptop".into()],
            max_samples: Some(200)
        }]
    );
}

// R-199
#[test]
fn r199_ui_and_mcp_fail_with_built_without_and_exit_2() {
    let c = never_called();
    for name in ["ui", "mcp"] {
        if (name == "mcp" && crate::MCP_BUILT) || (name == "ui" && crate::UI_BUILT) {
            // with the feature the command serves stdio or opens a window; the front ends
            // have their own tests
            continue;
        }
        let r = Harness::default().run(&c, &[name]);
        assert_eq!(r.code, 2);
        if cfg!(not(any(feature = "ui", feature = "mcp"))) {
            assert!(
                r.err.contains(&format!("built without {name}")),
                "{}",
                r.err
            );
        }
    }
    assert!(
        c.requests().is_empty(),
        "no daemon request, no daemon start"
    );
    assert!(crate::not_built("ui").contains("built without ui"));
}

// R-352
#[test]
fn r352_ui_and_mcp_help_tell_the_truth_for_this_build() {
    use clap::CommandFactory;
    let cmd = crate::args::Cli::command();
    for (name, built) in [("ui", crate::UI_BUILT), ("mcp", crate::MCP_BUILT)] {
        let about = cmd
            .find_subcommand(name)
            .and_then(|c| c.get_about())
            .map(|a| a.to_string())
            .unwrap_or_default();
        assert_eq!(about.contains("not built"), !built, "{name}: {about}");
        assert_eq!(about.contains("(built in)"), built, "{name}: {about}");
    }
}

#[test]
fn r199_the_headless_build_keeps_the_command_line() {
    assert_eq!(
        (crate::UI_BUILT, crate::MCP_BUILT),
        (cfg!(feature = "ui"), cfg!(feature = "mcp"))
    );
    let r = Harness::default().run(&never_called(), &["--version"]);
    assert_eq!(r.code, 0);
    assert!(r.out.starts_with("loadtable "));
    let r = Harness::default().run(&never_called(), &["bogus"]);
    assert_eq!(r.code, 2);
    assert!(
        HelloInfo {
            protocol: 1,
            version: String::new(),
            pid: 1,
            state_dir: PathBuf::new(),
            lock_dir: PathBuf::new(),
            machine: String::new()
        }
        .pid == 1
    );
}

// ---- priority ---------------------------------------------------------------------------

// R-339
#[test]
fn r339_blocking_needs_a_reason_and_nothing_is_sent_without_one() {
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let h = Harness::default();
    let r = h.run(
        &c,
        &["enqueue", "--priority", "blocking", "--", "cargo", "check"],
    );
    assert_eq!(r.code, 2, "{}", r.err);
    assert!(
        r.err.contains("needs a reason") && r.err.contains("--reason"),
        "{}",
        r.err
    );
    assert_eq!(submits(&c), 0);
    let r = h.run(&c, &["enqueue", "--reason", "why", "--", "cargo", "check"]);
    assert_eq!(r.code, 2, "{}", r.err);
    let r = h.run(
        &c,
        &["enqueue", "--priority", "urgent", "--", "cargo", "check"],
    );
    assert_eq!(r.code, 2, "{}", r.err);
    assert_eq!(submits(&c), 0);
    let r = h.run(
        &c,
        &[
            "enqueue",
            "--priority",
            "blocking",
            "--reason",
            "release waits",
            "--",
            "cargo",
            "check",
        ],
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let r = h.run(
        &c,
        &[
            "enqueue",
            "--priority",
            "background",
            "--",
            "cargo",
            "check",
        ],
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let specs: Vec<_> = c
        .requests()
        .into_iter()
        .filter_map(|r| match r {
            Request::Submit { spec } => Some(spec),
            _ => None,
        })
        .collect();
    assert_eq!(
        (specs[0].priority, specs[0].priority_reason.as_deref()),
        (tasks_core::Priority::Blocking, Some("release waits"))
    );
    assert_eq!(specs[1].priority, tasks_core::Priority::Background);
}

// R-350
#[test]
fn r350_prioritize_sends_the_class_and_reason_and_prints_the_change() {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::Prioritize { id, priority, .. } => Ok(Response::Prioritized {
            id: *id,
            from: tasks_core::Priority::Normal,
            to: *priority,
        }),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    let h = Harness::default();
    let r = h.run(
        &c,
        &[
            "prioritize",
            "9",
            "--priority",
            "blocking",
            "--reason",
            "user waits",
        ],
    );
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(
        r.out.contains("job 9: normal -> blocking") && r.out.contains("(reason: user waits)"),
        "{}",
        r.out
    );
    assert_eq!(
        c.requests(),
        [Request::Prioritize {
            id: JobId(9),
            who: "me".into(),
            priority: tasks_core::Priority::Blocking,
            reason: Some("user waits".into())
        }]
    );
    // blocking without a reason is refused before the daemon is asked
    let before = c.requests().len();
    let r = h.run(&c, &["prioritize", "9", "--priority", "blocking"]);
    assert_eq!(r.code, 2, "{}", r.err);
    assert_eq!(c.requests().len(), before);
}

#[test]
fn r350_a_refusal_of_prioritize_reaches_the_user() {
    let c = Arc::new(FakeClient::from_fn(|_| {
        Err(api_err(
            ErrorCode::Refused,
            "job 9 is Running; only a queued or waiting job can change priority",
        ))
    }));
    let r = Harness::default().run(&c, &["prioritize", "9", "--priority", "background"]);
    assert_ne!(r.code, 0);
    assert!(r.err.contains("only a queued or waiting job"), "{}", r.err);
}

// R-343: there is no flag that claims the short lane
#[test]
fn r343_no_command_has_a_flag_that_claims_the_short_lane() {
    use clap::CommandFactory;
    fn walk(cmd: &clap::Command, path: &str, bad: &mut Vec<String>) {
        for a in cmd.get_arguments() {
            let names: Vec<String> = std::iter::once(a.get_id().to_string())
                .chain(a.get_long().map(str::to_string))
                .chain(
                    a.get_all_aliases()
                        .into_iter()
                        .flatten()
                        .map(str::to_string),
                )
                .collect();
            if names.iter().any(|n| {
                let n = n.to_lowercase();
                n.contains("short") || n.contains("lane") || n.contains("quick-start")
            }) {
                bad.push(format!("{path}: {names:?}"));
            }
        }
        for sub in cmd.get_subcommands() {
            walk(sub, &format!("{path} {}", sub.get_name()), bad);
        }
    }
    let mut bad = Vec::new();
    walk(&crate::args::Cli::command(), "loadtable", &mut bad);
    assert!(bad.is_empty(), "{bad:?}");
    // and clap refuses one an agent invents
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let r = Harness::default().run(&c, &["enqueue", "--short", "--", "cargo", "check"]);
    assert_ne!(r.code, 0);
    assert_eq!(submits(&c), 0);
}

// R-349
#[test]
fn r349_the_status_and_the_table_show_a_job_held_by_a_peer_as_waiting() {
    let mut j = job(5, "me", "/w/repo", Phase::Running, "cargo test");
    j.forwarded_to = Some(("nixos".into(), Some(7)));
    j.remote_phase = Some(Phase::Waiting(tasks_core::WaitReason::Pressure(
        "CPU contended".into(),
    )));
    j.remote_wait = Some("waiting on nixos: CPU contended".into());
    let t = crate::render::job_table(
        &[j.clone()],
        crate::testutil::NOW,
        None,
        &[],
        Default::default(),
    );
    assert!(t.contains("waiting → nixos #7"), "{t}");
    assert!(!t.contains("running →"), "{t}");
    assert!(
        t.contains("waits for: waiting on nixos: CPU contended"),
        "{t}"
    );
    let d = crate::render::job_detail(&j, crate::testutil::NOW, None);
    assert!(d.starts_with("job 5: waiting"), "{d}");
}

// R-368
#[test]
fn r368_submit_warnings_from_the_daemon_are_printed_by_the_cli() {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::Submit { .. } => Ok(Response::Submitted {
            id: JobId(6),
            warnings: vec!["this builds every test target (policy: warn)".into()],
        }),
        Request::List { .. } => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
        Request::Logs { .. } => Ok(chunk("ok\n", 3, true)),
        Request::Get { .. } => Ok(Response::Job {
            job: Box::new(finished(6, Phase::Passed)),
        }),
        other => Err(ClientError::BadReply(format!("unexpected {other:?}"))),
    }));
    let r = Harness::default().run(&c, &["run", "--", "cargo", "test"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(
        r.err
            .contains("loadtable: warning: this builds every test target (policy: warn)"),
        "{}",
        r.err
    );
    // no warning, no line
    let quiet = Harness::default().run(
        &daemon(7, vec![], finished(7, Phase::Passed)),
        &["run", "--", "x"],
    );
    assert!(!quiet.err.contains("warning"), "{}", quiet.err);
}

// R-504, R-517
#[test]
fn r504_fetch_is_carried_on_the_spec_and_a_failed_fetch_of_a_passed_job_exits_one() {
    use tasks_core::{FetchResult, FetchStatus};
    let mut j = finished(7, Phase::Passed);
    j.fetch = vec![FetchResult {
        path: "out/app.apk".into(),
        status: FetchStatus::Missing,
    }];
    let c = daemon(7, vec![chunk("built\n", 6, true)], j);
    let r = Harness::default().run(
        &c,
        &[
            "run",
            "--fetch",
            "out/app.apk",
            "--fetch",
            "out/b.txt",
            "--",
            "make",
        ],
    );
    assert_eq!(
        r.code, 1,
        "the command passed, the fetch did not: {}",
        r.err
    );
    assert!(
        r.err.contains("1 requested file(s) could not be fetched"),
        "{}",
        r.err
    );
    let Some(Request::Submit { spec }) = c
        .requests()
        .into_iter()
        .find(|r| matches!(r, Request::Submit { .. }))
    else {
        panic!("no submit")
    };
    assert_eq!(spec.fetch, ["out/app.apk", "out/b.txt"]);
    // a fetch that worked leaves the exit code alone
    let mut ok = finished(8, Phase::Passed);
    ok.fetch = vec![FetchResult {
        path: "out/app.apk".into(),
        status: FetchStatus::Fetched {
            bytes: 1,
            sha256: "ab".into(),
            unchanged: false,
        },
    }];
    let c = daemon(8, vec![chunk("", 0, true)], ok);
    assert_eq!(
        Harness::default()
            .run(&c, &["run", "--fetch", "a", "--", "make"])
            .code,
        0
    );
    // a failed job keeps its own code whatever the fetch says
    let mut bad = finished(9, Phase::Failed { exit: Some(101) });
    bad.fetch = vec![FetchResult {
        path: "a".into(),
        status: FetchStatus::Failed {
            reason: "not fetched: the job did not pass (failed)".into(),
        },
    }];
    let c = daemon(9, vec![chunk("", 0, true)], bad);
    assert_eq!(
        Harness::default()
            .run(&c, &["run", "--fetch", "a", "--", "make"])
            .code,
        101
    );
}

// R-517
#[test]
fn r517_a_fetch_path_outside_the_checkout_is_refused_before_anything_is_sent() {
    for bad in ["../x", "/etc/passwd", "a/../b"] {
        let c = daemon(1, vec![], finished(1, Phase::Passed));
        let r = Harness::default().run(&c, &["run", "--fetch", bad, "--", "make"]);
        assert_ne!(r.code, 0, "{bad}");
        assert!(r.err.contains("cannot fetch"), "{}", r.err);
        assert_eq!(submits(&c), 0);
    }
}

const SPLIT_POLICY: &str = r#"version = 1
[suites.core]
[[suites.core.steps]]
name = "tests"
command = ["cargo", "test", "-p", "p2pcore", "--test", "calls", "--test", "vault"]
split = "cargo-test"
"#;

const SPLIT_METADATA: &str = r#"{"workspace_members":["p2pcore 0.1.0"],"packages":[
  {"name":"p2pcore","id":"p2pcore 0.1.0","features":{},"targets":[
    {"name":"p2pcore","kind":["lib"],"test":true},
    {"name":"calls","kind":["test"],"test":true},
    {"name":"vault","kind":["test"],"test":true},
    {"name":"link","kind":["test"],"test":true}]}]}"#;

fn suite_request(h: &mut Harness) -> (Vec<Request>, String) {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::SubmitSuite { .. } => Ok(Response::PlanSubmitted {
            plan_id: 1,
            jobs: vec![("tests".into(), JobId(1))],
        }),
        other => Err(ClientError::BadReply(format!("{other:?}"))),
    }));
    h.fs.files.insert(
        PathBuf::from("/w/repo/.loadtable/run-policy.toml"),
        SPLIT_POLICY.into(),
    );
    h.fs.files
        .insert(PathBuf::from("/w/repo/.git"), String::new());
    let r = h.run(&c, &["suite", "run", "core"]);
    assert_eq!(r.code, 0, "{}", r.err);
    (c.requests(), r.err)
}

// R-706
#[test]
fn r706_suite_run_sends_the_test_targets_read_from_cargo_metadata() {
    let mut h = Harness::default();
    h.fs.metadata = Some(SPLIT_METADATA.into());
    let (reqs, err) = suite_request(&mut h);
    let Some(Request::SubmitSuite { executables, .. }) = reqs.into_iter().next() else {
        panic!()
    };
    let names: Vec<&str> = executables["tests"]
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(names, ["calls", "vault"], "only the tests the step names");
    assert!(err.is_empty(), "{err}");
}

// R-706
#[test]
fn r706_when_cargo_metadata_fails_the_step_is_sent_whole_with_a_note() {
    let mut h = Harness::default();
    let (reqs, err) = suite_request(&mut h);
    let Some(Request::SubmitSuite { executables, .. }) = reqs.into_iter().next() else {
        panic!()
    };
    assert!(executables.is_empty());
    assert!(
        err.contains("step \"tests\" stays whole") && err.contains("cargo metadata failed"),
        "{err}"
    );
}

// R-714, R-716
#[test]
fn r714_light_and_retry_alone_travel_on_the_spec_and_default_to_off() {
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let r = Harness::default().run(
        &c,
        &[
            "enqueue",
            "--light",
            "--retry-alone",
            "--",
            "cargo",
            "test",
            "-p",
            "x",
            "--test",
            "calls",
        ],
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let s = submitted_spec(&c);
    assert!(s.light && s.retry_alone && !s.isolate);
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let r = Harness::default().run(&c, &["enqueue", "--", "true"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let s = submitted_spec(&c);
    assert!(!s.light && !s.retry_alone);
}

#[test]
fn r799_interactive_travels_on_the_spec_and_defaults_to_off() {
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let r = Harness::default().run(&c, &["enqueue", "--interactive", "--", "./app"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(submitted_spec(&c).interactive);
    let c = daemon(1, vec![], finished(1, Phase::Passed));
    let r = Harness::default().run(&c, &["enqueue", "--", "./app"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!submitted_spec(&c).interactive);
}

// ---------------------------------------------------------------- wait

/// A daemon whose job `id` answers `get` with each phase of `phases` in turn (the last one
/// repeats) and serves `tail` as its log tail.
fn waited(id: i64, phases: Vec<Phase>, tail: &[&str]) -> Arc<FakeClient> {
    let phases = Mutex::new(
        phases
            .into_iter()
            .collect::<std::collections::VecDeque<_>>(),
    );
    let tail: Vec<String> = tail.iter().map(|l| l.to_string()).collect();
    Arc::new(FakeClient::from_fn(move |req| match req {
        Request::Get { id: JobId(asked) } if *asked == id => {
            let mut q = phases
                .lock()
                .map_err(|_| ClientError::BadReply("poisoned".into()))?;
            let phase = if q.len() > 1 {
                q.pop_front()
            } else {
                q.front().cloned()
            };
            Ok(Response::Job {
                job: Box::new(finished(id, phase.unwrap_or(Phase::Running))),
            })
        }
        Request::Logs { .. } => Ok(Response::LogTail {
            lines: tail.clone(),
            running: false,
            end_offset: None,
        }),
        other => Err(ClientError::BadReply(format!("unexpected {other:?}"))),
    }))
}

// R-720
#[test]
fn r720_wait_blocks_until_the_job_ends_then_prints_its_tail_and_result_read_only() {
    let c = waited(
        7,
        vec![Phase::Queued, Phase::Running, Phase::Running, Phase::Passed],
        &["test a ... ok", "test result: ok. 1 passed"],
    );
    let r = Harness::default().run(&c, &["wait", "7"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("test result: ok. 1 passed"), "{}", r.out);
    let last = r.out.lines().last().unwrap_or_default();
    assert!(
        last.contains("job 7") && last.contains("passed"),
        "{}",
        r.out
    );
    let gets = c
        .requests()
        .iter()
        .filter(|q| matches!(q, Request::Get { .. }))
        .count();
    assert_eq!(gets, 4, "one get per poll until the job ended");
    assert!(c.requests().iter().all(is_read_only));
}

// R-721
#[test]
fn r721_wait_exits_with_the_jobs_code_like_run_and_tail_0_prints_only_the_result() {
    let c = waited(8, vec![Phase::Failed { exit: Some(3) }], &["boom"]);
    let r = Harness::default().run(&c, &["wait", "--tail", "0", "8"]);
    let failed = finished(8, Phase::Failed { exit: Some(3) });
    assert_eq!(r.code, render::job_exit_code(&failed));
    assert!(!r.out.contains("boom"), "{}", r.out);
    assert_eq!(r.out.trim(), render::summary_line(&failed));
}

// R-721
#[test]
fn r721_waiting_on_several_jobs_reports_each_and_exits_with_the_first_failure() {
    let c = Arc::new(FakeClient::from_fn(|req| match req {
        Request::Get { id } => Ok(Response::Job {
            job: Box::new(finished(
                id.0,
                if id.0 == 2 {
                    Phase::Failed { exit: Some(101) }
                } else {
                    Phase::Passed
                },
            )),
        }),
        Request::Logs { .. } => Ok(Response::LogTail {
            lines: vec![],
            running: false,
            end_offset: None,
        }),
        other => Err(ClientError::BadReply(format!("unexpected {other:?}"))),
    }));
    let r = Harness::default().run(&c, &["wait", "1", "2", "3", "2"]);
    assert_eq!(r.code, 101);
    for id in [1, 2, 3] {
        assert_eq!(
            r.out
                .lines()
                .filter(|l| l.contains(&format!("job {id} ")))
                .count(),
            1,
            "{}",
            r.out
        );
    }
}

// R-722
#[test]
fn r722_wait_gives_up_after_its_timeout_with_124_and_leaves_the_job_alone() {
    let c = waited(9, vec![Phase::Running], &[]);
    let r = Harness::default().run(&c, &["wait", "--timeout", "3", "9"]);
    assert_eq!(r.code, crate::error::EXIT_TIMED_OUT);
    assert!(r.err.contains("job 9 is untouched"), "{}", r.err);
    assert!(r.err.contains("loadtable logs 9"), "{}", r.err);
    assert!(c.requests().iter().all(is_read_only));
    let gets = c
        .requests()
        .iter()
        .filter(|q| matches!(q, Request::Get { .. }))
        .count();
    assert_eq!(gets, 4, "polls at 0, 1, 2 and 3 s, then gives up");
}
