#![cfg(unix)]
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::Duration;
use tinline_task_manager::{Action, PROTOCOL_VERSION, Response, request_with_reconnect};

struct Fixture(PathBuf);
impl Fixture {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("task-manager-{label}-{}.sock", std::process::id()));
        assert!(!path.exists(), "refusing to overwrite another fixture");
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn reply_once(listener: UnixListener) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::Write;
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(&mut stream).read_line(&mut line).unwrap();
        assert!(line.contains("list"));
        let response = Response {
            version: PROTOCOL_VERSION,
            execution_enabled: false,
            submitted_id: None,
            jobs: vec![],
            estimates: vec![],
            error: None,
            log: None,
        };
        serde_json::to_writer(&mut stream, &response).unwrap();
        stream.write_all(b"\n").unwrap();
    })
}

#[test]
fn missing_socket_starts_before_sending() {
    let fixture = Fixture::new("missing");
    let mut server = None;
    let response = request_with_reconnect(&fixture.0, Action::List, || {
        server = Some(reply_once(UnixListener::bind(&fixture.0)?));
        Ok(())
    })
    .unwrap();
    assert!(response.error.is_none());
    server.unwrap().join().unwrap();
}

#[test]
fn live_socket_does_not_trigger_restart() {
    let fixture = Fixture::new("live");
    let server = reply_once(UnixListener::bind(&fixture.0).unwrap());
    let response = request_with_reconnect(&fixture.0, Action::List, || {
        panic!("live daemon must never be replaced")
    })
    .unwrap();
    assert!(response.error.is_none());
    server.join().unwrap();
}

#[test]
fn delivered_mutation_with_lost_reply_is_never_replayed() {
    let fixture = Fixture::new("lost-reply");
    let listener = UnixListener::bind(&fixture.0).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        assert!(line.contains("cancel"));
        // The mutation may already be applied: close without a response.
    });
    let result = request_with_reconnect(&fixture.0, Action::Cancel(1), || {
        panic!("delivered mutation must never be replayed")
    });
    server.join().unwrap();
    assert!(result.unwrap_err().to_string().contains("incomplete"));
}

#[test]
fn startup_failure_is_reported_without_sending() {
    let fixture = Fixture::new("unavailable");
    let result = request_with_reconnect(&fixture.0, Action::Cancel(1), || {
        Err("fixture startup failed".into())
    });
    assert_eq!(result.unwrap_err().to_string(), "fixture startup failed");
}

use tinline_task_manager::{
    ClientTimeouts, Cost, Job, JobSpec, Phase, STORE_BUSY_TIMEOUT, request_with_reconnect_timeouts,
    submit_candidates,
};

fn quick() -> ClientTimeouts {
    ClientTimeouts {
        reply: Duration::from_millis(300),
        total: Duration::from_secs(10),
    }
}

fn spec(owner: &str) -> JobSpec {
    JobSpec {
        suite: "local.command".into(),
        owner: owner.into(),
        source: "/synthetic/checkout".into(),
        revision: "live-workspace".into(),
        configuration: "{\"argv\":[\"true\"],\"resource\":\"run\"}".into(),
        cost: Cost::CpuSimulation,
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn listing(jobs: Vec<Job>) -> Response {
    Response {
        version: PROTOCOL_VERSION,
        execution_enabled: false,
        submitted_id: None,
        jobs,
        estimates: vec![],
        error: None,
        log: None,
    }
}

/// Serves connections in order: `handler(n, request_line)` returns the reply, or
/// `None` to stall past the client's reply timeout and then hang up.
fn serve(
    listener: UnixListener,
    connections: usize,
    handler: impl Fn(usize, &str) -> Option<Response> + Send + 'static,
) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        use std::io::Write;
        let mut seen = vec![];
        // Bounded: a client that stops early must fail the test, not hang it.
        listener.set_nonblocking(true).unwrap();
        for n in 0..connections {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() > deadline {
                            return seen;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream).read_line(&mut line).unwrap();
            seen.push(line.clone());
            match handler(n, &line) {
                Some(response) => {
                    serde_json::to_writer(&mut stream, &response).unwrap();
                    stream.write_all(b"\n").unwrap();
                }
                None => std::thread::sleep(Duration::from_millis(350)),
            }
        }
        seen
    })
}

#[test]
fn client_waits_longer_than_the_ledger_busy_timeout() {
    let defaults = ClientTimeouts::default();
    assert!(defaults.reply >= STORE_BUSY_TIMEOUT * 3, "{defaults:?}");
    assert!(defaults.total > defaults.reply);
}

#[test]
fn slow_daemon_read_is_retried_not_failed_with_eagain() {
    let fixture = Fixture::new("slow-list");
    let server = serve(UnixListener::bind(&fixture.0).unwrap(), 2, |n, line| {
        assert!(line.contains("list"));
        (n == 1).then(|| listing(vec![]))
    });
    let response = request_with_reconnect_timeouts(
        &fixture.0,
        Action::List,
        || panic!("live daemon must never be replaced"),
        quick(),
    )
    .unwrap();
    assert!(response.error.is_none());
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn lost_submit_reply_is_reconciled_without_resubmitting() {
    let fixture = Fixture::new("lost-submit");
    let mine = spec("reconcile-agent");
    let recorded = mine.clone();
    let server = serve(
        UnixListener::bind(&fixture.0).unwrap(),
        2,
        move |n, line| match n {
            0 => {
                assert!(line.contains("submit"));
                None
            }
            _ => {
                assert!(line.contains("list"), "a lost submit was replayed: {line}");
                Some(listing(vec![
                    Job {
                        id: 41,
                        spec: spec("other-agent"),
                        phase: Phase::Queued,
                        submitted_at: now(),
                        execution: None,
                    },
                    Job {
                        id: 42,
                        spec: recorded.clone(),
                        phase: Phase::Queued,
                        submitted_at: now(),
                        execution: None,
                    },
                ]))
            }
        },
    );
    let response = request_with_reconnect_timeouts(
        &fixture.0,
        Action::Submit(mine),
        || panic!("live daemon must never be replaced"),
        quick(),
    )
    .unwrap();
    assert_eq!(response.submitted_id, Some(42));
    let seen = server.join().unwrap();
    assert_eq!(seen.iter().filter(|l| l.contains("submit")).count(), 1);
}

#[test]
fn unconfirmed_submit_says_where_to_look_and_not_to_resubmit() {
    let fixture = Fixture::new("unconfirmed-submit");
    let server = serve(UnixListener::bind(&fixture.0).unwrap(), 4, |n, line| {
        if n == 0 {
            assert!(line.contains("submit"));
            return None;
        }
        assert!(line.contains("list"), "a lost submit was replayed: {line}");
        Some(listing(vec![]))
    });
    let error = request_with_reconnect_timeouts(
        &fixture.0,
        Action::Submit(spec("unconfirmed-agent")),
        || panic!("live daemon must never be replaced"),
        quick(),
    )
    .unwrap_err()
    .to_string();
    assert_eq!(
        server.join().unwrap().len(),
        4,
        "expected 3 reconciling listings"
    );
    assert!(error.contains("Do NOT resubmit"), "{error}");
    assert!(error.contains("owner=unconfirmed-agent"), "{error}");
    assert!(error.contains("cwd=/synthetic/checkout"), "{error}");
}

#[test]
fn submit_candidates_need_an_exact_recent_spec_match() {
    let mine = spec("me");
    let since = 1_000;
    let job = |id, spec: JobSpec, submitted_at| Job {
        id,
        spec,
        phase: Phase::Queued,
        submitted_at,
        execution: None,
    };
    let mut other_cwd = mine.clone();
    other_cwd.source = "/synthetic/other".into();
    let jobs = vec![
        job(1, mine.clone(), since - 1),
        job(2, spec("someone-else"), since),
        job(3, other_cwd, since),
        job(4, mine.clone(), since),
    ];
    assert_eq!(submit_candidates(&jobs, &mine, since), vec![4]);
}

fn busy(submitted_id: Option<i64>) -> Response {
    Response {
        submitted_id,
        error: Some("database is locked".into()),
        ..listing(vec![])
    }
}

#[test]
fn busy_submit_reply_without_an_id_is_safely_resent() {
    let fixture = Fixture::new("busy-submit");
    let server = serve(UnixListener::bind(&fixture.0).unwrap(), 2, |n, line| {
        assert!(line.contains("submit"));
        Some(if n == 0 {
            busy(None)
        } else {
            Response {
                submitted_id: Some(7),
                ..listing(vec![])
            }
        })
    });
    let response = request_with_reconnect_timeouts(
        &fixture.0,
        Action::Submit(spec("busy-agent")),
        || panic!("live daemon must never be replaced"),
        quick(),
    )
    .unwrap();
    assert_eq!(response.submitted_id, Some(7));
    assert!(response.error.is_none());
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn recorded_submit_with_busy_listing_keeps_its_id_and_is_not_resent() {
    let fixture = Fixture::new("recorded-busy");
    let server = serve(UnixListener::bind(&fixture.0).unwrap(), 1, |_, line| {
        assert!(line.contains("submit"));
        Some(busy(Some(9)))
    });
    let response = request_with_reconnect_timeouts(
        &fixture.0,
        Action::Submit(spec("recorded-agent")),
        || panic!("live daemon must never be replaced"),
        quick(),
    )
    .unwrap();
    assert_eq!(response.submitted_id, Some(9));
    assert!(response.error.is_none());
    assert_eq!(server.join().unwrap().len(), 1);
}
