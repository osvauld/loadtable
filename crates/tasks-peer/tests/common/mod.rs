//! Shared fixtures: an origin checkout on disk, a fake nixos peer and a manual clock.
#![allow(dead_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tasks_core::{JobSpec, Kind, MachineName, Owner, Placement, Requirements};
use tasks_peer::fake::{FakeNet, FakeRunner, FakeStatus, FakeTransport};
use tasks_peer::outputs::FetchPolicy;
use tasks_peer::receiver::{AllowAll, Allowlist, Receiver, SubmitPolicy};
use tasks_peer::sync::{GixSnapshot, GixTransfer, OriginSync, SyncLimits, SyncStore, testrepo};
use tasks_peer::{CheckoutLocks, Forward, ForwardIo, ForwardJob, ForwardPolicy, Handler, PeerId};

pub struct World {
    pub tmp: tempfile::TempDir,
    pub net: FakeNet,
    pub runner: Arc<FakeRunner>,
    pub store: Arc<SyncStore>,
    /// The origin side of sync: gix snapshots, thin packs over the fake network.
    pub origin_sync: Arc<OriginSync>,
    pub origin_state: PathBuf,
    pub origin: PeerId,
    pub nixos: PeerId,
    pub transport: FakeTransport,
    pub locks: Arc<CheckoutLocks>,
    pub root: PathBuf,
}

pub fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(p, body).unwrap();
}

/// A committed git repository with the three sample files, at `root`.
pub fn make_repo(root: &Path) {
    fs::create_dir_all(root).unwrap();
    testrepo::init_repo(root).unwrap();
    write(root, "Cargo.toml", "[package]\n");
    write(root, "src/main.rs", "fn main() {}\n");
    write(root, "tests/fixtures/run.log", "fixture log line\n");
    write(root, ".gitignore", "target/\n");
    testrepo::commit_all(root, "initial").unwrap();
}

impl World {
    pub fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("origin/checkout");
        make_repo(&root);
        let origin_state = tmp.path().join("origin-state");
        let origin_sync = Arc::new(OriginSync::new(
            Arc::new(GixSnapshot::new(&origin_state)),
            Arc::new(GixTransfer::new()),
            SyncLimits::default(),
            &origin_state,
        ));
        let net = FakeNet::new();
        let origin = PeerId::new("laptop-node").unwrap();
        let nixos = PeerId::new("nixos-node").unwrap();
        let store = Arc::new(SyncStore::new(
            tmp.path().join("peer-store"),
            SyncLimits::default(),
            Arc::new(GixTransfer::new()),
        ));
        let runner = Arc::new(FakeRunner::new());
        let w = World {
            transport: net.transport(&origin),
            net,
            runner,
            store,
            origin_sync,
            origin_state,
            origin,
            nixos,
            locks: Arc::new(CheckoutLocks::new()),
            root,
            tmp,
        };
        w.restart_receiver(Arc::clone(&w.runner));
        w
    }

    pub fn receiver_with(
        &self,
        runner: Arc<FakeRunner>,
        policy: Arc<dyn SubmitPolicy>,
        fetch: FetchPolicy,
    ) -> Arc<Receiver> {
        Arc::new(Receiver::new(
            "nixos",
            Allowlist::new([self.origin.clone()]),
            Arc::clone(&self.store),
            runner,
            Arc::new(FakeStatus::nixos()),
            policy,
            fetch,
        ))
    }

    /// Replaces the daemon behind `nixos` with one that keeps its copies in `store`.
    pub fn serve_store(&self, store: Arc<SyncStore>) {
        let r = Receiver::new(
            "nixos",
            Allowlist::new([self.origin.clone()]),
            store,
            Arc::clone(&self.runner) as Arc<dyn tasks_peer::JobRunner>,
            Arc::new(FakeStatus::nixos()),
            Arc::new(AllowAll),
            FetchPolicy::default(),
        );
        self.net.serve(&self.nixos, Arc::new(r) as Arc<dyn Handler>);
    }

    /// Replaces the daemon behind `nixos` (a restart) over the given runner.
    pub fn restart_receiver(&self, runner: Arc<FakeRunner>) {
        let r = self.receiver_with(runner, Arc::new(AllowAll), FetchPolicy::default());
        self.net.serve(&self.nixos, r as Arc<dyn Handler>);
    }

    pub fn spec(&self, rel_cwd: &str) -> JobSpec {
        JobSpec {
            owner: Owner::new("tester").unwrap(),
            argv: vec!["cargo".into(), "test".into()],
            cwd: self.root.join(rel_cwd),
            env: vec![],
            kind: Kind::Test,
            requirements: Requirements::default(),
            placement: Placement::Peer(MachineName("nixos".into())),
            after: vec![],
            timeout_secs: None,
            broad_test_escape: false,
            priority: tasks_core::Priority::Normal,
            priority_reason: None,
        }
    }

    pub fn forward(&self, id: i64, policy: ForwardPolicy) -> Forward {
        self.forward_at(id, &self.root, policy)
    }

    pub fn forward_at(&self, id: i64, root: &Path, policy: ForwardPolicy) -> Forward {
        let job = ForwardJob {
            local_id: id,
            spec: JobSpec {
                cwd: root.to_path_buf(),
                ..self.spec("")
            },
            root: root.to_path_buf(),
            origin_host: "laptop".into(),
            peer: self.nixos.clone(),
            key: ForwardJob::default_key("laptop", id).unwrap(),
        };
        Forward::new(job, policy, Arc::clone(&self.locks), Duration::ZERO)
    }

    pub fn tick(&self, f: &mut Forward, now: Duration, log: &mut Vec<u8>) {
        let mut io = ForwardIo {
            transport: &self.transport,
            sync: &*self.origin_sync,
            log,
        };
        f.tick(now, &mut io);
    }

    /// Ticks once per second until `done` or `max` ticks. Returns the clock.
    pub fn run(
        &self,
        f: &mut Forward,
        log: &mut Vec<u8>,
        clock: &mut Duration,
        max: usize,
        mut each: impl FnMut(&World, &Forward),
    ) {
        for _ in 0..max {
            if f.is_done() {
                return;
            }
            *clock += Duration::from_secs(1);
            self.tick(f, *clock, log);
            each(self, f);
        }
    }
}

pub fn fast_policy() -> ForwardPolicy {
    ForwardPolicy {
        reattach_interval: Duration::from_secs(1),
        lost_grace: Duration::from_secs(30),
        cancel_grace: Duration::from_secs(10),
        stream_idle: Duration::from_secs(20),
        ..ForwardPolicy::default()
    }
}
