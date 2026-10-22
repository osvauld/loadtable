//! Snapshots of real git repositories: the exact working tree as a tree id, read cheaply, with
//! the user's repository left alone (R-297 to R-304, R-319).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tasks_peer::CheckoutId;
use tasks_peer::sync::testrepo::{age_file, commit_all, git, sample_repo, write_file};
use tasks_peer::sync::{
    ExtraRules, GixSnapshot, Snapshot, SnapshotError, SnapshotRequest, SyncLimits, TreeSnapshot,
};

struct Fx {
    tmp: tempfile::TempDir,
    root: PathBuf,
    snap: GixSnapshot,
    id: CheckoutId,
    limits: SyncLimits,
    extras: ExtraRules,
}

impl Fx {
    fn new() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("work");
        fs::create_dir_all(&root).unwrap();
        sample_repo(&root).unwrap();
        let snap = GixSnapshot::new(tmp.path().join("state"));
        let id = CheckoutId::derive("laptop", &root);
        Fx {
            tmp,
            root,
            snap,
            id,
            limits: SyncLimits::default(),
            extras: ExtraRules::default(),
        }
    }

    fn run(&self) -> Result<TreeSnapshot, SnapshotError> {
        self.snap.snapshot(&SnapshotRequest {
            root: &self.root,
            checkout: &self.id,
            origin_host: "laptop",
            limits: &self.limits,
            extras: &self.extras,
        })
    }

    fn ok(&self) -> TreeSnapshot {
        self.run().unwrap()
    }
}

/// The tree id `git add -A` would write for the working tree, from a private copy of the index.
/// Writes objects into the repo, so call it last.
fn git_tree(root: &Path, scratch: &Path) -> String {
    let idx = scratch.join("cross-check-index");
    fs::copy(root.join(".git/index"), &idx).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_INDEX_FILE", &idx)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    run(&["add", "-A"]);
    run(&["write-tree"])
}

/// Every file under `dir` with its bytes and mtime, for "nothing changed" comparisons.
fn digest(dir: &Path) -> Vec<(String, Vec<u8>, std::time::SystemTime)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let m = fs::symlink_metadata(e.path()).unwrap();
            if m.is_dir() {
                stack.push(e.path());
            } else {
                let rel = e.path().strip_prefix(dir).unwrap().display().to_string();
                out.push((
                    rel,
                    fs::read(e.path()).unwrap_or_default(),
                    m.modified().unwrap(),
                ));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn r297_the_snapshot_has_the_tree_id_git_add_would_write_for_the_same_working_tree() {
    let f = Fx::new();
    // uncommitted edit, staged new file, untracked file, deleted file, rename, exec bit,
    // symlink, odd names, an ignored build output
    write_file(&f.root, "src/main.rs", "fn main() { println!(); }\n").unwrap();
    write_file(&f.root, "staged.txt", "staged").unwrap();
    git(&f.root, &["add", "staged.txt"]).unwrap();
    write_file(&f.root, "untracked/new file ü.txt", "new").unwrap();
    fs::remove_file(f.root.join("tests/fixtures/run.log")).unwrap();
    git(&f.root, &["mv", "Cargo.toml", "Cargo2.toml"]).unwrap();
    write_file(&f.root, "run.sh", "#!/bin/sh\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(f.root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::os::unix::fs::symlink("src/main.rs", f.root.join("link")).unwrap();
    write_file(&f.root, "target/debug/app", "binary").unwrap();
    write_file(&f.root, "local.properties", "sdk.dir=/x\n").unwrap();
    fs::create_dir_all(f.root.join("empty/dir")).unwrap();
    let s = f.ok();
    assert_eq!(s.tree.as_str(), git_tree(&f.root, f.tmp.path()));
    // ignored files are not in it
    let listing = git(
        &f.root,
        &[
            "-c",
            "core.quotepath=false",
            "ls-tree",
            "-r",
            "--name-only",
            s.tree.as_str(),
        ],
    )
    .unwrap();
    assert!(
        !listing.contains("target/") && !listing.contains("local.properties"),
        "{listing}"
    );
    assert!(listing.contains("untracked/new file ü.txt"));
    assert!(!listing.contains("tests/fixtures/run.log"));
    assert!(listing.contains("Cargo2.toml") && !listing.lines().any(|l| l == "Cargo.toml"));
}

#[test]
fn r318_the_snapshot_records_head_and_how_many_files_differ_from_it() {
    let f = Fx::new();
    let clean = f.ok();
    assert_eq!(clean.dirty_files, 0);
    assert_eq!(
        clean.base.as_ref().map(ToString::to_string),
        Some(git(&f.root, &["rev-parse", "HEAD"]).unwrap())
    );
    write_file(&f.root, "src/main.rs", "changed\n").unwrap();
    write_file(&f.root, "new/a.txt", "a").unwrap();
    write_file(&f.root, "new/b.txt", "b").unwrap();
    fs::remove_file(f.root.join("Cargo.toml")).unwrap();
    let dirty = f.ok();
    assert_eq!(dirty.dirty_files, 4, "1 modified + 2 added + 1 deleted");
    assert_eq!(dirty.files, 5);
    assert_ne!(dirty.tree, clean.tree);
    // an unborn repository has no base and every file is new
    let empty = f.tmp.path().join("fresh");
    fs::create_dir_all(&empty).unwrap();
    tasks_peer::sync::testrepo::init_repo(&empty).unwrap();
    write_file(&empty, "a.txt", "a").unwrap();
    let id = CheckoutId::derive("laptop", &empty);
    let s = f
        .snap
        .snapshot(&SnapshotRequest {
            root: &empty,
            checkout: &id,
            origin_host: "laptop",
            limits: &f.limits,
            extras: &f.extras,
        })
        .unwrap();
    assert_eq!((s.base, s.dirty_files, s.files), (None, 1, 1));
}

#[test]
fn r298_unchanged_files_are_not_read_again() {
    let f = Fx::new();
    // a file big enough that reading it would show: tracked, committed, aged
    write_file(&f.root, "assets/big.bin", vec![7u8; 5 * 1024 * 1024]).unwrap();
    commit_all(&f.root, "big").unwrap();
    let first = f.ok();
    // the user's index already knows the big file, so even the first snapshot does not read it
    assert!(first.io.bytes_read < 4096, "{:?}", first.io);
    assert!(first.io.files_checked >= 5);
    let second = f.ok();
    assert_eq!(second.tree, first.tree);
    assert_eq!(
        (
            second.io.files_read,
            second.io.bytes_read,
            second.io.objects_written
        ),
        (0, 0, 0),
        "{:?}",
        second.io
    );
    // one edit: exactly that file is read, and its new blob is the only object written besides
    // the trees on its path
    write_file(&f.root, "src/main.rs", "fn main() { let _ = 1; }\n").unwrap();
    let third = f.ok();
    assert_eq!(
        (third.io.files_read, third.io.bytes_read),
        (1, 25),
        "{:?}",
        third.io
    );
    assert_ne!(third.tree, second.tree);
    // an edit this recent is not trusted by stat alone (git's racily-clean rule): it is read
    // once more, until its mtime is older than the cache
    let fourth = f.ok();
    assert_eq!(fourth.io.files_read, 1, "{:?}", fourth.io);
    age_file(&f.root, "src/main.rs").unwrap();
    let fifth = f.ok();
    assert_eq!(fifth.io.files_read, 1);
    assert_eq!(f.ok().io.files_read, 0);
}

#[test]
fn r298_a_touched_file_with_the_same_content_is_hashed_but_adds_no_object() {
    let f = Fx::new();
    let first = f.ok();
    let p = f.root.join("src/main.rs");
    let body = fs::read(&p).unwrap();
    fs::write(&p, &body).unwrap();
    age_file(&f.root, "src/main.rs").unwrap();
    let again = f.ok();
    assert_eq!(again.tree, first.tree);
    assert_eq!(again.io.files_read, 1);
    assert_eq!(again.io.objects_written, 0, "the repo already has the blob");
}

#[test]
fn r299_the_users_repository_is_byte_identical_after_a_snapshot() {
    let f = Fx::new();
    write_file(&f.root, "src/main.rs", "uncommitted\n").unwrap();
    write_file(&f.root, "untracked.txt", "u").unwrap();
    git(&f.root, &["stash", "push", "-q", "-u"]).unwrap();
    write_file(&f.root, "src/main.rs", "uncommitted again\n").unwrap();
    write_file(&f.root, "another.txt", "x").unwrap();
    git(&f.root, &["add", "another.txt"]).unwrap();
    let before = digest(&f.root.join(".git"));
    let s1 = f.ok();
    let s2 = f.ok();
    assert_eq!(s1.tree, s2.tree);
    assert_eq!(
        digest(&f.root.join(".git")),
        before,
        "index, refs, stash and objects"
    );
    // the stash and staged state are intact
    assert!(
        git(&f.root, &["stash", "list"])
            .unwrap()
            .contains("stash@{0}")
    );
    assert!(
        git(&f.root, &["status", "--short"])
            .unwrap()
            .contains("A  another.txt")
    );
    // the new objects went to the tasks-owned store
    assert!(f.tmp.path().join("state/sync").is_dir());
    assert!(s1.git_dir.starts_with(f.tmp.path().join("state")));
}

#[test]
fn r304_lfs_pointers_submodules_nested_repos_and_oversize_files_are_refused_by_name() {
    let mut f = Fx::new();
    write_file(
        &f.root,
        "models/net.bin",
        "version https://git-lfs.github.com/spec/v1\noid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\nsize 12345\n",
    )
    .unwrap();
    git(&f.root, &["add", "models/net.bin"]).unwrap();
    git(
        &f.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},vendor/lib", "a".repeat(40)),
        ],
    )
    .unwrap();
    let nested = f.root.join("scratch/other");
    fs::create_dir_all(&nested).unwrap();
    tasks_peer::sync::testrepo::init_repo(&nested).unwrap();
    write_file(&nested, "x.txt", "x").unwrap();
    f.limits.max_file_bytes = 1000;
    write_file(&f.root, "data/huge.csv", "x".repeat(2000)).unwrap();
    let Err(SnapshotError::Refused(r)) = f.run() else {
        panic!("expected refusals")
    };
    for code in ["lfs_pointer", "submodule", "nested_repository", "too_large"] {
        assert!(r.has(code), "{code} missing from {r}");
    }
    let text = r.to_string();
    assert!(
        text.contains("models/net.bin") && text.contains("Git LFS"),
        "{text}"
    );
    assert!(
        text.contains("vendor/lib") && text.contains("submodule"),
        "{text}"
    );
    assert!(
        text.contains("scratch/other") && text.contains("nested git repository"),
        "{text}"
    );
    assert!(
        text.contains("data/huge.csv") && text.contains("2000 bytes, over the 1000 byte limit"),
        "{text}"
    );
    // a refused snapshot leaves no cache behind: fix the files and the next one is complete
    git(
        &f.root,
        &["rm", "-q", "--cached", "models/net.bin", "vendor/lib"],
    )
    .unwrap();
    fs::remove_file(f.root.join("models/net.bin")).unwrap();
    fs::remove_dir_all(&nested).unwrap();
    fs::remove_file(f.root.join("data/huge.csv")).unwrap();
    assert!(f.run().is_ok());
}

#[test]
fn r303_secret_looking_files_are_refused_by_name_and_by_content() {
    let mut f = Fx::new();
    // a tracked file that happens to hold a key pattern is already in the repo: not scanned
    write_file(
        &f.root,
        "docs/example.pem.txt",
        "-----BEGIN RSA PRIVATE KEY-----\nabc\n",
    )
    .unwrap();
    commit_all(&f.root, "doc").unwrap();
    f.ok();
    // untracked, not ignored: by name and by content
    write_file(&f.root, ".env", "A=1\n").unwrap();
    write_file(
        &f.root,
        "notes.txt",
        format!("token = ghp_{}\n", "ab".repeat(20)),
    )
    .unwrap();
    // allowlisted extras: by name and by content
    write_file(
        &f.root,
        "local.properties",
        "storePassword=abcdefghijklmnopqrstuvwx\n",
    )
    .unwrap();
    write_file(&f.root, "deploy.secret-ignored", "fine\n").unwrap();
    f.extras = ExtraRules::new(&[
        "local.properties".to_string(),
        "deploy.secret-ignored".to_string(),
    ])
    .unwrap();
    let Err(SnapshotError::Refused(r)) = f.run() else {
        panic!("expected refusals")
    };
    let text = r.to_string();
    assert!(
        text.contains(".env looks like a secret by name (environment file)"),
        "{text}"
    );
    assert!(
        text.contains("notes.txt looks like a secret by content (GitHub token)"),
        "{text}"
    );
    assert!(
        text.contains("local.properties looks like a secret by content (secret-named assignment"),
        "{text}"
    );
    assert!(
        !text.contains("deploy.secret-ignored"),
        "an ordinary extra passes: {text}"
    );
    assert!(!text.contains("docs/example.pem.txt"), "{text}");
    // with the secrets gone the same snapshot succeeds and carries the extra
    fs::remove_file(f.root.join(".env")).unwrap();
    fs::remove_file(f.root.join("notes.txt")).unwrap();
    fs::remove_file(f.root.join("local.properties")).unwrap();
    let s = f.ok();
    assert_eq!(s.extra_files, ["deploy.secret-ignored"]);
    assert!(s.extras_tree.is_some());
}

#[test]
fn r302_extras_are_ignored_files_in_their_own_tree_and_nothing_else_from_the_ignored_set() {
    let mut f = Fx::new();
    write_file(&f.root, "local.properties", "sdk.dir=/x\n").unwrap();
    write_file(&f.root, "target/debug/app", "binary").unwrap();
    let plain = f.ok();
    assert!(plain.extras_tree.is_none() && plain.extra_files.is_empty());
    f.extras =
        ExtraRules::new(&["local.properties".to_string(), "missing.cfg".to_string()]).unwrap();
    let s = f.ok();
    assert_eq!(s.extra_files, ["local.properties"]);
    assert_eq!(s.tree, plain.tree, "the main tree does not change");
    let x = s.extras_tree.unwrap();
    let files = git(
        &f.root,
        &[
            "--git-dir",
            s.git_dir.to_str().unwrap(),
            "ls-tree",
            "-r",
            "--name-only",
            x.as_str(),
        ],
    )
    .unwrap();
    assert_eq!(files, "local.properties");
    // an allowlisted symlink is refused by name
    std::os::unix::fs::symlink("/etc/hostname", f.root.join("linked.cfg")).unwrap();
    write_file(
        &f.root,
        ".gitignore",
        "target/\nlocal.properties\nlinked.cfg\n",
    )
    .unwrap();
    f.extras = ExtraRules::new(&["linked.cfg".to_string()]).unwrap();
    let Err(SnapshotError::Refused(r)) = f.run() else {
        panic!("expected a refusal")
    };
    assert!(r.has("extra_symlink"), "{r}");
}

#[test]
fn r320_too_many_files_is_a_named_error() {
    let mut f = Fx::new();
    f.limits.max_files = 2;
    assert!(matches!(
        f.run(),
        Err(SnapshotError::TooManyFiles { limit: 2, .. })
    ));
}

#[test]
fn r310_prune_keeps_what_the_newest_snapshots_need_and_frees_the_rest() {
    let f = Fx::new();
    write_file(&f.root, "src/old.rs", "old content\n").unwrap();
    let old = f.ok();
    let old_commit = f.snap.seal(&old, None, 1_000).unwrap();
    fs::remove_file(f.root.join("src/old.rs")).unwrap();
    write_file(&f.root, "src/new.rs", "new content\n").unwrap();
    let new = f.ok();
    let new_commit = f.snap.seal(&new, Some(&old_commit), 2_000).unwrap();
    let blob_of = |tree: &str, path: &str| {
        git(
            &f.root,
            &[
                "--git-dir",
                old.git_dir.to_str().unwrap(),
                "rev-parse",
                &format!("{tree}:{path}"),
            ],
        )
        .unwrap()
    };
    let old_blob = blob_of(old.tree.as_str(), "src/old.rs");
    let new_blob = blob_of(new.tree.as_str(), "src/new.rs");
    let exists = |id: &str| {
        git(
            &f.root,
            &[
                "--git-dir",
                old.git_dir.to_str().unwrap(),
                "cat-file",
                "-e",
                id,
            ],
        )
        .is_ok()
    };
    // young objects are never touched
    let young = f
        .snap
        .prune(
            &old.repo,
            &[new_commit.clone()],
            std::time::Duration::from_secs(3600),
        )
        .unwrap();
    assert_eq!(young.objects, 0);
    // everything counts as old now: only what the newest commit's tree reaches stays
    let stats = f
        .snap
        .prune(&old.repo, &[new_commit.clone()], std::time::Duration::ZERO)
        .unwrap();
    assert!(stats.objects >= 3 && stats.bytes > 0, "{stats:?}");
    assert!(exists(&new_blob) && exists(new.tree.as_str()) && exists(new_commit.as_str()));
    assert!(!exists(&old_blob), "the old snapshot's blob is gone");
    assert!(!exists(old.tree.as_str()) && !exists(old_commit.as_str()));
    // the parent link of the kept commit is history, not a root: pruning did not need it
    assert!(
        f.snap
            .prune(&old.repo, &[new_commit], std::time::Duration::ZERO)
            .unwrap()
            .objects
            == 0
    );
}
