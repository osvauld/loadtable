use super::*;
use crate::{
    cache::TargetLease,
    retention,
    workspace_snapshot::{
        self,
        tests::{Fixture, REV},
    },
};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;

fn prepared(f: &Fixture) -> (PathBuf, crate::snapshot::Snapshot) {
    let capsule = f.root.join("capsule");
    let snap = workspace_snapshot::capture(&f.source, REV, &capsule).unwrap();
    (capsule, snap)
}
fn managed_root(f: &Fixture) -> (PathBuf, TargetLease) {
    let root = root(&f.root.join("lane")).unwrap();
    let lease = TargetLease::try_acquire(&root).unwrap().unwrap();
    (root, lease)
}
#[test]
fn writable_materialization_does_not_hardlink_or_mutate_capsule() {
    let f = Fixture::new();
    let (capsule, snap) = prepared(&f);
    let destination = f.root.join("writable");
    workspace_snapshot::materialize(&capsule, &snap.digest, &destination).unwrap();
    assert_ne!(
        fs::metadata(capsule.join("Cargo.toml")).unwrap().ino(),
        fs::metadata(destination.join("Cargo.toml")).unwrap().ino()
    );
    assert_eq!(
        fs::metadata(destination.join("android/gradlew"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
    fs::write(destination.join("crates/core/src/lib.rs"), "// changed\n").unwrap();
    workspace_snapshot::verify(&capsule, &snap.digest).unwrap();
    assert!(workspace_snapshot::verify_inputs(&destination, &snap.digest).is_err());
    assert!(workspace_snapshot::materialize(&capsule, &snap.digest, &destination).is_err());
}
#[test]
fn lane_has_stable_exclusive_target_fresh_apk_and_distinct_ownership() {
    let f = Fixture::new();
    let (capsule, snap) = prepared(&f);
    let (root, lease) = managed_root(&f);
    let usage = retention::begin_android_use(&lease, &root).unwrap();
    let first = Layout::prepare(&root, &capsule, &snap.digest, &"a".repeat(64)).unwrap();
    let second = Layout::prepare(&root, &capsule, &snap.digest, &"a".repeat(64)).unwrap();
    assert_eq!(
        fs::read_link(first.tree.join("target")).unwrap(),
        fs::read_link(second.tree.join("target")).unwrap()
    );
    assert_ne!(first.apk(), second.apk());
    assert!(!second.apk().exists());
    assert_eq!(first.native(), second.native());
    assert!(TargetLease::try_existing(&root).unwrap().is_none());
    verify_lease(&root, lease.file()).unwrap();
    let unrelated = File::create(f.root.join("unrelated-lock")).unwrap();
    assert!(verify_lease(&root, &unrelated).is_err());
    assert!(retention::begin_use(&lease, &root).is_err());
    first.verify(&snap.digest).unwrap();
    drop(usage);
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".tasks-target.usage.json")).unwrap()).unwrap();
    assert!(metadata["finished_at"].as_i64().is_some());
}
#[test]
fn foreign_lane_and_replaced_mappings_are_refused() {
    let f = Fixture::new();
    let (capsule, snap) = prepared(&f);
    let (root, lease) = managed_root(&f);
    fs::write(root.join("legacy"), "preserve").unwrap();
    assert!(retention::begin_android_use(&lease, &root).is_err());
    assert_eq!(fs::read_to_string(root.join("legacy")).unwrap(), "preserve");
    let owned = root.join("owned");
    fs::create_dir(&owned).unwrap();
    let l = TargetLease::try_acquire(&owned).unwrap().unwrap();
    let _usage = retention::begin_android_use(&l, &owned).unwrap();
    let layout = Layout::prepare(&owned, &capsule, &snap.digest, &"b".repeat(64)).unwrap();
    fs::remove_file(layout.tree.join("target")).unwrap();
    symlink(&f.source, layout.tree.join("target")).unwrap();
    assert!(layout.verify(&snap.digest).is_err());
    assert!(!f.source.join("target").exists());
}
#[test]
fn seeding_never_repairs_corrupt_archive_or_changes_shared_input() {
    let f = Fixture::new();
    let source = f.root.join("shared.tar.gz");
    let dest = f.root.join("managed.tar.gz");
    fs::write(&source, b"synthetic archive bytes").unwrap();
    let hash = file_hash(&source, 1024).unwrap().0;
    seed_archive(&source, &dest, &hash).unwrap();
    fs::write(&dest, b"corrupt").unwrap();
    assert!(seed_archive(&source, &dest, &hash).is_err());
    assert_eq!(fs::read(&dest).unwrap(), b"corrupt");
    assert_eq!(file_hash(&source, 1024).unwrap().0, hash);
}
#[test]
fn durable_publication_is_new_copied_and_source_bound() {
    let f = Fixture::new();
    let (capsule, snap) = prepared(&f);
    let (root, lease) = managed_root(&f);
    let _usage = retention::begin_android_use(&lease, &root).unwrap();
    let layout = Layout::prepare(&root, &capsule, &snap.digest, &"c".repeat(64)).unwrap();
    let apk = layout.apk();
    fs::create_dir_all(apk.parent().unwrap()).unwrap();
    fs::write(
        &apk,
        b"fixture for copy contract, ZIP verification separately tested",
    )
    .unwrap();
    let results = f.root.join("results");
    fs::create_dir(&results).unwrap();
    let receipt = publish(
        &layout,
        &results,
        &snap.digest,
        "debug/x86_64/rust-debug/offline",
        &serde_json::json!({"synthetic":true}),
    )
    .unwrap();
    assert_eq!(receipt.source_digest, snap.digest);
    assert!(receipt.bytes > 0);
    assert_eq!(
        file_hash(&results.join(&receipt.path), MAX_APK).unwrap().0,
        receipt.sha256
    );
    assert_ne!(
        fs::metadata(&apk).unwrap().ino(),
        fs::metadata(results.join(&receipt.path)).unwrap().ino()
    );
    fs::write(&apk, b"changed build output").unwrap();
    assert_eq!(
        file_hash(&results.join(&receipt.path), MAX_APK).unwrap().0,
        receipt.sha256
    );
    assert!(
        publish(
            &layout,
            &results,
            &snap.digest,
            "debug/x86_64/rust-debug/offline",
            &serde_json::json!({})
        )
        .is_err()
    );
}
fn zip_fixture(path: &Path, abi: &str, machine: u16) {
    assert!(
        Command::new("python3")
            .args([
                "-c",
                r#"
import sys,zipfile,struct
head=b'\x7fELF\x02\x01'+b'\x00'*12+struct.pack('<H',int(sys.argv[3]))
with zipfile.ZipFile(sys.argv[1],'w') as z:
 z.writestr('AndroidManifest.xml',b'synthetic manifest')
 z.writestr('lib/'+sys.argv[2]+'/libp2pcore.so',head)
"#
            ])
            .arg(path)
            .arg(abi)
            .arg(machine.to_string())
            .status()
            .unwrap()
            .success()
    );
}
fn check_zip(path: &Path, abis: &str) -> bool {
    Command::new("python3")
        .args(["-c", CHECK_APK])
        .arg(path)
        .arg(abis)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}
#[test]
fn apk_checks_reject_wrong_abi_machine_and_corruption_without_devices() {
    let f = Fixture::new();
    let path = f.root.join("synthetic.apk");
    zip_fixture(&path, "x86_64", 62);
    assert!(check_zip(&path, "x86_64"));
    assert!(!check_zip(&path, "arm64-v8a"));
    assert!(!check_zip(&path, "x86_64,arm64-v8a"));
    zip_fixture(&path, "x86_64", 183);
    assert!(!check_zip(&path, "x86_64"));
    fs::write(&path, b"not a ZIP").unwrap();
    assert!(!check_zip(&path, "x86_64"));
}
#[test]
fn host_identity_recheck_detects_changed_tool_without_executing_it() {
    let f = Fixture::new();
    let marker = f.root.join("tool");
    fs::write(&marker, "synthetic tool").unwrap();
    let host = Host {
        sdk: f.root.clone(),
        java: f.root.clone(),
        rust: f.root.clone(),
        toolchain: "synthetic".into(),
        absent_config: vec![f.root.join("missing-config")],
        init_dir: f.root.join("init.d"),
        gradle: marker.clone(),
        identities: BTreeMap::from([(marker.clone(), file_hash(&marker, 1024).unwrap().0)]),
    };
    host.verify().unwrap();
    fs::write(&host.absent_config[0], "new config").unwrap();
    assert!(host.verify().is_err());
    fs::remove_file(&host.absent_config[0]).unwrap(); // test-owned fixture only
    host.verify().unwrap();
    fs::write(marker, "changed").unwrap();
    assert!(host.verify().is_err());
}
