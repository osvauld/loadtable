//! Synthetic artifact-contract tests, runnable on Linux. These do NOT build,
//! sign, validate or launch an Apple app, nor exercise a macOS worker.
use std::os::unix::fs::{PermissionsExt, symlink};

use iroh_blobs::api::Store;
use iroh_blobs::store::mem::MemStore;
use sha2::{Digest, Sha256};
use task_manager_peer::artifact;
use task_manager_peer::source::{self, Limits};

#[tokio::test]
async fn packaged_app_and_ipa_are_opaque_hash_verified_regular_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let checkout = root.path().join("checkouts/example/out");
    std::fs::create_dir_all(&checkout).unwrap();
    // Deliberately NOT valid app packages: the transport must not interpret them
    // as executable instructions or confuse transfer success with build success.
    let payload = b"synthetic archive bytes, not an Apple package";
    for name in ["Simulator.app.zip", "Device.ipa"] {
        std::fs::write(checkout.join(name), payload).unwrap();
    }
    let store: Store = MemStore::new().into();
    let paths = vec!["out/Simulator.app.zip".into(), "out/Device.ipa".into()];
    let export = artifact::export(
        &store,
        &root.path().join("checkouts"),
        "example",
        &paths,
        &Limits::artifact(),
    )
    .await
    .unwrap();
    assert!(export.skipped.is_empty());
    assert_eq!(export.manifest.files.len(), 2);
    for entry in &export.manifest.files {
        assert_eq!(entry.size, payload.len() as u64);
        assert_eq!(entry.sha256, format!("{:x}", Sha256::digest(payload)));
    }
    let result_root = root.path().join("results");
    std::fs::create_dir(&result_root).unwrap();
    source::materialize(
        &store,
        &result_root,
        "job-1",
        &export.manifest,
        &Limits::artifact(),
    )
    .await
    .unwrap();
    for path in &paths {
        assert_eq!(
            std::fs::read(result_root.join("job-1").join(path)).unwrap(),
            payload
        );
    }
    // Existing result directories must never be overwritten, even on retry.
    assert!(
        source::materialize(
            &store,
            &result_root,
            "job-1",
            &export.manifest,
            &Limits::artifact()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn raw_app_directory_skips_links_so_packaging_is_required() {
    let root = tempfile::tempdir().unwrap();
    let checkout = root.path().join("checkouts/example");
    let binary = checkout.join("out/Example.app/Contents/MacOS/Example");
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::write(&binary, b"synthetic app executable").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    symlink(
        "Contents/MacOS/Example",
        checkout.join("out/Example.app/Current"),
    )
    .unwrap();
    symlink(root.path(), checkout.join("out/Example.app/Outside")).unwrap();
    let store: Store = MemStore::new().into();
    let export = artifact::export(
        &store,
        &root.path().join("checkouts"),
        "example",
        &["out/Example.app".into()],
        &Limits::artifact(),
    )
    .await
    .unwrap();
    assert_eq!(export.manifest.files.len(), 1);
    assert!(export.manifest.files[0].executable);
    assert_eq!(export.skipped.len(), 2);
    assert!(
        artifact::export(
            &store,
            &root.path().join("checkouts"),
            "example",
            &["out/Example.app/Current".into()],
            &Limits::artifact(),
        )
        .await
        .is_err(),
        "a directly requested symlink must be refused"
    );
}

#[test]
fn artifact_bounds_are_distinct_from_source_bounds() {
    let source = Limits::default();
    let artifact = Limits::artifact();
    assert_eq!(source.max_file_bytes, 64 * 1024 * 1024);
    assert_eq!(artifact.max_file_bytes, 2 * 1024 * 1024 * 1024);
    assert_eq!(artifact.max_bytes, 4 * 1024 * 1024 * 1024);
}
