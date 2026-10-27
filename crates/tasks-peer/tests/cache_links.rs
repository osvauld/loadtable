//! Output links into the worker's cache (R-406): sync neither ships, replaces nor deletes them,
//! and retention of the working copy leaves the cache directory they point at.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;
use common::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tasks_peer::sync::{GixTransfer, SyncDriver, SyncJob, SyncLimits, SyncStore, WorkerRetention};
use tasks_peer::transport::Transport;
use tasks_peer::{CheckoutId, PeerId};

fn push(w: &World, root: &Path, session: u64) {
    let mut conn = w
        .transport
        .connect(&w.nixos, Duration::from_secs(5))
        .unwrap();
    let id = CheckoutId::derive("laptop", root);
    w.origin_sync
        .push(
            conn.as_mut(),
            &SyncJob {
                root,
                checkout: &id,
                origin_host: "laptop",
                peer: &w.nixos,
                local_job: 1,
                session,
            },
        )
        .unwrap();
}

/// A cache directory with a built artifact, and the link to it from `tree`.
fn link_into_cache(w: &World, tree: &Path) -> PathBuf {
    let cache = w
        .tmp
        .path()
        .join("build-cache/loadtable/checkouts/peer/laptop-node/x/target");
    write(&cache, "debug/app", "warm artifact");
    std::os::unix::fs::symlink(&cache, tree.join("target")).unwrap();
    cache
}

fn tree(w: &World, root: &Path) -> PathBuf {
    w.store
        .tree_root(&w.origin, &CheckoutId::derive("laptop", root))
}

// R-406: a re-sync (files changed, added, removed) leaves the worker's `target` link and what it
// points at alone.
#[test]
fn r406_a_resync_keeps_the_output_link_and_its_contents() {
    let w = World::new();
    push(&w, &w.root, 1);
    let t = tree(&w, &w.root);
    let cache = link_into_cache(&w, &t);
    write(&w.root, "src/main.rs", "fn main() { /* v2 */ }\n");
    write(&w.root, "src/new.rs", "pub fn n() {}\n");
    fs::remove_file(w.root.join("tests/fixtures/run.log")).unwrap();
    push(&w, &w.root, 2);
    assert!(fs::symlink_metadata(t.join("target")).unwrap().is_symlink());
    assert_eq!(fs::read_link(t.join("target")).unwrap(), cache);
    assert_eq!(
        fs::read_to_string(t.join("target/debug/app")).unwrap(),
        "warm artifact"
    );
    assert!(
        fs::read_to_string(t.join("src/main.rs"))
            .unwrap()
            .contains("v2")
    );
    assert!(!t.join("tests/fixtures/run.log").exists());
}

// R-406: the origin's own output link never travels, even when .gitignore says `target/` (which
// does not match a symlink).
#[test]
fn r406_the_origins_output_link_is_not_shipped() {
    let w = World::new();
    let cache = w.tmp.path().join("origin-cache/target");
    write(&cache, "debug/app", "origin artifact");
    std::os::unix::fs::symlink(&cache, w.root.join("target")).unwrap();
    // a symlink that is not an output link still travels (control)
    std::os::unix::fs::symlink("src/main.rs", w.root.join("main-link")).unwrap();
    push(&w, &w.root, 1);
    let t = tree(&w, &w.root);
    assert!(
        fs::symlink_metadata(t.join("target")).is_err(),
        "no target on the worker"
    );
    assert!(
        fs::symlink_metadata(t.join("main-link"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(cache.join("debug/app")).unwrap(),
        "origin artifact"
    );
}

// R-406: retention deletes the idle working copy, the link with it, and never what it points at.
#[test]
fn r406_retention_of_the_copy_leaves_the_cache_directory() {
    let w = World::new();
    let clock = Arc::new(AtomicI64::new(1_000 * 86_400_000));
    let c = clock.clone();
    let store = Arc::new(
        SyncStore::new(
            w.tmp.path().join("ret-store"),
            SyncLimits::default(),
            Arc::new(GixTransfer::new()),
        )
        .with_clock(Arc::new(move || c.load(Ordering::SeqCst))),
    );
    w.serve_store(store.clone());
    push(&w, &w.root, 1);
    let t = store.tree_root(&w.origin, &CheckoutId::derive("laptop", &w.root));
    let cache = link_into_cache(&w, &t);
    clock.fetch_add(8 * 86_400_000, Ordering::SeqCst);
    let free = |_: &PeerId, _: &Path| false;
    let events = store.sweep(&WorkerRetention::default(), &free);
    assert!(!events.is_empty());
    assert!(!t.exists(), "the working copy is gone");
    assert_eq!(
        fs::read_to_string(cache.join("debug/app")).unwrap(),
        "warm artifact",
        "the cache directory is not"
    );
}
