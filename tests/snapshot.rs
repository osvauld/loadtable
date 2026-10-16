use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use tinline_task_manager::snapshot::{Snapshot, capture, verify};

const HEAD: &str = "1111111111111111111111111111111111111111";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "task-snapshot-integration-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let source = base.join("repo");
        fs::create_dir(&source).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        // No commits or objects are created. rev-parse HEAD only needs a ref;
        // this synthetic ref exercises the API's exact revision check without
        // modifying the real checkout or needing commit authorization.
        fs::write(source.join(".git/HEAD"), "ref: refs/heads/synthetic\n").unwrap();
        fs::write(
            source.join(".git/refs/heads/synthetic"),
            format!("{HEAD}\n"),
        )
        .unwrap();
        let root = source.join("tools/task-manager");
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::create_dir(root.join("tests")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n",
        )
        .unwrap();
        fs::write(root.join("Cargo.lock"), "version = 4\n").unwrap();
        fs::write(root.join("src/lib.rs"), "// untracked source\n").unwrap();
        fs::write(root.join("src/nested/module.rs"), "// nested source\n").unwrap();
        fs::write(root.join("tests/sample.rs"), "// untracked test\n").unwrap();
        fs::write(root.join("README.md"), "not captured").unwrap();
        fs::write(source.join("other.rs"), "outside selected tree").unwrap();
        Self(base)
    }
    fn source(&self) -> PathBuf {
        self.0.join("repo")
    }
    fn root(&self) -> PathBuf {
        self.source().join("tools/task-manager")
    }
    fn dest(&self) -> PathBuf {
        self.0.join("snapshot")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        unseal(&self.0);
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn writable(path: &Path) {
    let mut permissions = fs::symlink_metadata(path).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() | 0o700);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions).unwrap();
}
fn unseal(path: &Path) {
    let kind = fs::symlink_metadata(path).unwrap().file_type();
    if kind.is_symlink() {
        return;
    }
    writable(path);
    if kind.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            unseal(&entry.unwrap().path());
        }
    }
}

#[test]
fn captures_untracked_and_dirty_content_as_flat_readonly_workspace() {
    let fixture = Fixture::new();
    let snapshot = capture(&fixture.source(), HEAD, &fixture.dest()).unwrap();
    assert_eq!(snapshot.revision, HEAD);
    assert_eq!(
        snapshot.source,
        fs::canonicalize(fixture.source())
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert_eq!(snapshot.files, 5);
    assert_eq!(
        snapshot.bytes,
        [
            "Cargo.toml",
            "Cargo.lock",
            "src/lib.rs",
            "src/nested/module.rs",
            "tests/sample.rs"
        ]
        .iter()
        .map(|p| fs::metadata(fixture.root().join(p)).unwrap().len())
        .sum::<u64>()
    );
    assert!(!fixture.dest().join("tools").exists());
    assert!(!fixture.dest().join("README.md").exists());
    assert!(!fixture.dest().join("other.rs").exists());
    verify(&fixture.dest(), &snapshot.digest).unwrap();
    let decoded: Snapshot =
        serde_json::from_str(&serde_json::to_string(&snapshot.clone()).unwrap()).unwrap();
    assert_eq!(snapshot.digest, decoded.digest);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            fixture.dest(),
            fixture.dest().join("src"),
            fixture.dest().join("src/nested"),
            fixture.dest().join("src/lib.rs"),
        ] {
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o222, 0);
        }
    }
    let original = fs::read(fixture.dest().join("src/lib.rs")).unwrap();
    fs::write(
        fixture.root().join("src/lib.rs"),
        "// edited dirty source\n",
    )
    .unwrap();
    assert_eq!(
        fs::read(fixture.dest().join("src/lib.rs")).unwrap(),
        original
    );
    verify(&fixture.dest(), &snapshot.digest).unwrap();
    let changed = capture(&fixture.source(), HEAD, &fixture.0.join("changed")).unwrap();
    assert_ne!(snapshot.digest, changed.digest);
    let repeated = capture(&fixture.source(), HEAD, &fixture.0.join("repeated")).unwrap();
    assert_eq!(changed.digest, repeated.digest);
}

#[test]
fn rejects_existing_destinations_without_overwriting_or_deleting() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.dest()).unwrap();
    fs::write(fixture.dest().join("sentinel"), "keep me").unwrap();
    assert!(capture(&fixture.source(), HEAD, &fixture.dest()).is_err());
    assert_eq!(
        fs::read_to_string(fixture.dest().join("sentinel")).unwrap(),
        "keep me"
    );
    fs::remove_file(fixture.dest().join("sentinel")).unwrap();
    assert!(capture(&fixture.source(), HEAD, &fixture.dest()).is_err());
    assert!(fixture.dest().is_dir());
    assert!(capture(&fixture.source(), HEAD, &fixture.root().join("snapshot")).is_err());
    assert!(!fixture.root().join("snapshot").exists());
}

#[test]
fn rejects_non_root_relative_and_wrong_or_abbreviated_revisions() {
    let fixture = Fixture::new();
    for revision in [
        "1111111",
        "not-hex",
        "2222222222222222222222222222222222222222",
    ] {
        assert!(capture(&fixture.source(), revision, &fixture.dest()).is_err());
        assert!(!fixture.dest().exists());
    }
    assert!(capture(Path::new("."), HEAD, &fixture.dest()).is_err());
    assert!(capture(&fixture.source().join("tools"), HEAD, &fixture.dest()).is_err());
    assert!(!fixture.dest().exists());
}

#[test]
fn rejects_snapshot_modification_and_extra_metadata_or_source() {
    let fixture = Fixture::new();
    let snapshot = capture(&fixture.source(), HEAD, &fixture.dest()).unwrap();
    let file = fixture.dest().join("tests/sample.rs");
    writable(&file);
    fs::write(&file, "// corrupted\n").unwrap();
    assert!(verify(&fixture.dest(), &snapshot.digest).is_err());
    fs::copy(fixture.root().join("tests/sample.rs"), &file).unwrap();
    verify(&fixture.dest(), &snapshot.digest).unwrap();
    writable(&fixture.dest());
    fs::write(fixture.dest().join("job.json"), "{}").unwrap();
    assert!(verify(&fixture.dest(), &snapshot.digest).is_err());
    fs::remove_file(fixture.dest().join("job.json")).unwrap();
    writable(&fixture.dest().join("tests"));
    fs::write(fixture.dest().join("tests/extra.rs"), "// injected\n").unwrap();
    assert!(verify(&fixture.dest(), &snapshot.digest).is_err());
    assert!(verify(&fixture.dest(), "short").is_err());
}

#[cfg(unix)]
#[test]
fn rejects_symlink_destination_without_touching_its_target() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let target = fixture.0.join("original");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("sentinel"), "original").unwrap();
    symlink(&target, fixture.dest()).unwrap();
    assert!(capture(&fixture.source(), HEAD, &fixture.dest()).is_err());
    assert_eq!(
        fs::read_to_string(target.join("sentinel")).unwrap(),
        "original"
    );
    assert!(verify(&fixture.dest(), &"0".repeat(64)).is_err());
}
