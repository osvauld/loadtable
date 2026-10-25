//! Fakes and a rig for the scheduler tests.

use crate::audited::{AuditSink, AuditedLedger};
use crate::config::DaemonConfig;
use crate::forwarder::{
    ForwardError, ForwardPoll, ForwardProgress, ForwardTicket, Forwarder, LinkReport, NoForwarder,
};
use crate::holders::{Holder, HolderSource};
use crate::machine_policy::MachinePolicy;
use crate::platform::{Ledger, Platform};
use crate::scheduler::{Scheduler, SchedulerParts, lock};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tasks_core::{JobId, JobSpec, Kind, MachineName, Owner, Placement, Requirements, SlotKind};
use tasks_ledger::{AuditLog, JobRecord, ManualClock, MemoryAudit, MemoryStore, Store};
use tasks_placement::MachineView;
use tasks_platform::{
    Child, Disk, FlockSlots, GroupId, Level, Memory, MemorySnapshot, PlatformError, Pressure,
    PressureSnapshot, PressureThresholds, Processes, Signal, SpawnSpec, Usage,
};

/// A directory removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "tasks-daemon-t-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Settable memory, pressure and disk readings.
pub struct FakeMachine {
    pub state: Mutex<MachineState>,
}

pub struct MachineState {
    pub memory: MemorySnapshot,
    pub pressure: PressureSnapshot,
    pub disk_free: u64,
}

impl FakeMachine {
    pub fn new() -> FakeMachine {
        FakeMachine {
            state: Mutex::new(MachineState {
                memory: MemorySnapshot {
                    total_bytes: 16 << 30,
                    available_bytes: 8 << 30,
                    ..MemorySnapshot::default()
                },
                pressure: PressureSnapshot {
                    memory: Level::Normal,
                    memory_some: None,
                    memory_full: None,
                    cpu: Some(5.0),
                    io: Some(5.0),
                },
                disk_free: 100 << 30,
            }),
        }
    }
    pub fn set_disk(&self, free: u64) {
        lock(&self.state).disk_free = free;
    }
    pub fn set_io(&self, pct: f32) {
        lock(&self.state).pressure.io = Some(pct);
    }
    pub fn set_memory_some(&self, pct: Option<f32>) {
        lock(&self.state).pressure.memory_some = pct;
    }
    pub fn set_memory_level(&self, l: Level) {
        lock(&self.state).pressure.memory = l;
    }
}

impl Memory for FakeMachine {
    fn snapshot(&self) -> Result<MemorySnapshot, PlatformError> {
        Ok(lock(&self.state).memory)
    }
}
impl Pressure for FakeMachine {
    fn snapshot(&self, _: &PressureThresholds) -> Result<PressureSnapshot, PlatformError> {
        Ok(lock(&self.state).pressure)
    }
}
impl Disk for FakeMachine {
    fn free_bytes(&self, _: &Path) -> Result<u64, PlatformError> {
        Ok(lock(&self.state).disk_free)
    }
}

/// Scripted process groups.
#[derive(Default)]
pub struct FakeProcs {
    inner: Mutex<ProcState>,
}

#[derive(Default)]
struct ProcState {
    next: i32,
    procs: BTreeMap<i32, Proc>,
    spawned: Vec<SpawnSpec>,
    signals: Vec<(i32, Signal)>,
    auto_exit: Option<i32>,
    output: Option<Vec<u8>>,
    /// Per spawn, in order: the log text and the exit code; used before `auto_exit`/`output`.
    script: std::collections::VecDeque<(Vec<u8>, i32)>,
    fail_spawn: Option<String>,
}

struct Proc {
    start: u64,
    exit: Option<i32>,
    cpu: f64,
}

pub const START: u64 = 7;

impl FakeProcs {
    pub fn new() -> FakeProcs {
        FakeProcs::default()
    }
    /// Every spawned process exits at once with this code.
    pub fn auto_exit(&self, code: i32) {
        lock(&self.inner).auto_exit = Some(code);
    }
    /// Text appended to the log of every spawned process.
    pub fn output(&self, bytes: &[u8]) {
        lock(&self.inner).output = Some(bytes.to_vec());
    }
    /// The next spawns, in order, each write this text to its log and exit with this code at
    /// once; after the script the `auto_exit` and `output` settings apply again.
    pub fn script_spawns(&self, spawns: &[(&[u8], i32)]) {
        lock(&self.inner)
            .script
            .extend(spawns.iter().map(|(t, c)| (t.to_vec(), *c)));
    }
    pub fn fail_spawn(&self, why: &str) {
        lock(&self.inner).fail_spawn = Some(why.to_string());
    }
    pub fn spawned(&self) -> Vec<SpawnSpec> {
        lock(&self.inner).spawned.clone()
    }
    pub fn spawn_count(&self) -> usize {
        lock(&self.inner).spawned.len()
    }
    pub fn signals(&self) -> Vec<(i32, Signal)> {
        lock(&self.inner).signals.clone()
    }
    /// The pgid of the n-th spawned process (0-based).
    pub fn pgid(&self, n: usize) -> i32 {
        1000 + i32::try_from(n).unwrap()
    }
    pub fn exit(&self, pgid: i32, code: i32) {
        if let Some(p) = lock(&self.inner).procs.get_mut(&pgid) {
            p.exit = Some(code);
        }
    }
    pub fn set_cpu(&self, pgid: i32, cpu: f64) {
        if let Some(p) = lock(&self.inner).procs.get_mut(&pgid) {
            p.cpu = cpu;
        }
    }
    /// A group that already exists (it survived a daemon restart).
    pub fn survivor(&self, pgid: i32, start: u64) {
        lock(&self.inner).procs.insert(
            pgid,
            Proc {
                start,
                exit: None,
                cpu: 0.0,
            },
        );
    }
}

impl Processes for FakeProcs {
    fn spawn_group(&self, cmd: &SpawnSpec) -> Result<Child, PlatformError> {
        let mut st = lock(&self.inner);
        if let Some(why) = &st.fail_spawn {
            return Err(PlatformError::InvalidSpawn(why.clone()));
        }
        let pgid = 1000 + st.next;
        st.next += 1;
        let scripted = st.script.pop_front();
        let exit = scripted.as_ref().map(|s| s.1).or(st.auto_exit);
        st.procs.insert(
            pgid,
            Proc {
                start: START,
                exit,
                cpu: 0.0,
            },
        );
        st.spawned.push(cmd.clone());
        let out = scripted.map(|s| s.0).or_else(|| st.output.clone());
        if let Some(out) = &out {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&cmd.log_path)
                .map_err(|e| PlatformError::io(&cmd.log_path, e))?;
            f.write_all(out)
                .map_err(|e| PlatformError::io(&cmd.log_path, e))?;
        }
        Ok(Child {
            pid: pgid,
            group: GroupId {
                pgid,
                start_time: START,
            },
        })
    }
    fn signal_group(&self, group: GroupId, sig: Signal) -> Result<(), PlatformError> {
        let mut st = lock(&self.inner);
        let live = st
            .procs
            .get(&group.pgid)
            .is_some_and(|p| p.start == group.start_time && p.exit.is_none());
        if !live {
            return Err(PlatformError::Gone { pgid: group.pgid });
        }
        st.signals.push((group.pgid, sig));
        if let Some(p) = st.procs.get_mut(&group.pgid) {
            match sig {
                Signal::Term => p.exit = Some(143),
                Signal::Kill => p.exit = Some(137),
                _ => {}
            }
        }
        Ok(())
    }
    fn usage(&self, group: GroupId) -> Result<Usage, PlatformError> {
        let st = lock(&self.inner);
        match st.procs.get(&group.pgid) {
            Some(p) if p.start == group.start_time && p.exit.is_none() => Ok(Usage {
                cpu_user_s: p.cpu,
                cpu_sys_s: 0.0,
                rss_bytes: 0,
                processes: 1,
            }),
            _ => Err(PlatformError::Gone { pgid: group.pgid }),
        }
    }
    fn peek_exit(&self, child: &Child) -> Result<Option<i32>, PlatformError> {
        Ok(lock(&self.inner)
            .procs
            .get(&child.group.pgid)
            .and_then(|p| p.exit))
    }
    fn try_wait(&self, child: &Child) -> Result<Option<i32>, PlatformError> {
        self.peek_exit(child)
    }
}

/// Holders returned for every busy slot lock.
#[derive(Default)]
pub struct FakeHolders(pub Mutex<Vec<Holder>>);

impl HolderSource for FakeHolders {
    fn holders(&self, _: &Path) -> Vec<Holder> {
        lock(&self.0).clone()
    }
}

/// A forwarder with scripted peers and scripted progress.
pub struct FakeForwarder {
    pub peers: Mutex<Vec<MachineView>>,
    pub forwarded: Mutex<Vec<(JobId, String)>>,
    pub fail: Mutex<bool>,
    pub registered: Mutex<BTreeSet<JobId>>,
    /// Jobs `ready_to_start` says no to.
    pub not_ready: Mutex<BTreeSet<JobId>>,
    pub started: Mutex<Vec<JobId>>,
    pub resumed: Mutex<Vec<JobId>>,
    /// What `poll` answers per job; a job with no script is `Active` and not accepted.
    pub script: Mutex<BTreeMap<JobId, ForwardPoll>>,
    pub cancelled: Mutex<Vec<(JobId, String)>>,
    pub remote_cancelled: Mutex<Vec<(String, i64)>>,
    pub cancel_remote_fails: Mutex<bool>,
    pub links: Mutex<Vec<LinkReport>>,
    pub quality: Mutex<Vec<crate::forwarder::LinkQuality>>,
    /// The classes the scheduler told the forwarder before starting a job.
    pub classes: Mutex<Vec<(JobId, tasks_core::Priority, Option<String>)>>,
    /// What `remote_metrics` answers, and what it was asked.
    pub metrics: Mutex<Option<tasks_core::metrics::JobMetricsView>>,
    pub metrics_asked: Mutex<Vec<(String, i64)>>,
}

impl FakeForwarder {
    pub fn new(peers: Vec<MachineView>) -> FakeForwarder {
        FakeForwarder {
            peers: Mutex::new(peers),
            forwarded: Mutex::new(Vec::new()),
            fail: Mutex::new(false),
            registered: Mutex::new(BTreeSet::new()),
            not_ready: Mutex::new(BTreeSet::new()),
            started: Mutex::new(Vec::new()),
            resumed: Mutex::new(Vec::new()),
            script: Mutex::new(BTreeMap::new()),
            cancelled: Mutex::new(Vec::new()),
            remote_cancelled: Mutex::new(Vec::new()),
            cancel_remote_fails: Mutex::new(false),
            links: Mutex::new(Vec::new()),
            quality: Mutex::new(Vec::new()),
            classes: Mutex::new(Vec::new()),
            metrics: Mutex::new(None),
            metrics_asked: Mutex::new(Vec::new()),
        }
    }
    pub fn script(&self, id: JobId, p: ForwardPoll) {
        lock(&self.script).insert(id, p);
    }
}

impl Forwarder for FakeForwarder {
    fn peers(&self) -> Vec<MachineView> {
        lock(&self.peers).clone()
    }
    fn links(&self) -> Vec<LinkReport> {
        lock(&self.links).clone()
    }
    fn link_quality(&self) -> Vec<crate::forwarder::LinkQuality> {
        lock(&self.quality).clone()
    }
    fn forward(&self, job: &JobRecord, peer: &MachineName) -> Result<ForwardTicket, ForwardError> {
        if *lock(&self.fail) {
            return Err(ForwardError::Unavailable("link down".into()));
        }
        lock(&self.forwarded).push((job.id, peer.0.clone()));
        lock(&self.registered).insert(job.id);
        Ok(ForwardTicket {
            remote_job: job.forward.as_ref().and_then(|f| f.remote_job),
            remote_key: Some(
                job.forward
                    .as_ref()
                    .and_then(|f| f.remote_key.clone())
                    .unwrap_or_else(|| format!("testbox:{}", job.id.0)),
            ),
        })
    }
    fn registered(&self, job: JobId) -> bool {
        lock(&self.registered).contains(&job)
    }
    fn set_class(&self, job: JobId, priority: tasks_core::Priority, reason: Option<String>) {
        lock(&self.classes).push((job, priority, reason));
    }
    fn ready_to_start(&self, job: JobId) -> bool {
        !lock(&self.not_ready).contains(&job)
    }
    fn wait_reason(&self, job: JobId) -> Option<String> {
        lock(&self.not_ready)
            .contains(&job)
            .then(|| "waiting for checkout: job #0 is using the remote copy".to_string())
    }
    fn start(&self, job: JobId, _log: &std::path::Path) -> Result<(), ForwardError> {
        lock(&self.started).push(job);
        Ok(())
    }
    fn resume(&self, job: &JobRecord, _log: &std::path::Path) -> Result<(), ForwardError> {
        lock(&self.resumed).push(job.id);
        Ok(())
    }
    fn poll(&self, job: JobId) -> ForwardPoll {
        lock(&self.script)
            .get(&job)
            .cloned()
            .unwrap_or(ForwardPoll::Active(ForwardProgress {
                mirror: "syncing checkout".into(),
                remote_job: None,
                accepted: false,
                remote_phase: None,
            }))
    }
    fn cancel(&self, job: JobId, reason: &str) {
        lock(&self.cancelled).push((job, reason.to_string()));
    }
    fn cancel_remote(&self, peer: &MachineName, remote: i64) -> Result<(), ForwardError> {
        if *lock(&self.cancel_remote_fails) {
            return Err(ForwardError::Unavailable("unreachable".into()));
        }
        lock(&self.remote_cancelled).push((peer.0.clone(), remote));
        Ok(())
    }
    fn remote_metrics(
        &self,
        peer: &MachineName,
        remote: i64,
    ) -> Result<tasks_core::metrics::JobMetricsView, ForwardError> {
        lock(&self.metrics_asked).push((peer.0.clone(), remote));
        lock(&self.metrics)
            .clone()
            .ok_or_else(|| ForwardError::Unavailable("no metrics scripted".into()))
    }
    fn forget(&self, job: JobId) {
        lock(&self.registered).remove(&job);
    }
}

/// Everything a scheduler test needs.
pub struct Rig {
    pub dir: TempDir,
    pub sch: Arc<Scheduler>,
    pub store: Arc<MemoryStore>,
    pub procs: Arc<FakeProcs>,
    pub machine: Arc<FakeMachine>,
    pub clock: Arc<ManualClock>,
    pub holders: Arc<FakeHolders>,
    pub cfg: DaemonConfig,
    pub platform: Platform,
    /// The store behind the audited ledger the scheduler runs on.
    pub ledger: Arc<dyn Ledger>,
    /// The audit sink (a signing log over [`Rig::audit_store`] on the rig's clock).
    pub audit: Arc<AuditSink>,
    pub audit_store: Arc<MemoryAudit>,
    /// The rig's cgroups: unavailable unless a test swaps the platform.
    pub cgroups: Arc<tasks_platform::fake::FakeCgroups>,
    /// The process table the rig reports.
    pub table: Arc<tasks_platform::fake::FakePlatform>,
    /// The scripted power source and sleep guard.
    pub power: Arc<tasks_platform::fake::FakePower>,
}

/// The identity the rig signs its audit checkpoints with.
pub fn rig_identity() -> Arc<tasks_identity::Identity> {
    Arc::new(tasks_identity::Identity::from_entropy(&[9u8; 32]).unwrap())
}

impl Rig {
    pub fn new() -> Rig {
        Rig::build("", Arc::new(NoForwarder), false)
    }
    /// Real process groups (`sh`, `sleep`); fake machine readings.
    pub fn native() -> Rig {
        Rig::build("", Arc::new(NoForwarder), true)
    }
    pub fn with_policy(policy: &str) -> Rig {
        Rig::build(policy, Arc::new(NoForwarder), false)
    }
    /// Fake cgroups that work, so jobs are placed in `tasks.slice/job-<id>`.
    pub fn delegated() -> Rig {
        Rig::build_with(
            "",
            Arc::new(NoForwarder),
            false,
            tasks_platform::fake::FakeCgroups::delegated(),
        )
    }
    pub fn with_forwarder(f: Arc<dyn Forwarder>) -> Rig {
        Rig::build("", f, false)
    }
    pub fn with_policy_forwarder(policy: &str, f: Arc<dyn Forwarder>) -> Rig {
        Rig::build(policy, f, false)
    }

    /// A second scheduler over the same ledger, slots and directories: the daemon after a
    /// restart. The first one is left to its own devices, as a crashed process would be.
    pub fn restart_with(&self, forwarder: Arc<dyn Forwarder>) -> Arc<Scheduler> {
        let sch = Scheduler::new(SchedulerParts {
            cfg: self.cfg.clone(),
            policy: MachinePolicy::from_text(None, &|_| None),
            ledger: Arc::new(AuditedLedger::new(self.store.clone(), self.audit.clone())),
            platform: self.platform.clone(),
            clock: self.clock.clone(),
            forwarder,
            parent_env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            execution_enabled: true,
            runner_poll: Duration::from_millis(2),
            transfer_gate: None,
        });
        sch.set_audit(self.audit.clone());
        sch
    }

    fn build(policy: &str, forwarder: Arc<dyn Forwarder>, native: bool) -> Rig {
        Rig::build_with(
            policy,
            forwarder,
            native,
            tasks_platform::fake::FakeCgroups::unavailable("test rig"),
        )
    }

    fn build_with(
        policy: &str,
        forwarder: Arc<dyn Forwarder>,
        native: bool,
        cgroups: tasks_platform::fake::FakeCgroups,
    ) -> Rig {
        let dir = TempDir::new("rig");
        let cfg = DaemonConfig {
            state_dir: dir.path().join("state"),
            config_dir: dir.path().join("config"),
            lock_dir: dir.path().join("state/slots"),
            cache_dir: dir.path().join("cache"),
            machine: "testbox".to_string(),
        };
        cfg.create_dirs().unwrap();
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let store = Arc::new(MemoryStore::with_clock(clock.clone()));
        let procs = Arc::new(FakeProcs::new());
        let machine = Arc::new(FakeMachine::new());
        let holders = Arc::new(FakeHolders::default());
        let cgroups = Arc::new(cgroups);
        let table = Arc::new(tasks_platform::fake::FakePlatform::default());
        let power = Arc::new(tasks_platform::fake::FakePower::default());
        let platform = Platform {
            memory: machine.clone(),
            pressure: machine.clone(),
            procs: if native {
                Arc::new(tasks_platform::native())
            } else {
                procs.clone()
            },
            disk: machine.clone(),
            locks: Arc::new(FlockSlots::new(&cfg.lock_dir)),
            holders: holders.clone(),
            cgroups: cgroups.clone(),
            table: table.clone(),
            power: power.clone(),
            sleep: power.clone(),
        };
        let policy = MachinePolicy::from_text(
            if policy.is_empty() {
                None
            } else {
                Some(policy)
            },
            &|_| None,
        );
        assert!(policy.problems.is_empty(), "{:?}", policy.problems);
        let audit_store = Arc::new(MemoryAudit::new());
        let log = AuditLog::new(audit_store.clone(), clock.clone(), "did:key:zTestbox")
            .with_signer(rig_identity());
        log.set_checkpoint_every(0);
        let audit = AuditSink::new(Arc::new(log));
        let ledger: Arc<dyn Ledger> = Arc::new(AuditedLedger::new(store.clone(), audit.clone()));
        let sch = Scheduler::new(SchedulerParts {
            cfg: cfg.clone(),
            policy,
            ledger: ledger.clone(),
            platform: platform.clone(),
            clock: clock.clone(),
            forwarder,
            parent_env: vec![
                (
                    "PATH".into(),
                    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
                ),
                // a service's HOME: its ~/.cargo/bin is searched for tools (R-543)
                ("HOME".into(), dir.path().join("home").display().to_string()),
            ],
            execution_enabled: true,
            runner_poll: Duration::from_millis(2),
            transfer_gate: None,
        });
        sch.set_audit(audit.clone());
        Rig {
            dir,
            sch,
            store,
            procs,
            machine,
            clock,
            holders,
            cfg,
            platform,
            ledger,
            audit,
            audit_store,
            cgroups,
            table,
            power,
        }
    }

    /// A job spec for `owner` running in the rig's directory.
    pub fn spec(&self, owner: &str, argv: &[&str], kind: Kind) -> JobSpec {
        JobSpec {
            owner: Owner::new(owner).unwrap(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: self.dir.path().to_path_buf(),
            env: vec![],
            kind,
            requirements: Requirements::default(),
            placement: Placement::Here,
            after: vec![],
            timeout_secs: None,
            broad_test_escape: false,
            script_escape: false,
            priority: tasks_core::Priority::Normal,
            priority_reason: None,
            fetch: Vec::new(),
            isolate: false,
            light: false,
            retry_alone: false,
        }
    }

    /// A spec working in its own directory, so it has its own checkout.
    pub fn spec_in(&self, owner: &str, argv: &[&str], kind: Kind, sub: &str) -> JobSpec {
        let mut s = self.spec(owner, argv, kind);
        s.cwd = self.dir.path().join(sub);
        std::fs::create_dir_all(&s.cwd).unwrap();
        s
    }

    pub fn submit(&self, spec: &JobSpec) -> JobId {
        let id = self.ledger.submit(spec).unwrap();
        // distinct queue times keep the queue order unambiguous
        self.clock.advance(10);
        id
    }

    pub fn tick(&self) -> crate::scheduler::TickReport {
        self.sch.tick()
    }

    pub fn phase(&self, id: JobId) -> tasks_core::Phase {
        self.store.get(id).unwrap().unwrap().phase
    }

    pub fn record(&self, id: JobId) -> JobRecord {
        self.store.get(id).unwrap().unwrap()
    }

    /// Polls `cond` for up to 5 s.
    pub fn until(&self, what: &str, cond: impl Fn() -> bool) {
        let t0 = Instant::now();
        while !cond() {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Waits until the job is running (its process spawned and recorded).
    pub fn until_running(&self, id: JobId) {
        self.until("job running", || {
            self.store.get(id).unwrap().unwrap().phase == tasks_core::Phase::Running
        });
    }

    /// Ticks and waits for the job to reach a terminal phase.
    pub fn until_done(&self, id: JobId) -> tasks_core::Phase {
        self.until("job done", || {
            self.sch.tick();
            self.store.get(id).unwrap().unwrap().phase.is_terminal()
        });
        // The terminal write is visible before the rows that follow it (the audit row, the
        // per-target samples) are written; a busy machine made tests that read them race.
        std::thread::sleep(Duration::from_millis(10));
        self.phase(id)
    }

    pub fn log_text(&self, id: JobId) -> String {
        std::fs::read_to_string(self.cfg.log_path(id)).unwrap_or_default()
    }
}

/// A worker daemon's side of the fake network: a receiver over a scripted runner, and a store
/// for synced copies. The origin side is a [`PeerLink`](crate::peer_link::PeerLink) over
/// [`PeerWorld::net`].
pub struct PeerWorld {
    pub dir: TempDir,
    pub net: tasks_peer::fake::FakeNet,
    pub runner: Arc<tasks_peer::fake::FakeRunner>,
    pub origin: tasks_peer::PeerId,
    pub nixos: tasks_peer::PeerId,
    /// A checkout with a few files, and a second one.
    pub root_a: PathBuf,
    pub root_b: PathBuf,
    /// The worker's store of synced copies: tests put build outputs into a copy's tree.
    pub store: Arc<tasks_peer::sync::SyncStore>,
    /// What the worker lets a peer fetch; `restart_worker` applies it.
    pub fetch_policy: Mutex<tasks_peer::outputs::FetchPolicy>,
}

/// A worker built before output fetch existed: it answers `fetch_list` and `fetch_chunk` the way
/// an older receiver answers a message kind it does not know, and everything else as `inner`.
struct NoFetch(Arc<tasks_peer::receiver::Receiver>);

impl tasks_peer::Handler for NoFetch {
    fn handle(
        &self,
        from: &tasks_peer::PeerId,
        msg: tasks_peer::proto::Body,
    ) -> tasks_peer::proto::Body {
        use tasks_peer::proto::Body;
        match msg {
            Body::FetchList { .. } | Body::FetchChunk { .. } => {
                Body::Refuse(tasks_peer::Refusal::UnknownMessage {
                    name: "fetch_list".into(),
                })
            }
            other => self.0.handle(from, other),
        }
    }
    fn events(
        &self,
        from: &tasks_peer::PeerId,
        remote: tasks_peer::RemoteJobId,
        from_offset: u64,
    ) -> Result<Box<dyn tasks_peer::transport::EventStream>, tasks_peer::Refusal> {
        self.0.events(from, remote, from_offset)
    }
    fn accept_upload(
        &self,
        from: &tasks_peer::PeerId,
        head: &tasks_peer::proto::Body,
    ) -> Result<(), tasks_peer::Refusal> {
        self.0.accept_upload(from, head)
    }
    fn upload(
        &self,
        from: &tasks_peer::PeerId,
        head: tasks_peer::proto::Body,
        data: &mut dyn std::io::Read,
    ) -> tasks_peer::proto::Body {
        self.0.upload(from, head, data)
    }
}

impl PeerWorld {
    pub fn new() -> PeerWorld {
        let dir = TempDir::new("peerworld");
        let mk = |name: &str| {
            use tasks_peer::sync::testrepo;
            let root = dir.path().join("origin").join(name);
            std::fs::create_dir_all(root.join("src")).unwrap();
            testrepo::init_repo(&root).unwrap();
            std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
            std::fs::write(
                root.join("src/main.rs"),
                format!("fn main() {{}} // {name}\n"),
            )
            .unwrap();
            testrepo::commit_all(&root, "initial").unwrap();
            root
        };
        let (root_a, root_b) = (mk("a"), mk("b"));
        let net = tasks_peer::fake::FakeNet::new();
        let runner = Arc::new(tasks_peer::fake::FakeRunner::new());
        let origin = tasks_peer::PeerId::new("laptop-node").unwrap();
        let nixos = tasks_peer::PeerId::new("nixos-node").unwrap();
        let store = Arc::new(tasks_peer::sync::SyncStore::new(
            dir.path().join("peer-store"),
            tasks_peer::sync::SyncLimits::default(),
            Arc::new(tasks_peer::sync::GixTransfer::new()),
        ));
        let w = PeerWorld {
            dir,
            net,
            runner,
            origin,
            nixos,
            root_a,
            root_b,
            store,
            fetch_policy: Mutex::new(tasks_peer::outputs::FetchPolicy::default()),
        };
        w.restart_worker();
        w
    }

    /// The worker's copy of the checkout at `root` (the origin's path) as the worker has it.
    pub fn worker_tree(&self, root: &Path) -> PathBuf {
        let id = crate::peer_link::checkout_of("laptop", root);
        self.store.tree_root(&self.origin, &id)
    }

    /// Replaces the daemon behind `nixos` with one that predates output fetch.
    pub fn restart_worker_without_fetch(&self) {
        self.net.serve(
            &self.nixos,
            Arc::new(NoFetch(Arc::new(self.receiver()))) as Arc<dyn tasks_peer::Handler>,
        );
    }

    /// Replaces the daemon behind `nixos` with a fresh receiver over the same runner and
    /// store, as after a restart of the worker.
    pub fn restart_worker(&self) {
        self.net.serve(
            &self.nixos,
            Arc::new(self.receiver()) as Arc<dyn tasks_peer::Handler>,
        );
    }

    /// A fresh receiver over the worker's runner and store, for tests that wrap it.
    pub fn receiver_for_test(&self) -> tasks_peer::receiver::Receiver {
        self.receiver()
    }

    fn receiver(&self) -> tasks_peer::receiver::Receiver {
        use tasks_peer::receiver::{AllowAll, Allowlist, Receiver};
        let status = tasks_peer::proto::PeerStatus {
            machine: tasks_core::Machine {
                name: MachineName("nixos".into()),
                os: tasks_core::Os::Linux,
                arch: tasks_core::Arch::X86_64,
                cores: 16,
                memory_bytes: 32 << 30,
                tools: Default::default(),
                devices: Default::default(),
            },
            slots: [SlotKind::Compile, SlotKind::Run, SlotKind::Offload]
                .into_iter()
                .map(|kind| tasks_peer::proto::SlotStatus {
                    kind,
                    total: 4,
                    used: 0,
                })
                .collect(),
            queue: Default::default(),
            pressure: Default::default(),
            jobs: Vec::new(),
            cores_in_use_milli: None,
            power: None,
        };
        Receiver::new(
            "nixos",
            Allowlist::new([self.origin.clone()]),
            self.store.clone(),
            self.runner.clone(),
            Arc::new(tasks_peer::fake::FakeStatus(status)),
            Arc::new(AllowAll),
            lock(&self.fetch_policy).clone(),
        )
    }

    /// Options for a link whose timeouts are short enough for a test.
    pub fn options(&self) -> crate::peer_link::PeerLinkOptions {
        use crate::peer_link::PeerLinkOptions;
        use tasks_peer::sync::{GixSnapshot, GixTransfer, OriginSync, SyncLimits};
        let state = self.dir.path().join("origin-state");
        let origin = OriginSync::new(
            Arc::new(GixSnapshot::new(&state)),
            Arc::new(GixTransfer::new()),
            SyncLimits::default(),
            &state,
        );
        let mut o = PeerLinkOptions::new(
            "laptop",
            vec![crate::peers_config::PeerSpec {
                name: "nixos".into(),
                node_id: self.nixos.clone(),
                did: String::new(),
            }],
            Arc::new(origin),
        );
        o.policy.reattach_interval = Duration::from_millis(5);
        o.policy.lost_grace = Duration::from_secs(30);
        o.policy.cancel_grace = Duration::from_secs(10);
        o.policy.stream_idle = Duration::from_secs(20);
        o.tick_every = Duration::from_millis(2);
        o.stale_after = Duration::from_secs(20);
        o.background_status = false;
        o
    }

    pub fn link(&self) -> crate::peer_link::PeerLink<tasks_peer::fake::FakeTransport> {
        crate::peer_link::PeerLink::new(self.net.transport(&self.origin), self.options())
    }

    /// How many requests of `kind` reached the worker.
    pub fn requests(&self, kind: &str) -> usize {
        self.net
            .requests()
            .iter()
            .filter(|(_, k)| k == kind)
            .count()
    }
}

/// A scripted [`TrustControl`](crate::trust_control::TrustControl).
#[derive(Default)]
pub struct StubTrust {
    pub peers: Mutex<Vec<crate::trust_control::PeerTrustView>>,
    pub expiring: Mutex<Vec<crate::trust_control::ExpiringView>>,
}

impl StubTrust {
    pub fn set_peers(&self, p: Vec<crate::trust_control::PeerTrustView>) {
        *lock(&self.peers) = p;
    }
    pub fn set_expiring(&self, e: Vec<crate::trust_control::ExpiringView>) {
        *lock(&self.expiring) = e;
    }
}

/// A peer view with the given roles each way, expiring at a fixed time.
pub fn peer_view(
    name: &str,
    did: &str,
    out: &[&str],
    inn: &[&str],
) -> crate::trust_control::PeerTrustView {
    use crate::trust_control::{PeerTrustView, RoleView};
    let roles = |v: &[&str]| {
        v.iter()
            .map(|r| RoleView {
                role: (*r).to_string(),
                expires_at: 2_000_000_000,
            })
            .collect()
    };
    PeerTrustView {
        name: name.into(),
        did: did.into(),
        node_id: format!("{name}-node"),
        roles_out: roles(out),
        roles_in: roles(inn),
        expires_at: Some(2_000_000_000),
        revoked: false,
        path: "unknown".into(),
        rtt_ms: None,
        reply_ms: None,
    }
}

impl crate::trust_control::TrustControl for StubTrust {
    fn identity(&self) -> crate::trust_control::IdentityView {
        crate::trust_control::IdentityView {
            did: "did:key:zSelf".into(),
            node_id: "self-node".into(),
            name: "self".into(),
        }
    }
    fn invite(
        &self,
        _roles: &[String],
        _both_ways: bool,
        _ttl: Option<u64>,
    ) -> Result<crate::trust_control::InviteView, String> {
        Err("stub".into())
    }
    fn accept(&self, _ticket: &str) -> Result<crate::trust_control::PairedView, String> {
        Err("stub".into())
    }
    fn reset(&self) -> Result<u64, String> {
        Ok(1)
    }
    fn peers(&self) -> Vec<crate::trust_control::PeerTrustView> {
        lock(&self.peers).clone()
    }
    fn revoke(&self, _name: &str) -> Result<crate::trust_control::RevokeView, String> {
        Err("stub".into())
    }
    fn expiring(&self) -> Vec<crate::trust_control::ExpiringView> {
        lock(&self.expiring).clone()
    }
    fn set_link_quality(&self, _q: Vec<crate::forwarder::LinkQuality>) {}
}
