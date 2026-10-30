//! One group of tests per tool, against a scripted daemon.
#![allow(missing_docs)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::*;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tasks_core::JobId;
use tasks_core::{Kind, Phase, Placement};
use tasks_daemon::api::{ApiError, ErrorCode, GrepGroup, GrepLine, LogView, Request, Response};
use tasks_mcp::cancel::CancelToken;
use tasks_mcp::tools::Outcome;
use tasks_mcp::{DaemonError, FakeDaemon};

fn submitted(id: i64) -> Arc<FakeDaemon> {
    fake(move |r| match r {
        Request::Submit { .. } => Ok(Response::Submitted {
            id: JobId(id),
            warnings: vec![],
        }),
        Request::List { .. } => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
        _ => Err(DaemonError::Failed(format!("unexpected {r:?}"))),
    })
}

fn submits(d: &FakeDaemon) -> Vec<tasks_core::JobSpec> {
    d.requests()
        .into_iter()
        .filter_map(|r| match r {
            Request::Submit { spec } => Some(spec),
            _ => None,
        })
        .collect()
}

// ---- submit ------------------------------------------------------------------------------

// R-193
#[test]
fn r193_submit_routes_through_the_submit_request_with_the_tasks_run_spec() {
    let d = submitted(7);
    let r = call(
        &tools_with(&d),
        "submit",
        json!({"argv": ["cargo", "test", "-p", "x", "--lib"], "who": "agent-1", "placement": "pin"}),
    );
    assert!(!r.is_error, "{}", r.text);
    assert_eq!(r.structured["id"], 7);
    assert!(r.text.contains("submitted job 7"));
    let specs = submits(&d);
    assert_eq!(specs.len(), 1);
    let s = &specs[0];
    assert_eq!(s.owner.as_str(), "agent-1");
    assert_eq!(s.cwd, std::path::PathBuf::from("/w"));
    assert_eq!(s.argv, argv("cargo test -p x --lib"));
    assert_eq!(s.placement, Placement::Pin);
    assert_eq!(s.kind, Kind::Test);
}

// R-193, R-191
#[test]
fn r193_the_daemons_refusal_of_a_broad_test_reaches_the_agent() {
    let d = fake(|r| match r {
        Request::Submit { spec } if !spec.broad_test_escape => {
            Err(DaemonError::Refused(ApiError::new(
                ErrorCode::Refused,
                "refused: this cargo test builds every target; use --lib or --test NAME",
            )))
        }
        Request::Submit { .. } => Ok(Response::Submitted {
            id: JobId(1),
            warnings: vec![],
        }),
        _ => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
    });
    let args = json!({"argv": ["cargo", "test", "-p", "x", "some_filter"]});
    let r = call(&tools_with(&d), "submit", args);
    assert_eq!(error_kind(&r), "refused");
    assert!(r.text.contains("--lib"), "{}", r.text);
    assert_eq!(
        submits(&d).len(),
        1,
        "the daemon decided, not the MCP server"
    );
    // R-191: the escape is carried on the job for the daemon to honour and record
    let args = json!({"argv": ["cargo", "test", "-p", "x", "some_filter"], "broad_test": true});
    let r = call(&tools_with(&d), "submit", args);
    assert!(!r.is_error, "{}", r.text);
    assert!(submits(&d)[1].broad_test_escape);
}

// R-193
#[test]
fn r193_arguments_are_checked_before_any_daemon_request() {
    let bad = [
        json!({"argv": []}),
        json!({"argv": ["echo"], "who": "bad name"}),
        json!({"argv": ["echo"], "cwd": "relative/dir"}),
        json!({"argv": ["echo"], "placement": "pin", "peer": "nixos"}),
        json!({"argv": ["echo"], "placement": "elsewhere"}),
        json!({"argv": ["echo"], "device": "emu-1", "placement": "anywhere"}),
        json!({"argv": ["echo"], "device": "emu-1", "peer": "nixos"}),
        json!({"argv": ["echo"], "timeout_s": 0}),
        json!({"argv": ["echo"], "after": [0]}),
        json!({"argv": ["echo"], "surprise": 1}),
        json!({"cwd": "/w"}),
        json!({"argv": "echo hi"}),
    ];
    for args in bad {
        let d = never_called();
        let r = call(&tools_with(&d), "submit", args.clone());
        assert!(r.is_error, "{args} was accepted: {}", r.text);
        assert!(d.requests().is_empty(), "{args} reached the daemon");
    }
}

// R-210
#[test]
fn r210_a_denied_device_refusal_from_the_daemon_reaches_the_agent() {
    let d = fake(|r| match r {
        Request::Submit { .. } => Err(DaemonError::Refused(ApiError::new(
            ErrorCode::Refused,
            "refused: the command names the denied device",
        ))),
        _ => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
    });
    let r = call(
        &tools_with(&d),
        "submit",
        json!({"argv": ["adb", "-s", "phone01x", "shell"]}),
    );
    assert_eq!(error_kind(&r), "refused");
    assert!(r.text.contains("denied device"), "{}", r.text);
}

// R-102
#[test]
fn a_full_daemon_is_a_retryable_error() {
    let d = fake(|_| {
        Err(DaemonError::Refused(ApiError::new(
            ErrorCode::ConnectionLimit,
            "full",
        )))
    });
    let r = call(&tools_with(&d), "status", json!({}));
    assert_eq!(error_kind(&r), "connection_limit");
    assert!(
        r.text.contains("not processed") && r.text.contains("try again"),
        "{}",
        r.text
    );
}

#[test]
fn timeout_s_goes_to_the_job_spec() {
    let d = submitted(1);
    let r = call(
        &tools_with(&d),
        "submit",
        json!({"argv": ["true"], "timeout_s": 90}),
    );
    assert!(!r.is_error, "{}", r.text);
    assert_eq!(submits(&d)[0].timeout_secs, Some(90));
    assert!(!submits(&d)[0].broad_test_escape);
}

#[test]
fn a_device_job_holds_a_device_slot() {
    let d = submitted(3);
    let r = call(
        &tools_with(&d),
        "submit",
        json!({"argv": ["adb", "-s", "9BWAY1LSDC", "shell"], "device": "9BWAY1LSDC"}),
    );
    assert!(!r.is_error, "{}", r.text);
    let s = &submits(&d)[0];
    assert_eq!(s.kind, Kind::Device);
    assert_eq!(
        s.requirements.device.as_ref().map(|d| d.0.as_str()),
        Some("9BWAY1LSDC")
    );
}

#[test]
fn placement_peer_and_anywhere_reach_the_spec() {
    for (args, want) in [
        (json!({"argv": ["true"], "peer": "nixos"}), "peer nixos"),
        (
            json!({"argv": ["cargo", "check"], "placement": "anywhere"}),
            "anywhere",
        ),
        (json!({"argv": ["true"]}), "here"),
    ] {
        let d = submitted(1);
        let r = call(&tools_with(&d), "submit", args);
        assert_eq!(r.structured["placement"], want, "{}", r.text);
    }
}

#[test]
fn the_owner_defaults_to_the_server_environment() {
    let d = submitted(1);
    let r = call(&tools_with(&d), "submit", json!({"argv": ["true"]}));
    assert!(!r.is_error);
    assert_eq!(submits(&d)[0].owner.as_str(), "me");
    // and without any default the call says what to pass
    let d = never_called();
    let mut cfg = config();
    cfg.default_who = None;
    let t = tasks_mcp::Tools::new(d.clone() as Arc<dyn tasks_mcp::Daemon>, cfg);
    let r = call(&t, "submit", json!({"argv": ["true"]}));
    assert_eq!(error_kind(&r), "invalid_arguments");
    assert!(r.text.contains("who"));
}

// R-187
#[test]
fn r187_other_owners_in_the_same_cwd_are_a_warning_not_a_block() {
    let d = fake(|r| match r {
        Request::List { .. } => Ok(Response::Jobs {
            jobs: vec![job(4, "someone", Phase::Running, "cargo build")],
            truncated: false,
        }),
        Request::Submit { .. } => Ok(Response::Submitted {
            id: JobId(5),
            warnings: vec![],
        }),
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let r = call(&tools_with(&d), "submit", json!({"argv": ["true"]}));
    assert!(!r.is_error);
    assert!(r.text.contains("someone"), "{}", r.text);
}

// R-099, R-100
#[test]
fn r099_a_lost_submit_reply_is_reconciled_and_never_resent() {
    let d = fake(|r| match r {
        Request::Submit { .. } => Err(DaemonError::ReplyLost("closed".into())),
        Request::List { owner: Some(_), .. } => Ok(Response::Jobs {
            jobs: vec![job(9, "me", Phase::Queued, "true now")],
            truncated: false,
        }),
        Request::List { .. } => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    // submitted_at_ms of the fake job is NOW - 100s: too old, so no match yet
    let r = call(
        &tools_with(&d),
        "submit",
        json!({"argv": ["true", "now"], "cwd": "/w"}),
    );
    assert!(r.is_error);
    assert!(r.text.contains("Do NOT resubmit"), "{}", r.text);
    assert_eq!(submits(&d).len(), 1, "never resent");

    // a recent exact match is found
    let d = fake(|r| match r {
        Request::Submit { .. } => Err(DaemonError::ReplyLost("closed".into())),
        Request::List { owner: Some(_), .. } => {
            let mut j = job(9, "me", Phase::Queued, "true now");
            j.submitted_at_ms = NOW;
            Ok(Response::Jobs {
                jobs: vec![j],
                truncated: false,
            })
        }
        Request::List { .. } => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let r = call(&tools_with(&d), "submit", json!({"argv": ["true", "now"]}));
    assert!(!r.is_error, "{}", r.text);
    assert_eq!(r.structured["id"], 9);
    assert_eq!(submits(&d).len(), 1);

    // two identical jobs are ambiguous: say where to look
    let d = fake(|r| match r {
        Request::Submit { .. } => Err(DaemonError::ReplyLost("closed".into())),
        Request::List { owner: Some(_), .. } => {
            let mut a = job(9, "me", Phase::Queued, "true now");
            a.submitted_at_ms = NOW;
            let mut b = a.clone();
            b.id = JobId(10);
            Ok(Response::Jobs {
                jobs: vec![a, b],
                truncated: false,
            })
        }
        _ => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
    });
    let r = call(&tools_with(&d), "submit", json!({"argv": ["true", "now"]}));
    assert!(r.is_error);
    assert!(
        r.text.contains("9, 10") && r.text.contains("Do NOT resubmit"),
        "{}",
        r.text
    );
}

#[test]
fn a_busy_or_refusing_daemon_is_a_clean_tool_error() {
    let d = fake(|r| match r {
        Request::Submit { .. } => Err(DaemonError::Refused(ApiError::new(
            ErrorCode::Refused,
            "no such repo",
        ))),
        _ => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
    });
    let r = call(&tools_with(&d), "submit", json!({"argv": ["true"]}));
    assert_eq!(error_kind(&r), "refused");
    assert_eq!(r.text, "no such repo");
}

// R-194
#[test]
fn r194_daemon_down_is_a_clean_tool_error() {
    let d = fake(|_| {
        Err(DaemonError::Unreachable(
            "cannot start the daemon: no luck".into(),
        ))
    });
    for (name, args) in [
        ("status", json!({})),
        ("list", json!({})),
        ("get", json!({"id": 1})),
        ("submit", json!({"argv": ["true"]})),
    ] {
        let r = call(&tools_with(&d), name, args);
        assert_eq!(error_kind(&r), "daemon_unreachable", "{name}");
        assert!(r.text.contains("no luck"));
        assert!(r.to_json()["isError"] == true);
    }
}

// ---- submit_steps / run_suite -------------------------------------------------------------

fn plan_daemon() -> Arc<FakeDaemon> {
    fake(|r| match r {
        Request::SubmitPlan { plan, .. } => Ok(Response::PlanSubmitted {
            plan_id: 3,
            jobs: plan
                .steps
                .iter()
                .enumerate()
                .map(|(i, s)| (s.name.clone(), JobId(10 + i as i64)))
                .collect(),
        }),
        Request::SubmitSuite { .. } => Ok(Response::PlanSubmitted {
            plan_id: 4,
            jobs: vec![("build".into(), JobId(20)), ("test".into(), JobId(21))],
        }),
        _ => Err(DaemonError::Failed("unexpected".into())),
    })
}

#[test]
fn submit_steps_sends_one_plan_and_lists_the_job_ids() {
    let d = plan_daemon();
    let r = call(
        &tools_with(&d),
        "submit_steps",
        json!({"steps": [
            {"name": "build", "argv": ["cargo", "build"]},
            {"name": "test", "argv": ["cargo", "test", "-p", "x", "--lib"], "after": ["build"], "kind": "test"}
        ], "who": "me", "placement": "pin"}),
    );
    assert!(!r.is_error, "{}", r.text);
    assert_eq!(r.structured["plan_id"], 3);
    assert_eq!(r.structured["jobs"][1], json!({"step": "test", "id": 11}));
    assert!(r.text.contains("test -> job 11"));
    let reqs = d.requests();
    assert_eq!(reqs.len(), 1);
    let Request::SubmitPlan {
        plan,
        owner,
        placement,
        ..
    } = &reqs[0]
    else {
        panic!("{reqs:?}")
    };
    assert_eq!((owner.as_str(), placement), ("me", &Placement::Pin));
    assert_eq!(plan.steps[1].after, vec!["build".to_string()]);
    assert_eq!(plan.steps[1].kind, Kind::Test);
}

// R-731: only a placement the caller named stops a step's own hint from applying
#[test]
fn r731_run_suite_says_whether_the_caller_named_a_placement() {
    let given = |args: serde_json::Value| {
        let d = plan_daemon();
        let r = call(&tools_with(&d), "run_suite", args);
        assert!(!r.is_error, "{}", r.text);
        let reqs = d.requests();
        let Request::SubmitSuite {
            placement_given, ..
        } = &reqs[0]
        else {
            panic!("{reqs:?}")
        };
        *placement_given
    };
    assert!(!given(json!({"name": "core"})));
    assert!(given(json!({"name": "core", "placement": "here"})));
    assert!(given(json!({"name": "core", "placement": "anywhere"})));
    assert!(given(json!({"name": "core", "peer": "mac"})));
}

// R-193
#[test]
fn r193_plan_arguments_are_checked_before_any_daemon_request() {
    let d = never_called();
    for steps in [
        json!([]),
        json!([{"name": "a", "argv": ["true"], "after": ["a"]}]),
        json!([{"name": "a", "argv": ["true"], "after": ["zz"]}]),
        json!([{"name": "a", "argv": ["true"]}, {"name": "a", "argv": ["true"]}]),
        json!([{"name": "a", "argv": []}]),
        json!([{"name": "a", "argv": ["true"], "kind": "huge"}]),
    ] {
        let r = call(&tools_with(&d), "submit_steps", json!({"steps": steps}));
        assert!(r.is_error, "{steps} accepted");
    }
    assert!(d.requests().is_empty());
}

// R-193
#[test]
fn r193_a_daemon_refusal_of_a_step_reaches_the_agent_and_the_flag_is_sent() {
    let d = fake(|r| match r {
        Request::SubmitPlan {
            broad_test: false, ..
        } => Err(DaemonError::Refused(ApiError::new(
            ErrorCode::Refused,
            "step \"broad\": refused: builds every target",
        ))),
        Request::SubmitPlan { .. } => Ok(Response::PlanSubmitted {
            plan_id: 1,
            jobs: vec![("broad".into(), JobId(2))],
        }),
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let steps = json!([{"name": "broad", "argv": ["cargo", "test", "-p", "x", "filter"]}]);
    let r = call(&tools_with(&d), "submit_steps", json!({"steps": steps}));
    assert_eq!(error_kind(&r), "refused");
    assert!(r.text.contains("step \"broad\""), "{}", r.text);
    let r = call(
        &tools_with(&d),
        "submit_steps",
        json!({"steps": steps, "broad_test": true}),
    );
    assert!(!r.is_error, "{}", r.text);
}

#[test]
fn run_suite_sends_the_suite_request() {
    let d = plan_daemon();
    let r = call(
        &tools_with(&d),
        "run_suite",
        json!({"name": "ci", "quick": true, "peer": "nixos"}),
    );
    assert!(!r.is_error, "{}", r.text);
    assert!(r.text.contains("suite ci") && r.text.contains("build -> job 20"));
    let Request::SubmitSuite {
        name,
        quick,
        placement,
        owner,
        ..
    } = &d.requests()[0]
    else {
        panic!()
    };
    assert_eq!((name.as_str(), *quick, owner.as_str()), ("ci", true, "me"));
    assert!(matches!(placement, Placement::Peer(m) if m.0 == "nixos"));
    let d = never_called();
    assert!(call(&tools_with(&d), "run_suite", json!({})).is_error);
}

// ---- list / get ---------------------------------------------------------------------------

#[test]
fn list_asks_for_active_plus_recent_and_shows_one_line_per_job() {
    let d = fake(|r| match r {
        Request::List {
            active_only,
            recent_finished,
            owner,
            ..
        } => {
            assert_eq!(
                (*active_only, *recent_finished, owner.as_deref()),
                (false, Some(5), Some("me"))
            );
            Ok(Response::Jobs {
                jobs: vec![
                    job(1, "me", Phase::Passed, "cargo test"),
                    job(2, "other", Phase::Running, "cargo build"),
                    job(3, "me", Phase::Failed { exit: Some(101) }, "cargo clippy"),
                ],
                truncated: false,
            })
        }
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let r = call(&tools_with(&d), "list", json!({"who": "me", "limit": 5}));
    assert!(!r.is_error, "{}", r.text);
    // R-185: the owner filter holds even if the daemon answers more
    assert_eq!(r.structured["jobs"].as_array().map(Vec::len), Some(2));
    assert!(r.text.contains("#3 failed (exit 101) me"), "{}", r.text);
    assert_eq!(r.text.lines().count(), 3);
}

#[test]
fn list_active_drops_finished_jobs_and_says_so_when_empty() {
    let d = fake(|r| match r {
        Request::List {
            active_only: true,
            recent_finished: None,
            ..
        } => Ok(Response::Jobs {
            jobs: vec![job(1, "me", Phase::Passed, "x")],
            truncated: false,
        }),
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let r = call(&tools_with(&d), "list", json!({"active": true}));
    assert_eq!(r.text, "no jobs");
}

#[test]
fn a_cut_list_says_how_to_narrow_it() {
    let d = fake(|_| {
        Ok(Response::Jobs {
            jobs: vec![job(1, "me", Phase::Running, "x")],
            truncated: true,
        })
    });
    let r = call(&tools_with(&d), "list", json!({}));
    assert!(r.text.contains("truncated"), "{}", r.text);
    assert_eq!(r.structured["truncated"], true);
}

#[test]
fn get_describes_one_job_and_a_missing_job_is_not_found() {
    let d = fake(|r| match r {
        Request::Get { id } if id.0 == 5 => {
            let mut j = job(5, "me", Phase::Running, "cargo test -p x");
            j.wait_text = Some("waiting for a compile slot".into());
            j.forwarded_to = Some(("nixos".into(), Some(77)));
            Ok(Response::Job { job: Box::new(j) })
        }
        _ => Err(DaemonError::Refused(ApiError::new(
            ErrorCode::NotFound,
            "no job 6",
        ))),
    });
    let t = tools_with(&d);
    let r = call(&t, "get", json!({"id": 5}));
    assert!(
        r.text.contains("running on nixos as #77") && r.text.contains("compile slot"),
        "{}",
        r.text
    );
    assert_eq!(r.structured["phase"], "running");
    assert_eq!(error_kind(&call(&t, "get", json!({"id": 6}))), "not_found");
    assert_eq!(
        error_kind(&call(&t, "get", json!({"id": 0}))),
        "invalid_arguments"
    );
}

// ---- logs ---------------------------------------------------------------------------------

#[test]
fn logs_default_to_the_last_hundred_lines() {
    let d = fake(|r| match r {
        Request::Logs {
            view: LogView::Tail { lines },
            ..
        } => {
            assert_eq!(*lines, 100);
            Ok(Response::LogTail {
                lines: vec!["a".into(), "b".into()],
                running: true,
                end_offset: None,
            })
        }
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let r = call(&tools_with(&d), "logs", json!({"id": 1}));
    assert!(
        r.text.starts_with("a\nb") && r.text.contains("still running"),
        "{}",
        r.text
    );
}

#[test]
fn a_huge_tail_is_bounded_with_a_note() {
    let d = fake(|_| {
        Ok(Response::LogTail {
            lines: (0..5000)
                .map(|i| format!("line {i} {}", "x".repeat(40)))
                .collect(),
            running: false,
            end_offset: None,
        })
    });
    let r = call(&tools_with(&d), "logs", json!({"id": 1, "tail": 5000}));
    assert!(r.text.len() < 40 * 1024, "{}", r.text.len());
    assert!(
        r.text.starts_with("truncated:") && r.text.contains("offset 0"),
        "{}",
        &r.text[..120]
    );
    assert!(r.text.contains("line 4999"), "the tail end is kept");
    assert_eq!(r.structured["truncated"], true);
}

#[test]
fn logs_grep_shows_numbered_matches_and_context() {
    let d = fake(|r| match r {
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
            assert_eq!((pattern.as_str(), *context, *regex), ("FAILED", 1, false));
            Ok(Response::LogGrep {
                groups: vec![
                    GrepGroup {
                        lines: vec![
                            GrepLine {
                                n: 4,
                                text: "before".into(),
                                matched: false,
                            },
                            GrepLine {
                                n: 5,
                                text: "test x FAILED".into(),
                                matched: true,
                            },
                        ],
                    },
                    GrepGroup {
                        lines: vec![GrepLine {
                            n: 90,
                            text: "FAILED again".into(),
                            matched: true,
                        }],
                    },
                ],
                matches: 2,
                truncated: true,
            })
        }
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let r = call(
        &tools_with(&d),
        "logs",
        json!({"id": 1, "grep": "FAILED", "fixed": true, "context": 1}),
    );
    assert!(
        r.text
            .contains("4-before\n5:test x FAILED\n--\n90:FAILED again"),
        "{}",
        r.text
    );
    assert!(r.text.contains("match limit"));
    let d = fake(|_| {
        Ok(Response::LogGrep {
            groups: vec![],
            matches: 0,
            truncated: false,
        })
    });
    assert_eq!(
        call(&tools_with(&d), "logs", json!({"id": 1, "grep": "zz"})).text,
        "no matching lines"
    );
}

#[test]
fn logs_from_an_offset_names_the_next_offset() {
    let d = fake(|r| match r {
        Request::Logs {
            view: LogView::Follow { offset, max_bytes },
            ..
        } => {
            assert_eq!((*offset, *max_bytes), (100, Some(10)));
            Ok(Response::LogChunk {
                data: "0123456789".into(),
                next_offset: 110,
                done: false,
            })
        }
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let r = call(
        &tools_with(&d),
        "logs",
        json!({"id": 1, "offset": 100, "max_bytes": 10}),
    );
    assert!(
        r.text.contains("truncated, use logs with offset 110"),
        "{}",
        r.text
    );
    assert_eq!(r.structured["next_offset"], 110);

    let d = fake(|_| {
        Ok(Response::LogChunk {
            data: "tail\n".into(),
            next_offset: 505,
            done: true,
        })
    });
    let r = call(&tools_with(&d), "logs", json!({"id": 1, "offset": 500}));
    assert!(r.text.contains("end of log at offset 505"), "{}", r.text);
    assert_eq!(r.structured["done"], true);

    let d = fake(|_| {
        Ok(Response::LogChunk {
            data: "x".repeat(100_000),
            next_offset: 100_000,
            done: false,
        })
    });
    let r = call(
        &tools_with(&d),
        "logs",
        json!({"id": 1, "offset": 0, "max_bytes": 100_000}),
    );
    assert!(r.text.len() < 40 * 1024);
    let next = r.structured["next_offset"].as_u64().unwrap();
    assert!(
        next < 40 * 1024 && r.text.contains(&format!("offset {next}")),
        "{}",
        &r.text[r.text.len() - 80..]
    );
}

#[test]
fn logs_rejects_conflicting_views_and_a_missing_log_is_explained() {
    let d = never_called();
    for args in [
        json!({"id": 1, "tail": 5, "grep": "x"}),
        json!({"id": 1, "offset": 0, "tail": 5}),
        json!({"id": 1, "offset": 0, "grep": "x"}),
        json!({"id": 1, "context": 2}),
        json!({"id": 1, "max_bytes": 5}),
        json!({"id": 1, "tail": 0}),
        json!({"id": 1, "tail": 5001}),
        json!({"id": 1, "grep": "x", "context": 101}),
    ] {
        assert!(
            call(&tools_with(&d), "logs", args.clone()).is_error,
            "{args}"
        );
    }
    assert!(d.requests().is_empty());
    let d = fake(|_| {
        Err(DaemonError::Refused(ApiError::new(
            ErrorCode::NotFound,
            "no log",
        )))
    });
    let r = call(&tools_with(&d), "logs", json!({"id": 3}));
    assert_eq!(error_kind(&r), "not_found");
    assert!(r.text.contains("has not started yet"), "{}", r.text);
}

// ---- wait ---------------------------------------------------------------------------------

#[test]
fn wait_returns_the_state_and_log_tail_once_the_job_is_over() {
    let gets = Arc::new(AtomicUsize::new(0));
    let g = Arc::clone(&gets);
    let d = fake(move |r| match r {
        Request::Get { .. } => {
            let n = g.fetch_add(1, Ordering::SeqCst);
            let phase = if n < 2 {
                Phase::Running
            } else {
                Phase::Failed { exit: Some(101) }
            };
            Ok(Response::Job {
                job: Box::new(job(8, "me", phase, "cargo test")),
            })
        }
        Request::Logs {
            view: LogView::Tail { lines },
            ..
        } => {
            assert_eq!(*lines, 3);
            Ok(Response::LogTail {
                lines: vec!["a".into(), "test result: FAILED".into()],
                running: false,
                end_offset: None,
            })
        }
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let r = call(
        &tools_with(&d),
        "wait",
        json!({"id": 8, "timeout_s": 30, "tail_lines": 3}),
    );
    assert!(!r.is_error, "{}", r.text);
    assert!(
        r.text.starts_with("job 8 failed (exit 101)") && r.text.contains("test result: FAILED"),
        "{}",
        r.text
    );
    assert_eq!(r.structured["finished"], true);
    assert_eq!(r.structured["passed"], false);
    assert_eq!(gets.load(Ordering::SeqCst), 3);
}

// R-104, R-105
#[test]
fn r104_a_wait_that_times_out_never_touches_the_job() {
    let d = fake(|r| match r {
        Request::Get { .. } => Ok(Response::Job {
            job: Box::new(job(8, "me", Phase::Running, "cargo build")),
        }),
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let r = call(&tools_with(&d), "wait", json!({"id": 8, "timeout_s": 0}));
    assert!(!r.is_error);
    assert_eq!(r.structured["finished"], false);
    assert!(
        r.text.contains("still running") && r.text.contains("untouched"),
        "{}",
        r.text
    );
    assert!(
        d.requests()
            .iter()
            .all(|r| matches!(r, Request::Get { .. })),
        "only reads: {:?}",
        d.requests()
    );
}

#[test]
fn a_cancelled_wait_ends_with_no_result_and_sends_no_mutation() {
    let d = fake(|r| match r {
        Request::Get { .. } => Ok(Response::Job {
            job: Box::new(job(8, "me", Phase::Queued, "cargo build")),
        }),
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let t = tools_with(&d);
    let tok = CancelToken::new();
    let tok2 = Arc::clone(&tok);
    let h = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        tok2.cancel();
    });
    let out = t.call("wait", json!({"id": 8, "timeout_s": 3600}), &tok);
    h.join().unwrap();
    assert_eq!(out, Some(Outcome::Cancelled));
    assert!(
        d.requests()
            .iter()
            .all(|r| matches!(r, Request::Get { .. }))
    );
}

#[test]
fn wait_on_a_missing_job_is_an_error_not_a_hang() {
    let d = fake(|_| {
        Err(DaemonError::Refused(ApiError::new(
            ErrorCode::NotFound,
            "no job",
        )))
    });
    assert_eq!(
        error_kind(&call(
            &tools_with(&d),
            "wait",
            json!({"id": 4, "timeout_s": 60})
        )),
        "not_found"
    );
}

// ---- cancel -------------------------------------------------------------------------------

// R-108
#[test]
fn r108_cancel_requires_the_owner() {
    let d = fake(|r| match r {
        Request::Cancel { who, .. } if who == "me" => Ok(Response::Cancelled {
            id: JobId(3),
            result: "requested".into(),
        }),
        Request::Cancel { .. } => Err(DaemonError::Refused(ApiError::new(
            ErrorCode::NotOwner,
            "job 3 belongs to me",
        ))),
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let t = tools_with(&d);
    let r = call(&t, "cancel", json!({"id": 3, "who": "intruder"}));
    assert_eq!(error_kind(&r), "not_owner");
    let r = call(
        &t,
        "cancel",
        json!({"id": 3, "who": "me", "reason": "stale"}),
    );
    assert!(
        r.text.contains("cancel requested") && r.text.contains("reason: stale"),
        "{}",
        r.text
    );
    // who is required: there is no owner fallback for a destructive call
    assert_eq!(
        error_kind(&call(&t, "cancel", json!({"id": 3}))),
        "invalid_arguments"
    );
    assert_eq!(d.requests().len(), 2);
}

// R-100
#[test]
fn r100_a_cancel_with_a_lost_reply_is_not_resent() {
    let d = fake(|_| Err(DaemonError::ReplyLost("eof".into())));
    let r = call(&tools_with(&d), "cancel", json!({"id": 3, "who": "me"}));
    assert!(r.is_error && r.text.contains("not resent"), "{}", r.text);
    assert_eq!(d.requests().len(), 1);
}

#[test]
fn cancel_results_say_what_happened() {
    for (res, want) in [
        ("cancelled", "before it started"),
        ("already_finished", "already finished"),
    ] {
        let d = fake(move |_| {
            Ok(Response::Cancelled {
                id: JobId(1),
                result: res.into(),
            })
        });
        let r = call(&tools_with(&d), "cancel", json!({"id": 1, "who": "me"}));
        assert!(r.text.contains(want), "{}", r.text);
    }
}

// ---- status / estimate --------------------------------------------------------------------

fn snapshot() -> tasks_daemon::api::StatusSnapshot {
    use tasks_daemon::api::*;
    StatusSnapshot {
        daemon: DaemonView {
            pid: 9,
            version: "0.1".into(),
            machine: "laptop".into(),
            state_dir: "/s".into(),
            lock_dir: "/l".into(),
            policy_file: "/p".into(),
            policy_file_loaded: false,
            policy_problems: vec![],
            draining: true,
            drained_by: Some("ops".into()),
            drained_at_ms: Some(1),
            now_ms: 2,
            execution_enabled: true,
            uptime_secs: 5,
        },
        slots: vec![SlotGroupView {
            kind: "compile".into(),
            total: 2,
            free: 1,
            busy: vec![SlotView {
                index: 0,
                job: Some(JobId(4)),
                external: false,
                holder: None,
            }],
        }],
        waiting: vec![WaiterView {
            id: JobId(6),
            owner: "o".into(),
            command: "cargo test".into(),
            wait_text: "waiting for a compile slot".into(),
            position: 1,
            start_estimate_secs: None,
            rough: true,
            class: String::new(),
            class_note: String::new(),
            on_peer: None,
        }],
        waiting_more: 0,
        running: vec![RunningView {
            id: JobId(4),
            owner: "me".into(),
            command: "cargo build".into(),
            slot: None,
            elapsed_secs: 65.0,
            done_targets: 1,
            total_targets: 4,
            current: None,
            eta_secs: Some(30.0),
            eta_low_secs: None,
            recent_secs: None,
            over_by_secs: None,
            eta_rough_from: None,
            no_eta_why: None,
            log_bytes: 10,
            figures: None,
            cores: None,
            mem_bytes: None,
            transfer: None,
        }],
        pressure: PressureView {
            memory: "ok".into(),
            memory_some: Some(1.0),
            memory_full: Some(0.0),
            memory_stall_limit: None,
            cpu: Some(40.0),
            io: None,
            available_memory: Some(8 << 30),
            memory_total: None,
            cpu_busy_pct: None,
            cpu_cores: None,
            cores_in_use: None,
            swap_total: None,
            swap_used: None,
            swap_in_per_s: None,
            swap_out_per_s: None,
            cache_free: Some(50 << 30),
            state_free: None,
            compile_blocker: Some("cpu pressure 40%".into()),
            warning: None,
        },
        alerts: vec![AlertView {
            id: "a".into(),
            level: "warn".into(),
            message: "disk low".into(),
            since_ms: 0,
        }],
        settings: vec![SettingView {
            key: "k".into(),
            value: "v".into(),
            source: Source::Default,
        }],
        peers: vec![PeerView {
            name: "nixos".into(),
            node_id: "n".into(),
            state: "up".into(),
            last_seen_ago_secs: Some(1),
            detail: String::new(),
            reply_ms: None,
            machine: None,
            did: String::new(),
            roles_out: vec![],
            roles_in: vec![],
            grant_expires_at: None,
            path: "unknown".into(),
            rtt_ms: None,
        }],
        attribution: "attribution: process tree (no cgroup delegation: test)".into(),
        transfer: None,
        trips: vec![],
        power: None,
        endpoint: None,
    }
}

#[test]
fn status_summarises_the_machine_and_keeps_the_structure_small() {
    let d = fake(|_| Ok(Response::Status(Box::new(snapshot()))));
    let r = call(&tools_with(&d), "status", json!({}));
    assert!(!r.is_error, "{}", r.text);
    for want in [
        "laptop",
        "DRAINING",
        "slots compile: 1/2 free (held by job 4)",
        "running #4 me cargo build for 1m05s",
        "waiting: 1 job(s)",
        "would wait: cpu pressure 40%",
        "alert [warn] disk low",
        "peer nixos: up",
    ] {
        assert!(r.text.contains(want), "missing {want:?} in:\n{}", r.text);
    }
    assert!(r.structured.get("settings").is_none());
    assert_eq!(r.structured["slots"][0]["free"], 1);
    assert!(call(&tools_with(&d), "status", json!({"x": 1})).is_error);
}

#[test]
fn estimate_reports_median_and_p90() {
    let d = fake(|r| match r {
        Request::Estimate { argv, .. } => {
            assert_eq!(argv, &argv_of("cargo test"));
            Ok(Response::Estimate(tasks_daemon::api::EstimateView {
                recipe: "cargo test".into(),
                median_secs: Some(90.0),
                p90_secs: Some(200.0),
                samples: 4,
                text: "from 4 runs".into(),
                why_not: None,
                rough: false,
            }))
        }
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let r = call(
        &tools_with(&d),
        "estimate",
        json!({"argv": ["cargo", "test"]}),
    );
    assert_eq!(
        r.text,
        "from 4 runs (median 1m30s, p90 3m20s, 4 passing run(s))"
    );
    assert!(call(&tools_with(&d), "estimate", json!({"argv": []})).is_error);
}

fn argv_of(s: &str) -> Vec<String> {
    argv(s)
}

#[test]
fn an_unknown_tool_is_not_a_result() {
    let d = never_called();
    assert!(
        tools_with(&d)
            .call("nope", json!({}), &CancelToken::new())
            .is_none()
    );
}

#[test]
fn the_cancel_reason_is_passed_to_the_daemon() {
    let d = fake(|_| {
        Ok(Response::Cancelled {
            id: JobId(3),
            result: "cancelled".into(),
        })
    });
    call(
        &tools_with(&d),
        "cancel",
        json!({"id": 3, "who": "me", "reason": "stale"}),
    );
    assert!(matches!(
        &d.requests()[0],
        Request::Cancel { reason: Some(r), .. } if r == "stale"
    ));
}

// ---- audit -------------------------------------------------------------------------------

fn audit_row(seq: i64, action: &str, job: Option<i64>) -> tasks_daemon::api::AuditRowView {
    tasks_daemon::api::AuditRowView {
        seq,
        at_ms: 1_700_000_000_000 + seq,
        actor: "did:key:zMe".into(),
        actor_name: Some("kurukkan".into()),
        owner: "agent-1".into(),
        action: action.into(),
        job,
        peer: None,
        peer_name: None,
        target: None,
        details: json!({"by": "agent-1", "reason": "wrong branch"}),
        prev_hash: "00".into(),
        hash: "11".into(),
    }
}

// R-284
#[test]
fn r284_audit_is_read_only_and_asks_the_daemon_for_the_filters_given() {
    let d = fake(|r| match r {
        Request::Audit { .. } => Ok(Response::Audit {
            rows: vec![
                audit_row(3, "job.cancelled", Some(9)),
                audit_row(2, "job.started", Some(9)),
            ],
            truncated: false,
        }),
        _ => Err(DaemonError::Failed(format!("unexpected {r:?}"))),
    });
    let r = call(
        &tools_with(&d),
        "audit",
        json!({"job": 9, "action": "job", "peer": "nixos", "since": "2h", "limit": 5}),
    );
    assert!(!r.is_error, "{}", r.text);
    assert!(r.text.contains("2 audit row(s)"), "{}", r.text);
    assert!(r.text.contains("job.cancelled by kurukkan"), "{}", r.text);
    assert!(r.text.contains("wrong branch"), "{}", r.text);
    assert_eq!(r.structured["rows"].as_array().unwrap().len(), 2);
    assert_eq!(r.structured["rows"][0]["action"], "job.cancelled");
    let reqs = d.requests();
    assert_eq!(reqs.len(), 1);
    match &reqs[0] {
        Request::Audit {
            since_ms,
            peer,
            job,
            action,
            limit,
            ..
        } => {
            assert!(since_ms.is_some());
            assert_eq!(peer.as_deref(), Some("nixos"));
            assert_eq!(
                (*job, action.as_deref(), *limit),
                (Some(9), Some("job"), Some(5))
            );
        }
        other => panic!("{other:?}"),
    }
}

// R-284
#[test]
fn r284_audit_rejects_unknown_arguments_and_bad_times() {
    let d = never_called();
    let r = call(&tools_with(&d), "audit", json!({"verify": true}));
    assert!(r.is_error);
    let r = call(&tools_with(&d), "audit", json!({"since": "yesterday-ish"}));
    assert!(r.is_error, "{}", r.text);
    assert!(d.requests().is_empty());
}

// R-284
#[test]
fn r284_an_empty_audit_log_says_so() {
    let d = fake(|_| {
        Ok(Response::Audit {
            rows: vec![],
            truncated: false,
        })
    });
    let r = call(&tools_with(&d), "audit", json!({}));
    assert!(!r.is_error);
    assert_eq!(r.text, "no audit rows match");
}

// ---- priority -------------------------------------------------------------------------------

// R-339
#[test]
fn r339_submit_takes_a_class_and_blocking_needs_a_reason_before_anything_is_sent() {
    let d = submitted(7);
    let t = tools_with(&d);
    let r = call(
        &t,
        "submit",
        json!({"argv": ["cargo", "check"], "who": "a", "priority": "blocking"}),
    );
    assert_eq!(error_kind(&r), "invalid_arguments");
    assert!(r.text.contains("needs a reason"), "{}", r.text);
    let r = call(
        &t,
        "submit",
        json!({"argv": ["cargo", "check"], "who": "a", "priority": "urgent"}),
    );
    assert_eq!(error_kind(&r), "invalid_arguments");
    let r = call(
        &t,
        "submit",
        json!({"argv": ["cargo", "check"], "who": "a", "reason": "no class"}),
    );
    assert_eq!(error_kind(&r), "invalid_arguments");
    assert!(submits(&d).is_empty(), "nothing reached the daemon");
    let r = call(
        &t,
        "submit",
        json!({"argv": ["cargo", "check"], "who": "a", "priority": "blocking", "reason": "user waits"}),
    );
    assert!(!r.is_error, "{}", r.text);
    let r = call(
        &t,
        "submit",
        json!({"argv": ["cargo", "check"], "who": "a", "priority": "background"}),
    );
    assert!(!r.is_error, "{}", r.text);
    let specs = submits(&d);
    assert_eq!(
        (specs[0].priority, specs[0].priority_reason.as_deref()),
        (tasks_core::Priority::Blocking, Some("user waits"))
    );
    assert_eq!(specs[1].priority, tasks_core::Priority::Background);
}

// R-350
#[test]
fn r350_prioritize_asks_the_daemon_and_says_what_changed() {
    let d = fake(|r| match r {
        Request::Prioritize { who, .. } if who == "me" => Ok(Response::Prioritized {
            id: JobId(3),
            from: tasks_core::Priority::Normal,
            to: tasks_core::Priority::Blocking,
        }),
        Request::Prioritize { .. } => Err(DaemonError::Refused(ApiError::new(
            ErrorCode::NotOwner,
            "job 3 belongs to me",
        ))),
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let t = tools_with(&d);
    let r = call(
        &t,
        "prioritize",
        json!({"id": 3, "who": "intruder", "priority": "background"}),
    );
    assert_eq!(error_kind(&r), "not_owner");
    // blocking without a reason never leaves the server
    let r = call(
        &t,
        "prioritize",
        json!({"id": 3, "who": "me", "priority": "blocking"}),
    );
    assert_eq!(error_kind(&r), "invalid_arguments");
    let r = call(
        &t,
        "prioritize",
        json!({"id": 3, "who": "me", "priority": "blocking", "reason": "release"}),
    );
    assert!(
        r.text.contains("normal -> blocking") && r.text.contains("reason: release"),
        "{}",
        r.text
    );
    assert_eq!(d.requests().len(), 2);
    // who is required, as for cancel
    assert_eq!(
        error_kind(&call(
            &t,
            "prioritize",
            json!({"id": 3, "priority": "background"})
        )),
        "invalid_arguments"
    );
}

// R-343: no tool accepts a way to claim the short lane
#[test]
fn r343_no_tool_has_a_short_lane_input_and_an_invented_one_is_refused() {
    for def in tasks_mcp::defs::tool_defs() {
        let props = def.input_schema["properties"].as_object().unwrap();
        for name in props.keys() {
            assert!(
                !name.to_lowercase().contains("short") && !name.to_lowercase().contains("lane"),
                "{}: {name}",
                def.name
            );
        }
        assert_eq!(
            def.input_schema["additionalProperties"], false,
            "{}",
            def.name
        );
    }
    let d = submitted(1);
    let r = call(
        &tools_with(&d),
        "submit",
        json!({"argv": ["cargo", "check"], "who": "a", "short": true}),
    );
    assert_eq!(error_kind(&r), "invalid_arguments");
    assert!(submits(&d).is_empty());
}

// R-349
#[test]
fn r349_a_forwarded_job_the_peer_holds_back_reads_as_waiting() {
    let mut j = job(5, "me", Phase::Running, "cargo test");
    j.forwarded_to = Some(("nixos".into(), Some(7)));
    j.remote_wait = Some("waiting on nixos: CPU contended".into());
    let d = fake(move |_| {
        Ok(Response::Job {
            job: Box::new(j.clone()),
        })
    });
    let r = call(&tools_with(&d), "get", json!({"id": 5}));
    assert!(r.text.starts_with("job 5 waiting on nixos"), "{}", r.text);
    assert!(
        r.text.contains("waiting: waiting on nixos: CPU contended"),
        "{}",
        r.text
    );
    assert_eq!(
        r.structured["remote_wait"],
        "waiting on nixos: CPU contended"
    );
    // R-822: the JSON says waiting too, as the text does
    assert_eq!(r.structured["phase"], "waiting");
    assert_eq!(r.structured["terminal"], false);
}

// R-368
#[test]
fn r368_submit_rule_warnings_from_the_daemon_reach_the_agent_as_notes() {
    let d = fake(|r| match r {
        Request::Submit { .. } => Ok(Response::Submitted {
            id: JobId(3),
            warnings: vec!["this builds every test target".into()],
        }),
        _ => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
    });
    let r = call(
        &tools_with(&d),
        "submit",
        json!({"argv": ["cargo", "test", "-p", "x"]}),
    );
    assert!(!r.is_error, "{}", r.text);
    assert!(
        r.text.contains("warning: this builds every test target"),
        "{}",
        r.text
    );
    assert_eq!(
        r.structured["notes"][0],
        "warning: this builds every test target"
    );
}

// R-358
#[test]
fn r358_status_reports_cpu_busy_swap_and_the_peers_path_and_reply_time() {
    let d = fake(|_| {
        let mut s = snapshot();
        s.pressure.cpu_busy_pct = Some(37.0);
        s.pressure.swap_total = Some(4 << 30);
        s.pressure.swap_used = Some(1 << 30);
        s.pressure.swap_in_per_s = Some(0);
        s.pressure.swap_out_per_s = Some(0);
        s.peers[0].path = "direct".into();
        s.peers[0].rtt_ms = Some(3);
        s.peers[0].reply_ms = Some(87);
        Ok(Response::Status(Box::new(s)))
    });
    let r = call(&tools_with(&d), "status", json!({}));
    assert!(!r.is_error, "{}", r.text);
    for want in [
        "cpu busy 37%",
        "swap 1.0 GiB of 4.0 GiB used",
        "peer nixos: up (direct; path RTT 3 ms, reply time 87 ms)",
    ] {
        assert!(r.text.contains(want), "missing {want:?} in:\n{}", r.text);
    }
}

// R-692, R-695
#[test]
fn r695_status_says_the_endpoint_and_marks_an_offline_peers_figures_as_old() {
    use tasks_daemon::api::{PeerMachineView, PeerSlotView};
    let d = fake(|_| {
        let mut s = snapshot();
        s.endpoint = Some(tasks_daemon::trust_control::EndpointView::none());
        s.peers[0].state = "offline".into();
        s.peers[0].last_seen_ago_secs = Some(2700);
        s.peers[0].machine = Some(PeerMachineView {
            cores: 10,
            memory_bytes: 16 << 30,
            slots: vec![PeerSlotView {
                kind: "compile".into(),
                total: 4,
                used: 1,
            }],
            waiting: 0,
            running: 0,
            cpu_pct: None,
            memory_pct: None,
            memory_level: None,
            io_pct: None,
            pressure_summary: String::new(),
            cpu_busy_pct: Some(48),
            memory_available_bytes: None,
            swap_total: None,
            swap_used: None,
            swap_in_per_s: None,
            swap_out_per_s: None,
            jobs: Vec::new(),
            cores_in_use_milli: None,
            power: None,
            battery_pct: None,
            holds_peer_jobs: false,
            version: None,
        });
        Ok(Response::Status(Box::new(s)))
    });
    let r = call(&tools_with(&d), "status", json!({}));
    assert!(!r.is_error, "{}", r.text);
    for want in [
        "this machine: no peer transport",
        "last known (last seen 45m",
        "10 cores, slots compile 1/4",
    ] {
        assert!(r.text.contains(want), "missing {want:?} in:\n{}", r.text);
    }
}

// R-517
#[test]
fn r517_submit_and_steps_take_a_fetch_list_checked_before_any_daemon_request() {
    let apk = "android/app/build/outputs/apk/debug/app-debug.apk";
    let d = submitted(7);
    let r = call(
        &tools_with(&d),
        "submit",
        json!({"argv": ["./android/gradlew", ":app:assembleDebug"], "peer": "nixos", "fetch": [apk]}),
    );
    assert!(!r.is_error, "{}", r.text);
    assert_eq!(submits(&d)[0].fetch, [apk]);
    let d = plan_daemon();
    let r = call(
        &tools_with(&d),
        "submit_steps",
        json!({"steps": [{"name": "apk", "argv": ["./gradlew", "assembleDebug"], "fetch": [apk]}], "peer": "nixos"}),
    );
    assert!(!r.is_error, "{}", r.text);
    let Request::SubmitPlan { plan, .. } = &d.requests()[0] else {
        panic!()
    };
    assert_eq!(plan.steps[0].hints.fetch, [apk]);
    // refused without asking the daemon
    let never = never_called();
    for bad in ["../x", "/etc/passwd", "a/../../b"] {
        let r = call(
            &tools_with(&never),
            "submit",
            json!({"argv": ["true"], "fetch": [bad]}),
        );
        assert!(r.is_error, "{bad} accepted");
        let r = call(
            &tools_with(&never),
            "submit_steps",
            json!({"steps": [{"name": "a", "argv": ["true"], "fetch": [bad]}]}),
        );
        assert!(r.is_error, "{bad} accepted in a step");
    }
    assert!(never.requests().is_empty());
}

// R-801: a job whose command is over on the peer while its outputs come back says so, with the
// peer's result and how far the file got, instead of looking like it still runs.
#[test]
fn r801_get_and_wait_show_a_job_fetching_its_outputs_after_the_remote_exit() {
    let d = fake(|r| match r {
        Request::Get { .. } => {
            let mut j = job(750, "bot", Phase::Running, "cargo build");
            j.forwarded_to = Some(("nixos".into(), Some(352)));
            j.fetch_paths = vec!["target/debug/p2p-desktop".into()];
            j.transfer = Some(tasks_core::TransferProgress {
                kind: tasks_core::TransferKind::Fetch,
                peer: "nixos".into(),
                path: Some("target/debug/p2p-desktop".into()),
                done_bytes: 120 << 20,
                total_bytes: Some(362 << 20),
                rate_bps: Some(f64::from(4u32 << 20)),
                remote_result: Some("command exit 0".into()),
                at_ms: 0,
            });
            Ok(Response::Job { job: Box::new(j) })
        }
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let line = "command exit 0 on nixos; fetching target/debug/p2p-desktop 120/362 MiB, 4 MiB/s, ~61s left";
    let r = call(&tools_with(&d), "get", json!({"id": 750}));
    assert!(r.text.contains("fetching outputs on nixos"), "{}", r.text);
    assert!(
        r.text.contains(&format!("transfer: 33% {line}")),
        "{}",
        r.text
    );
    assert_eq!(r.structured["transfer"]["done_bytes"], 120u64 << 20);
    assert_eq!(r.structured["transfer_text"], json!(format!("33% {line}")));
    let r = call(&tools_with(&d), "wait", json!({"id": 750, "timeout_s": 0}));
    assert!(
        r.text.contains("is still fetching outputs on nixos"),
        "{}",
        r.text
    );
}

// R-796: clients that show only structuredContent saw counts but no log lines, and no sign of a
// requested fetch until the job ended.
#[test]
fn r796_logs_and_get_carry_their_text_and_requested_fetches_in_the_structured_result() {
    let d = fake(|r| match r {
        Request::Logs {
            view: LogView::Tail { .. },
            ..
        } => Ok(Response::LogTail {
            lines: vec!["Compiling a".into(), "error: b".into()],
            running: false,
            end_offset: None,
        }),
        Request::Get { .. } => {
            let mut j = job(681, "bot", Phase::Running, "cargo build");
            j.fetch_paths = vec!["target/debug/p2p-desktop".into()];
            Ok(Response::Job { job: Box::new(j) })
        }
        _ => Err(DaemonError::Failed("unexpected".into())),
    });
    let r = call(&tools_with(&d), "logs", json!({"id": 682, "tail": 5}));
    assert_eq!(r.structured["lines"], 2);
    assert!(
        r.structured["text"]
            .as_str()
            .is_some_and(|t| t.contains("error: b")),
        "{}",
        r.structured
    );
    let r = call(&tools_with(&d), "get", json!({"id": 681}));
    assert_eq!(
        r.structured["fetch_requested"],
        json!(["target/debug/p2p-desktop"])
    );
    assert!(
        r.text.contains("fetch: requested target/debug/p2p-desktop"),
        "{}",
        r.text
    );
}
