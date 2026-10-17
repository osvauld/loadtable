//! `lan-peer serve --status` and `lan-peer status-watch` as separate processes on 127.0.0.1 with
//! a stub status command (a script printing a fixture snapshot). Synthetic installations only.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lan-peer");
const WAIT: Duration = Duration::from_secs(120);

fn run(args: &[&str]) -> (i32, Vec<Value>) {
    let out = Command::new(BIN).args(args).output().expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines = stdout
        .lines()
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| {
                panic!(
                    "{args:?}: bad json {l:?} ({e}); stderr {}",
                    String::from_utf8_lossy(&out.stderr)
                )
            })
        })
        .collect();
    (out.status.code().unwrap_or(-1), lines)
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A child whose stdout JSON lines arrive on a channel; killed on drop.
struct Lines {
    child: Child,
    lines: mpsc::Receiver<Value>,
}

impl Lines {
    fn spawn(args: &[&str]) -> Self {
        let mut child = Command::new(BIN)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).expect("one JSON object per line");
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }

    fn next(&self) -> Value {
        self.lines.recv_timeout(WAIT).expect("line")
    }
}

impl Drop for Lines {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn serve_status_and_watch_cli_with_offline_markers() {
    let work = tempfile::tempdir().unwrap();
    let w = work.path();
    let (server_dir, client_dir, stranger_dir) =
        (w.join("server"), w.join("client"), w.join("stranger"));
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/status-snapshot.json");
    let stub = w.join("fake-tasks-status");
    std::fs::write(
        &stub,
        "#!/bin/sh\n[ \"$2\" = --json ] || exit 9\nexec cat \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (code, init) = run(&["init", "--data-dir", p(&client_dir)]);
    assert_eq!(code, 0, "{init:?}");
    let client_att = init[0]["attestation_file"].as_str().unwrap().to_owned();
    let (code, _) = run(&["init", "--data-dir", p(&stranger_dir)]);
    assert_eq!(code, 0);

    // Interval below the 1 s minimum and status flags without --status are refused.
    let base = [
        "serve",
        "--bind",
        "127.0.0.1:0",
        "--data-dir",
        p(&server_dir),
        "--allow",
        &client_att,
    ];
    let (code, out) = run(&[&base[..], &["--status", "--status-interval", "0.5"]].concat());
    assert_eq!(code, 2, "{out:?}");
    let (code, out) = run(&[&base[..], &["--status-interval", "2"]].concat());
    assert_eq!(code, 2, "{out:?}");

    let status_cmd = format!("{} {} --json", p(&stub), p(&fixture));
    let serve_args = [
        &base[..],
        &[
            "--status",
            "--status-cmd",
            &status_cmd,
            "--status-interval",
            "1",
        ],
    ]
    .concat();
    let server = Lines::spawn(&serve_args);
    let ready = server.next();
    assert_eq!(ready["event"], "listening", "{ready}");
    assert_eq!(ready["status_stream"]["interval_s"], 1.0, "{ready}");
    assert_eq!(ready["status_stream"]["max_bytes"], 4 * 1024 * 1024);
    let addr = ready["addr"].as_str().unwrap().to_owned();
    let dial = |dir: &Path| {
        vec![
            "status-watch".to_owned(),
            "--peer".to_owned(),
            ready["endpoint_id"].as_str().unwrap().to_owned(),
            "--addr".to_owned(),
            addr.clone(),
            "--data-dir".to_owned(),
            p(dir).to_owned(),
            "--allow".to_owned(),
            ready["attestation_file"].as_str().unwrap().to_owned(),
        ]
    };
    fn refs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    // Authorized: two live snapshots, one line each, annotated online.
    let mut args = dial(&client_dir);
    args.extend(["--count".into(), "2".into()]);
    let (code, lines) = run(&refs(&args));
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        lines.len(),
        3,
        "2 snapshots + the final stopped line: {lines:?}"
    );
    for line in &lines[..2] {
        assert_eq!(line["version"], 1);
        assert_eq!(line["machine"]["id"], "synthetic-worker");
        assert_eq!(line["machine"]["online"], true);
        assert_eq!(line["jobs"][0]["id"], 7);
        assert_eq!(line["stream"]["transport"], "iroh");
    }
    assert!(lines[0]["stream"]["seq"].as_u64() < lines[1]["stream"]["seq"].as_u64());
    let ended = server.next();
    assert_eq!(ended["event"], "status_ended", "{ended}");
    assert!(ended["snapshots_sent"].as_u64().unwrap() >= 2, "{ended}");

    // Not allowlisted: an offline marker naming the refusal; nothing sampled for it.
    let mut args = dial(&stranger_dir);
    args.extend(["--count".into(), "1".into()]);
    let (code, lines) = run(&refs(&args));
    assert_eq!(code, 0, "{lines:?}");
    let marker = &lines[0];
    assert_eq!(marker["machine"]["online"], false, "{marker}");
    assert_eq!(
        marker["machine"]["id"], "127.0.0.1",
        "no snapshot yet: address fallback"
    );
    assert_eq!(marker["stream"]["marker"], "offline");
    assert!(
        marker["stream"]["error"]
            .as_str()
            .unwrap()
            .contains("NotAllowlisted"),
        "{marker}"
    );
    let rejected = server.next();
    assert_eq!(rejected["event"], "rejected", "{rejected}");
    assert_eq!(rejected["alpn"], "status");

    // Disconnect: the server dies mid-stream; the watcher prints an offline marker that keeps
    // the machine id it learned, then keeps retrying.
    let mut args = dial(&client_dir);
    args.extend([
        "--stall-timeout".into(),
        "2".into(),
        "--backoff-max".into(),
        "1".into(),
    ]);
    let watcher = Lines::spawn(&refs(&args));
    let first = watcher.next();
    assert_eq!(first["machine"]["online"], true, "{first}");
    drop(server);
    let mut offline = watcher.next();
    while offline["machine"]["online"] == true {
        offline = watcher.next();
    }
    assert_eq!(offline["machine"]["id"], "synthetic-worker", "{offline}");
    assert_eq!(offline["stream"]["marker"], "offline");
    assert!(offline["stream"]["retry_in_s"].as_f64().is_some());
    // Still alive and retrying: another offline marker follows.
    let again = watcher.next();
    assert_eq!(again["machine"]["online"], false, "{again}");
}
