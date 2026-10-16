//! Phase 2 source transfer: content-addressed (raw BLAKE3 / iroh-blobs) missing-blob push over
//! the attested LAN-only `lan-peer` transport, into a file-backed iroh-blobs store, then a
//! path-safe materialized checkout. Loopback only; synthetic installations and fixtures only.
//! Proves the transfer/materialize contract, NOT execution, a second machine or LAN confinement.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use iroh::{Endpoint, SecretKey};
use iroh_blobs::Hash;
use iroh_blobs::store::mem::MemStore;
use serde_json::Value;
use task_manager_osvauld_foundation::source::{
    self, Limits, Manifest, ManifestFile, PushMode, SOURCE_ALPN, SourceError, SourceOutcome,
};
use task_manager_osvauld_foundation::{
    Policy, attest_endpoint, direct_addr, lan_endpoint, load_or_create_transport_key, now_secs,
    open_synthetic_installation,
};
use tokio::time::timeout;

const BIN: &str = env!("CARGO_BIN_EXE_lan-peer");
const READY: Duration = Duration::from_secs(120);
const EVENT: Duration = Duration::from_secs(60);
const STEP: Duration = Duration::from_secs(60);
/// Published BLAKE3 values (= iroh-blobs raw blob hashes) of "" and "abc".
const BLAKE3_EMPTY: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
const BLAKE3_ABC: &str = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";

// ---------------------------------------------------------------------------------------------
// Process helpers

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

fn s(v: &Value, key: &str) -> String {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} in {v}"))
        .to_owned()
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

struct Server {
    child: Child,
    events: mpsc::Receiver<Value>,
}

impl Server {
    fn start(args: &[&str]) -> (Self, Value) {
        let mut child = Command::new(BIN)
            .args(args)
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
        let ready = server.next();
        assert_eq!(ready["event"], "listening", "{ready}");
        (server, ready)
    }

    fn next(&self) -> Value {
        self.events.recv_timeout(READY).expect("server event")
    }

    fn next_within(&self, wait: Duration) -> Value {
        self.events.recv_timeout(wait).expect("server event")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write(root: &Path, rel: &str, content: &[u8], executable: bool) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Runs `lan-peer manifest` over an explicit NUL-separated file list; returns (manifest path, json).
fn manifest(work: &Path, source: &Path, files: &[&str], tag: &str) -> (PathBuf, Value) {
    let list = work.join(format!("{tag}.files0"));
    let mut bytes = Vec::new();
    for f in files {
        bytes.extend_from_slice(f.as_bytes());
        bytes.push(0);
    }
    std::fs::write(&list, bytes).unwrap();
    let (code, value) = run(&["manifest", p(source), "--files-from0", p(&list)]);
    assert_eq!(code, 0, "{value}");
    let out = work.join(format!("{tag}.manifest.json"));
    std::fs::write(&out, serde_json::to_vec(&value).unwrap()).unwrap();
    (out, value)
}

fn hash_of(manifest: &Value, path: &str) -> String {
    manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == path)
        .map(|f| s(f, "iroh_blobs_hash"))
        .unwrap_or_else(|| panic!("{path} in manifest"))
}

/// Every regular file under `dir` as (relative path, bytes, executable). Fails on symlinks.
fn tree(dir: &Path) -> Vec<(String, Vec<u8>, bool)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>, bool)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let meta = std::fs::symlink_metadata(entry.path()).unwrap();
            assert!(!meta.file_type().is_symlink(), "{:?}", entry.path());
            if meta.is_dir() {
                walk(base, &entry.path(), out);
            } else {
                let rel = entry.path().strip_prefix(base).unwrap().to_owned();
                out.push((
                    rel.to_str().unwrap().to_owned(),
                    std::fs::read(entry.path()).unwrap(),
                    meta.permissions().mode() & 0o111 != 0,
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn expected(source: &Path, files: &[&str]) -> Vec<(String, Vec<u8>, bool)> {
    let mut out: Vec<_> = files
        .iter()
        .map(|f| {
            let path = source.join(f);
            let meta = std::fs::metadata(&path).unwrap();
            (
                (*f).to_owned(),
                std::fs::read(&path).unwrap(),
                meta.permissions().mode() & 0o111 != 0,
            )
        })
        .collect();
    out.sort();
    out
}

fn distinct_nonempty(manifest: &Value) -> usize {
    manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["size"] != 0)
        .map(|f| s(f, "iroh_blobs_hash"))
        .collect::<BTreeSet<_>>()
        .len()
}

fn source_event(server: &Server) -> Value {
    // Skip nothing silently: the only events expected here are per-connection outcomes.
    server.next_within(EVENT)
}

// ---------------------------------------------------------------------------------------------
// Two-process loopback acceptance

#[test]
fn push_source_sends_only_missing_blobs_and_materializes_checkouts() {
    let work = tempfile::tempdir().unwrap();
    let server_dir = work.path().join("server");
    let store_dir = work.path().join("server-store");
    let checkouts = work.path().join("server-checkouts");
    let client_dir = work.path().join("client");
    let stranger_dir = work.path().join("stranger");
    let src = work.path().join("src-tree");
    std::fs::create_dir_all(&checkouts).unwrap();

    let (code, client) = run(&["init", "--data-dir", p(&client_dir)]);
    assert_eq!(code, 0, "{client}");
    let (code, stranger) = run(&["init", "--data-dir", p(&stranger_dir)]);
    assert_eq!(code, 0, "{stranger}");
    let client_att = s(&client, "attestation_file");

    // Synthetic fixture tree: duplicate content, an executable and an empty file.
    write(
        &src,
        "src/main.rs",
        b"fn main() { println!(\"v1\"); }\n",
        false,
    );
    write(&src, "run.sh", b"#!/bin/sh\necho synthetic\n", true);
    write(&src, "docs/a.txt", b"abc", false);
    write(&src, "docs/copy-of-a.txt", b"abc", false);
    write(&src, "empty.txt", b"", false);
    let files_v1 = [
        "src/main.rs",
        "run.sh",
        "docs/a.txt",
        "docs/copy-of-a.txt",
        "empty.txt",
    ];

    // Manifest: BLAKE3 hex == iroh-blobs raw hash; the SHA-256 receipt matches the Python capture.
    let (m1_path, m1) = manifest(work.path(), &src, &files_v1, "v1");
    assert_eq!(m1["schema"], "task-manager-local-source-manifest-v1");
    assert_eq!(hash_of(&m1, "docs/a.txt"), BLAKE3_ABC);
    assert_eq!(hash_of(&m1, "empty.txt"), BLAKE3_EMPTY);
    assert_eq!(hash_of(&m1, "docs/a.txt"), Hash::new(b"abc").to_hex());
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/worker_source.py");
    let mut py = Command::new("python3");
    py.arg("-I")
        .arg(&script)
        .arg("--root")
        .arg(&src)
        .env("PYTHONDONTWRITEBYTECODE", "1");
    for f in files_v1 {
        py.args(["--file", f]);
    }
    let py = py.output().expect("python3");
    assert!(
        py.status.success(),
        "{}",
        String::from_utf8_lossy(&py.stdout)
    );
    let py: Value = serde_json::from_slice(&py.stdout).unwrap();
    assert_eq!(py["manifest"]["receipt"], m1["receipt"], "receipt interop");
    for f in py["manifest"]["files"].as_array().unwrap() {
        let rust = m1["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["path"] == f["path"])
            .unwrap();
        for key in ["size", "executable", "sha256"] {
            assert_eq!(f[key], rust[key], "{key} of {}", f["path"]);
        }
    }

    let (server, ready) = Server::start(&[
        "serve",
        "--bind",
        "127.0.0.1:0",
        "--data-dir",
        p(&server_dir),
        "--allow",
        &client_att,
        "--store",
        p(&store_dir),
        "--checkouts",
        p(&checkouts),
    ]);
    assert_eq!(ready["source_transfer"], true, "{ready}");
    let server_id = s(&ready, "endpoint_id");
    let server_att = s(&ready, "attestation_file");
    let addr = s(&ready, "addr");

    let push = |dir: &Path, manifest: &Path, name: &str| {
        run(&[
            "push-source",
            "--peer",
            &server_id,
            "--addr",
            &addr,
            "--data-dir",
            p(dir),
            "--allow",
            &server_att,
            "--source",
            p(&src),
            "--manifest",
            p(manifest),
            "--checkout",
            name,
        ])
    };

    // 1. First transfer: every distinct non-empty blob is sent exactly once (the empty blob is
    //    implicitly present in any iroh-blobs store).
    let (code, out) = push(&client_dir, &m1_path, "v1");
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["status"], "ok");
    assert_eq!(out["response_fresh"], true);
    let first = distinct_nonempty(&m1);
    assert_eq!(first, 3);
    assert_eq!(out["blobs_sent"], first, "{out}");
    assert_eq!(out["blobs_missing"].as_array().unwrap().len(), first);
    let event = source_event(&server);
    assert_eq!(event["event"], "source_materialized", "{event}");
    assert_eq!(event["remote"], client["endpoint_id"]);
    assert_eq!(event["blobs_received"], first);
    let v1 = checkouts.join("v1");
    assert_eq!(event["checkout"], p(&v1));
    assert_eq!(tree(&v1), expected(&src, &files_v1));
    let run_sh = std::fs::metadata(v1.join("run.sh")).unwrap();
    assert_eq!(run_sh.permissions().mode() & 0o777, 0o755);
    let main_rs = std::fs::metadata(v1.join("src/main.rs")).unwrap();
    assert_eq!(main_rs.permissions().mode() & 0o777, 0o644);

    // 2. Edit exactly one file: exactly one blob crosses the wire.
    write(
        &src,
        "src/main.rs",
        b"fn main() { println!(\"v2\"); }\n",
        false,
    );
    let (m2_path, m2) = manifest(work.path(), &src, &files_v1, "v2");
    let (code, out) = push(&client_dir, &m2_path, "v2");
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["blobs_sent"], 1, "{out}");
    assert_eq!(
        out["blobs_missing"],
        serde_json::json!([hash_of(&m2, "src/main.rs")])
    );
    let event = source_event(&server);
    assert_eq!(event["event"], "source_materialized", "{event}");
    assert_eq!(event["blobs_received"], 1);
    assert_eq!(tree(&checkouts.join("v2")), expected(&src, &files_v1));
    assert_eq!(
        std::fs::read(v1.join("src/main.rs")).unwrap(),
        b"fn main() { println!(\"v1\"); }\n"
    );

    // 3. Renames/moves only: nothing new is sent.
    std::fs::create_dir_all(src.join("bin")).unwrap();
    std::fs::rename(src.join("run.sh"), src.join("bin/run.sh")).unwrap();
    std::fs::rename(src.join("docs/a.txt"), src.join("docs/renamed.txt")).unwrap();
    let files_v3 = [
        "src/main.rs",
        "bin/run.sh",
        "docs/renamed.txt",
        "docs/copy-of-a.txt",
        "empty.txt",
    ];
    let (m3_path, _) = manifest(work.path(), &src, &files_v3, "v3");
    let (code, out) = push(&client_dir, &m3_path, "v3");
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["blobs_sent"], 0, "{out}");
    assert_eq!(out["blobs_missing"], serde_json::json!([]));
    let event = source_event(&server);
    assert_eq!(event["blobs_received"], 0, "{event}");
    let v3 = checkouts.join("v3");
    assert_eq!(tree(&v3), expected(&src, &files_v3));
    let moved = std::fs::metadata(v3.join("bin/run.sh")).unwrap();
    assert_eq!(moved.permissions().mode() & 0o777, 0o755);

    // 4. Checkouts are immutable: an existing name is refused, contents unchanged.
    let (code, out) = push(&client_dir, &m1_path, "v3");
    assert_eq!(code, 3, "{out}");
    assert!(s(&out, "reason").contains("Exists"), "{out}");
    let event = source_event(&server);
    assert_eq!(event["event"], "rejected", "{event}");
    assert_eq!(tree(&v3), expected(&src, &files_v3));

    // 5. Same account and valid attestation, but not allowlisted: refused before any transfer.
    let (code, out) = push(&stranger_dir, &m3_path, "stranger");
    assert_eq!(code, 3, "{out}");
    assert_eq!(out["stage"], "receiver");
    assert!(s(&out, "reason").contains("NotAllowlisted"), "{out}");
    let event = source_event(&server);
    assert_eq!(event["event"], "rejected", "{event}");
    assert_eq!(event["remote"], stranger["endpoint_id"]);
    assert!(!checkouts.join("stranger").exists());

    // 6. A path-escaping manifest is refused by the server before any blob is requested.
    let mut evil: Value = serde_json::from_slice(&std::fs::read(&m3_path).unwrap()).unwrap();
    evil["files"][0]["path"] = Value::from("../escape.txt");
    let evil_path = work.path().join("evil.manifest.json");
    std::fs::write(&evil_path, serde_json::to_vec(&evil).unwrap()).unwrap();
    let (code, out) = push(&client_dir, &evil_path, "evil");
    assert_eq!(code, 3, "{out}");
    assert_eq!(out["stage"], "receiver", "{out}");
    assert!(s(&out, "reason").contains("UnsafePath"), "{out}");
    let event = source_event(&server);
    assert_eq!(event["event"], "rejected", "{event}");
    assert!(!work.path().join("escape.txt").exists());
    assert!(!checkouts.join("evil").exists());

    // No temp directories are left behind in the checkout root.
    let names: BTreeSet<String> = std::fs::read_dir(&checkouts)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        names,
        ["v1", "v2", "v3"].iter().map(|n| n.to_string()).collect()
    );
    drop(server);
}

// ---------------------------------------------------------------------------------------------
// In-process: tampered blob

struct Peer {
    key: SecretKey,
    attestation: task_manager_osvauld_foundation::AttestationFile,
    did: String,
    _dir: tempfile::TempDir,
}

fn peer() -> Peer {
    let dir = tempfile::tempdir().unwrap();
    let (vault, did) = open_synthetic_installation(dir.path()).unwrap();
    let key = load_or_create_transport_key(&vault).unwrap();
    let now = now_secs();
    let token = vault
        .with_signer(|s| attest_endpoint(s, &key.public(), now, now + 3600))
        .unwrap()
        .unwrap();
    Peer {
        attestation: task_manager_osvauld_foundation::AttestationFile {
            synthetic: true,
            account_did: did.clone(),
            endpoint_id: key.public().to_string(),
            attestation: token,
        },
        key,
        did,
        _dir: dir,
    }
}

async fn endpoint(key: &SecretKey, alpns: Vec<Vec<u8>>) -> Endpoint {
    lan_endpoint(key.clone(), "127.0.0.1:0".parse().unwrap(), alpns)
        .await
        .unwrap()
}

#[tokio::test]
async fn tampered_blob_is_rejected_and_never_stored_or_materialized() {
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    let checkouts = work.path().join("checkouts");
    std::fs::create_dir_all(&checkouts).unwrap();
    write(&src, "a.txt", b"synthetic content A\n", false);
    write(&src, "b.txt", b"synthetic content B\n", false);
    let files = vec!["a.txt".to_owned(), "b.txt".to_owned()];
    let manifest = source::manifest_from_files(&src, &files, &Limits::default()).unwrap();

    let server = peer();
    let client = peer();
    let mut policy = Policy::new(&server.did, now_secs());
    policy
        .approve_attestation(&client.attestation.attestation)
        .unwrap();
    let store = MemStore::new();
    let limits = Limits::default();
    let server_ep = endpoint(&server.key, vec![SOURCE_ALPN.to_vec()]).await;
    let client_ep = endpoint(&client.key, vec![]).await;

    let (served, pushed) = timeout(STEP, async {
        tokio::join!(
            source::serve_source_one(&server_ep, &policy, &store, &checkouts, &limits),
            source::push_source(
                &client_ep,
                direct_addr(&server_ep).unwrap(),
                Some(client.attestation.attestation.clone()),
                &src,
                &manifest,
                "tampered",
                PushMode::TamperBlobs,
            )
        )
    })
    .await
    .expect("bounded round");
    let err = pushed.expect_err("tampered push must fail");
    assert!(err.contains("HashMismatch"), "{err}");
    match served.expect("receiver io") {
        SourceOutcome::Rejected(reason) => assert!(reason.contains("HashMismatch"), "{reason}"),
        other => panic!("expected rejection, got {other:?}"),
    }
    for f in &manifest.files {
        let hash: Hash = f.iroh_blobs_hash.as_deref().unwrap().parse().unwrap();
        assert!(!store.blobs().has(hash).await.unwrap(), "{}", f.path);
    }
    assert_eq!(std::fs::read_dir(&checkouts).unwrap().count(), 0);

    // The same session shape with honest bytes succeeds against the same store/policy.
    let (served, pushed) = timeout(STEP, async {
        tokio::join!(
            source::serve_source_one(&server_ep, &policy, &store, &checkouts, &limits),
            source::push_source(
                &client_ep,
                direct_addr(&server_ep).unwrap(),
                Some(client.attestation.attestation.clone()),
                &src,
                &manifest,
                "honest",
                PushMode::Honest,
            )
        )
    })
    .await
    .expect("bounded round");
    let report = pushed.expect("honest push");
    assert!(report.response_fresh);
    assert_eq!(report.blobs_sent, 2);
    assert!(matches!(
        served.unwrap(),
        SourceOutcome::Materialized { .. }
    ));
    assert_eq!(
        std::fs::read(checkouts.join("honest/b.txt")).unwrap(),
        b"synthetic content B\n"
    );
    client_ep.close().await;
    server_ep.close().await;
}

// ---------------------------------------------------------------------------------------------
// In-process: materializer path safety

fn entry(path: &str, content: &[u8], executable: bool) -> ManifestFile {
    use sha2::{Digest, Sha256};
    ManifestFile {
        path: path.to_owned(),
        size: content.len() as u64,
        executable,
        sha256: Sha256::digest(content)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        iroh_blobs_hash: Some(Hash::new(content).to_hex()),
    }
}

/// Builds a manifest WITHOUT validation (sorted as given; receipt recomputed).
fn raw_manifest(files: Vec<ManifestFile>) -> Manifest {
    Manifest::from_files_unchecked(files)
}

#[tokio::test]
async fn materializer_refuses_path_escapes_and_symlinks() {
    let work = tempfile::tempdir().unwrap();
    let root = work.path().join("checkouts");
    let outside = work.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let store = MemStore::new();
    store.add_bytes(b"payload".to_vec()).await.unwrap();
    let limits = Limits::default();

    let unsafe_paths = [
        "../escape",
        "/etc/escape",
        "a/../../escape",
        "a/./b",
        "a//b",
        "a/",
        "",
        ".",
        "..",
        "a\\..\\b",
        "a/\u{0}b",
        "a/\nb",
    ];
    for bad in unsafe_paths {
        let m = raw_manifest(vec![entry(bad, b"payload", false)]);
        let err = source::materialize(&store, &root, "bad", &m, &limits)
            .await
            .expect_err(bad);
        assert!(
            matches!(err, SourceError::UnsafePath(_)),
            "{bad:?}: {err:?}"
        );
    }

    // Duplicates and file/directory conflicts.
    for files in [
        vec![entry("a", b"payload", false), entry("a", b"payload", false)],
        vec![
            entry("a", b"payload", false),
            entry("a/b", b"payload", false),
        ],
    ] {
        let m = raw_manifest(files);
        let err = source::materialize(&store, &root, "bad", &m, &limits)
            .await
            .expect_err("conflict");
        assert!(matches!(err, SourceError::UnsafePath(_)), "{err:?}");
    }

    // Unsafe checkout names.
    let ok = raw_manifest(vec![entry("x.txt", b"payload", true)]);
    for name in ["..", ".", "", "a/b", "/abs", ".hidden", "x\u{0}"] {
        let err = source::materialize(&store, &root, name, &ok, &limits)
            .await
            .expect_err(name);
        assert!(
            matches!(err, SourceError::UnsafePath(_)),
            "{name:?}: {err:?}"
        );
    }

    // A pre-existing symlink at the checkout name is never written through.
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    let err = source::materialize(&store, &root, "link", &ok, &limits)
        .await
        .expect_err("symlink destination");
    assert!(matches!(err, SourceError::Exists(_)), "{err:?}");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);

    // A wrong SHA-256 is caught by post-write verification; nothing is left behind.
    let mut wrong = entry("x.txt", b"payload", false);
    wrong.sha256 = "0".repeat(64);
    let err = source::materialize(&store, &root, "wrong", &raw_manifest(vec![wrong]), &limits)
        .await
        .expect_err("sha mismatch");
    assert!(matches!(err, SourceError::HashMismatch(_)), "{err:?}");

    // A blob absent from the store is refused.
    let absent = raw_manifest(vec![entry("x.txt", b"not in store", false)]);
    let err = source::materialize(&store, &root, "absent", &absent, &limits)
        .await
        .expect_err("absent");
    assert!(matches!(err, SourceError::MissingBlob(_)), "{err:?}");

    // Only the symlink remains: no temp dirs, no partial checkouts.
    let names: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["link"]);
    assert!(!work.path().join("escape").exists());

    // Positive control with nested dirs and modes.
    let good = raw_manifest(vec![
        entry("bin/tool", b"payload", true),
        entry("x.txt", b"payload", false),
    ]);
    let out = source::materialize(&store, &root, "good", &good, &limits)
        .await
        .unwrap();
    assert_eq!(out, root.join("good"));
    let tool = std::fs::metadata(out.join("bin/tool")).unwrap();
    assert_eq!(tool.permissions().mode() & 0o777, 0o755);
    let x = std::fs::metadata(out.join("x.txt")).unwrap();
    assert_eq!(x.permissions().mode() & 0o777, 0o644);
}

#[test]
fn manifest_source_reader_refuses_symlinks_and_escapes() {
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(work.path().join("secret.txt"), b"outside").unwrap();
    std::os::unix::fs::symlink(work.path().join("secret.txt"), src.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(work.path(), src.join("dirlink")).unwrap();
    write(&src, "ok.txt", b"ok", false);
    let limits = Limits::default();
    for bad in [
        "link.txt",
        "dirlink/secret.txt",
        "../secret.txt",
        "/etc/hostname",
    ] {
        let err = source::manifest_from_files(&src, &[bad.to_owned()], &limits).expect_err(bad);
        assert!(
            matches!(err, SourceError::UnsafePath(_) | SourceError::Io(_)),
            "{bad}: {err:?}"
        );
    }
    let m = source::manifest_from_files(&src, &["ok.txt".to_owned()], &limits).unwrap();
    assert_eq!(m.files.len(), 1);
    assert_eq!(
        source::parse_files0(b"b\0a\0").unwrap(),
        vec!["b".to_owned(), "a".to_owned()]
    );
    assert!(source::parse_files0(b"a\0a\0").is_err(), "duplicates");
    assert!(source::parse_files0(b"a\0\0b").is_err(), "empty entry");
}
