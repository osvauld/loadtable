//! Fetching outputs back (R-159, REQUIREMENTS 7.13).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;
use common::*;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tasks_peer::fake::FakeRunner;
use tasks_peer::outputs::{FetchPolicy, fetch};
use tasks_peer::proto::MAX_CHUNK_BYTES;
use tasks_peer::transport::Transport;
use tasks_peer::{CheckoutId, Handler};

struct Synced {
    w: World,
    id: CheckoutId,
    tree: std::path::PathBuf,
    dest: std::path::PathBuf,
}

fn synced() -> Synced {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    assert!(f.remote().is_some());
    let id = CheckoutId::derive("laptop", &w.root);
    let tree = w.store.tree_root(&w.origin, &id);
    let dest = w.tmp.path().join("fetched");
    fs::create_dir_all(&dest).unwrap();
    Synced { w, id, tree, dest }
}

fn run_fetch(s: &Synced, paths: &[&str], max: u64) -> tasks_peer::outputs::FetchReport {
    let mut conn =
        s.w.transport
            .connect(&s.w.nixos, Duration::from_secs(5))
            .unwrap();
    let paths: Vec<String> = paths.iter().map(|p| (*p).to_string()).collect();
    fetch(conn.as_mut(), &s.id, &paths, &s.dest, max).unwrap()
}

#[test]
fn r159_outputs_arrive_intact_in_chunks_and_a_rerun_transfers_nothing() {
    let s = synced();
    let big = "z".repeat(MAX_CHUNK_BYTES * 2 + 77);
    write(&s.tree, "target/release/app", &big);
    write(&s.tree, "target/release/small.txt", "ok");
    let r = run_fetch(
        &s,
        &["target/release/app", "target/release/small.txt"],
        u64::MAX,
    );
    assert_eq!(r.fetched.len(), 2, "{r:?}");
    assert_eq!(
        fs::read_to_string(s.dest.join("target/release/app")).unwrap(),
        big
    );
    let chunks = |s: &Synced| {
        s.w.net
            .requests()
            .iter()
            .filter(|(_, k)| k == "fetch_chunk")
            .count()
    };
    let before = chunks(&s);
    assert!(before >= 4);
    let again = run_fetch(
        &s,
        &["target/release/app", "target/release/small.txt"],
        u64::MAX,
    );
    assert!(again.fetched.iter().all(|f| f.unchanged));
    assert_eq!(chunks(&s), before, "content-addressed: nothing re-sent");
}

#[test]
fn r159_the_exec_bit_is_restored() {
    use std::os::unix::fs::PermissionsExt;
    let s = synced();
    write(&s.tree, "out/tool", "#!/bin/sh\n");
    fs::set_permissions(s.tree.join("out/tool"), fs::Permissions::from_mode(0o755)).unwrap();
    run_fetch(&s, &["out/tool"], u64::MAX);
    assert_ne!(
        fs::metadata(s.dest.join("out/tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
}

#[test]
fn r159_paths_outside_the_checkout_are_refused_and_the_rest_still_arrive() {
    let s = synced();
    write(&s.tree, "ok/file.txt", "fine");
    let secret = s.w.tmp.path().join("secret.txt");
    fs::write(&secret, "do not leak").unwrap();
    std::os::unix::fs::symlink(&secret, s.tree.join("ok/link.txt")).unwrap();
    std::os::unix::fs::symlink(s.w.tmp.path(), s.tree.join("outdir")).unwrap();
    let r = run_fetch(
        &s,
        &[
            "../secret.txt",
            "/etc/passwd",
            "ok/../../secret.txt",
            ".git/config",
            "ok/link.txt",
            "outdir/secret.txt",
            "ok/file.txt",
            "nothing/here.txt",
        ],
        u64::MAX,
    );
    assert_eq!(r.fetched.len(), 1);
    assert_eq!(r.fetched[0].path, "ok/file.txt");
    assert_eq!(r.refused.len(), 6, "{:?}", r.refused);
    assert_eq!(r.missing, ["nothing/here.txt"]);
    assert!(!s.dest.join("secret.txt").exists());
    assert!(!s.dest.join("outdir").exists());
}

#[test]
fn r159_an_output_link_is_followed_only_when_the_policy_names_it() {
    let s = synced();
    let cache = s.w.tmp.path().join("cache-disk/target");
    write(&cache, "debug/app", "built");
    std::os::unix::fs::symlink(&cache, s.tree.join("target")).unwrap();
    let r = run_fetch(&s, &["target/debug/app"], u64::MAX);
    assert!(r.fetched.is_empty());
    assert!(r.refused[0].1.contains("symlink"), "{:?}", r.refused);
    // A daemon that allows the `target` link serves it.
    let rcv = s.w.receiver_with(
        Arc::new(FakeRunner::new()),
        Arc::new(tasks_peer::receiver::AllowAll),
        FetchPolicy {
            allowed_links: vec!["target".into()],
            ..FetchPolicy::default()
        },
    );
    s.w.net.serve(&s.w.nixos, rcv as Arc<dyn Handler>);
    let r = run_fetch(&s, &["target/debug/app"], u64::MAX);
    assert_eq!(r.fetched.len(), 1, "{r:?}");
    assert_eq!(
        fs::read_to_string(s.dest.join("target/debug/app")).unwrap(),
        "built"
    );
}

#[test]
fn r13_oversized_files_are_reported_with_their_limit_and_the_rest_are_fetched() {
    let s = synced();
    write(&s.tree, "out/huge.bin", &"h".repeat(1000));
    write(&s.tree, "out/small.txt", "s");
    let r = run_fetch(&s, &["out/huge.bin", "out/small.txt"], 100);
    assert_eq!(r.too_large, [("out/huge.bin".to_string(), 1000, 100)]);
    assert_eq!(r.fetched.len(), 1);
    assert!(!s.dest.join("out/huge.bin").exists());
    // The serving side enforces its own limit too.
    let rcv = s.w.receiver_with(
        Arc::new(FakeRunner::new()),
        Arc::new(tasks_peer::receiver::AllowAll),
        FetchPolicy {
            max_file_bytes: 10,
            ..FetchPolicy::default()
        },
    );
    s.w.net.serve(&s.w.nixos, rcv as Arc<dyn Handler>);
    let r = run_fetch(&s, &["out/huge.bin"], u64::MAX);
    assert_eq!(r.too_large, [("out/huge.bin".to_string(), 1000, 10)]);
}

#[test]
fn a_file_that_changes_between_listing_and_fetching_is_not_served_as_something_else() {
    use tasks_peer::proto::Body;
    let s = synced();
    write(&s.tree, "out/a.txt", "first");
    let mut conn =
        s.w.transport
            .connect(&s.w.nixos, Duration::from_secs(5))
            .unwrap();
    let Body::FetchListing { entries } = conn
        .request(Body::FetchList {
            checkout: s.id.clone(),
            paths: vec!["out/a.txt".into()],
        })
        .unwrap()
    else {
        panic!()
    };
    let tasks_peer::proto::FetchEntry::Found { hash, .. } = entries[0].clone() else {
        panic!()
    };
    write(&s.tree, "out/a.txt", "second");
    let r = conn.request(Body::FetchChunk {
        checkout: s.id.clone(),
        path: "out/a.txt".into(),
        hash,
        offset: 0,
        len: 100,
    });
    assert!(
        matches!(
            r,
            Err(tasks_peer::PeerError::Refused(
                tasks_peer::Refusal::HashMismatch
            ))
        ),
        "{r:?}"
    );
}

#[test]
fn fetching_from_an_unsynced_checkout_is_a_named_refusal() {
    use tasks_peer::proto::Body;
    let w = World::new();
    let mut conn = w
        .transport
        .connect(&w.nixos, Duration::from_secs(5))
        .unwrap();
    let r = conn.request(Body::FetchList {
        checkout: CheckoutId::derive("laptop", &w.root),
        paths: vec!["a".into()],
    });
    assert!(matches!(
        r,
        Err(tasks_peer::PeerError::Refused(
            tasks_peer::Refusal::NotSynced { .. }
        ))
    ));
}

// R-405: the Android output directories are links two levels down; a policy that names them
// serves an APK through them, and one that names only `target` does not.
#[test]
fn r405_an_android_output_link_below_the_top_level_is_followed_only_when_named() {
    let s = synced();
    let cache = s.w.tmp.path().join("cache-disk/android-app-build");
    write(&cache, "outputs/apk/app.apk", "apk bytes");
    fs::create_dir_all(s.tree.join("android/app")).unwrap();
    std::os::unix::fs::symlink(&cache, s.tree.join("android/app/build")).unwrap();
    let path = "android/app/build/outputs/apk/app.apk";
    for links in [vec![], vec!["target".to_string()]] {
        let rcv = s.w.receiver_with(
            Arc::new(FakeRunner::new()),
            Arc::new(tasks_peer::receiver::AllowAll),
            FetchPolicy {
                allowed_links: links,
                ..FetchPolicy::default()
            },
        );
        s.w.net.serve(&s.w.nixos, rcv as Arc<dyn Handler>);
        let r = run_fetch(&s, &[path], u64::MAX);
        assert!(
            r.fetched.is_empty() && r.refused[0].1.contains("symlink"),
            "{r:?}"
        );
    }
    let rcv = s.w.receiver_with(
        Arc::new(FakeRunner::new()),
        Arc::new(tasks_peer::receiver::AllowAll),
        FetchPolicy {
            allowed_links: vec!["android/app/build".into()],
            ..FetchPolicy::default()
        },
    );
    s.w.net.serve(&s.w.nixos, rcv as Arc<dyn Handler>);
    let r = run_fetch(&s, &[path], u64::MAX);
    assert_eq!(r.fetched.len(), 1, "{r:?}");
    // a link elsewhere in the path is still refused
    std::os::unix::fs::symlink(&cache, s.tree.join("other")).unwrap();
    let r = run_fetch(&s, &["other/outputs/apk/app.apk"], u64::MAX);
    assert!(r.fetched.is_empty(), "{r:?}");
}
