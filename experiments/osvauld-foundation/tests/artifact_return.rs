//! Phase 2b step 2: artifact return over iroh-blobs. The client names relative paths (files or
//! directories) under a worker checkout; the worker validates them (inside the checkout, no
//! symlinks, the same limits), imports them into its store under `task-manager/artifact/<hex>`
//! tags and answers with a SIGNED manifest; the client asks only for hashes its own store lacks,
//! BLAKE3-verifies each blob, and materializes into a NEW destination. In-process, loopback,
//! synthetic fixtures only.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use iroh::{Endpoint, SecretKey};
use iroh_blobs::Hash;
use iroh_blobs::store::mem::MemStore;
use task_manager_osvauld_foundation::artifact::{
    self, ARTIFACT_ALPN, ArtifactOutcome, FetchReport, ServeMode,
};
use task_manager_osvauld_foundation::source::{Limits, SourceError};
use task_manager_osvauld_foundation::{
    Policy, attest_endpoint, direct_addr, lan_endpoint, load_or_create_transport_key, now_secs,
    open_synthetic_installation,
};
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(60);

fn write(root: &Path, rel: &str, content: &[u8], executable: bool) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn tree(dir: &Path) -> Vec<(String, Vec<u8>, u32)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>, u32)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            assert!(!meta.file_type().is_symlink(), "{path:?}");
            if meta.is_dir() {
                walk(base, &path, out);
            } else {
                out.push((
                    path.strip_prefix(base)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                    std::fs::read(&path).unwrap(),
                    meta.permissions().mode() & 0o777,
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// A worker checkout with build outputs: an executable, a Cargo-style hardlink, a symlink inside
/// a requested directory (skipped), duplicate content and an empty file.
fn worker_checkout(root: &Path) {
    let co = root.join("co");
    write(&co, "Cargo.toml", b"[package]\n", false);
    write(&co, "target/debug/app", b"synthetic binary v1", true);
    write(&co, "target/debug/deps/libx.rlib", b"rlib bytes", false);
    write(&co, "target/debug/deps/copy.rlib", b"rlib bytes", false);
    write(&co, "target/debug/deps/empty.d", b"", false);
    std::fs::hard_link(
        co.join("target/debug/app"),
        co.join("target/debug/deps/app-0123"),
    )
    .unwrap();
    std::os::unix::fs::symlink("/etc/hostname", co.join("target/debug/deps/escape")).unwrap();
    std::os::unix::fs::symlink("app", co.join("target/debug/app-link")).unwrap();
}

#[tokio::test]
async fn export_validates_paths_and_never_follows_symlinks() {
    let work = tempfile::tempdir().unwrap();
    let root = work.path().join("checkouts");
    worker_checkout(&root);
    let store = MemStore::new();
    let limits = Limits::default();
    let p = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

    let ok = artifact::export(
        &store,
        &root,
        "co",
        &p(&["target/debug/app", "target/debug/deps"]),
        &limits,
    )
    .await
    .unwrap();
    let paths: Vec<&str> = ok.manifest.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "target/debug/app",
            "target/debug/deps/app-0123",
            "target/debug/deps/copy.rlib",
            "target/debug/deps/empty.d",
            "target/debug/deps/libx.rlib",
        ]
    );
    assert_eq!(ok.skipped, vec!["target/debug/deps/escape".to_owned()]);
    let app = &ok.manifest.files[0];
    assert!(app.executable);
    assert_eq!(
        app.iroh_blobs_hash.as_deref(),
        Some(Hash::new(b"synthetic binary v1").to_hex().as_str())
    );
    // Imported and tagged in the worker store.
    assert!(store.blobs().has(Hash::new(b"rlib bytes")).await.unwrap());
    let tag = format!(
        "task-manager/artifact/{}",
        Hash::new(b"rlib bytes").to_hex()
    );
    let info = store.tags().get(&tag).await.unwrap().expect("artifact tag");
    assert_eq!(info.hash, Hash::new(b"rlib bytes"));

    // Overlapping requests are deduplicated.
    let both = artifact::export(
        &store,
        &root,
        "co",
        &p(&["target/debug/deps", "target/debug/deps/libx.rlib"]),
        &limits,
    )
    .await
    .unwrap();
    assert_eq!(both.manifest.files.len(), 4);

    for (bad, what) in [
        ("../outside", "escape"),
        ("/etc/hostname", "absolute"),
        ("target/debug/app-link", "requested symlink"),
        ("target/debug/deps/escape", "requested symlink"),
        ("", "empty"),
        ("target/./debug", "dot"),
    ] {
        let err = artifact::export(&store, &root, "co", &p(&[bad]), &limits)
            .await
            .expect_err(what);
        assert!(matches!(err, SourceError::UnsafePath(_)), "{what}: {err:?}");
    }
    let err = artifact::export(&store, &root, "co", &p(&["target/nope"]), &limits)
        .await
        .expect_err("missing");
    assert!(matches!(err, SourceError::Io(_)), "{err:?}");
    for name in ["..", ".hidden", "a/b"] {
        let err = artifact::export(&store, &root, name, &p(&["Cargo.toml"]), &limits)
            .await
            .expect_err(name);
        assert!(matches!(err, SourceError::UnsafePath(_)), "{name}: {err:?}");
    }
    // A symlinked checkout is refused.
    std::os::unix::fs::symlink(root.join("co"), root.join("linked")).unwrap();
    let err = artifact::export(&store, &root, "linked", &p(&["Cargo.toml"]), &limits)
        .await
        .expect_err("symlinked checkout");
    assert!(matches!(err, SourceError::UnsafePath(_)), "{err:?}");
    // Limits apply exactly as for source.
    let tight = Limits {
        max_file_bytes: 5,
        ..Limits::default()
    };
    let err = artifact::export(&store, &root, "co", &p(&["target/debug/app"]), &tight)
        .await
        .expect_err("file limit");
    assert!(matches!(err, SourceError::Limit(_)), "{err:?}");
    let few = Limits {
        max_files: 2,
        ..Limits::default()
    };
    let err = artifact::export(&store, &root, "co", &p(&["target/debug/deps"]), &few)
        .await
        .expect_err("count limit");
    assert!(matches!(err, SourceError::Limit(_)), "{err:?}");
}

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
async fn fetch_transfers_only_missing_blobs_and_refuses_tampering() {
    let work = tempfile::tempdir().unwrap();
    let root = work.path().join("checkouts");
    worker_checkout(&root);
    let out = work.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let limits = Limits::default();

    let server = peer();
    let client = peer();
    let stranger = peer();
    let mut policy = Policy::new(&server.did, now_secs());
    policy
        .approve_attestation(&client.attestation.attestation)
        .unwrap();
    let server_store = MemStore::new();
    let client_store = MemStore::new();
    let server_ep = endpoint(&server.key, vec![ARTIFACT_ALPN.to_vec()]).await;
    let client_ep = endpoint(&client.key, vec![]).await;
    let stranger_ep = endpoint(&stranger.key, vec![]).await;
    let wanted: Vec<String> = ["target/debug/app", "target/debug/deps"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();

    let round = |dest: &'static str, mode: ServeMode, who: &'static str| {
        let (server_ep, policy, server_store, root, limits) =
            (&server_ep, &policy, &server_store, &root, &limits);
        let (ep, att) = if who == "stranger" {
            (&stranger_ep, stranger.attestation.attestation.clone())
        } else {
            (&client_ep, client.attestation.attestation.clone())
        };
        let (client_store, wanted, dest) = (&client_store, &wanted, out.join(dest));
        async move {
            timeout(STEP, async {
                tokio::join!(
                    artifact::serve_artifact_one_as(
                        server_ep,
                        policy,
                        server_store,
                        root,
                        limits,
                        mode
                    ),
                    artifact::fetch_artifacts(
                        ep,
                        direct_addr(server_ep).unwrap(),
                        Some(att),
                        "co",
                        wanted,
                        client_store,
                        &dest,
                    )
                )
            })
            .await
            .expect("bounded round")
        }
    };

    // 1. First fetch: every distinct non-empty blob crosses once.
    let (served, fetched) = round("first", ServeMode::Honest, "client").await;
    let report: FetchReport = fetched.expect("first fetch");
    assert!(report.response_fresh);
    assert_eq!(report.manifest.files.len(), 5);
    let distinct = [
        b"synthetic binary v1".len() as u64,
        b"rlib bytes".len() as u64,
    ];
    assert_eq!(report.blobs_received, 2, "{report:?}");
    assert_eq!(report.bytes_received, distinct.iter().sum::<u64>());
    assert_eq!(report.skipped, vec!["target/debug/deps/escape".to_owned()]);
    assert!(matches!(
        served.unwrap(),
        ArtifactOutcome::Served { blobs_sent: 2, .. }
    ));
    let first = tree(&out.join("first"));
    assert_eq!(
        first,
        vec![
            (
                "target/debug/app".into(),
                b"synthetic binary v1".to_vec(),
                0o755
            ),
            (
                "target/debug/deps/app-0123".into(),
                b"synthetic binary v1".to_vec(),
                0o755
            ),
            (
                "target/debug/deps/copy.rlib".into(),
                b"rlib bytes".to_vec(),
                0o644
            ),
            ("target/debug/deps/empty.d".into(), Vec::new(), 0o644),
            (
                "target/debug/deps/libx.rlib".into(),
                b"rlib bytes".to_vec(),
                0o644
            ),
        ]
    );

    // 2. Repeated fetch of unchanged artifacts: ZERO blob bytes, same tree.
    let (served, fetched) = round("second", ServeMode::Honest, "client").await;
    let report = fetched.expect("second fetch");
    assert_eq!((report.blobs_received, report.bytes_received), (0, 0));
    assert!(report.blobs_missing.is_empty());
    assert!(matches!(
        served.unwrap(),
        ArtifactOutcome::Served {
            blobs_sent: 0,
            bytes_sent: 0,
            ..
        }
    ));
    assert_eq!(tree(&out.join("second")), first);

    // 3. One rebuilt artifact: one blob. (Replace by rename, as a linker would.)
    let co = root.join("co");
    std::fs::remove_file(co.join("target/debug/app")).unwrap();
    write(&co, "target/debug/app", b"synthetic binary v2", true);
    let (_, fetched) = round("third", ServeMode::Honest, "client").await;
    let report = fetched.expect("third fetch");
    assert_eq!(report.blobs_received, 1);
    assert_eq!(
        std::fs::read(out.join("third/target/debug/app")).unwrap(),
        b"synthetic binary v2"
    );

    // 4. The destination must be NEW: refused locally before any connection.
    let err = artifact::fetch_artifacts(
        &client_ep,
        direct_addr(&server_ep).unwrap(),
        Some(client.attestation.attestation.clone()),
        "co",
        &wanted,
        &client_store,
        &out.join("first"),
    )
    .await
    .expect_err("existing destination");
    assert!(err.contains("Exists"), "{err}");

    // 5. Same account but not allowlisted: refused before a challenge.
    let (served, fetched) = round("stranger", ServeMode::Honest, "stranger").await;
    let err = fetched.expect_err("stranger");
    assert!(err.contains("NotAllowlisted"), "{err}");
    assert!(matches!(served.unwrap(), ArtifactOutcome::Rejected(_)));
    assert!(!out.join("stranger").exists());

    // 6. A worker that flips bytes under the right hash: rejected, nothing stored or written.
    std::fs::remove_file(co.join("target/debug/app")).unwrap();
    write(&co, "target/debug/app", b"synthetic binary v3", true);
    let (_, fetched) = round("tampered", ServeMode::TamperBlobs, "client").await;
    let err = fetched.expect_err("tampered");
    assert!(err.contains("HashMismatch"), "{err}");
    assert!(!out.join("tampered").exists());
    assert!(
        !client_store
            .blobs()
            .has(Hash::new(b"synthetic binary v3"))
            .await
            .unwrap()
    );

    // 7. A worker that signs a manifest naming paths outside the request: rejected.
    let (_, fetched) = round("extra", ServeMode::ExtraPath, "client").await;
    let err = fetched.expect_err("extra path");
    assert!(err.contains("UnrequestedPath"), "{err}");
    assert!(!out.join("extra").exists());

    // No temp directories left in the destination root.
    let names: BTreeSet<String> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        names,
        ["first", "second", "third"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    );
    client_ep.close().await;
    stranger_ep.close().await;
    server_ep.close().await;
}
