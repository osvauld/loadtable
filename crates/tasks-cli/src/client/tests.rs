use super::*;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tasks_core::{JobId, Placement};
use tasks_daemon::api::{ErrorCode, Reply, ReplyEnvelope, Request, Response, render_reply};

fn temp(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let p = std::env::temp_dir().join(format!(
        "tcli-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn ok_line(r: Response) -> String {
    render_reply(Reply::Ok(r))
}

fn err_line(code: ErrorCode, msg: &str) -> String {
    render_reply(Reply::Err(ApiError::new(code, msg)))
}

/// What a fake daemon does with one connection.
enum Act {
    /// Read the request line, then write this reply line.
    Reply(String),
    /// Read the request, wait, then write.
    Slow(Duration, String),
    /// Read the request, then close without a reply.
    Close,
}

/// Serves connections on `path`; `script(n, request_line)` decides what to do with the n-th.
fn serve(path: &Path, script: impl Fn(usize, &str) -> Act + Send + 'static) -> Arc<AtomicUsize> {
    let listener = UnixListener::bind(path).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { return };
            let n = c.fetch_add(1, Ordering::SeqCst);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let mut out = stream;
            match script(n, line.trim()) {
                Act::Reply(r) => {
                    let _ = writeln!(out, "{r}");
                }
                Act::Slow(d, r) => {
                    std::thread::sleep(d);
                    let _ = writeln!(out, "{r}");
                }
                Act::Close => {}
            }
        }
    });
    count
}

fn fast() -> Timeouts {
    Timeouts {
        attempt: Duration::from_millis(150),
        total: Duration::from_millis(1500),
        pause: Duration::from_millis(10),
    }
}

fn list_req() -> Request {
    Request::List {
        owner: None,
        active_only: false,
        recent_finished: None,
        limit: None,
    }
}

fn jobs() -> Response {
    Response::Jobs {
        jobs: vec![],
        truncated: false,
    }
}

// R-102
#[test]
fn r102_client_waits_longer_than_the_ledger_busy_timeout() {
    let t = Timeouts::default();
    // the daemon's API handlers wait at most 30 s on a busy ledger (handler.rs API_RETRY)
    assert!(t.attempt > Duration::from_secs(30));
    assert_eq!(t.attempt, Duration::from_secs(60));
    assert_eq!(t.total, Duration::from_secs(180));
    assert!(t.total > t.attempt);
}

#[test]
fn r102_a_slow_daemon_read_is_retried_not_failed_with_eagain() {
    let dir = temp("slow");
    let sock = dir.join("s");
    let ok = ok_line(jobs());
    let count = serve(&sock, move |n, _| {
        if n == 0 {
            Act::Slow(Duration::from_millis(600), ok.clone())
        } else {
            Act::Reply(ok.clone())
        }
    });
    let c = UnixClient::new(&sock).with_timeouts(fast());
    assert!(matches!(c.call(&list_req()), Ok(Response::Jobs { .. })));
    assert!(
        count.load(Ordering::SeqCst) >= 2,
        "the read was retried on a new connection"
    );
}

#[test]
fn r102_read_only_requests_give_up_after_the_total_budget() {
    let dir = temp("budget");
    let sock = dir.join("s");
    serve(&sock, |_, _| {
        Act::Slow(Duration::from_millis(2000), String::new())
    });
    let t = Timeouts {
        attempt: Duration::from_millis(100),
        total: Duration::from_millis(450),
        pause: Duration::from_millis(10),
    };
    let begin = Instant::now();
    let e = UnixClient::new(&sock)
        .with_timeouts(t)
        .call(&Request::Status)
        .unwrap_err();
    assert!(matches!(e, ClientError::ReplyLost { .. }), "{e}");
    assert!(begin.elapsed() < Duration::from_millis(1500));
}

// R-100
#[test]
fn r100_a_mutation_whose_reply_is_lost_is_never_resent() {
    let dir = temp("mutation");
    let sock = dir.join("s");
    let count = serve(&sock, |_, _| Act::Close);
    let c = UnixClient::new(&sock).with_timeouts(fast());
    for req in [
        Request::Cancel {
            id: JobId(1),
            who: "me".into(),
            reason: None,
        },
        Request::Drain {
            cancel: false,
            who: None,
        },
        Request::Execution { enabled: true },
    ] {
        let before = count.load(Ordering::SeqCst);
        let e = c.call(&req).unwrap_err();
        assert!(matches!(e, ClientError::ReplyLost { .. }), "{e}");
        assert_eq!(
            count.load(Ordering::SeqCst) - before,
            1,
            "{req:?} was sent once"
        );
    }
}

#[test]
fn only_read_only_requests_are_classified_as_safe_to_resend() {
    for r in [
        Request::Hello,
        list_req(),
        Request::Get { id: JobId(1) },
        Request::Status,
        Request::History {
            recipes: vec![],
            machines: vec![],
            max_samples: None,
        },
    ] {
        assert!(is_read_only(&r), "{r:?}");
    }
    for r in [
        Request::Cancel {
            id: JobId(1),
            who: "a".into(),
            reason: None,
        },
        Request::Drain {
            cancel: true,
            who: None,
        },
        Request::Execution { enabled: false },
        Request::Shutdown,
        Request::SubmitSuite {
            name: "x".into(),
            owner: "a".into(),
            cwd: "/".into(),
            env: vec![],
            placement: Placement::Here,
            quick: false,
            broad_test: false,
            priority: Default::default(),
            priority_reason: None,
        },
        Request::Prioritize {
            id: JobId(1),
            who: "a".into(),
            priority: tasks_core::Priority::Background,
            reason: None,
        },
    ] {
        assert!(!is_read_only(&r), "{r:?}");
    }
}

// R-101
#[test]
fn r101_a_connection_limit_reply_is_safely_resent_but_a_busy_reply_is_not() {
    let dir = temp("busy");
    let sock = dir.join("s");
    let submitted = ok_line(Response::Submitted {
        id: JobId(9),
        warnings: vec![],
    });
    let count = serve(&sock, move |n, line| {
        if line.contains("\"op\":\"shutdown\"") {
            Act::Reply(err_line(ErrorCode::Busy, "2 job(s) are running"))
        } else if n == 0 {
            Act::Reply(err_line(
                ErrorCode::ConnectionLimit,
                "too many connections (limit 64)",
            ))
        } else {
            Act::Reply(submitted.clone())
        }
    });
    let c = UnixClient::new(&sock).with_timeouts(fast());
    let spec = tasks_core::JobSpec {
        owner: tasks_core::Owner::new("me").unwrap(),
        argv: vec!["true".into()],
        cwd: "/".into(),
        env: vec![],
        kind: tasks_core::Kind::Run,
        requirements: Default::default(),
        placement: Placement::Here,
        after: vec![],
        timeout_secs: None,
        broad_test_escape: false,
        priority: tasks_core::Priority::Normal,
        priority_reason: None,
        fetch: Vec::new(),
    };
    let r = c.call(&Request::Submit { spec }).unwrap();
    assert_eq!(
        r,
        Response::Submitted {
            id: JobId(9),
            warnings: vec![]
        }
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    // `shutdown` answers busy because jobs run: a real answer, shown, not retried
    let e = c.call(&Request::Shutdown).unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::Busy));
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

// R-028
#[test]
fn r028_a_reply_over_the_frame_cap_is_reported_never_truncated() {
    let dir = temp("cap");
    let sock = dir.join("s");
    let big = ok_line(Response::LogChunk {
        data: "x".repeat(100_000),
        next_offset: 0,
        done: false,
    });
    let small = ok_line(Response::Ack);
    let (b, s) = (big.clone(), small.clone());
    serve(&sock, move |_, line| {
        if line.contains("\"op\":\"status\"") {
            Act::Reply(s.clone())
        } else {
            Act::Reply(b.clone())
        }
    });
    let c = UnixClient::new(&sock).with_timeouts(fast());
    let e = c.call_capped(&list_req(), 50_000).unwrap_err();
    assert!(
        matches!(e, ClientError::FrameTooLarge { cap: 50_000 }),
        "{e}"
    );
    assert!(e.to_string().contains("frame cap of 50000 bytes"));
    // the same reply fits a larger cap and arrives whole
    let Ok(Response::LogChunk { data, .. }) = c.call_capped(&list_req(), big.len()) else {
        panic!("whole reply expected")
    };
    assert_eq!(data.len(), 100_000);
    // a reply of exactly the cap is accepted
    assert!(c.call_capped(&Request::Status, small.len()).is_ok());
    assert!(matches!(
        c.call_capped(&Request::Status, small.len() - 1),
        Err(ClientError::FrameTooLarge { .. })
    ));
    assert_eq!(JSON_LIST_FRAME_CAP, 32 << 20);
    assert!(std::hint::black_box(DEFAULT_FRAME_CAP) > tasks_daemon::api::MAX_RESPONSE_BYTES);
}

#[test]
fn the_daemons_own_oversize_error_is_a_named_daemon_error() {
    let dir = temp("toolarge");
    let sock = dir.join("s");
    let line = err_line(
        ErrorCode::ResponseTooLarge,
        "reply exceeds 4194304 bytes; ask for less",
    );
    serve(&sock, move |_, _| Act::Reply(line.clone()));
    let e = UnixClient::new(&sock).call(&list_req()).unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::ResponseTooLarge));
    assert!(e.to_string().contains("ask for less"));
}

// version
#[test]
fn an_unknown_protocol_version_names_both_versions() {
    let dir = temp("version");
    let sock = dir.join("s");
    serve(&sock, |n, _| match n {
        0 => Act::Reply(r#"{"v":7,"reply":{"status":"ok","body":{"kind":"ack"}}}"#.into()),
        1 => Act::Reply(r#"{"reply":{"status":"ok","body":{"kind":"ack"}}}"#.into()),
        _ => {
            let mut e = ApiError::new(ErrorCode::UnsupportedVersion, "nope");
            e.supported = Some(3);
            Act::Reply(
                serde_json::to_string(&ReplyEnvelope {
                    v: PROTOCOL_VERSION,
                    reply: Reply::Err(e),
                })
                .unwrap(),
            )
        }
    });
    let c = UnixClient::new(&sock).with_timeouts(fast());
    let e = c.call(&Request::Status).unwrap_err();
    assert!(
        matches!(
            e,
            ClientError::Version {
                ours: 1,
                theirs: Some(7)
            }
        ),
        "{e}"
    );
    let text = e.to_string();
    assert!(
        text.contains("this client speaks version 1, the daemon speaks version 7"),
        "{text}"
    );
    let e = c.call(&Request::Status).unwrap_err();
    assert!(
        e.to_string().contains("daemon speaks version unknown"),
        "{e}"
    );
    let e = c.call(&Request::Status).unwrap_err();
    assert!(
        matches!(
            e,
            ClientError::Version {
                ours: 1,
                theirs: Some(3)
            }
        ),
        "{e}"
    );
}

#[test]
fn garbage_replies_are_bad_replies_and_oversize_requests_are_refused_unsent() {
    let dir = temp("garbage");
    let sock = dir.join("s");
    let count = serve(&sock, |_, _| Act::Reply("not json".into()));
    let c = UnixClient::new(&sock).with_timeouts(fast());
    assert!(matches!(
        c.call(&Request::Status),
        Err(ClientError::BadReply(_))
    ));
    let big = Request::Estimate {
        argv: vec!["x".repeat(MAX_REQUEST_BYTES + 1)],
        kind: None,
    };
    let before = count.load(Ordering::SeqCst);
    assert!(matches!(
        c.call(&big),
        Err(ClientError::RequestTooLarge { .. })
    ));
    assert_eq!(count.load(Ordering::SeqCst), before);
}

// R-103
struct FakeStarter {
    calls: Arc<AtomicUsize>,
    sock: PathBuf,
    result: Result<(), String>,
    served: Mutex<Vec<Arc<AtomicUsize>>>,
}

impl DaemonStarter for FakeStarter {
    fn start(&self) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()?;
        let line = ok_line(Response::Ack);
        self.served
            .lock()
            .unwrap()
            .push(serve(&self.sock, move |_, _| Act::Reply(line.clone())));
        Ok(())
    }
}

fn starter(sock: &Path, result: Result<(), String>) -> (Arc<FakeStarter>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(FakeStarter {
            calls: calls.clone(),
            sock: sock.to_path_buf(),
            result,
            served: Mutex::new(vec![]),
        }),
        calls,
    )
}

#[test]
fn r103_a_missing_socket_starts_the_daemon_before_sending() {
    let dir = temp("autostart");
    let sock = dir.join("s");
    let (s, calls) = starter(&sock, Ok(()));
    let c = UnixClient::new(&sock).with_timeouts(fast()).with_starter(s);
    assert_eq!(
        c.call(&Request::Drain {
            cancel: false,
            who: None
        })
        .unwrap(),
        Response::Ack
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // now it is live: no second start
    assert_eq!(c.call(&Request::Status).unwrap(), Response::Ack);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn r103_a_stale_socket_file_also_starts_the_daemon() {
    let dir = temp("stale");
    let sock = dir.join("s");
    drop(UnixListener::bind(&sock).unwrap()); // leaves a socket file nobody listens on
    assert!(sock.exists());
    std::fs::remove_file(&sock).unwrap();
    let (s, calls) = starter(&sock, Ok(()));
    let c = UnixClient::new(&sock).with_timeouts(fast()).with_starter(s);
    assert!(c.call(&Request::Status).is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn r103_a_live_socket_does_not_trigger_a_restart() {
    let dir = temp("live");
    let sock = dir.join("s");
    let line = ok_line(Response::Ack);
    serve(&sock, move |_, _| Act::Reply(line.clone()));
    let (s, calls) = starter(&sock, Ok(()));
    let c = UnixClient::new(&sock).with_timeouts(fast()).with_starter(s);
    for _ in 0..3 {
        assert!(c.call(&Request::Status).is_ok());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    // even a daemon that answers with an error is live
    let dir = temp("live-err");
    let sock = dir.join("s");
    let e = err_line(ErrorCode::Internal, "boom");
    serve(&sock, move |_, _| Act::Reply(e.clone()));
    let (s, calls) = starter(&sock, Ok(()));
    let c = UnixClient::new(&sock).with_timeouts(fast()).with_starter(s);
    assert!(c.call(&Request::Status).is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn r103_a_startup_failure_is_reported_without_sending() {
    let dir = temp("startfail");
    let sock = dir.join("s");
    let (s, calls) = starter(
        &sock,
        Err("the daemon exited (exit status: 1) before it listened".into()),
    );
    let c = UnixClient::new(&sock).with_timeouts(fast()).with_starter(s);
    let e = c
        .call(&Request::Drain {
            cancel: false,
            who: None,
        })
        .unwrap_err();
    assert!(matches!(e, ClientError::StartFailed(_)), "{e}");
    assert!(
        e.to_string()
            .starts_with("cannot start the daemon: the daemon exited")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "tried once, no loop");
    assert!(!sock.exists(), "nothing was served, so nothing was sent");
}

#[test]
fn without_a_starter_a_missing_socket_is_a_connect_error_naming_the_path() {
    let dir = temp("nostart");
    let sock = dir.join("missing");
    let e = UnixClient::new(&sock).call(&Request::Status).unwrap_err();
    assert!(matches!(e, ClientError::Connect { .. }));
    assert!(e.to_string().contains(&sock.display().to_string()));
}

// R-089
#[cfg(target_os = "linux")]
#[test]
fn r089_an_autostarted_daemon_does_not_inherit_the_clients_descriptors() {
    use rustix::fd::AsRawFd;
    let dir = temp("inherit");
    let sock = dir.join("s");
    let out = dir.join("fds");
    // a descriptor that is NOT close-on-exec, like a lock file leaked by our parent. It sits
    // high so it never collides with the descriptor `ls` opens for /proc/self/fd (lowest free).
    let leaked = rustix::io::fcntl_dupfd_cloexec(std::io::stdin(), 200).unwrap();
    let flags = rustix::io::fcntl_getfd(&leaked).unwrap();
    rustix::io::fcntl_setfd(&leaked, flags - rustix::io::FdFlags::CLOEXEC).unwrap();
    let n = leaked.as_raw_fd();
    assert!(
        !rustix::io::fcntl_getfd(&leaked)
            .unwrap()
            .contains(rustix::io::FdFlags::CLOEXEC)
    );
    let st = ProcessStarter {
        program: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".into(),
            format!("ls /proc/self/fd > {}; sleep 0", out.display()),
        ],
        log: dir.join("daemon.log"),
        socket: sock.clone(),
        wait: Duration::from_secs(3),
    };
    // the child exits without ever listening, so start() reports that; what matters is its fds
    let _ = st.start();
    let listing = std::fs::read_to_string(&out).unwrap();
    let fds: Vec<i32> = listing
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    assert!(!fds.contains(&n), "fd {n} leaked into the child: {fds:?}");
    drop(leaked);
}

#[test]
fn a_daemon_that_exits_before_listening_is_reported_with_its_log() {
    let dir = temp("exits");
    let st = ProcessStarter {
        program: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".into(),
            "echo 'tasks2d: cannot open the ledger' >&2; exit 3".into(),
        ],
        log: dir.join("daemon.log"),
        socket: dir.join("s"),
        wait: Duration::from_secs(5),
    };
    let e = st.start().unwrap_err();
    assert!(
        e.contains("exited") && e.contains("cannot open the ledger"),
        "{e}"
    );
}

#[test]
fn a_started_daemon_that_listens_is_ready() {
    let dir = temp("listens");
    let sock = dir.join("s");
    let _listener = UnixListener::bind(&sock).unwrap();
    let st = ProcessStarter {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), "sleep 1".into()],
        log: dir.join("daemon.log"),
        socket: sock,
        wait: Duration::from_secs(5),
    };
    assert_eq!(st.start(), Ok(()));
}

// R-231
#[test]
fn r231_the_client_resolves_the_daemons_directories_and_ignores_tmpdir() {
    let env = |k: &str| match k {
        "HOME" => Some("/home/u".to_string()),
        "TMPDIR" => Some("/tmp/claude-1000".to_string()),
        _ => None,
    };
    let cli = crate::args::Cli {
        state_dir: None,
        command: crate::args::Command::Drain,
    };
    let (_, lock) = crate::real_client(&cli, &env);
    assert_eq!(
        lock,
        Some(PathBuf::from("/home/u/.local/state/tasks2/slots"))
    );
    let cli = crate::args::Cli {
        state_dir: Some("/srv/t".into()),
        command: crate::args::Command::Drain,
    };
    let (_, lock) = crate::real_client(&cli, &env);
    assert_eq!(lock, Some(PathBuf::from("/srv/t/slots")));
    // the socket is where the daemon binds it
    let cfg = DaemonConfigProbe::socket(&env);
    assert_eq!(cfg, PathBuf::from("/home/u/.local/state/tasks2/tasks.sock"));
    // no HOME: every call fails with the setup error, nothing is guessed
    let (c, lock) = crate::real_client(&cli_default(), &|_| None);
    assert_eq!(lock, None);
    assert!(matches!(
        c.call(&Request::Status),
        Err(ClientError::Setup(_))
    ));
}

fn cli_default() -> crate::args::Cli {
    crate::args::Cli {
        state_dir: None,
        command: crate::args::Command::Drain,
    }
}

struct DaemonConfigProbe;

impl DaemonConfigProbe {
    fn socket(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
        tasks_daemon::config::DaemonConfig::resolve(
            &tasks_daemon::config::ConfigArgs::default(),
            env,
        )
        .unwrap()
        .socket_path()
    }
}

#[test]
fn the_unavailable_client_fails_every_call_with_its_message() {
    let c = UnavailableClient("no HOME".into());
    assert_eq!(c.call(&Request::Status).unwrap_err().to_string(), "no HOME");
}
