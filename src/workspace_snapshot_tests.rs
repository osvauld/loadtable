use super::*;
use std::os::unix::{ffi::OsStrExt, fs::symlink};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
pub(crate) const REV: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub(crate) struct Fixture {
    pub root: PathBuf,
    pub source: PathBuf,
}
impl Fixture {
    pub(crate) fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "tasks-workspace-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let source = root.join("source");
        fs::create_dir(&source).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        fs::write(source.join(".git/HEAD"), format!("{REV}\n")).unwrap();
        let fixture = Self { root, source };
        fixture.put("Cargo.toml", "[workspace]\nmembers=[\"crates/core\"]\n");
        fixture.put("Cargo.lock", "version=4\n");
        fixture.put(
            "crates/core/Cargo.toml",
            "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\n[lib]\npath=\"src/lib.rs\"\n",
        );
        fixture.put("crates/core/src/lib.rs", "pub fn fixture() {}\n");
        fixture.put("android/gradlew", "#!/bin/sh\nexit 1\n");
        fs::set_permissions(
            fixture.source.join("android/gradlew"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fixture.put("android/settings.gradle.kts", "include(\":app\")\n");
        fixture.put(
            "android/app/build.gradle.kts",
            "// synthetic source, never executed\n",
        );
        fixture.put(
            "android/gradle/wrapper/gradle-wrapper.jar",
            "synthetic wrapper fixture, not executable Java\n",
        );
        fixture.put(
            "android/gradle/wrapper/gradle-wrapper.properties",
            "distributionUrl=https://example.invalid/gradle.zip\n",
        );
        fixture.put(
            "scripts/build_libaom.py",
            &format!("VERSION = \"3.15.2\"\nSHA256 = \"{}\"\n", "a".repeat(64)),
        );
        fixture
    }
    pub(crate) fn put(&self, relative: &str, text: &str) {
        let path = self.source.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}
fn thaw(path: &Path) {
    if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        for item in fs::read_dir(path).unwrap() {
            thaw(&item.unwrap().path());
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        thaw(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn captures_dirty_untracked_assets_native_and_generated_sources_with_modes() {
    let f = Fixture::new();
    f.put(
        "crates/core/src/untracked.rs",
        "// dirty and untracked are intentionally included\n",
    );
    f.put("crates/video/csrc/aom_rt.c", "// native input\n");
    f.put(
        "crates/tin-icons/generated/TinIcons.kt",
        "// generated INPUT, not disposable output\n",
    );
    f.put("android/app/src/main/assets/sample.bin", "asset payload\n");
    let inspected = inspect(&f.source, REV).unwrap();
    let dest = f.root.join("capsule");
    let captured = capture(&f.source, REV, &dest).unwrap();
    assert_eq!(inspected.digest, captured.digest);
    assert_eq!(
        fs::read_to_string(dest.join("crates/tin-icons/generated/TinIcons.kt")).unwrap(),
        "// generated INPUT, not disposable output\n"
    );
    assert_eq!(
        fs::metadata(dest.join("android/gradlew"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o555
    );
    verify(&dest, &captured.digest).unwrap();
    f.put("crates/core/src/lib.rs", "// changed after capture\n");
    verify(&dest, &captured.digest).unwrap();
    assert_ne!(inspect(&f.source, REV).unwrap().digest, captured.digest);
}
#[test]
fn excluded_outputs_and_local_configuration_are_not_followed_or_captured() {
    let f = Fixture::new();
    f.put(
        "android/local.properties",
        "sdk.dir=/synthetic/user-specific/path\n",
    );
    f.put("android/.gradle/cache.bin", "not source\n");
    symlink(
        f.root.join("absent-foreign-output"),
        f.source.join("android/app/build"),
    )
    .unwrap();
    let dest = f.root.join("capsule");
    let captured = capture(&f.source, REV, &dest).unwrap();
    assert!(!dest.join("android/local.properties").exists());
    assert!(!dest.join("android/.gradle").exists());
    fs::set_permissions(dest.join("android"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir(dest.join("android/.gradle")).unwrap();
    assert!(verify(&dest, &captured.digest).is_err());
}
#[test]
fn rejects_existing_alias_and_in_source_destinations_without_overwrite() {
    let f = Fixture::new();
    let existing = f.root.join("existing");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("preserve"), "user data").unwrap();
    assert!(capture(&f.source, REV, &existing).is_err());
    let alias = f.root.join("alias");
    symlink(&existing, &alias).unwrap();
    assert!(capture(&f.source, REV, &alias).is_err());
    assert_eq!(
        fs::read_to_string(existing.join("preserve")).unwrap(),
        "user data"
    );
    assert!(capture(&f.source, REV, &f.source.join("capsule")).is_err());
}
#[test]
fn rejects_source_symlinks_and_special_files_without_reading_them() {
    let f = Fixture::new();
    let bad = f.source.join("crates/core/src/escape.rs");
    symlink(f.root.join("not-source"), &bad).unwrap();
    assert!(inspect(&f.source, REV).is_err());
    fs::remove_file(&bad).unwrap();
    let fifo = std::ffi::CString::new(bad.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(inspect(&f.source, REV).is_err());
}
#[test]
fn rejects_escaping_or_missing_local_manifest_paths() {
    let f = Fixture::new();
    f.put("crates/core/Cargo.toml","[package]\nname=\"fixture\"\nversion=\"0.1.0\"\n[dependencies]\nother={path=\"../../../private\"}\n");
    assert!(inspect(&f.source, REV).is_err());
    f.put("crates/core/Cargo.toml","[package]\nname=\"fixture\"\nversion=\"0.1.0\"\n[dependencies]\nother={path=\"../missing\"}\n");
    assert!(inspect(&f.source, REV).is_err());
}
#[test]
fn executable_mode_and_content_changes_invalidate_capsules() {
    let f = Fixture::new();
    let dest = f.root.join("capsule");
    let captured = capture(&f.source, REV, &dest).unwrap();
    fs::set_permissions(
        dest.join("android/gradlew"),
        fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    assert!(verify(&dest, &captured.digest).is_err());
}
#[test]
fn rejects_oversized_private_and_unreviewed_configuration_inputs() {
    let f = Fixture::new();
    let large = f.source.join("crates/core/large.bin");
    fs::File::create(&large)
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    assert!(inspect(&f.source, REV).is_err());
    fs::remove_file(large).unwrap();
    f.put("android/private.jks", "synthetic, not a real credential");
    assert!(inspect(&f.source, REV).is_err());
    fs::remove_file(f.source.join("android/private.jks")).unwrap();
    fs::create_dir(f.source.join(".cargo")).unwrap();
    assert!(inspect(&f.source, REV).is_err());
}
#[test]
fn requires_exact_git_head_and_rejects_capsule_extra_files() {
    let f = Fixture::new();
    assert!(inspect(&f.source, &"b".repeat(40)).is_err());
    let dest = f.root.join("capsule");
    let captured = capture(&f.source, REV, &dest).unwrap();
    fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(dest.join("extra"), "not in receipt").unwrap();
    assert!(verify(&dest, &captured.digest).is_err());
}
