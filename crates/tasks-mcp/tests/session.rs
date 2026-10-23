//! Scripted stdio sessions end to end against a scripted daemon (R-194, R-195).
#![allow(missing_docs)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::*;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tasks_core::{JobId, Phase};
use tasks_daemon::api::{Request, Response};
use tasks_mcp::{DaemonError, Server, TOOL_NAMES, tool_defs};

fn lines(msgs: &[Value]) -> String {
    msgs.iter().map(|m| format!("{m}\n")).collect()
}

fn text_of(m: &Value) -> String {
    m["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// R-194
#[test]
fn r194_initialize_negotiates_the_protocol_version() {
    let d = never_called();
    let input = lines(&[
        rpc(
            1,
            "initialize",
            json!({"protocolVersion": "2024-11-05", "capabilities": {}}),
        ),
        rpc(2, "initialize", json!({"protocolVersion": "2099-01-01"})),
        rpc(3, "initialize", json!({})),
    ]);
    let out = session(&d, &input);
    assert_eq!(out[0]["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(out[1]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(out[2]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(out[0]["result"]["serverInfo"]["name"], "tasks");
    assert_eq!(
        out[0]["result"]["capabilities"]["tools"]["listChanged"],
        false
    );
    assert!(d.requests().is_empty());
}

// R-195
#[test]
fn r195_tools_list_matches_the_schemas() {
    let d = never_called();
    let out = session(&d, &lines(&[rpc(1, "tools/list", json!({}))]));
    let tools = out[0]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "submit",
            "submit_steps",
            "run_suite",
            "list",
            "get",
            "logs",
            "wait",
            "cancel",
            "prioritize",
            "status",
            "estimate",
            "audit"
        ]
    );
    assert_eq!(names, TOOL_NAMES);
    for (t, def) in tools.iter().zip(tool_defs()) {
        assert_eq!(t, &def.to_json());
        assert_eq!(t["inputSchema"]["additionalProperties"], false);
        assert!(!t["description"].as_str().unwrap().is_empty());
    }
}

// R-195
#[test]
fn r195_every_listed_tool_can_be_called_and_a_bad_call_is_a_tool_error() {
    let d = never_called();
    let calls: Vec<Value> = TOOL_NAMES
        .iter()
        .enumerate()
        .map(|(i, n)| tool_call(i as i64 + 1, n, json!({"unknown_key": true})))
        .collect();
    let out = session(&d, &lines(&calls));
    assert_eq!(out.len(), TOOL_NAMES.len());
    for m in &out {
        assert_eq!(m["result"]["isError"], true, "{m}");
        assert!(
            m.get("error").is_none(),
            "known tools never fail at protocol level: {m}"
        );
    }
    assert!(d.requests().is_empty());
}

// R-194
#[test]
fn r194_malformed_messages_get_jsonrpc_errors_and_the_session_goes_on() {
    let d = never_called();
    let long = format!("{}\n", "x".repeat(5 << 20));
    let mut input = String::new();
    input.push_str("this is not json\n");
    input.push_str("[1,2]\n");
    input.push_str("{\"jsonrpc\":\"2.0\",\"id\":5}\n");
    input.push_str("\n   \n");
    input.push_str(&long);
    input.push_str(&lines(&[
        rpc(6, "no/such/method", json!({})),
        rpc(7, "tools/call", json!({"arguments": {}})),
        rpc(8, "tools/call", json!({"name": "nope"})),
        rpc(9, "ping", json!({})),
    ]));
    let mut raw = input.into_bytes();
    raw.extend_from_slice(b"\xff\xfe not utf8\n");
    let out = SharedBuf::default();
    Server::new(tools_with(&d))
        .serve(raw.as_slice(), out.clone())
        .unwrap();
    let m = out.messages();
    let codes: Vec<i64> = m
        .iter()
        .filter_map(|m| m["error"]["code"].as_i64())
        .collect();
    assert_eq!(
        codes,
        [
            -32700, -32600, -32600, -32600, -32601, -32602, -32602, -32700
        ]
    );
    assert_eq!(m[0]["id"], Value::Null);
    assert_eq!(m[2]["id"], 5);
    assert_eq!(m[5]["id"], 7);
    assert_eq!(m[6]["error"]["message"], "unknown tool: nope");
    assert_eq!(m[6]["id"], 8);
    // the session survived all of it
    assert_eq!(m[m.len() - 2]["id"], 9);
    assert_eq!(m[m.len() - 2]["result"], json!({}));
}

// R-194
#[test]
fn r194_only_jsonrpc_goes_to_the_output() {
    let d = fake(|_| Err(DaemonError::Unreachable("down".into())));
    let input = lines(&[
        rpc(1, "initialize", json!({})),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        rpc(2, "tools/list", json!({})),
        tool_call(3, "status", json!({})),
        tool_call(4, "submit", json!({"argv": ["true"]})),
        rpc(5, "ping", json!({})),
    ]) + "garbage\n";
    let out = SharedBuf::default();
    Server::new(tools_with(&d))
        .serve(input.as_bytes(), out.clone())
        .unwrap();
    let lines = out.lines();
    assert_eq!(
        lines.len(),
        6,
        "one reply per request, none for the notification"
    );
    for l in &lines {
        let v: Value = serde_json::from_str(l).unwrap();
        assert_eq!(v["jsonrpc"], "2.0", "{l}");
        assert!(v.get("result").is_some() || v.get("error").is_some(), "{l}");
        assert!(!l.contains('\n'));
    }
}

// R-194
#[test]
fn r194_daemon_down_is_a_clean_tool_error() {
    let d = fake(|_| {
        Err(DaemonError::Unreachable(
            "cannot start the daemon: nope".into(),
        ))
    });
    let out = session(&d, &lines(&[tool_call(1, "status", json!({}))]));
    assert_eq!(out[0]["result"]["isError"], true);
    assert!(text_of(&out[0]).contains("cannot start the daemon: nope"));
    assert_eq!(
        out[0]["result"]["structuredContent"]["error"]["kind"],
        "daemon_unreachable"
    );
}

#[test]
fn a_scripted_session_submits_waits_and_reads_logs() {
    let polls = Arc::new(Mutex::new(0));
    let p = Arc::clone(&polls);
    let d = fake(move |r| match r {
        Request::List { .. } => Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        }),
        Request::Submit { spec } => {
            assert_eq!(spec.owner.as_str(), "me");
            Ok(Response::Submitted {
                id: JobId(12),
                warnings: vec![],
            })
        }
        Request::Get { id } => {
            assert_eq!(id.0, 12);
            let mut n = p.lock().unwrap();
            *n += 1;
            let phase = if *n < 3 {
                Phase::Running
            } else {
                Phase::Passed
            };
            Ok(Response::Job {
                job: Box::new(job(12, "me", phase, "cargo test -p x --lib")),
            })
        }
        Request::Logs { .. } => Ok(Response::LogTail {
            lines: vec!["test result: ok. 3 passed".into()],
            running: false,
            end_offset: None,
        }),
        other => Err(DaemonError::Failed(format!("unexpected {other:?}"))),
    });
    let input = lines(&[
        rpc(1, "initialize", json!({"protocolVersion": "2025-06-18"})),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        rpc(2, "tools/list", json!({})),
        tool_call(
            3,
            "submit",
            json!({"argv": ["cargo", "test", "-p", "x", "--lib"]}),
        ),
        tool_call(
            4,
            "wait",
            json!({"id": 12, "timeout_s": 60, "tail_lines": 5}),
        ),
        tool_call(5, "logs", json!({"id": 12, "tail": 5})),
    ]);
    let mut out = session(&d, &input);
    // calls run on their own threads, so replies may be in any order: match by id
    out.sort_by_key(|m| m["id"].as_i64());
    assert_eq!(out.len(), 5);
    assert!(text_of(&out[2]).contains("submitted job 12"));
    assert!(text_of(&out[3]).contains("job 12 passed") && text_of(&out[3]).contains("3 passed"));
    assert_eq!(out[3]["result"]["structuredContent"]["finished"], true);
    assert!(text_of(&out[4]).contains("test result: ok"));
    assert_eq!(out[4]["result"].get("isError"), None);
}

// R-194
#[test]
fn r194_cancelled_requests_get_no_response_and_the_job_is_untouched() {
    let d = fake(|r| match r {
        Request::Get { .. } => Ok(Response::Job {
            job: Box::new(job(1, "me", Phase::Running, "cargo build")),
        }),
        other => Err(DaemonError::Failed(format!("unexpected {other:?}"))),
    });
    let (tx, input) = input_channel();
    let out = SharedBuf::default();
    let d2 = Arc::clone(&d);
    let o2 = out.clone();
    let h = std::thread::spawn(move || Server::new(tools_with(&d2)).serve(input, o2));
    send(
        &tx,
        &tool_call(1, "wait", json!({"id": 1, "timeout_s": 3600})),
    );
    while d.requests().is_empty() {
        std::thread::sleep(Duration::from_millis(2));
    }
    send(
        &tx,
        &json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1, "reason": "user"}}),
    );
    send(&tx, &rpc(2, "ping", json!({})));
    out.wait_for_id(2);
    drop(tx);
    h.join().unwrap().unwrap();
    let m = out.messages();
    assert_eq!(m.len(), 1, "only the ping is answered: {m:?}");
    assert!(
        d.requests()
            .iter()
            .all(|r| matches!(r, Request::Get { .. }))
    );
}

// R-194
#[test]
fn r194_waits_are_concurrent_and_client_cancellable() {
    let released = Arc::new(AtomicBool::new(false));
    let rel = Arc::clone(&released);
    let d = fake(move |r| match r {
        Request::Get { id } => {
            let done = id.0 == 2 && rel.load(Ordering::SeqCst);
            let phase = if done { Phase::Passed } else { Phase::Running };
            Ok(Response::Job {
                job: Box::new(job(id.0, "me", phase, "cargo build")),
            })
        }
        Request::Logs { .. } => Ok(Response::LogTail {
            lines: vec![],
            running: false,
            end_offset: None,
        }),
        other => Err(DaemonError::Failed(format!("unexpected {other:?}"))),
    });
    let (tx, input) = input_channel();
    let out = SharedBuf::default();
    let d2 = Arc::clone(&d);
    let o2 = out.clone();
    let h = std::thread::spawn(move || Server::new(tools_with(&d2)).serve(input, o2));
    send(
        &tx,
        &tool_call(1, "wait", json!({"id": 1, "timeout_s": 3600})),
    );
    send(
        &tx,
        &tool_call(2, "wait", json!({"id": 2, "timeout_s": 3600})),
    );
    // both waits are polling while other requests are answered
    send(&tx, &rpc(3, "ping", json!({})));
    out.wait_for_id(3);
    send(&tx, &tool_call(4, "status", json!({})));
    assert_eq!(
        out.wait_for_id(4)["result"]["isError"],
        true,
        "status fails in the fake, but answers"
    );
    // cancel the first wait; finish the second
    send(
        &tx,
        &json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}}),
    );
    released.store(true, Ordering::SeqCst);
    let second = out.wait_for_id(2);
    assert!(text_of(&second).starts_with("job 2 passed"), "{second}");
    drop(tx);
    h.join().unwrap().unwrap();
    assert!(
        out.messages().iter().all(|m| m["id"] != json!(1)),
        "the cancelled wait stays silent"
    );
}

// R-194
#[test]
fn r194_responses_are_sent_after_stdin_closes() {
    let d = fake(|r| match r {
        Request::Get { .. } => {
            std::thread::sleep(Duration::from_millis(150));
            Ok(Response::Job {
                job: Box::new(job(1, "me", Phase::Passed, "true")),
            })
        }
        other => Err(DaemonError::Failed(format!("unexpected {other:?}"))),
    });
    // the whole input is already closed when the call starts
    let out = session(&d, &lines(&[tool_call(1, "get", json!({"id": 1}))]));
    assert_eq!(out.len(), 1);
    assert!(text_of(&out[0]).starts_with("job 1 passed"));
}

// R-194
#[test]
fn r194_one_slow_daemon_call_never_blocks_other_requests() {
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let gate_rx = Mutex::new(gate_rx);
    let d = fake(move |r| match r {
        Request::Get { .. } => {
            let _ = gate_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(20));
            Ok(Response::Job {
                job: Box::new(job(1, "me", Phase::Passed, "true")),
            })
        }
        Request::Estimate { .. } => Ok(Response::Estimate(tasks_daemon::api::EstimateView {
            recipe: "r".into(),
            median_secs: 1.0,
            p90_secs: 2.0,
            samples: 1,
            text: "quick".into(),
        })),
        other => Err(DaemonError::Failed(format!("unexpected {other:?}"))),
    });
    let (tx, input) = input_channel();
    let out = SharedBuf::default();
    let d2 = Arc::clone(&d);
    let o2 = out.clone();
    let h = std::thread::spawn(move || Server::new(tools_with(&d2)).serve(input, o2));
    send(&tx, &tool_call(1, "get", json!({"id": 1})));
    while d.requests().is_empty() {
        std::thread::sleep(Duration::from_millis(2));
    }
    send(&tx, &tool_call(2, "estimate", json!({"argv": ["true"]})));
    send(&tx, &rpc(3, "tools/list", json!({})));
    out.wait_for_id(2);
    out.wait_for_id(3);
    assert!(
        out.messages().iter().all(|m| m["id"] != json!(1)),
        "the slow call is still pending"
    );
    gate_tx.send(()).unwrap();
    out.wait_for_id(1);
    drop(tx);
    h.join().unwrap().unwrap();
}

#[test]
fn too_many_calls_in_flight_are_refused_not_queued_forever() {
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let gate_rx = Mutex::new(gate_rx);
    let d = fake(move |_| {
        let _ = gate_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(20));
        Ok(Response::Jobs {
            jobs: vec![],
            truncated: false,
        })
    });
    let (tx, input) = input_channel();
    let out = SharedBuf::default();
    let d2 = Arc::clone(&d);
    let o2 = out.clone();
    let h = std::thread::spawn(move || Server::new(tools_with(&d2)).serve(input, o2));
    let n = tasks_mcp::server::MAX_IN_FLIGHT as i64;
    for i in 1..=n + 1 {
        send(&tx, &tool_call(i, "list", json!({})));
    }
    let busy = out.wait_for_id(n + 1);
    assert_eq!(busy["error"]["code"], -32000);
    for _ in 0..n {
        let _ = gate_tx.send(());
    }
    drop(tx);
    h.join().unwrap().unwrap();
    assert_eq!(out.messages().len() as i64, n + 1);
}
