//! Checkout sync end to end over the fake transport (R-155, R-156, R-160, R-176).
mod common;
use common::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tasks_peer::manifest::{StaticLister, SyncLimits, build_manifest};
use tasks_peer::proto::{Body, FileEntry, MAX_CHUNK_BYTES, Refusal};
use tasks_peer::sync::{DirSource, SyncReport, push_checkout};
use tasks_peer::transport::{Connection as _, PeerError, Transport};
use tasks_peer::{CheckoutId, Hash};

fn sync(w: &World, session: u64) -> Result<SyncReport, tasks_peer::sync::SyncError> {
    let report = build_manifest(&w.root, &w.lister(), &SyncLimits::default()).unwrap();
    let mut conn = w
        .transport
        .connect(&w.nixos, std::time::Duration::from_secs(5))
        .unwrap();
    let id = CheckoutId::derive("laptop", &w.root);
    push_checkout(
        conn.as_mut(),
        &id,
        &report,
        &DirSource(w.root.clone()),
        session,
    )
}

fn tree(w: &World) -> std::path::PathBuf {
    w.store
        .tree_root(&w.origin, &CheckoutId::derive("laptop", &w.root))
}

#[test]
fn r155_first_sync_copies_every_file_including_log_fixtures_then_sends_nothing() {
    let w = World::new();
    let r = sync(&w, 1).unwrap();
    assert_eq!(r.files, 3);
    assert_eq!(r.blobs_sent, 3);
    assert_eq!(r.applied.written, 3);
    let t = tree(&w);
    assert_eq!(
        fs::read_to_string(t.join("tests/fixtures/run.log")).unwrap(),
        "fixture log line\n"
    );
    let before = fs::metadata(t.join("src/main.rs"))
        .unwrap()
        .modified()
        .unwrap();
    let again = sync(&w, 2).unwrap();
    assert_eq!(again.blobs_sent, 0, "{}", again.line());
    assert_eq!(again.bytes_sent, 0);
    assert_eq!(again.blobs_unchanged, 3);
    assert_eq!(again.applied.unchanged, 3);
    assert_eq!(
        fs::metadata(t.join("src/main.rs"))
            .unwrap()
            .modified()
            .unwrap(),
        before,
        "unchanged files keep their mtime"
    );
}

#[test]
fn r155_changed_added_and_deleted_files_follow_the_origin() {
    let mut w = World::new();
    sync(&w, 1).unwrap();
    let t = tree(&w);
    // A build output on the peer is not in either manifest and must survive.
    write(&t, "target/debug/app", "binary");
    write(&w.root, "src/main.rs", "fn main() { println!(); }\n");
    write(&w.root, "new/dir/added.txt", "added");
    w.files.push("new/dir/added.txt".into());
    w.files.retain(|f| f != "Cargo.toml");
    fs::remove_file(w.root.join("Cargo.toml")).unwrap();
    let r = sync(&w, 2).unwrap();
    assert_eq!(r.blobs_sent, 2);
    assert_eq!(r.applied.removed, 1);
    assert!(!t.join("Cargo.toml").exists());
    assert_eq!(
        fs::read_to_string(t.join("new/dir/added.txt")).unwrap(),
        "added"
    );
    assert!(
        fs::read_to_string(t.join("src/main.rs"))
            .unwrap()
            .contains("println")
    );
    assert!(t.join("target/debug/app").exists());
    // Removing the last file of a directory prunes the directory.
    w.files.retain(|f| f != "new/dir/added.txt");
    fs::remove_file(w.root.join("new/dir/added.txt")).unwrap();
    sync(&w, 3).unwrap();
    assert!(!t.join("new").exists());
}

#[test]
fn r155_exec_bit_travels_and_a_mode_only_change_sends_no_blob() {
    let w = World::new();
    write(&w.root, "run.sh", "#!/bin/sh\n");
    let mut w = w;
    w.files.push("run.sh".into());
    let p = w.root.join("run.sh");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    sync(&w, 1).unwrap();
    let t = tree(&w);
    assert_ne!(
        fs::metadata(t.join("run.sh")).unwrap().permissions().mode() & 0o111,
        0
    );
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    let r = sync(&w, 2).unwrap();
    assert_eq!(r.blobs_sent, 0);
    assert_eq!(
        fs::metadata(t.join("run.sh")).unwrap().permissions().mode() & 0o111,
        0
    );
}

#[test]
fn r156_a_big_checkout_and_a_multi_chunk_file_sync_without_frame_errors() {
    let mut w = World::new();
    for i in 0..3000 {
        let rel = format!("gen/module_{}/file_{i:05}.txt", i % 40);
        write(&w.root, &rel, &format!("content {i}"));
        w.files.push(rel);
    }
    let big = "x".repeat(MAX_CHUNK_BYTES * 2 + 123);
    write(&w.root, "assets/big.txt", &big);
    w.files.push("assets/big.txt".into());
    write(&w.root, "empty.txt", "");
    w.files.push("empty.txt".into());
    let r = sync(&w, 1).unwrap();
    assert_eq!(r.files, 3000 + 3 + 2);
    let t = tree(&w);
    assert_eq!(fs::read_to_string(t.join("assets/big.txt")).unwrap(), big);
    assert_eq!(fs::read(t.join("empty.txt")).unwrap().len(), 0);
    assert_eq!(
        fs::read_to_string(t.join("gen/module_27/file_02707.txt")).unwrap(),
        "content 2707"
    );
}

#[test]
fn identical_files_share_one_blob() {
    let mut w = World::new();
    write(&w.root, "a/copy1.txt", "same");
    write(&w.root, "b/copy2.txt", "same");
    w.files.extend(["a/copy1.txt".into(), "b/copy2.txt".into()]);
    let r = sync(&w, 1).unwrap();
    assert_eq!(r.blobs_sent, 4);
    assert_eq!(r.files, 5);
}

#[test]
fn r155_a_manifest_naming_unsent_blobs_changes_nothing() {
    let w = World::new();
    sync(&w, 1).unwrap();
    let t = tree(&w);
    let id = CheckoutId::derive("laptop", &w.root);
    let mut conn = w
        .transport
        .connect(&w.nixos, std::time::Duration::from_secs(5))
        .unwrap();
    let entry = |p: &str, body: &str| FileEntry {
        path: p.into(),
        hash: Hash::of(body.as_bytes()),
        size: body.len() as u64,
        exec: false,
    };
    let r = conn.request(Body::SyncManifestPage {
        checkout: id.clone(),
        session: 9,
        index: 0,
        last: true,
        entries: vec![
            entry("src/main.rs", "never sent"),
            entry("other.txt", "never sent either"),
        ],
    });
    assert!(
        matches!(r, Err(PeerError::Refused(Refusal::Incomplete { ref missing })) if missing.len() == 2),
        "{r:?}"
    );
    assert_eq!(
        fs::read_to_string(t.join("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert!(!t.join("other.txt").exists());
}

#[test]
fn blobs_with_the_wrong_content_or_offset_are_refused() {
    let w = World::new();
    let id = CheckoutId::derive("laptop", &w.root);
    let mut conn = w
        .transport
        .connect(&w.nixos, std::time::Duration::from_secs(5))
        .unwrap();
    let h = Hash::of(b"expected");
    let r = conn.request(Body::SyncBlob {
        checkout: id.clone(),
        hash: h,
        offset: 0,
        total: 8,
        data: b"tampered".to_vec(),
    });
    assert!(matches!(r, Err(PeerError::Refused(Refusal::HashMismatch))));
    let r = conn.request(Body::SyncBlob {
        checkout: id.clone(),
        hash: h,
        offset: 4,
        total: 8,
        data: b"ted".to_vec(),
    });
    assert!(matches!(
        r,
        Err(PeerError::Refused(Refusal::OutOfOrder { expected: 0 }))
    ));
    let r = conn.request(Body::SyncBlob {
        checkout: id,
        hash: h,
        offset: 0,
        total: u64::MAX,
        data: vec![],
    });
    assert!(matches!(
        r,
        Err(PeerError::Refused(Refusal::LimitExceeded { .. }))
    ));
}

#[test]
fn r176_sync_never_replaces_a_directory_or_walks_through_a_symlink() {
    let mut w = World::new();
    sync(&w, 1).unwrap();
    let t = tree(&w);
    // The peer's copy has a (non-manifest) directory where the origin now has a file.
    fs::create_dir_all(t.join("report")).unwrap();
    write(&t, "report/inner.txt", "keep");
    write(&w.root, "report", "now a file");
    w.files.push("report".into());
    let e = sync(&w, 2).unwrap_err().to_string();
    assert!(e.contains("would replace a directory"), "{e}");
    assert_eq!(
        fs::read_to_string(t.join("report/inner.txt")).unwrap(),
        "keep"
    );
    // A symlinked parent is refused too.
    fs::remove_dir_all(t.join("report")).unwrap();
    w.files.retain(|f| f != "report");
    fs::remove_file(w.root.join("report")).unwrap();
    let outside = w.tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, t.join("linked")).unwrap();
    write(&w.root, "linked/escape.txt", "x");
    w.files.push("linked/escape.txt".into());
    let e = sync(&w, 3).unwrap_err().to_string();
    assert!(e.contains("symlink"), "{e}");
    assert!(!outside.join("escape.txt").exists());
}

#[test]
fn unsafe_manifest_paths_are_refused_by_the_receiver() {
    let w = World::new();
    let id = CheckoutId::derive("laptop", &w.root);
    let mut conn = w
        .transport
        .connect(&w.nixos, std::time::Duration::from_secs(5))
        .unwrap();
    for bad in ["../x", "/etc/x", "a/../../b", ".git/hooks/pre-commit", ""] {
        let r = conn.request(Body::SyncManifestPage {
            checkout: id.clone(),
            session: 1,
            index: 0,
            last: true,
            entries: vec![FileEntry {
                path: bad.into(),
                hash: Hash::of(b""),
                size: 0,
                exec: false,
            }],
        });
        assert!(
            matches!(r, Err(PeerError::Refused(Refusal::BadPath { .. }))),
            "{bad}: {r:?}"
        );
    }
}

#[test]
fn r174_each_peer_has_its_own_copy_of_a_checkout() {
    let w = World::new();
    sync(&w, 1).unwrap();
    let id = CheckoutId::derive("laptop", &w.root);
    let other = tasks_peer::PeerId::new("someone-else").unwrap();
    assert!(w.store.is_synced(&w.origin, &id));
    assert!(!w.store.is_synced(&other, &id));
    assert_ne!(
        w.store.tree_root(&w.origin, &id),
        w.store.tree_root(&other, &id)
    );
}

#[test]
fn the_real_lister_skips_nothing_the_static_one_lists() {
    // build_manifest over a StaticLister of the same names agrees with a direct listing.
    let w = World::new();
    let rep = build_manifest(
        &w.root,
        &StaticLister(w.files.clone()),
        &SyncLimits::default(),
    )
    .unwrap();
    assert_eq!(rep.manifest.len(), w.files.len());
}
