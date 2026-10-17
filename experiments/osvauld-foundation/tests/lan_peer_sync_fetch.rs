//! Phase 2b step 3: `lan-peer sync-push` and `lan-peer fetch` as separate processes against a
//! `lan-peer serve --store --checkouts` on 127.0.0.1. Each CLI run is a fresh process, so the
//! zero-byte repeated fetch proves the client's file-backed store persists between runs.
//! Synthetic installations and fixtures only; loopback only.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lan-peer");
const WAIT: Duration = Duration::from_secs(120);
const OLD: Duration = Duration::from_secs(1_000_000_000);

fn run(args: &[&str]) -> (i32, Value) {
    let out = Command::new(BIN).args(args).output().expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last = stdout.lines().last().unwrap_or_default();
    let value = serde_json::from_str(last).unwrap_or_else(|e| {
        panic!(
            "{args:?}: bad json {last:?} ({e}); stderr {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap_or(-1), value)
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn s(v: &Value, key: &str) -> String {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} in {v}"))
        .to_owned()
}

fn write(root: &Path, rel: &str, content: &[u8], executable: bool) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
}

struct Server {
    child: Child,
    events: mpsc::Receiver<Value>,
}

impl Server {
    fn next(&self) -> Value {
        self.events.recv_timeout(WAIT).expect("server event")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn manifest(work: &Path, src: &Path, files: &[&str], tag: &str) -> String {
    let list = work.join(format!("{tag}.files0"));
    let mut bytes = Vec::new();
    for f in files {
        bytes.extend_from_slice(f.as_bytes());
        bytes.push(0);
    }
    std::fs::write(&list, bytes).unwrap();
    let (code, value) = run(&["manifest", p(src), "--files-from0", p(&list)]);
    assert_eq!(code, 0, "{value}");
    let out = work.join(format!("{tag}.manifest.json"));
    std::fs::write(&out, serde_json::to_vec(&value).unwrap()).unwrap();
    out.to_str().unwrap().to_owned()
}

#[test]
fn sync_push_and_fetch_cli_report_counts_and_reuse_blobs() {
    let work = tempfile::tempdir().unwrap();
    let w = work.path();
    let (server_dir, client_dir) = (w.join("server"), w.join("client"));
    let (store, checkouts) = (w.join("server-store"), w.join("checkouts"));
    let (client_store, out) = (w.join("client-store"), w.join("out"));
    let src = w.join("src");
    std::fs::create_dir_all(&out).unwrap();

    let (code, client) = run(&["init", "--data-dir", p(&client_dir)]);
    assert_eq!(code, 0, "{client}");
    let mut child = Command::new(BIN)
        .args([
            "serve",
            "--bind",
            "127.0.0.1:0",
            "--data-dir",
            p(&server_dir),
            "--allow",
            &s(&client, "attestation_file"),
            "--store",
            p(&store),
            "--checkouts",
            p(&checkouts),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
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
    let server = Server { child, events };
    let ready = server.next();
    assert_eq!(ready["event"], "listening", "{ready}");
    assert_eq!(ready["source_transfer"], true);
    assert_eq!(ready["artifact_return"], true, "{ready}");
    let dial = [
        "--peer".to_owned(),
        s(&ready, "endpoint_id"),
        "--addr".to_owned(),
        s(&ready, "addr"),
        "--data-dir".to_owned(),
        p(&client_dir).to_owned(),
        "--allow".to_owned(),
        s(&ready, "attestation_file"),
    ];
    let with = |cmd: &str, extra: &[&str]| -> (i32, Value) {
        let mut args: Vec<&str> = vec![cmd];
        args.extend(dial.iter().map(String::as_str));
        args.extend_from_slice(extra);
        run(&args)
    };

    write(
        &src,
        "Cargo.toml",
        b"[package]\nname = \"synthetic\"\n",
        false,
    );
    write(&src, "src/lib.rs", b"pub fn v1() {}\n", false);
    write(&src, "run.sh", b"#!/bin/sh\necho synthetic\n", true);
    let files = ["Cargo.toml", "src/lib.rs", "run.sh"];
    let m1 = manifest(w, &src, &files, "m1");
    let push = |m: &str| {
        with(
            "sync-push",
            &["--source", p(&src), "--manifest", m, "--checkout", "stable"],
        )
    };

    // 1. First sync: every blob, every file written.
    let (code, r) = push(&m1);
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["status"], "ok");
    assert_eq!(r["response_fresh"], true);
    assert_eq!(
        (r["blobs_sent"].as_u64(), r["written"].as_u64()),
        (Some(3), Some(3)),
        "{r}"
    );
    assert_eq!(r["unchanged"], 0);
    assert_eq!(r["receiver"]["created"], true, "{r}");
    let co = checkouts.join("stable");
    assert_eq!(s(&r["receiver"], "checkout"), p(&co));
    let ev = server.next();
    assert_eq!(ev["event"], "source_synced", "{ev}");
    assert_eq!(ev["written"], 3);
    std::fs::File::options()
        .write(true)
        .open(co.join("Cargo.toml"))
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + OLD)
        .unwrap();
    write(&co, "target/debug/app", b"synthetic artifact v1", true);
    write(&co, "stale.rs", b"// stale", false);

    // 2. No change: zero blobs, zero bytes, nothing written; stale source pruned, target kept.
    let (code, r) = push(&m1);
    assert_eq!(code, 0, "{r}");
    assert_eq!(
        (
            r["blobs_sent"].as_u64(),
            r["bytes_sent"].as_u64(),
            r["written"].as_u64()
        ),
        (Some(0), Some(0), Some(0)),
        "{r}"
    );
    assert_eq!(r["unchanged"], 3);
    assert_eq!(r["deleted"], 1);
    assert_eq!(
        r["receiver"]["deleted_paths"],
        serde_json::json!(["stale.rs"])
    );
    assert_eq!(server.next()["event"], "source_synced");
    assert_eq!(
        std::fs::metadata(co.join("Cargo.toml"))
            .unwrap()
            .modified()
            .unwrap(),
        SystemTime::UNIX_EPOCH + OLD
    );
    assert!(co.join("target/debug/app").is_file());

    // 3. One edit: one blob.
    write(&src, "src/lib.rs", b"pub fn v2() {}\n", false);
    let m2 = manifest(w, &src, &files, "m2");
    let (code, r) = push(&m2);
    assert_eq!(code, 0, "{r}");
    assert_eq!(
        (r["blobs_sent"].as_u64(), r["written"].as_u64()),
        (Some(1), Some(1)),
        "{r}"
    );
    assert_eq!(server.next()["event"], "source_synced");

    // 4. Fetch artifacts into a new directory, then again: the second moves zero bytes.
    let fetch = |dest: &str| {
        with(
            "fetch",
            &[
                "--checkout",
                "stable",
                "--path",
                "target/debug/app",
                "--path",
                "src",
                "--store",
                p(&client_store),
                "--dest",
                p(&out.join(dest)),
            ],
        )
    };
    let (code, r) = fetch("f1");
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["status"], "ok");
    assert_eq!(r["files"], 2);
    assert_eq!(r["blobs_received"], 2, "{r}");
    assert_eq!(
        r["bytes_received"].as_u64().unwrap(),
        (b"synthetic artifact v1".len() + b"pub fn v2() {}\n".len()) as u64
    );
    let ev = server.next();
    assert_eq!(ev["event"], "artifacts_served", "{ev}");
    assert_eq!(ev["blobs_sent"], 2);
    let app = out.join("f1/target/debug/app");
    assert_eq!(std::fs::read(&app).unwrap(), b"synthetic artifact v1");
    assert_eq!(
        std::fs::metadata(&app).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::read(out.join("f1/src/lib.rs")).unwrap(),
        b"pub fn v2() {}\n"
    );

    let (code, r) = fetch("f2");
    assert_eq!(code, 0, "{r}");
    assert_eq!(
        (r["blobs_received"].as_u64(), r["bytes_received"].as_u64()),
        (Some(0), Some(0)),
        "{r}"
    );
    assert_eq!(server.next()["bytes_sent"], 0);
    assert_eq!(
        std::fs::read(out.join("f2/target/debug/app")).unwrap(),
        b"synthetic artifact v1"
    );

    // 5. Local refusals happen before dialing (no server event): existing dest, escaping path.
    let (code, r) = fetch("f1");
    assert_eq!(code, 2, "{r}");
    assert_eq!(r["stage"], "local", "{r}");
    let (code, r) = with(
        "fetch",
        &[
            "--checkout",
            "stable",
            "--path",
            "../x",
            "--store",
            p(&client_store),
            "--dest",
            p(&out.join("f3")),
        ],
    );
    assert_eq!(code, 2, "{r}");
    assert!(!out.join("f3").exists());
    // A path the worker refuses (a symlink): rejected with exit 3.
    std::os::unix::fs::symlink("/etc/hostname", co.join("target/link")).unwrap();
    let (code, r) = with(
        "fetch",
        &[
            "--checkout",
            "stable",
            "--path",
            "target/link",
            "--store",
            p(&client_store),
            "--dest",
            p(&out.join("f4")),
        ],
    );
    assert_eq!(code, 3, "{r}");
    assert!(s(&r, "reason").contains("UnsafePath"), "{r}");
    assert_eq!(server.next()["event"], "rejected");
    assert!(!out.join("f4").exists());
}
