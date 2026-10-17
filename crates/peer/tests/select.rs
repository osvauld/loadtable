//! Sender-side file selection: the same candidates and path rules as `tasks-offload`
//! (`git ls-files -c -o --exclude-standard` + worker_source rules), and the same checkout id.
use std::path::Path;
use std::process::Command;

use task_manager_peer::select;
use task_manager_peer::source::Limits;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn git(repo: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("HOME", repo)
        .status()
        .unwrap();
    assert!(ok.success(), "git {args:?}");
}

#[test]
fn selection_matches_tasks_offload_rules() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    write(&repo, "Cargo.toml", "[package]\n");
    write(&repo, "src/lib.rs", "pub fn f() {}\n");
    write(&repo, "target/debug/app", "top-level output\n");
    write(&repo, "crate/Cargo.toml", "[package]\n");
    write(&repo, "crate/target/y", "marker-adjacent output\n");
    write(
        &repo,
        "lib/target/z.rs",
        "not an output: no Cargo.toml beside it\n",
    );
    write(&repo, ".env.local", "SYNTHETIC=1\n");
    write(&repo, "secrets/key.txt", "synthetic\n");
    write(&repo, "notes.log", "log\n");
    write(&repo, ".gitignore", "ignored.txt\n");
    write(&repo, "ignored.txt", "never listed\n");
    // Tracked (staged) but deleted in the worktree: skipped and counted.
    write(&repo, "gone.txt", "x\n");
    git(&repo, &["add", "gone.txt"]);
    std::fs::remove_file(repo.join("gone.txt")).unwrap();

    let top = select::git_toplevel(&repo.join("src")).unwrap();
    assert_eq!(top, repo.canonicalize().unwrap());
    let s = select::select(&top, &Limits::default()).unwrap();
    let paths: Vec<&str> = s.manifest.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            ".gitignore",
            "Cargo.toml",
            "crate/Cargo.toml",
            "lib/target/z.rs",
            "src/lib.rs"
        ]
    );
    let mut excluded = s.excluded.clone();
    excluded.sort();
    assert_eq!(
        excluded,
        [
            ".env.local",
            "crate/target/y",
            "notes.log",
            "secrets/key.txt",
            "target/debug/app"
        ]
    );
    assert_eq!(s.deleted_tracked, 1);
    assert!(s.manifest.files.iter().all(|f| f.iroh_blobs_hash.is_some()));

    // In-tree symlinks are refused (the manifest carries regular files only).
    std::os::unix::fs::symlink("src/lib.rs", repo.join("link.rs")).unwrap();
    let err = select::select(&top, &Limits::default()).unwrap_err();
    assert!(err.contains("symlink"), "{err}");
}

#[test]
fn checkout_id_equals_the_python_offload_client() {
    let out = Command::new("python3")
        .args([
            "-I",
            "-c",
            "import hashlib;print(hashlib.sha256('kurukkan\\0/home/u/repo'.encode()).hexdigest()[:16])",
        ])
        .output()
        .expect("python3");
    let py = String::from_utf8(out.stdout).unwrap();
    assert_eq!(select::checkout_id("kurukkan", "/home/u/repo"), py.trim());
}
