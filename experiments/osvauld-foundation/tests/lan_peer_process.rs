//! Gate 4 loopback acceptance: two real `lan-peer` processes (serve, then connect) exchanging
//! the attested challenge protocol over iroh on 127.0.0.1. Proves process separation and the
//! out-of-band attestation files, NOT a second machine or LAN confinement. Synthetic only.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lan-peer");
const READY: Duration = Duration::from_secs(120);
const EVENT: Duration = Duration::from_secs(30);

fn run(args: &[&str]) -> (i32, Value) {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .expect("spawn lan-peer");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last = stdout.lines().last().unwrap_or_default();
    let value = serde_json::from_str(last).unwrap_or_else(|e| {
        panic!(
            "{args:?}: bad json {last:?} ({e}); stderr {:?}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap_or(-1), value)
}

fn init(dir: &Path) -> Value {
    let (code, v) = run(&["init", "--data-dir", dir.to_str().unwrap()]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["synthetic"], true);
    v
}

fn s(v: &Value, key: &str) -> String {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} in {v}"))
        .to_owned()
}

/// Kills the serve process on drop so a failed assertion never leaks it.
struct Server {
    child: Child,
    events: mpsc::Receiver<Value>,
}

impl Server {
    fn start(dir: &Path, allow: &[&str]) -> (Self, Value) {
        let mut args = vec![
            "serve",
            "--bind",
            "127.0.0.1:0",
            "--data-dir",
            dir.to_str().unwrap(),
        ];
        for file in allow {
            args.extend(["--allow", file]);
        }
        let mut child = Command::new(BIN)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn serve");
        let stdout = child.stdout.take().unwrap();
        let (tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let server = Self { child, events };
        let ready = server.next(READY);
        assert_eq!(ready["event"], "listening", "{ready}");
        (server, ready)
    }

    fn next(&self, wait: Duration) -> Value {
        self.events.recv_timeout(wait).expect("server event")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn two_processes_connect_over_loopback_and_reject_wrong_or_unapproved_peers() {
    let server_dir = tempfile::tempdir().unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let stranger_dir = tempfile::tempdir().unwrap();
    let client = init(client_dir.path());
    let stranger = init(stranger_dir.path());
    let client_att = s(&client, "attestation_file");
    let stranger_att = s(&stranger, "attestation_file");

    let (server, ready) = Server::start(server_dir.path(), &[&client_att]);
    let server_id = s(&ready, "endpoint_id");
    let server_att = s(&ready, "attestation_file");
    let addr = s(&ready, "addr");

    // One synthetic account, three distinct per-installation transport keys.
    assert_eq!(ready["account_did"], client["account_did"]);
    assert_eq!(ready["account_did"], stranger["account_did"]);
    let ids = [
        &server_id,
        &s(&client, "endpoint_id"),
        &s(&stranger, "endpoint_id"),
    ];
    assert_ne!(ids[0], ids[1]);
    assert_ne!(ids[0], ids[2]);
    assert_ne!(ids[1], ids[2]);

    // Server configuration: loopback only, no relay or address lookup.
    assert!(addr.starts_with("127.0.0.1:"), "{ready}");
    assert_eq!(ready["lan"]["address_lookup_services"], 0);
    assert_eq!(ready["lan"]["relay_urls"], 0);
    assert_eq!(ready["lan"]["home_relays"], 0);
    assert_eq!(ready["allowed_endpoints"], serde_json::json!([ids[1]]));

    let connect = |dir: &Path, peer: &str, allow: &str| {
        run(&[
            "connect",
            "--peer",
            peer,
            "--addr",
            &addr,
            "--data-dir",
            dir.to_str().unwrap(),
            "--allow",
            allow,
        ])
    };

    // 1. Approved client completes the challenge exchange.
    let (code, ok) = connect(client_dir.path(), &server_id, &server_att);
    assert_eq!(code, 0, "{ok}");
    assert_eq!(ok["status"], "ok");
    assert_eq!(ok["response_fresh"], true);
    assert_eq!(
        ok["local_endpoint"], client["endpoint_id"],
        "key persisted across runs"
    );
    let handled = server.next(EVENT);
    assert_eq!(handled["event"], "handled", "{handled}");
    assert_eq!(handled["remote"], client["endpoint_id"]);

    // 2. Same account, valid attestation, but not on the server's allowlist.
    let (code, rejected) = connect(stranger_dir.path(), &server_id, &server_att);
    assert_eq!(code, 3, "{rejected}");
    assert_eq!(rejected["stage"], "receiver");
    assert!(
        s(&rejected, "reason").contains("NotAllowlisted"),
        "{rejected}"
    );
    let event = server.next(EVENT);
    assert_eq!(event["event"], "rejected", "{event}");
    assert_eq!(event["remote"], stranger["endpoint_id"]);
    assert_eq!(event["reason"], "NotAllowlisted");

    // 3a. Wrong peer id against the server's attestation: refused before dialing.
    let (code, mismatch) = connect(client_dir.path(), ids[2], &server_att);
    assert_eq!(code, 3, "{mismatch}");
    assert_eq!(mismatch["stage"], "peer_attestation");

    // 3b. Wrong peer id with a matching (stranger) attestation, dialed at the server's address:
    // the server cannot prove that key, so the TLS handshake fails; nothing is handled.
    let (code, wrong) = connect(client_dir.path(), ids[2], &stranger_att);
    assert_eq!(code, 4, "{wrong}");
    assert_eq!(wrong["stage"], "transport");

    // The approved client still works afterwards (server survived the failures).
    let (code, again) = connect(client_dir.path(), &server_id, &server_att);
    assert_eq!(code, 0, "{again}");
    let mut saw_second = false;
    for _ in 0..3 {
        let event = server.next(EVENT);
        if event["event"] == "handled" {
            assert_eq!(event["remote"], client["endpoint_id"]);
            saw_second = true;
            break;
        }
    }
    assert!(saw_second, "second handled event");

    // Unspecified binds are refused.
    let (code, any) = run(&[
        "serve",
        "--bind",
        "0.0.0.0:0",
        "--data-dir",
        server_dir.path().to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "{any}");
    drop(server);
}
