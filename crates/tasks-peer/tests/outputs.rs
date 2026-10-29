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
    // A daemon that allows the `target` link into its cache serves it.
    let rcv = s.w.receiver_with(
        Arc::new(FakeRunner::new()),
        Arc::new(tasks_peer::receiver::AllowAll),
        FetchPolicy {
            allowed_links: vec!["target".into()],
            cache_root: Some(s.w.tmp.path().join("cache-disk")),
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

// R-829: an allowed link is followed only into the build cache. A `target` link that came with
// the synced tree (committed, or planted by a job) and leads to the home folder serves nothing:
// without this, a paired machine could read any file the daemon can read.
#[test]
fn r829_an_output_link_is_followed_only_into_the_build_cache() {
    use std::os::unix::fs::symlink;
    let s = synced();
    let cache = s.w.tmp.path().join("cache-disk");
    write(&cache, "target/debug/app", "built");
    let outside = s.w.tmp.path().join("outside");
    write(&outside, "home/me/.ssh/id_ed25519", "secret");
    let serve = |root: Option<std::path::PathBuf>| {
        let rcv = s.w.receiver_with(
            Arc::new(FakeRunner::new()),
            Arc::new(tasks_peer::receiver::AllowAll),
            FetchPolicy {
                allowed_links: vec!["target".into()],
                cache_root: root,
                ..FetchPolicy::default()
            },
        );
        s.w.net.serve(&s.w.nixos, rcv as Arc<dyn Handler>);
    };
    serve(Some(cache.clone()));
    let secret = "target/home/me/.ssh/id_ed25519";
    symlink(&outside, s.tree.join("target")).unwrap();
    let r = run_fetch(&s, &[secret], u64::MAX);
    assert!(r.fetched.is_empty(), "{r:?}");
    assert!(r.refused[0].1.contains("build cache"), "{r:?}");
    assert!(!s.dest.join(secret).exists());
    // a link that reaches the outside through a link inside the cache is not followed either
    fs::remove_file(s.tree.join("target")).unwrap();
    fs::create_dir_all(cache.join("hop")).unwrap();
    symlink(&outside, cache.join("hop/out")).unwrap();
    symlink(cache.join("hop/out"), s.tree.join("target")).unwrap();
    let r = run_fetch(&s, &[secret], u64::MAX);
    assert!(
        r.fetched.is_empty() && r.refused[0].1.contains("build cache"),
        "{r:?}"
    );
    // the link the daemon places, into its cache, is followed
    fs::remove_file(s.tree.join("target")).unwrap();
    symlink(cache.join("target"), s.tree.join("target")).unwrap();
    let r = run_fetch(&s, &["target/debug/app"], u64::MAX);
    assert_eq!(r.fetched.len(), 1, "{r:?}");
    // a policy without a cache root follows no link at all
    serve(None);
    let r = run_fetch(&s, &["target/debug/app"], u64::MAX);
    assert!(
        r.fetched.is_empty() && r.refused[0].1.contains("build cache"),
        "{r:?}"
    );
}

/// A worker whose listing names a file the origin did not ask for, and the asked file twice.
struct Lies {
    inner: Box<dyn tasks_peer::transport::Connection>,
}

impl tasks_peer::transport::Connection for Lies {
    fn request(
        &mut self,
        msg: tasks_peer::proto::Body,
    ) -> Result<tasks_peer::proto::Body, tasks_peer::PeerError> {
        use tasks_peer::proto::{Body, FetchEntry};
        Ok(match self.inner.request(msg)? {
            Body::FetchListing { mut entries } => {
                if let Some(FetchEntry::Found {
                    hash, size, exec, ..
                }) = entries.first().cloned()
                {
                    entries.push(FetchEntry::Found {
                        path: "Cargo.toml".into(),
                        hash,
                        size,
                        exec,
                        blob: None,
                    });
                    entries.push(FetchEntry::Found {
                        path: "out/a.txt".into(),
                        hash,
                        size,
                        exec,
                        blob: None,
                    });
                }
                Body::FetchListing { entries }
            }
            other => other,
        })
    }
    fn subscribe(
        &mut self,
        remote: tasks_peer::RemoteJobId,
        from_offset: u64,
    ) -> Result<Box<dyn tasks_peer::transport::EventStream>, tasks_peer::PeerError> {
        self.inner.subscribe(remote, from_offset)
    }
}

// R-830: only the files asked for are written, whatever the peer lists. Without this a worker
// could put its own `build.rs` or `Cargo.toml` into the origin's checkout under a fetched
// output's name.
#[test]
fn r830_only_the_files_asked_for_are_written_whatever_the_peer_lists() {
    let s = synced();
    write(&s.tree, "out/a.txt", "a");
    let manifest = "[package]\nname = \"mine\"\n";
    fs::write(s.dest.join("Cargo.toml"), manifest).unwrap();
    let inner =
        s.w.transport
            .connect(&s.w.nixos, Duration::from_secs(5))
            .unwrap();
    let mut conn = Lies { inner };
    let r = fetch(
        &mut conn,
        &s.id,
        &["out/a.txt".to_string()],
        &s.dest,
        u64::MAX,
    )
    .unwrap();
    assert_eq!(r.fetched.len(), 1, "{r:?}");
    assert_eq!(r.unasked, ["Cargo.toml", "out/a.txt"]);
    assert_eq!(
        fs::read_to_string(s.dest.join("Cargo.toml")).unwrap(),
        manifest
    );
    assert_eq!(fs::read_to_string(s.dest.join("out/a.txt")).unwrap(), "a");
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
                cache_root: Some(s.w.tmp.path().join("cache-disk")),
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
            cache_root: Some(s.w.tmp.path().join("cache-disk")),
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

/// A connection that dies after `ok_chunks` chunk requests, as a link lost mid-transfer.
struct Dies {
    inner: Box<dyn tasks_peer::transport::Connection>,
    ok_chunks: usize,
}

impl tasks_peer::transport::Connection for Dies {
    fn request(
        &mut self,
        msg: tasks_peer::proto::Body,
    ) -> Result<tasks_peer::proto::Body, tasks_peer::PeerError> {
        if matches!(msg, tasks_peer::proto::Body::FetchChunk { .. }) {
            if self.ok_chunks == 0 {
                return Err(tasks_peer::PeerError::LinkLost("cable pulled".into()));
            }
            self.ok_chunks -= 1;
        }
        self.inner.request(msg)
    }
    fn subscribe(
        &mut self,
        remote: tasks_peer::RemoteJobId,
        from_offset: u64,
    ) -> Result<Box<dyn tasks_peer::transport::EventStream>, tasks_peer::PeerError> {
        self.inner.subscribe(remote, from_offset)
    }
}

fn leftovers(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

// R-515
#[test]
fn r515_an_interrupted_fetch_leaves_no_partial_file_and_keeps_the_old_one() {
    let s = synced();
    let big = "q".repeat(MAX_CHUNK_BYTES * 3);
    write(&s.tree, "out/app.bin", &big);
    // nothing there before: after the failure still nothing, not even a temporary file
    let inner =
        s.w.transport
            .connect(&s.w.nixos, Duration::from_secs(5))
            .unwrap();
    let mut conn = Dies {
        inner,
        ok_chunks: 1,
    };
    let r = tasks_peer::outputs::fetch(
        &mut conn,
        &s.id,
        &["out/app.bin".to_string()],
        &s.dest,
        u64::MAX,
    );
    assert!(r.is_err(), "{r:?}");
    assert_eq!(leftovers(&s.dest.join("out")), Vec::<String>::new());
    // an older version in place stays exactly as it was
    write(&s.dest, "out/app.bin", "old build");
    let inner =
        s.w.transport
            .connect(&s.w.nixos, Duration::from_secs(5))
            .unwrap();
    let mut conn = Dies {
        inner,
        ok_chunks: 2,
    };
    assert!(
        tasks_peer::outputs::fetch(
            &mut conn,
            &s.id,
            &["out/app.bin".to_string()],
            &s.dest,
            u64::MAX
        )
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(s.dest.join("out/app.bin")).unwrap(),
        "old build"
    );
    assert_eq!(leftovers(&s.dest.join("out")), ["app.bin"]);
    // and a whole fetch replaces it in one rename
    run_fetch(&s, &["out/app.bin"], u64::MAX);
    assert_eq!(fs::read_to_string(s.dest.join("out/app.bin")).unwrap(), big);
    assert_eq!(leftovers(&s.dest.join("out")), ["app.bin"]);
}

// R-516
#[test]
fn r516_the_destination_is_confined_to_the_checkout_and_only_named_links_are_followed() {
    use std::os::unix::fs::symlink;
    let s = synced();
    write(&s.tree, "out/a.txt", "a");
    write(&s.tree, "out/b.txt", "b");
    let outside = s.w.tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    // a directory component that is a symlink
    symlink(&outside, s.dest.join("out")).unwrap();
    let r = run_fetch(&s, &["out/a.txt"], u64::MAX);
    assert!(
        r.fetched.is_empty() && r.refused[0].1.contains("symlink"),
        "{r:?}"
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    fs::remove_file(s.dest.join("out")).unwrap();
    // the file's own place being a symlink (it would be replaced, or written through)
    fs::create_dir_all(s.dest.join("out")).unwrap();
    let victim = outside.join("victim");
    fs::write(&victim, "keep").unwrap();
    symlink(&victim, s.dest.join("out/b.txt")).unwrap();
    let r = run_fetch(&s, &["out/b.txt"], u64::MAX);
    assert!(
        r.fetched.is_empty() && r.refused[0].1.contains("symlink"),
        "{r:?}"
    );
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
    // a directory where the file should go
    fs::create_dir_all(s.dest.join("out/a.txt")).unwrap();
    let r = run_fetch(&s, &["out/a.txt"], u64::MAX);
    assert!(r.refused[0].1.contains("directory"), "{r:?}");
    // an allowed link is followed, and only that one
    fs::remove_dir(s.dest.join("out/a.txt")).unwrap();
    fs::remove_dir_all(s.dest.join("out")).unwrap();
    symlink(&outside, s.dest.join("out")).unwrap();
    let mut conn =
        s.w.transport
            .connect(&s.w.nixos, Duration::from_secs(5))
            .unwrap();
    let r = tasks_peer::outputs::fetch_into(
        conn.as_mut(),
        &s.id,
        &["out/a.txt".to_string()],
        &s.dest,
        u64::MAX,
        &["out".to_string()],
    )
    .unwrap();
    assert_eq!(r.fetched.len(), 1, "{r:?}");
    assert_eq!(fs::read_to_string(outside.join("a.txt")).unwrap(), "a");
}
