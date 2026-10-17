use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Tree(PathBuf);
impl Tree {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "tasks-tmp-usage-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn file(&self, rel: &str, len: usize) -> PathBuf {
        let path = self.0.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![7u8; len]).unwrap();
        path
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::set_permissions(
            self.0.join("root/locked"),
            fs::Permissions::from_mode(0o700),
        );
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// Every path with its size and mtime: proves the scan changed nothing.
fn snapshot(root: &Path) -> Vec<(PathBuf, u64, i64, u32)> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let m = fs::symlink_metadata(&p).unwrap();
        out.push((p.clone(), m.len(), m.mtime(), m.mode()));
        if m.is_dir()
            && let Ok(rd) = fs::read_dir(&p)
        {
            stack.extend(rd.map(|e| e.unwrap().path()));
        }
    }
    out.sort();
    out
}
fn group<'a>(r: &'a Report, path: &Path) -> &'a Group {
    r.roots
        .iter()
        .flat_map(|r| &r.groups)
        .find(|g| g.path == path)
        .unwrap_or_else(|| panic!("no group {}: {r:#?}", path.display()))
}
fn job(id: i64, phase: &str, source: &Path) -> JobRef {
    JobRef {
        id,
        owner: "synthetic-agent".into(),
        phase: phase.into(),
        source: source.to_string_lossy().into_owned(),
    }
}

#[test]
fn groups_sizes_and_annotations_without_following_symlinks_or_deleting() {
    let t = Tree::new();
    let root = t.0.join("root");
    t.file("root/big/a.bin", 64 * 1024);
    t.file("root/big/nested/b.bin", 64 * 1024);
    t.file("root/small.txt", 10);
    // A symlink to a large outside directory must not be followed or counted.
    t.file("outside/huge.bin", 512 * 1024);
    std::os::unix::fs::symlink(t.0.join("outside"), root.join("link")).unwrap();
    // Bucket directory: its children become separate groups.
    t.file("root/bucket/one/x.bin", 4096);
    t.file("root/bucket/two/y.bin", 4096);
    // A job checkout and a scratch dir named after an encoded cwd.
    let checkout = t.0.join("work/checkout_a.v2");
    t.file("work/checkout_a.v2/Cargo.toml", 1);
    t.file("root/job-cwd/f", 1);
    let scratch = root.join(encoded(&checkout));
    t.file(&format!("root/{}/notes", encoded(&checkout)), 1);
    // Output cache key directory for the checkout.
    let key = format!(
        "{:x}",
        Sha256::digest(checkout.as_os_str().as_encoded_bytes())
    );
    t.file(&format!("root/checkouts/{key}/target/out"), 1);
    t.file(
        "root/checkouts/0000000000000000000000000000000000000000000000000000000000000000/t",
        1,
    );
    // Unreadable directory: counted as an error, not fatal.
    t.file("root/locked/secret", 1);
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

    let before = snapshot(&t.0);
    let ctx = Context {
        jobs: vec![
            job(7, "running", &root.join("job-cwd")),
            job(8, "passed", &checkout),
        ],
        processes: vec![(4242, root.join("big/nested"))],
        expand: vec![root.join("bucket"), root.join("checkouts")],
        lock_dir: None,
    };
    let report = inventory(std::slice::from_ref(&root), &ctx, Limits::default());
    let after = snapshot(&t.0);
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(before, after, "inventory must not modify anything");

    assert!(report.read_only && !report.truncated);
    let big = group(&report, &root.join("big"));
    assert_eq!((big.files, big.dirs), (2, 2));
    assert_eq!(
        big.apparent_bytes,
        128 * 1024
            + fs::metadata(root.join("big")).unwrap().len()
            + fs::metadata(root.join("big/nested")).unwrap().len()
    );
    assert_eq!(big.in_use_by_pids, vec![4242]);
    assert!(
        report.roots[0].groups[0].path == root.join("big"),
        "sorted by size"
    );
    let link = group(&report, &root.join("link"));
    assert_eq!((link.kind.as_str(), link.files), ("symlink", 1));
    assert!(link.apparent_bytes < 4096, "symlink target was followed");
    assert!(group(&report, &root.join("bucket/one")).files == 1);
    assert!(group(&report, &root.join("bucket/two")).files == 1);
    assert!(
        report.roots[0]
            .groups
            .iter()
            .all(|g| g.path != root.join("bucket"))
    );
    let cwd = group(&report, &root.join("job-cwd"));
    assert_eq!(cwd.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), vec![7]);
    assert!(cwd.notes.iter().any(|n| n.contains("active queue job")));
    assert_eq!(
        group(&report, &scratch).checkout.as_deref(),
        Some(checkout.as_path())
    );
    let cached = group(&report, &root.join("checkouts").join(&key));
    assert_eq!(cached.checkout.as_deref(), Some(checkout.as_path()));
    let unknown = group(
        &report,
        &root.join("checkouts/0000000000000000000000000000000000000000000000000000000000000000"),
    );
    assert!(unknown.checkout.is_none() && !unknown.notes.is_empty());
    if unsafe { libc::geteuid() } != 0 {
        assert!(group(&report, &root.join("locked")).errors >= 1);
    }
    assert!(!report.roots[0].fs_type.is_empty());
    let text = render(&report, 50);
    assert!(
        text.contains("Read-only") && text.contains("checkout "),
        "{text}"
    );
}

#[test]
fn scan_is_bounded_by_entry_count_and_time() {
    let t = Tree::new();
    for i in 0..50 {
        t.file(&format!("root/d/f{i}"), 1);
    }
    let root = t.0.join("root");
    let ctx = Context::default();
    let limited = inventory(
        std::slice::from_ref(&root),
        &ctx,
        Limits {
            max_entries: 10,
            max_time: Duration::from_secs(60),
        },
    );
    assert!(limited.truncated && limited.roots[0].truncated);
    assert!(limited.entries_scanned <= 10);
    assert!(limited.notes.iter().any(|n| n.contains("lower bounds")));
    let timed = inventory(
        &[root],
        &ctx,
        Limits {
            max_entries: 1_000_000,
            max_time: Duration::ZERO,
        },
    );
    assert!(timed.truncated && timed.entries_scanned == 0);
}

#[test]
fn budget_is_shared_so_a_large_root_cannot_starve_later_roots() {
    let t = Tree::new();
    for i in 0..200 {
        t.file(&format!("huge/d/f{i}"), 1);
    }
    t.file("small/one", 1);
    let report = inventory(
        &[t.0.join("huge"), t.0.join("small")],
        &Context::default(),
        Limits {
            max_entries: 40,
            max_time: Duration::from_secs(60),
        },
    );
    assert!(report.roots[0].truncated && !report.roots[1].truncated);
    assert_eq!(group(&report, &t.0.join("small/one")).files, 1);
    assert!(report.truncated && report.entries_scanned <= 40);
}

#[test]
fn nested_missing_and_symlinked_roots_are_reported_not_followed() {
    let t = Tree::new();
    t.file("root/inner/f", 1);
    std::os::unix::fs::symlink(t.0.join("root"), t.0.join("alias")).unwrap();
    let ctx = Context::default();
    let report = inventory(
        &[
            t.0.join("root"),
            t.0.join("root/inner"),
            t.0.join("alias"),
            t.0.join("missing"),
        ],
        &ctx,
        Limits::default(),
    );
    assert_eq!(report.roots.len(), 4);
    assert!(
        report.roots[1]
            .error
            .as_deref()
            .unwrap()
            .contains("already counted")
    );
    assert!(
        report.roots[2]
            .error
            .as_deref()
            .unwrap()
            .contains("symlinks")
    );
    assert!(!report.roots[3].exists && report.roots[3].error.is_some());
}

#[test]
fn cli_rejects_unknown_or_relative_arguments() {
    for args in [
        vec!["--max-files", "0"],
        vec!["relative"],
        vec!["--bogus"],
        vec!["--top"],
    ] {
        let args: Vec<String> = args.into_iter().map(String::from).collect();
        assert!(cli(&args).is_err(), "{args:?}");
    }
}
