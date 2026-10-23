//! Checkout sync end to end over the fake transport: real git repositories on both sides, the
//! origin's gix snapshot, a thin pack on the wire, the worker's mirror and working copy
//! (R-155 to R-176 carried over, R-300 to R-317).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;
use common::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tasks_peer::proto::{Body, MAX_CHUNK_BYTES, Refusal, SyncApply};
use tasks_peer::sync::testrepo::{commit_all, git};
use tasks_peer::sync::{
    ExtrasProvider, GixSnapshot, GixTransfer, OriginSync, SyncDriver, SyncError, SyncEvent,
    SyncGate, SyncJob, SyncLimits, SyncObserver, SyncPhase, SyncReport, SyncStore,
};
use tasks_peer::transport::{PeerError, Transport};
use tasks_peer::{CheckoutId, GitId, Handler, Hash, PeerId};

fn checkout(w: &World) -> CheckoutId {
    CheckoutId::derive("laptop", &w.root)
}

fn sync_with(
    w: &World,
    driver: &OriginSync,
    root: &Path,
    session: u64,
) -> Result<SyncReport, SyncError> {
    let mut conn = w
        .transport
        .connect(&w.nixos, std::time::Duration::from_secs(5))
        .unwrap();
    let id = CheckoutId::derive("laptop", root);
    driver.push(
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
}

fn sync(w: &World, session: u64) -> Result<SyncReport, SyncError> {
    sync_with(w, &w.origin_sync, &w.root, session)
}

fn tree(w: &World) -> PathBuf {
    w.store.tree_root(&w.origin, &checkout(w))
}

/// Files under `dir` (not `.git`, not in `skip`) with bytes and exec bit.
fn files(dir: &Path, skip: &[&str]) -> Vec<(String, Vec<u8>, bool)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let rel = e.path().strip_prefix(dir).unwrap().display().to_string();
            if rel == ".git"
                || skip
                    .iter()
                    .any(|s| rel == *s || rel.starts_with(&format!("{s}/")))
            {
                continue;
            }
            let m = fs::symlink_metadata(e.path()).unwrap();
            if m.is_dir() {
                stack.push(e.path());
            } else if m.file_type().is_symlink() {
                let t = fs::read_link(e.path()).unwrap();
                out.push((rel, t.display().to_string().into_bytes(), false));
            } else {
                out.push((
                    rel,
                    fs::read(e.path()).unwrap(),
                    m.permissions().mode() & 0o111 != 0,
                ));
            }
        }
    }
    out.sort();
    out
}

fn assert_same(w: &World, skip: &[&str]) {
    assert_eq!(files(&w.root, skip), files(&tree(w), skip));
}

fn pack_requests(w: &World) -> usize {
    w.net
        .requests()
        .iter()
        .filter(|(_, k)| k == "sync_pack_chunk" || k == "sync_pack_stream")
        .count()
}

fn kind_count(w: &World, kind: &str) -> usize {
    w.net.requests().iter().filter(|(_, k)| k == kind).count()
}

/// A daemon of the previous build: everything the receiver does except raw streams, which it
/// has never heard of (the trait defaults refuse them).
struct OldPeer(Arc<dyn Handler>);

impl Handler for OldPeer {
    fn handle(&self, from: &PeerId, msg: Body) -> Body {
        self.0.handle(from, msg)
    }
    fn events(
        &self,
        from: &PeerId,
        remote: tasks_peer::RemoteJobId,
        offset: u64,
    ) -> Result<Box<dyn tasks_peer::transport::EventStream>, Refusal> {
        self.0.events(from, remote, offset)
    }
}

#[test]
fn r155_first_sync_copies_every_file_including_log_fixtures() {
    let w = World::new();
    let r = sync(&w, 1).unwrap();
    assert_eq!(r.files, 4);
    assert_eq!(r.applied.written, 4);
    assert!(r.pack_bytes_sent > 0 && !r.already_there);
    assert_eq!(
        fs::read_to_string(tree(&w).join("tests/fixtures/run.log")).unwrap(),
        "fixture log line\n"
    );
    assert_same(&w, &[]);
    assert!(w.store.is_synced(&w.origin, &checkout(&w)));
}

#[test]
fn r300_uncommitted_deleted_and_renamed_files_arrive_and_build_outputs_stay_home() {
    let w = World::new();
    sync(&w, 1).unwrap();
    // uncommitted edit, a new untracked file in a new directory, a deletion and a rename
    write(&w.root, "src/main.rs", "fn main() { println!(); }\n");
    write(&w.root, "new/dir/added.txt", "added");
    fs::remove_file(w.root.join("tests/fixtures/run.log")).unwrap();
    fs::rename(w.root.join("Cargo.toml"), w.root.join("Cargo2.toml")).unwrap();
    // an ignored build output on the origin does not travel
    write(&w.root, "target/debug/app", "origin binary");
    let r = sync(&w, 2).unwrap();
    assert_eq!(
        r.dirty_files,
        5,
        "edit, add, delete, and a rename as delete + add: {}",
        r.line()
    );
    assert_same(&w, &["target"]);
    let t = tree(&w);
    assert!(!t.join("Cargo.toml").exists() && t.join("Cargo2.toml").exists());
    assert!(
        !t.join("tests").exists(),
        "an emptied directory goes with its last file"
    );
    assert!(!t.join("target").exists());
    assert_eq!(r.applied.removed, 2);
}

#[test]
fn r155_exec_bit_travels_and_a_mode_only_change_sends_no_blob() {
    let w = World::new();
    write(&w.root, "run.sh", "#!/bin/sh\n");
    commit_all(&w.root, "script").unwrap();
    sync(&w, 1).unwrap();
    assert_eq!(
        fs::metadata(tree(&w).join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    fs::set_permissions(w.root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    let r = sync(&w, 2).unwrap();
    assert_ne!(
        fs::metadata(tree(&w).join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert!(
        r.pack_bytes_sent < 2048,
        "a tree and a commit, no blob: {}",
        r.line()
    );
    fs::set_permissions(w.root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    sync(&w, 3).unwrap();
    assert_eq!(
        fs::metadata(tree(&w).join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
}

#[test]
fn r305_a_second_sync_of_the_same_tree_sends_nothing_and_reads_nothing() {
    let w = World::new();
    let first = sync(&w, 1).unwrap();
    let packs = pack_requests(&w);
    assert!(packs >= 1);
    let again = sync(&w, 2).unwrap();
    assert_eq!(again.tree, first.tree);
    assert!(again.already_there);
    assert_eq!(again.pack_bytes_sent, 0);
    assert_eq!(pack_requests(&w), packs, "no pack chunk on the wire");
    assert_eq!(
        (again.io.files_read, again.io.bytes_read),
        (0, 0),
        "{}",
        again.line()
    );
    assert_eq!(
        again.applied.written, 0,
        "the working copy was already right"
    );
    assert_eq!(again.applied.unchanged, 4);
    // the same holds across a restart of the worker's daemon
    w.restart_receiver(Arc::clone(&w.runner));
    assert!(sync(&w, 3).unwrap().already_there);
}

fn noise(n: usize) -> String {
    // incompressible-ish text lines
    let mut x = 0x2545_f491u32;
    let mut s = String::with_capacity(n);
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    };
    while s.len() < n {
        s.push_str(&format!("{:08x} {:08x} {:08x}\n", next(), next(), next()));
    }
    s
}

#[test]
fn r306_a_thin_pack_carries_only_the_change_to_a_worker_that_has_the_base() {
    let w = World::new();
    let big = noise(400 * 1024);
    write(&w.root, "data/big.txt", &big);
    commit_all(&w.root, "big").unwrap();
    let first = sync(&w, 1).unwrap();
    assert!(first.pack_bytes_sent > 100 * 1024, "{}", first.line());
    // one line of a 400 KiB file changes, uncommitted
    let edited = big.replacen("0", "1", 1);
    assert_ne!(edited, big);
    write(&w.root, "data/big.txt", &edited);
    let second = sync(&w, 2).unwrap();
    assert!(
        second.pack_bytes_sent < 10 * 1024,
        "a delta against the file the worker has, not the file: {}",
        second.line()
    );
    assert_same(&w, &[]);
    // a third state, again against the second
    write(&w.root, "data/big.txt", &edited.replacen("a", "b", 1));
    let third = sync(&w, 3).unwrap();
    assert!(third.pack_bytes_sent < 10 * 1024, "{}", third.line());
    assert_same(&w, &[]);
}

#[test]
fn r307_build_outputs_survive_files_a_job_edited_are_restored_and_removed_files_go() {
    let w = World::new();
    sync(&w, 1).unwrap();
    let t = tree(&w);
    write(&t, "target/debug/app", "worker binary");
    write(&t, "notes-from-the-job.txt", "keep me");
    // a job edited a tracked file on the worker
    fs::write(t.join("Cargo.toml"), "edited on the worker").unwrap();
    write(&w.root, "src/main.rs", "fn main() { /* v2 */ }\n");
    fs::remove_file(w.root.join("tests/fixtures/run.log")).unwrap();
    sync(&w, 2).unwrap();
    assert_eq!(
        fs::read_to_string(t.join("target/debug/app")).unwrap(),
        "worker binary"
    );
    assert_eq!(
        fs::read_to_string(t.join("notes-from-the-job.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(
        fs::read_to_string(t.join("Cargo.toml")).unwrap(),
        "[package]\n"
    );
    assert!(!t.join("tests/fixtures/run.log").exists());
    assert!(!t.join("tests").exists());
    assert!(
        fs::read_to_string(t.join("src/main.rs"))
            .unwrap()
            .contains("v2")
    );
}

#[test]
fn r176_sync_clears_what_is_in_the_way_without_walking_through_a_symlink() {
    let w = World::new();
    sync(&w, 1).unwrap();
    let t = tree(&w);
    // the worker's copy has an untracked file where the origin now has one, a directory where
    // the origin now has a file, and a symlink where the origin now needs a directory
    write(&t, "generated.txt", "a build output that is tracked now");
    write(&t, "report/inner.txt", "was a directory");
    let outside = w.tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    write(&outside, "escape.txt", "must survive");
    std::os::unix::fs::symlink(&outside, t.join("linked")).unwrap();
    write(&w.root, "generated.txt", "tracked content");
    write(&w.root, "report", "now a file");
    write(&w.root, "linked/escape.txt", "x");
    sync(&w, 2).unwrap();
    assert_eq!(
        fs::read_to_string(t.join("generated.txt")).unwrap(),
        "tracked content"
    );
    assert_eq!(fs::read_to_string(t.join("report")).unwrap(), "now a file");
    assert_eq!(
        fs::read_to_string(t.join("linked/escape.txt")).unwrap(),
        "x"
    );
    assert_eq!(
        fs::read_to_string(outside.join("escape.txt")).unwrap(),
        "must survive"
    );
    assert!(
        !fs::symlink_metadata(t.join("linked"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn r156_a_big_checkout_and_a_multi_chunk_pack_sync_without_frame_errors() {
    let w = World::new();
    for i in 0..3000 {
        write(
            &w.root,
            &format!("gen/module_{}/file_{i:05}.txt", i % 40),
            &format!("content {i} {}", noise(40)),
        );
    }
    write(&w.root, "assets/big.txt", &noise(MAX_CHUNK_BYTES * 3));
    write(&w.root, "empty.txt", "");
    let r = sync(&w, 1).unwrap();
    assert_eq!(r.files, 3000 + 4 + 2);
    // a new peer takes the pack as one raw stream: no chunk requests at all
    assert_eq!(kind_count(&w, "sync_pack_stream"), 1);
    assert_eq!(kind_count(&w, "sync_pack_chunk"), 0);
    assert!(r.pack_bytes_sent > (MAX_CHUNK_BYTES * 3) as u64);
    assert_same(&w, &[]);
}

// R-365
#[test]
fn r365_a_peer_that_does_not_know_streams_gets_the_pack_in_chunks_as_before() {
    let w = World::new();
    write(&w.root, "assets/big.txt", &noise(MAX_CHUNK_BYTES * 12));
    // the daemon behind nixos is the previous build
    let new = w.receiver_with(
        Arc::clone(&w.runner),
        Arc::new(tasks_peer::receiver::AllowAll),
        tasks_peer::outputs::FetchPolicy::default(),
    );
    w.net.serve(&w.nixos, Arc::new(OldPeer(new)));
    let r = sync(&w, 1).unwrap();
    assert!(r.pack_bytes_sent > MAX_CHUNK_BYTES as u64);
    assert!(kind_count(&w, "sync_pack_chunk") >= 2);
    // the stream was tried once, refused at its head, and nothing was sent on it
    assert_eq!(kind_count(&w, "sync_pack_stream"), 1);
    assert_same(&w, &[]);
}

// R-364
#[test]
fn r364_a_large_pack_over_a_slow_link_does_not_cost_a_round_trip_per_chunk() {
    let latency = std::time::Duration::from_millis(30);
    // one sync of a big pack on a slow link; (time, round trips, chunk requests)
    let run = |old_peer: bool| {
        let w = World::new();
        write(&w.root, "assets/big.txt", &noise(MAX_CHUNK_BYTES * 40));
        if old_peer {
            let new = w.receiver_with(
                Arc::clone(&w.runner),
                Arc::new(tasks_peer::receiver::AllowAll),
                tasks_peer::outputs::FetchPolicy::default(),
            );
            w.net.serve(&w.nixos, Arc::new(OldPeer(new)));
        }
        w.net.set_latency(latency);
        let before = w.net.round_trips();
        let t0 = std::time::Instant::now();
        sync(&w, 1).unwrap();
        (
            t0.elapsed(),
            w.net.round_trips() - before,
            kind_count(&w, "sync_pack_chunk"),
        )
    };
    let (streamed_time, streamed_trips, streamed_chunks) = run(false);
    let (chunked_time, chunked_trips, chunks) = run(true);
    assert_eq!(streamed_chunks, 0);
    assert!(chunks >= 6, "{chunks} chunks");
    // offer, stream open, stream end, apply: the same few round trips whatever the pack size
    assert!(streamed_trips <= 5, "{streamed_trips} round trips");
    // the chunked path pays one more per chunk
    assert!(
        chunked_trips >= streamed_trips + chunks as u64 - 1,
        "{chunked_trips} vs {streamed_trips} with {chunks} chunks"
    );
    // and it shows in the time: the chunk round trips alone are `chunks` x latency
    assert!(
        chunked_time >= latency * chunks as u32,
        "chunked took {chunked_time:?}"
    );
    assert!(
        streamed_time < chunked_time,
        "{streamed_time:?} vs {chunked_time:?}"
    );
}

// R-366
#[test]
fn r366_a_streamed_pack_over_the_workers_limit_is_cut_refused_and_leaves_no_file() {
    let w = World::new();
    let id = checkout(&w);
    let small = SyncStore::new(
        w.tmp.path().join("small-store"),
        SyncLimits {
            max_pack_bytes: 1000,
            ..SyncLimits::default()
        },
        Arc::new(GixTransfer::new()),
    );
    // exactly the limit is accepted, one more byte is not
    let ok = small.put_stream(
        &w.origin,
        &id,
        1,
        &mut std::io::Cursor::new(vec![7u8; 1000]),
    );
    assert_eq!(ok.unwrap(), 1000);
    let over = small.put_stream(
        &w.origin,
        &id,
        2,
        &mut std::io::Cursor::new(vec![7u8; 1001]),
    );
    assert!(
        matches!(over, Err(Refusal::LimitExceeded { .. })),
        "{over:?}"
    );
    let incoming = w
        .tmp
        .path()
        .join("small-store")
        .join(format!("laptop-node/{}/incoming", id));
    let left: Vec<_> = fs::read_dir(incoming)
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn r174_each_peer_has_its_own_copy_of_a_checkout() {
    let w = World::new();
    sync(&w, 1).unwrap();
    let id = checkout(&w);
    let other = tasks_peer::PeerId::new("someone-else").unwrap();
    assert!(w.store.is_synced(&w.origin, &id));
    assert!(!w.store.is_synced(&other, &id));
    assert_ne!(
        w.store.tree_root(&w.origin, &id),
        w.store.tree_root(&other, &id)
    );
}

#[test]
fn r317_two_checkouts_of_one_repo_share_the_workers_mirror_and_the_second_sends_the_difference() {
    let w = World::new();
    write(&w.root, "data/big.txt", &noise(300 * 1024));
    commit_all(&w.root, "big").unwrap();
    let first = sync(&w, 1).unwrap();
    let wt = w.tmp.path().join("origin/worktree");
    git(
        &w.root,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "other"],
    )
    .unwrap();
    write(&wt, "src/extra.rs", "pub fn extra() {}\n");
    let second = sync_with(&w, &w.origin_sync, &wt, 2).unwrap();
    assert!(first.pack_bytes_sent > 100 * 1024);
    assert!(second.pack_bytes_sent < 5 * 1024, "{}", second.line());
    let mirrors: Vec<_> = fs::read_dir(w.tmp.path().join("peer-store/laptop-node/mirrors"))
        .unwrap()
        .collect();
    assert_eq!(
        mirrors.len(),
        1,
        "one mirror for the repo, two working copies"
    );
    assert_eq!(
        fs::read_to_string(
            w.store
                .tree_root(&w.origin, &CheckoutId::derive("laptop", &wt))
                .join("src/extra.rs")
        )
        .unwrap(),
        "pub fn extra() {}\n"
    );
}

#[test]
fn r302_ignored_files_do_not_travel_unless_the_repo_policy_allowlists_them() {
    let w = World::new();
    write(&w.root, ".gitignore", "target/\nlocal.properties\n");
    commit_all(&w.root, "ignore").unwrap();
    write(&w.root, "local.properties", "sdk.dir=/opt/sdk\n");
    write(&w.root, "target/x", "out");
    sync(&w, 1).unwrap();
    assert!(!tree(&w).join("local.properties").exists());
    struct Policy;
    impl ExtrasProvider for Policy {
        fn extras(&self, _root: &Path) -> Result<Vec<String>, String> {
            Ok(vec!["local.properties".to_string()])
        }
    }
    let with = OriginSync::new(
        Arc::new(GixSnapshot::new(&w.origin_state)),
        Arc::new(GixTransfer::new()),
        SyncLimits::default(),
        &w.origin_state,
    )
    .with_extras(Arc::new(Policy));
    let r = sync_with(&w, &with, &w.root, 2).unwrap();
    assert_eq!(r.extra_files, ["local.properties"]);
    assert_eq!(r.applied.extras, 1);
    assert_eq!(
        fs::read_to_string(tree(&w).join("local.properties")).unwrap(),
        "sdk.dir=/opt/sdk\n"
    );
    assert!(!tree(&w).join("target").exists());
    // the file changes, and later leaves the allowlist: it is removed, build outputs stay
    write(&tree(&w), "target/worker-output", "kept");
    write(&w.root, "local.properties", "sdk.dir=/opt/other\n");
    sync_with(&w, &with, &w.root, 3).unwrap();
    assert_eq!(
        fs::read_to_string(tree(&w).join("local.properties")).unwrap(),
        "sdk.dir=/opt/other\n"
    );
    sync(&w, 4).unwrap();
    assert!(!tree(&w).join("local.properties").exists());
    assert!(tree(&w).join("target/worker-output").exists());
}

#[test]
fn r302_an_invalid_policy_stops_the_sync_by_name_instead_of_sending_less() {
    struct Bad;
    impl ExtrasProvider for Bad {
        fn extras(&self, _root: &Path) -> Result<Vec<String>, String> {
            Err("the repo run policy cannot be used: bad".to_string())
        }
    }
    let w = World::new();
    let d = OriginSync::new(
        Arc::new(GixSnapshot::new(&w.origin_state)),
        Arc::new(GixTransfer::new()),
        SyncLimits::default(),
        &w.origin_state,
    )
    .with_extras(Arc::new(Bad));
    let e = sync_with(&w, &d, &w.root, 1).unwrap_err();
    assert!(
        e.is_fatal() && e.to_string().contains("run policy cannot be used"),
        "{e}"
    );
    assert!(w.net.requests().is_empty(), "nothing reached the worker");
}

#[test]
fn r304_a_refusal_names_the_files_and_sends_nothing() {
    let w = World::new();
    write(
        &w.root,
        "models/net.bin",
        "version https://git-lfs.github.com/spec/v1\noid sha256:aa\nsize 9\n",
    );
    let e = sync(&w, 1).unwrap_err();
    assert!(e.is_fatal());
    let text = e.to_string();
    assert!(
        text.contains("models/net.bin") && text.contains("Git LFS"),
        "{text}"
    );
    assert!(w.net.requests().is_empty());
    assert!(!w.store.is_synced(&w.origin, &checkout(&w)));
}

fn conn(w: &World) -> Box<dyn tasks_peer::Connection> {
    w.transport
        .connect(&w.nixos, std::time::Duration::from_secs(5))
        .unwrap()
}

fn apply_req(w: &World, tree: &GitId, pack_bytes: u64, sha: Option<Hash>) -> Body {
    Body::SyncApply(SyncApply {
        checkout: checkout(w),
        repo: CheckoutId::derive("laptop", &w.root.join(".git")),
        session: 77,
        tree: tree.clone(),
        extras_tree: None,
        commit: None,
        base: None,
        dirty: 0,
        files: 1,
        pack_bytes,
        pack_sha256: sha,
    })
}

#[test]
fn r312_a_short_wrong_misordered_or_oversized_pack_is_refused_and_changes_nothing() {
    let w = World::new();
    sync(&w, 1).unwrap();
    let before = files(&tree(&w), &[]);
    let tree_id = GitId::parse(&"c".repeat(40)).unwrap();
    let id = checkout(&w);
    // a chunk that does not continue the pack
    let r = conn(&w).request(Body::SyncPackChunk {
        checkout: id.clone(),
        session: 77,
        offset: 5,
        data: vec![1],
    });
    assert!(
        matches!(
            r,
            Err(PeerError::Refused(Refusal::OutOfOrder { expected: 0 }))
        ),
        "{r:?}"
    );
    // the pack is shorter than announced
    conn(&w)
        .request(Body::SyncPackChunk {
            checkout: id.clone(),
            session: 77,
            offset: 0,
            data: vec![1, 2, 3],
        })
        .unwrap();
    let r = conn(&w).request(apply_req(&w, &tree_id, 10, Some(Hash::of(b"x"))));
    assert!(
        matches!(r, Err(PeerError::Refused(Refusal::BadPack { .. }))),
        "{r:?}"
    );
    // the right length but the wrong content
    let r = conn(&w).request(apply_req(&w, &tree_id, 3, Some(Hash::of(b"abc"))));
    assert!(
        matches!(r, Err(PeerError::Refused(Refusal::HashMismatch))),
        "{r:?}"
    );
    // no pack, and a tree the worker does not hold
    let r = conn(&w).request(apply_req(&w, &tree_id, 0, None));
    assert!(
        matches!(r, Err(PeerError::Refused(Refusal::MissingObjects { .. }))),
        "{r:?}"
    );
    // a pack git rejects
    conn(&w)
        .request(Body::SyncPackChunk {
            checkout: id.clone(),
            session: 78,
            offset: 0,
            data: b"not a pack".to_vec(),
        })
        .unwrap();
    let r = conn(&w).request(Body::SyncApply(SyncApply {
        session: 78,
        pack_bytes: 10,
        pack_sha256: Some(Hash::of(b"not a pack")),
        ..match apply_req(&w, &tree_id, 0, None) {
            Body::SyncApply(a) => a,
            _ => panic!("apply_req builds an apply"),
        }
    }));
    assert!(
        matches!(r, Err(PeerError::Refused(Refusal::BadPack { .. }))),
        "{r:?}"
    );
    assert_eq!(files(&tree(&w), &[]), before);
    assert!(
        w.store.is_synced(&w.origin, &id),
        "a refused sync leaves the last good copy usable"
    );
}

#[test]
fn r312_a_pack_over_the_workers_limit_is_refused() {
    let w = World::new();
    let small = SyncStore::new(
        w.tmp.path().join("small-store"),
        SyncLimits {
            max_pack_bytes: 1000,
            ..SyncLimits::default()
        },
        Arc::new(GixTransfer::new()),
    );
    let id = checkout(&w);
    let r = small.put_chunk(&w.origin, &id, 1, 0, &[0; 900]);
    assert_eq!(r.unwrap(), 900);
    let r = small.put_chunk(&w.origin, &id, 1, 900, &[0; 200]);
    assert!(matches!(r, Err(Refusal::LimitExceeded { .. })), "{r:?}");
    let origin_limited = OriginSync::new(
        Arc::new(GixSnapshot::new(&w.origin_state)),
        Arc::new(GixTransfer::new()),
        SyncLimits {
            max_pack_bytes: 100,
            ..SyncLimits::default()
        },
        &w.origin_state,
    );
    let e = sync_with(&w, &origin_limited, &w.root, 1).unwrap_err();
    assert!(
        e.is_fatal() && e.to_string().contains("100 byte limit"),
        "{e}"
    );
}

#[derive(Default)]
struct Events(Mutex<Vec<SyncEvent>>);
impl SyncObserver for Events {
    fn event(&self, e: &SyncEvent) {
        self.0.lock().unwrap().push(e.clone());
    }
}

#[test]
fn r311_sent_and_received_syncs_are_reported_with_tree_and_bytes() {
    let w = World::new();
    let ev = Arc::new(Events::default());
    let origin = OriginSync::new(
        Arc::new(GixSnapshot::new(&w.origin_state)),
        Arc::new(GixTransfer::new()),
        SyncLimits::default(),
        &w.origin_state,
    )
    .with_observer(ev.clone());
    let store = Arc::new(
        SyncStore::new(
            w.tmp.path().join("observed-store"),
            SyncLimits::default(),
            Arc::new(GixTransfer::new()),
        )
        .with_observer(ev.clone()),
    );
    w.net.serve(
        &w.nixos,
        Arc::new(tasks_peer::Receiver::new(
            "nixos",
            tasks_peer::Allowlist::open(),
            store,
            Arc::clone(&w.runner) as Arc<dyn tasks_peer::JobRunner>,
            Arc::new(tasks_peer::fake::FakeStatus::nixos()),
            Arc::new(tasks_peer::receiver::AllowAll),
            tasks_peer::outputs::FetchPolicy::default(),
        )) as Arc<dyn tasks_peer::Handler>,
    );
    let r = sync_with(&w, &origin, &w.root, 1).unwrap();
    write(
        &w.root,
        "models/net.bin",
        "version https://git-lfs.github.com/spec/v1\noid sha256:aa\nsize 9\n",
    );
    assert!(sync_with(&w, &origin, &w.root, 2).is_err());
    let all = ev.0.lock().unwrap().clone();
    assert!(all.iter().any(|e| matches!(e,
        SyncEvent::Sent { tree, bytes, dirty, job: Some(1), .. } if *tree == r.tree && *bytes == r.pack_bytes_sent && *dirty == 0)), "{all:?}");
    assert!(all.iter().any(|e| matches!(e,
        SyncEvent::Received { tree, bytes, written: 4, removed: 0, .. } if *tree == r.tree && *bytes == r.pack_bytes_sent)), "{all:?}");
    assert!(all.iter().any(|e| matches!(e,
        SyncEvent::Refused { files, .. } if files.iter().any(|(p, c)| p == "models/net.bin" && *c == "lfs_pointer"))), "{all:?}");
}

#[derive(Default)]
struct CountingGate(Mutex<Vec<SyncPhase>>);
impl SyncGate for CountingGate {
    fn enter(&self, phase: SyncPhase) -> Box<dyn Send> {
        self.0.lock().unwrap().push(phase);
        Box::new(())
    }
}

#[test]
fn r313_every_sync_phase_asks_the_io_gate_first() {
    let w = World::new();
    let gate = Arc::new(CountingGate::default());
    let origin = OriginSync::new(
        Arc::new(GixSnapshot::new(&w.origin_state)),
        Arc::new(GixTransfer::new()),
        SyncLimits::default(),
        &w.origin_state,
    )
    .with_gate(gate.clone());
    let store = Arc::new(
        SyncStore::new(
            w.tmp.path().join("gated-store"),
            SyncLimits::default(),
            Arc::new(GixTransfer::new()),
        )
        .with_gate(gate.clone()),
    );
    w.net.serve(
        &w.nixos,
        Arc::new(tasks_peer::Receiver::new(
            "nixos",
            tasks_peer::Allowlist::open(),
            store,
            Arc::clone(&w.runner) as Arc<dyn tasks_peer::JobRunner>,
            Arc::new(tasks_peer::fake::FakeStatus::nixos()),
            Arc::new(tasks_peer::receiver::AllowAll),
            tasks_peer::outputs::FetchPolicy::default(),
        )) as Arc<dyn tasks_peer::Handler>,
    );
    sync_with(&w, &origin, &w.root, 1).unwrap();
    assert_eq!(
        *gate.0.lock().unwrap(),
        [SyncPhase::Snapshot, SyncPhase::Pack, SyncPhase::Apply]
    );
    // nothing to pack the second time, so no pack phase
    gate.0.lock().unwrap().clear();
    sync_with(&w, &origin, &w.root, 2).unwrap();
    assert_eq!(
        *gate.0.lock().unwrap(),
        [SyncPhase::Snapshot, SyncPhase::Apply]
    );
}

#[test]
fn a_changed_file_that_vanishes_mid_sync_is_not_an_error_for_a_later_sync() {
    // files deleted between two syncs are simply absent from the second tree
    let w = World::new();
    write(&w.root, "tmp/scratch.txt", "x");
    sync(&w, 1).unwrap();
    fs::remove_file(w.root.join("tmp/scratch.txt")).unwrap();
    let r = sync(&w, 2).unwrap();
    assert_eq!(r.applied.removed, 1);
    assert!(!tree(&w).join("tmp").exists());
}
