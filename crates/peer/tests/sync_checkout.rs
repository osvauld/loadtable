//! Stable-checkout sync (ported with the code from experiments/osvauld-foundation): the receiver
//! updates `CHECKOUTS/<name>` IN PLACE from a manifest; unchanged files (same BLAKE3, size, exec
//! bit) keep their mtime so Cargo stays incremental; changed/new files are replaced atomically
//! (no-follow temp + rename); files absent from the manifest are pruned with the SAME rules as
//! `scripts/offload_agent.py` (parity tested against the Python agent on one fixture).
//! In-process; synthetic fixtures only. The over-the-wire path is tested end to end by the
//! coordinator's `tests/peer.rs` (two real daemons).

use std::collections::BTreeSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use iroh_blobs::Hash;
use iroh_blobs::api::Store;
use iroh_blobs::store::mem::MemStore;
use task_manager_peer::source::{self, Limits, Manifest, SourceError};
use task_manager_peer::{prune, sync};

const OLD: Duration = Duration::from_secs(1_000_000_000);

fn write(root: &Path, rel: &str, content: &[u8], executable: bool) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn age(path: &Path) {
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + OLD).unwrap();
}

fn mtime(path: &Path) -> SystemTime {
    std::fs::symlink_metadata(path).unwrap().modified().unwrap()
}

/// Manifest of `files` under `src`, with every blob imported into `store` (as a received push).
async fn manifest_into(store: &Store, src: &Path, files: &[&str]) -> Manifest {
    let paths: Vec<String> = files.iter().map(|f| (*f).to_owned()).collect();
    let manifest = source::manifest_from_files(src, &paths, &Limits::default()).unwrap();
    for f in &manifest.files {
        let bytes = std::fs::read(src.join(&f.path)).unwrap();
        let hash: Hash = f.iroh_blobs_hash.as_deref().unwrap().parse().unwrap();
        source::import_verified(store, hash, bytes).await.unwrap();
    }
    manifest
}

/// Every entry under `dir` (files, dirs, symlinks) with content/target and mode, sorted.
fn snapshot(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                out.push(format!(
                    "L {rel} -> {}",
                    std::fs::read_link(&path).unwrap().display()
                ));
            } else if meta.is_dir() {
                out.push(format!("D {rel}"));
                walk(base, &path, out);
            } else {
                out.push(format!(
                    "F {rel} {:o} {}",
                    meta.permissions().mode() & 0o777,
                    String::from_utf8_lossy(&std::fs::read(&path).unwrap())
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[tokio::test]
async fn sync_updates_in_place_preserving_unchanged_mtimes_and_outputs() {
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    let root = work.path().join("checkouts");
    std::fs::create_dir_all(&root).unwrap();
    let store = MemStore::new();
    let limits = Limits::default();

    write(
        &src,
        "Cargo.toml",
        b"[package]\nname = \"synthetic\"\n",
        false,
    );
    write(&src, "src/lib.rs", b"pub fn v1() {}\n", false);
    write(&src, "src/main.rs", b"fn main() {}\n", false);
    write(&src, "run.sh", b"#!/bin/sh\necho synthetic\n", true);
    write(
        &src,
        "crate/Cargo.toml",
        b"[package]\nname = \"c\"\n",
        false,
    );
    write(&src, "docs/a.txt", b"abc", false);
    write(&src, "empty.txt", b"", false);
    let v1 = [
        "Cargo.toml",
        "src/lib.rs",
        "src/main.rs",
        "run.sh",
        "crate/Cargo.toml",
        "docs/a.txt",
        "empty.txt",
    ];
    let m1 = manifest_into(&store, &src, &v1).await;

    // 1. First sync creates the checkout and writes every file.
    let r = sync::sync_checkout(&store, &root, "c1", &m1, &limits)
        .await
        .unwrap();
    let co = root.join("c1");
    assert_eq!(r.checkout, co);
    assert!(r.created);
    assert_eq!((r.files, r.written, r.unchanged), (7, 7, 0), "{r:?}");
    assert!(r.deleted.is_empty());
    assert_eq!(
        std::fs::read(co.join("src/lib.rs")).unwrap(),
        b"pub fn v1() {}\n"
    );
    assert_eq!(
        std::fs::metadata(co.join("run.sh")).unwrap().mode() & 0o777,
        0o755
    );

    // Worker-generated outputs and state that must survive every sync.
    write(&co, "target/debug/app", b"built", true);
    write(&co, "crate/target/x.rlib", b"rlib", false);
    write(&co, "android/build/out.apk", b"apk", false);
    write(&co, "android/app/build/out.dex", b"dex", false);
    write(&co, ".env", b"SYNTHETIC=1", false);
    write(&co, "build.log", b"log", false);
    write(&co, "node_modules/m/index.js", b"js", false);
    // Remote-only stale source that must be pruned.
    write(&co, "stale.txt", b"stale", false);
    write(&co, "stale-dir/deep/x.txt", b"stale", false);

    // Everything ages; only rewritten files may get a new mtime.
    for rel in v1 {
        age(&co.join(rel));
    }

    // 2. A no-change sync writes nothing and keeps every mtime and inode.
    let inodes: Vec<u64> = v1
        .iter()
        .map(|f| std::fs::metadata(co.join(f)).unwrap().ino())
        .collect();
    let r = sync::sync_checkout(&store, &root, "c1", &m1, &limits)
        .await
        .unwrap();
    assert!(!r.created);
    assert_eq!((r.written, r.unchanged, r.mode_fixed), (0, 7, 0), "{r:?}");
    assert_eq!(
        r.deleted,
        vec![
            "stale-dir/".to_owned(),
            "stale-dir/deep/".to_owned(),
            "stale-dir/deep/x.txt".to_owned(),
            "stale.txt".to_owned()
        ]
    );
    for (rel, ino) in v1.iter().zip(&inodes) {
        assert_eq!(mtime(&co.join(rel)), SystemTime::UNIX_EPOCH + OLD, "{rel}");
        assert_eq!(
            std::fs::metadata(co.join(rel)).unwrap().ino(),
            *ino,
            "{rel}"
        );
    }
    for kept in [
        "target/debug/app",
        "crate/target/x.rlib",
        "android/build/out.apk",
        "android/app/build/out.dex",
        ".env",
        "build.log",
        "node_modules/m/index.js",
    ] {
        assert!(co.join(kept).is_file(), "{kept} must be preserved");
    }

    // 3. Edit one file, delete one, add one, flip one exec bit, drift one mode (0664).
    write(&src, "src/lib.rs", b"pub fn v2() {}\n", false);
    std::fs::remove_file(src.join("docs/a.txt")).unwrap();
    write(&src, "src/new.rs", b"// new\n", false);
    std::fs::set_permissions(src.join("run.sh"), std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(
        co.join("src/main.rs"),
        std::fs::Permissions::from_mode(0o664),
    )
    .unwrap();
    age(&co.join("src/main.rs"));
    let v2 = [
        "Cargo.toml",
        "src/lib.rs",
        "src/main.rs",
        "src/new.rs",
        "run.sh",
        "crate/Cargo.toml",
        "empty.txt",
    ];
    let m2 = manifest_into(&store, &src, &v2).await;
    let old_lib_ino = std::fs::metadata(co.join("src/lib.rs")).unwrap().ino();
    let r = sync::sync_checkout(&store, &root, "c1", &m2, &limits)
        .await
        .unwrap();
    assert_eq!((r.written, r.unchanged, r.mode_fixed), (3, 4, 1), "{r:?}");
    assert_eq!(r.deleted, vec!["docs/".to_owned(), "docs/a.txt".to_owned()]);
    assert_eq!(
        std::fs::read(co.join("src/lib.rs")).unwrap(),
        b"pub fn v2() {}\n"
    );
    assert_ne!(
        std::fs::metadata(co.join("src/lib.rs")).unwrap().ino(),
        old_lib_ino,
        "replaced by rename, not rewritten in place"
    );
    assert_eq!(
        std::fs::metadata(co.join("run.sh")).unwrap().mode() & 0o777,
        0o644
    );
    let main = std::fs::metadata(co.join("src/main.rs")).unwrap();
    assert_eq!(main.mode() & 0o777, 0o644, "mode normalized");
    assert_eq!(
        main.modified().unwrap(),
        SystemTime::UNIX_EPOCH + OLD,
        "mode fix keeps mtime"
    );
    for rel in ["Cargo.toml", "crate/Cargo.toml", "empty.txt"] {
        assert_eq!(mtime(&co.join(rel)), SystemTime::UNIX_EPOCH + OLD, "{rel}");
    }
    assert!(co.join("target/debug/app").is_file());
    assert!(!co.join("docs").exists());

    // No temp files are left anywhere in the checkout or the root.
    let leftovers: Vec<_> = snapshot(&root)
        .into_iter()
        .filter(|e| e.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test]
async fn sync_never_follows_symlinks_and_refuses_excluded_paths() {
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    let root = work.path().join("checkouts");
    let outside = work.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("victim.txt"), b"untouched").unwrap();
    let store = MemStore::new();
    let limits = Limits::default();
    write(&src, "a.txt", b"A", false);
    write(&src, "sub/b.txt", b"B", false);
    let m = manifest_into(&store, &src, &["a.txt", "sub/b.txt"]).await;

    // The checkout name itself is a symlink: refused, nothing written through it.
    std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
    let err = sync::sync_checkout(&store, &root, "linked", &m, &limits)
        .await
        .expect_err("symlinked checkout");
    assert!(matches!(err, SourceError::UnsafePath(_)), "{err:?}");
    assert_eq!(snapshot(&outside), vec!["F victim.txt 644 untouched"]);

    // Inside the checkout: a symlink at a manifest file path and a symlinked directory on a
    // manifest path are replaced by real entries; their targets are never written.
    let co = root.join("c");
    std::fs::create_dir_all(&co).unwrap();
    std::os::unix::fs::symlink(outside.join("victim.txt"), co.join("a.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, co.join("sub")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", co.join("stale-link")).unwrap();
    let r = sync::sync_checkout(&store, &root, "c", &m, &limits)
        .await
        .unwrap();
    assert_eq!(r.written, 2, "{r:?}");
    assert_eq!(snapshot(&outside), vec!["F victim.txt 644 untouched"]);
    assert_eq!(
        snapshot(&co),
        vec!["D sub", "F a.txt 644 A", "F sub/b.txt 644 B"]
    );
    assert!(r.deleted.contains(&"stale-link".to_owned()), "{r:?}");
    assert!(r.deleted.contains(&"sub".to_owned()), "{r:?}");

    // Manifests naming excluded/output paths are refused before anything is touched.
    for bad in ["target/x.rs", ".env", "secrets/k.txt", "x.log", "build/o"] {
        write(&src, bad, b"X", false);
        let m = manifest_into(&store, &src, &[bad]).await;
        let err = sync::sync_checkout(&store, &root, "c", &m, &limits)
            .await
            .expect_err(bad);
        assert!(matches!(err, SourceError::UnsafePath(_)), "{bad}: {err:?}");
    }
    assert_eq!(
        snapshot(&co),
        vec!["D sub", "F a.txt 644 A", "F sub/b.txt 644 B"]
    );
    // Path through a marker-adjacent output dir is refused too (crate/target beside Cargo.toml).
    write(&co, "crate/Cargo.toml", b"[package]\n", false);
    write(&src, "crate/Cargo.toml", b"[package]\n", false);
    write(&src, "crate/target/x.rs", b"X", false);
    let m = manifest_into(&store, &src, &["crate/Cargo.toml", "crate/target/x.rs"]).await;
    let err = sync::sync_checkout(&store, &root, "c", &m, &limits)
        .await
        .expect_err("marker-adjacent output");
    assert!(matches!(err, SourceError::UnsafePath(_)), "{err:?}");
    assert!(!co.join("crate/target").exists());

    // Unsafe names are refused as for immutable checkouts.
    for name in ["..", ".hidden", "a/b", ""] {
        let err = sync::sync_checkout(&store, &root, name, &m, &limits)
            .await
            .expect_err(name);
        assert!(matches!(err, SourceError::UnsafePath(_)), "{name}: {err:?}");
    }
}

#[tokio::test]
async fn sync_is_serialized_per_checkout_by_a_lock() {
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    let root = work.path().join("checkouts");
    std::fs::create_dir_all(&root).unwrap();
    let store = MemStore::new();
    let limits = Limits::default();
    write(&src, "a.txt", b"A", false);
    let m = manifest_into(&store, &src, &["a.txt"]).await;

    let held = sync::lock_checkout(&root, "c", Duration::from_secs(1)).unwrap();
    let err =
        sync::sync_checkout_waiting(&store, &root, "c", &m, &limits, Duration::from_millis(300))
            .await
            .expect_err("lock is held");
    assert!(matches!(err, SourceError::Busy(_)), "{err:?}");
    assert!(!root.join("c").exists(), "nothing written while locked");
    // A different checkout is independent.
    sync::sync_checkout(&store, &root, "other", &m, &limits)
        .await
        .unwrap();
    drop(held);
    sync::sync_checkout(&store, &root, "c", &m, &limits)
        .await
        .unwrap();
    // The lock file is not a checkout name and is never pruned into a checkout.
    let names: BTreeSet<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| !n.starts_with('.'))
        .collect();
    assert_eq!(
        names,
        ["c", "other"].iter().map(|s| s.to_string()).collect()
    );
}

// ---------------------------------------------------------------------------------------------
// Prune parity with scripts/offload_agent.py

fn scripts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts")
}

#[test]
fn prune_rule_constants_match_worker_source_py() {
    let code = "import json, worker_source as w, offload_agent as a\n\
                print(json.dumps({'parts': sorted(w.FORBIDDEN_PARTS),\n\
                'top': sorted(w.TOP_LEVEL_OUTPUT_PARTS),\n\
                'marked': {k: list(v) for k, v in sorted(w.MARKED_OUTPUT_DIRS.items())},\n\
                'suffixes': list(w.FORBIDDEN_SUFFIXES), 'preserved': list(a.PRESERVED)}))";
    let out = Command::new("python3")
        .args(["-B", "-c", code])
        .env("PYTHONPATH", scripts())
        .output()
        .expect("python3");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let py: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut parts: Vec<&str> = prune::FORBIDDEN_PARTS.to_vec();
    parts.sort();
    let mut top: Vec<&str> = prune::TOP_LEVEL_OUTPUT_PARTS.to_vec();
    top.sort();
    assert_eq!(py["parts"], serde_json::json!(parts));
    assert_eq!(py["top"], serde_json::json!(top));
    let marked: serde_json::Map<String, serde_json::Value> = prune::MARKED_OUTPUT_DIRS
        .iter()
        .map(|(k, v)| ((*k).to_owned(), serde_json::json!(v)))
        .collect();
    assert_eq!(py["marked"], serde_json::Value::Object(marked));
    assert_eq!(py["suffixes"], serde_json::json!(prune::FORBIDDEN_SUFFIXES));
    assert_eq!(py["preserved"], serde_json::json!(prune::PRESERVED));
}

/// One fixture covering every rule; built twice.
fn prune_fixture(dir: &Path) {
    use std::os::unix::ffi::OsStrExt;
    for (rel, content) in [
        ("Cargo.toml", "kept: in manifest"),
        ("src/lib.rs", "kept: in manifest"),
        ("src/stale.rs", "pruned"),
        (
            "src/build/mod.rs",
            "pruned: build is source below the root without a marker",
        ),
        ("stale-dir/a/b.txt", "pruned with its dirs"),
        ("mixed/stale.txt", "pruned"),
        ("mixed/debug.log", "kept: excluded suffix, so mixed/ stays"),
        ("target/debug/app", "kept: top-level output"),
        ("crate/Cargo.toml", "kept: in manifest"),
        ("crate/target/x", "kept: marker-adjacent output"),
        ("lib/target/y.rs", "pruned: no Cargo.toml beside lib/target"),
        ("app/build.gradle", "kept: in manifest"),
        ("app/build/o.dex", "kept: marker-adjacent output"),
        ("android/build/o", "kept: preserved"),
        ("android/app/build/o", "kept: preserved"),
        (".env", "kept: excluded"),
        (".env.local", "kept: excluded"),
        ("secrets/x.txt", "kept: excluded dir"),
        ("node_modules/m/i.js", "kept: excluded dir"),
        (".cargo/config.toml", "kept: excluded dir"),
        ("deploy/id_ed25519", "kept: excluded prefix"),
        ("server.PEM", "kept: excluded suffix (case-insensitive)"),
        ("Token", "kept: excluded name (case-insensitive)"),
        ("Build/x", "kept: top-level output (case-insensitive)"),
        (
            "ctl\u{85}name.txt",
            "pruned: C1 controls are allowed by the Python rule",
        ),
        (
            "ctl\u{7f}name.txt",
            "kept: DEL is refused by the Python rule",
        ),
    ] {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    std::fs::create_dir_all(dir.join("empty-stale/inner")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", dir.join("stale-link")).unwrap();
    std::os::unix::fs::symlink("src", dir.join("dir-link")).unwrap();
    std::os::unix::fs::symlink("/", dir.join("stray-root-link")).unwrap();
    std::os::unix::fs::symlink("/nonexistent", dir.join("android/app/stray-link")).unwrap();
    // Non-UTF-8 name: refused by the Python rule, so kept.
    let raw = std::ffi::OsStr::from_bytes(b"bad-\xff.txt");
    std::fs::write(dir.join(raw), "kept: non-UTF-8").unwrap();
}

const PRUNE_KEEP: [&str; 5] = [
    "Cargo.toml",
    "src/lib.rs",
    "crate/Cargo.toml",
    "app/build.gradle",
    "src/missing-from-worker.rs",
];

#[test]
fn prune_matches_python_offload_agent_on_one_fixture() {
    let work = tempfile::tempdir().unwrap();
    // Python agent: requires HOME/offload/checkouts/<alnum>.
    let home = work.path().join("home");
    let py_dir = home.join("offload/checkouts/abc123");
    std::fs::create_dir_all(&py_dir).unwrap();
    prune_fixture(&py_dir);
    let rust_dir = work.path().join("rust/c");
    std::fs::create_dir_all(&rust_dir).unwrap();
    prune_fixture(&rust_dir);
    assert_eq!(snapshot(&py_dir), snapshot(&rust_dir), "same fixture");

    let manifest = serde_json::json!({"files": PRUNE_KEEP.iter().map(|p| serde_json::json!({
        "path": p, "type": "file", "size": 0, "executable": false, "sha256": "0".repeat(64)
    })).collect::<Vec<_>>()});
    let mut child = Command::new("python3")
        .args(["-I", "-B"])
        .arg(scripts().join("offload_agent.py"))
        .args(["apply", "--dir", "offload/checkouts/abc123"])
        .env("HOME", &home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("python3");
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(manifest.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let py: serde_json::Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();

    let keep: BTreeSet<String> = PRUNE_KEEP.iter().map(|s| (*s).to_owned()).collect();
    let report = prune::prune_dir(&rust_dir, &keep).unwrap();

    let py_pruned: Vec<String> = py["pruned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(report.pruned, py_pruned, "pruned lists");
    assert_eq!(
        report.kept_non_source as u64,
        py["kept_non_source"].as_u64().unwrap(),
        "kept counts"
    );
    assert_eq!(snapshot(&rust_dir), snapshot(&py_dir), "resulting trees");
    // Sanity: the fixture actually exercised both outcomes.
    assert!(report.pruned.contains(&"src/build/mod.rs".to_owned()));
    assert!(report.pruned.contains(&"lib/target/y.rs".to_owned()));
    assert!(report.pruned.contains(&"stale-link".to_owned()));
    assert!(report.pruned.contains(&"empty-stale/inner/".to_owned()));
    assert!(rust_dir.join("crate/target/x").exists());
    assert!(rust_dir.join("mixed/debug.log").exists());
}
