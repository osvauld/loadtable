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
