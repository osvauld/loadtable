//! Fakes and a rig for the scheduler tests.

use crate::config::DaemonConfig;
use crate::forwarder::{ForwardError, ForwardTicket, Forwarder, NoForwarder};
use crate::holders::{Holder, HolderSource};
use crate::machine_policy::MachinePolicy;
use crate::platform::{Ledger, Platform};
use crate::scheduler::{Scheduler, SchedulerParts, lock};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tasks_core::{JobId, JobSpec, Kind, MachineName, Owner, Placement, Requirements};
use tasks_ledger::{JobRecord, ManualClock, MemoryStore, Store};
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
        let exit = st.auto_exit;
        st.procs.insert(
            pgid,
            Proc {
                start: START,
                exit,
                cpu: 0.0,
            },
        );
        st.spawned.push(cmd.clone());
        if let Some(out) = &st.output {
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

/// A forwarder with scripted peers.
pub struct FakeForwarder {
    pub peers: Mutex<Vec<MachineView>>,
    pub forwarded: Mutex<Vec<(JobId, String)>>,
    pub fail: Mutex<bool>,
}

impl FakeForwarder {
    pub fn new(peers: Vec<MachineView>) -> FakeForwarder {
        FakeForwarder {
            peers: Mutex::new(peers),
            forwarded: Mutex::new(Vec::new()),
            fail: Mutex::new(false),
        }
    }
}

impl Forwarder for FakeForwarder {
    fn peers(&self) -> Vec<MachineView> {
        lock(&self.peers).clone()
    }
    fn forward(&self, job: &JobRecord, peer: &MachineName) -> Result<ForwardTicket, ForwardError> {
        if *lock(&self.fail) {
            return Err(ForwardError::Unavailable("link down".into()));
        }
        lock(&self.forwarded).push((job.id, peer.0.clone()));
        Ok(ForwardTicket {
            remote_job: Some(77),
            remote_key: None,
        })
    }
    fn cancel_remote(&self, _: &MachineName, _: i64) -> Result<(), ForwardError> {
        Ok(())
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
    pub fn with_forwarder(f: Arc<dyn Forwarder>) -> Rig {
        Rig::build("", f, false)
    }

    fn build(policy: &str, forwarder: Arc<dyn Forwarder>, native: bool) -> Rig {
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
        let ledger: Arc<dyn Ledger> = store.clone();
        let sch = Scheduler::new(SchedulerParts {
            cfg: cfg.clone(),
            policy,
            ledger,
            platform,
            clock: clock.clone(),
            forwarder,
            parent_env: vec![(
                "PATH".into(),
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )],
            execution_enabled: true,
            runner_poll: Duration::from_millis(2),
        });
        Rig {
            dir,
            sch,
            store,
            procs,
            machine,
            clock,
            holders,
            cfg,
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
        let id = self.store.submit(spec).unwrap();
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
        self.phase(id)
    }

    pub fn log_text(&self, id: JobId) -> String {
        std::fs::read_to_string(self.cfg.log_path(id)).unwrap_or_default()
    }
}
