//! Synthetic dashboards for tests and screenshots. Identities and paths are made up.

use crate::feed::Dashboard;
use std::path::PathBuf;
use tasks_core::metrics::{JobMetricsView, MetricSample, ProcGroup};
use tasks_core::{Attribution, JobId, Kind, MachineName, Placement, SlotKind, WaitReason};
use tasks_daemon::api::{
    AlertView, DaemonView, JobView, PeerMachineView, PeerSlotView, PeerView, PressureView,
    RunningView, SlotGroupView, SlotView, StatusSnapshot, WaiterView,
};

pub use tasks_core::Phase;

/// The fixtures' clock, ms since the epoch (2026-10-17 12:22:13Z).
pub const NOW_MS: i64 = 1_792_239_733_000;

/// A daemon that runs normally.
pub fn daemon(machine: &str) -> DaemonView {
    DaemonView {
        pid: 4242,
        version: "0.1.0".into(),
        machine: machine.into(),
        state_dir: PathBuf::from("/home/demo/.local/state/tasks"),
        lock_dir: PathBuf::from("/run/user/1000/tasks"),
        policy_file: PathBuf::from("/home/demo/.config/tasks/policy.toml"),
        policy_file_loaded: true,
        policy_problems: Vec::new(),
        draining: false,
        drained_by: None,
        drained_at_ms: None,
        execution_enabled: true,
        uptime_secs: 86_400,
        now_ms: NOW_MS,
    }
}

/// A job row with sensible defaults for its phase.
pub fn job(id: i64, owner: &str, phase: Phase) -> JobView {
    let age = |secs: i64| NOW_MS - secs * 1000;
    let (started, finished) = match &phase {
        Phase::Queued | Phase::Waiting(_) => (None, None),
        Phase::Running => (Some(age(120)), None),
        _ => (Some(age(900)), Some(age(600))),
    };
    JobView {
        id: JobId(id),
        owner: owner.into(),
        argv: vec![
            "cargo".into(),
            "test".into(),
            "-p".into(),
            "tasks-ui".into(),
        ],
        cwd: PathBuf::from("/home/demo/work/tasks"),
        kind: Kind::Test,
        placement: Placement::Here,
        after: Vec::new(),
        phase,
        wait_text: None,
        exit: None,
        summary: None,
        submitted_at_ms: age(1000),
        started_at_ms: started,
        finished_at_ms: finished,
        machine: Some("laptop".into()),
        forwarded_to: None,
        cancel_requested_by: None,
        cancel_reason: None,
        timeout_secs: None,
        remote_phase: None,
        plan: None,
        suite: None,
        tags: Vec::new(),
        log_path: Some(PathBuf::from(format!(
            "/home/demo/.local/state/tasks/logs/{id}.log"
        ))),
        figures: None,
        priority: Default::default(),
        priority_reason: None,
        remote_wait: None,
        fetch_paths: Vec::new(),
        fetch: Vec::new(),
        sync: None,
        transfer: None,
    }
}

fn pressure() -> PressureView {
    PressureView {
        memory: "normal".into(),
        memory_some: Some(1.5),
        memory_full: Some(0.0),
        memory_stall_limit: None,
        cpu: Some(12.0),
        io: Some(3.0),
        available_memory: Some(8 << 30),
        memory_total: Some(16 << 30),
        cpu_busy_pct: Some(23.0),
        cpu_cores: Some(10),
        cores_in_use: Some(3.7),
        swap_total: Some(8 << 30),
        swap_used: Some(1 << 30),
        swap_in_per_s: Some(0),
        swap_out_per_s: Some(0),
        cache_free: Some(280 << 30),
        state_free: Some(300 << 30),
        compile_blocker: None,
        warning: None,
    }
}

fn peer(state: &str, ago: Option<u64>) -> PeerView {
    named_peer("nixos", state, ago)
}

fn named_peer(name: &str, state: &str, ago: Option<u64>) -> PeerView {
    PeerView {
        name: name.into(),
        node_id: format!("node-{name}"),
        state: state.into(),
        last_seen_ago_secs: ago,
        detail: if state == "offline" {
            "connect timed out".into()
        } else {
            String::new()
        },
        did: String::new(),
        roles_out: vec![],
        roles_in: vec![],
        grant_expires_at: None,
        path: "unknown".into(),
        rtt_ms: None,
        reply_ms: None,
        machine: Some(PeerMachineView {
            cores: 16,
            memory_bytes: 64 << 30,
            slots: vec![
                PeerSlotView {
                    kind: "compile".into(),
                    total: 4,
                    used: 2,
                },
                PeerSlotView {
                    kind: "run".into(),
                    total: 4,
                    used: 0,
                },
            ],
            waiting: 0,
            running: 1,
            cpu_pct: Some(35),
            memory_pct: Some(4),
            memory_level: None,
            io_pct: Some(1),
            pressure_summary: String::new(),
            cpu_busy_pct: Some(30),
            memory_available_bytes: Some(51 << 30),
            swap_total: Some(4 << 30),
            swap_used: Some(0),
            swap_in_per_s: Some(0),
            swap_out_per_s: Some(0),
            jobs: Vec::new(),
            cores_in_use_milli: None,
            power: None,
        }),
    }
}

/// An idle laptop with a healthy worker and a short history.
pub fn idle() -> Dashboard {
    let mut jobs = vec![
        job(88, "claude@laptop", Phase::Passed),
        job(89, "v2-core@laptop", Phase::Failed { exit: Some(101) }),
    ];
    for j in &mut jobs {
        j.exit = Some(0);
    }
    Dashboard {
        status: StatusSnapshot {
            daemon: daemon("laptop"),
            slots: vec![
                SlotGroupView {
                    kind: "compile".into(),
                    total: 2,
                    free: 2,
                    busy: vec![],
                },
                SlotGroupView {
                    kind: "run".into(),
                    total: 2,
                    free: 2,
                    busy: vec![],
                },
            ],
            waiting: vec![],
            waiting_more: 0,
            running: vec![],
            pressure: pressure(),
            alerts: vec![],
            settings: vec![],
            peers: vec![peer("up", Some(1))],
            attribution: "attribution: process tree (no cgroup delegation: fixture)".into(),
            transfer: None,
            trips: vec![],
            power: None,
            endpoint: None,
        },
        jobs,
        jobs_truncated: false,
    }
}

fn waiter(
    id: i64,
    owner: &str,
    text: &str,
    pos: usize,
    est: Option<f64>,
    rough: bool,
) -> WaiterView {
    WaiterView {
        id: JobId(id),
        owner: owner.into(),
        command: "cargo test".into(),
        wait_text: text.into(),
        position: pos,
        start_estimate_secs: est,
        rough,
        class: String::new(),
        class_note: String::new(),
        on_peer: None,
    }
}

/// A busy laptop: running, queued and finished jobs, one forward to the worker.
pub fn busy() -> Dashboard {
    let mut d = idle();
    let owner_a = "ui-claude@laptop";
    let owner_b = "core-codex@laptop";
    let owner_c = "hard-errors@laptop";

    let mut r1 = job(101, owner_a, Phase::Running);
    r1.timeout_secs = Some(3600);
    let r2 = job(102, owner_b, Phase::Running);
    let mut r3 = job(103, owner_c, Phase::Running);
    r3.machine = Some("nixos".into());
    r3.forwarded_to = Some(("nixos".into(), Some(1496)));
    r3.remote_phase = Some(Phase::Running);
    r3.log_path = None;
    r3.placement = Placement::Peer(MachineName("nixos".into()));

    let mut q1 = job(
        110,
        owner_a,
        Phase::Waiting(WaitReason::Slot(SlotKind::Compile)),
    );
    q1.wait_text = Some("waiting for a compile slot".into());
    q1.log_path = None;
    let mut q2 = job(
        111,
        owner_b,
        Phase::Waiting(WaitReason::Pressure("memory stalls 12% over 10%".into())),
    );
    q2.log_path = None;
    let mut q3 = job(
        112,
        "lint-pi@laptop",
        Phase::Waiting(WaitReason::Slot(SlotKind::Compile)),
    );
    q3.log_path = None;
    // a forward the worker holds back on its own queue (R-349)
    let mut q4 = job(113, owner_c, Phase::Running);
    q4.machine = Some("nixos".into());
    q4.forwarded_to = Some(("nixos".into(), Some(1497)));
    q4.placement = Placement::Peer(MachineName("nixos".into()));
    q4.remote_phase = Some(Phase::Waiting(WaitReason::Slot(SlotKind::Compile)));
    q4.remote_wait = Some("waiting on nixos: its compile queue".into());
    q4.started_at_ms = None;
    q4.log_path = None;

    let mut f1 = job(90, owner_a, Phase::Passed);
    f1.finished_at_ms = Some(NOW_MS - 600_000);
    f1.exit = Some(0);
    let mut f2 = job(91, owner_b, Phase::TimedOut { exit: None });
    f2.finished_at_ms = Some(NOW_MS - 300_000);
    f2.timeout_secs = Some(1800);
    let mut f3 = job(92, owner_c, Phase::Cancelled);
    f3.finished_at_ms = Some(NOW_MS - 60_000);
    f3.cancel_requested_by = Some(owner_c.into());
    f3.cancel_reason = Some("wrong branch".into());

    let mut f4 = job(93, owner_b, Phase::Failed { exit: Some(101) });
    f4.started_at_ms = Some(NOW_MS - 1_500_000);
    f4.finished_at_ms = Some(NOW_MS - 1_200_000);
    f4.exit = Some(101);
    // older than an hour: in History only
    let mut f5 = job(85, owner_c, Phase::Passed);
    f5.started_at_ms = Some(NOW_MS - 7_300_000);
    f5.finished_at_ms = Some(NOW_MS - 7_200_000);
    f5.exit = Some(0);

    d.jobs = vec![f1, f2, f3, f4, f5, r1, r2, r3, q1, q2, q3, q4];
    for j in &mut d.jobs {
        let line = match j.id.0 {
            101 => "cargo test -p tasks-ui --lib",
            102 => "cargo test -p tasks-core --test lifecycle",
            103 => "cargo clippy --workspace -j 6",
            110 => "cargo build -p desktop --release",
            111 => "cargo test -p tasks-ledger --lib",
            112 => "./gradlew :app:lintDebug",
            113 => "cargo test -p tasks-peer --test sync",
            90 => "cargo check --workspace",
            91 => "cargo test -p tasks-peer --test sync",
            92 => "cargo test -p tasks-ui",
            93 => "cargo test -p tasks-daemon --lib",
            85 => "cargo clippy --workspace",
            _ => continue,
        };
        j.argv = line.split(' ').map(str::to_string).collect();
    }
    let st = &mut d.status;
    st.running = vec![
        RunningView {
            id: JobId(101),
            owner: owner_a.into(),
            command: "cargo test -p tasks-ui".into(),
            slot: Some(("compile".into(), 0)),
            elapsed_secs: 120.0,
            done_targets: 3,
            total_targets: 10,
            current: Some("test:lifecycle".into()),
            eta_secs: Some(80.0),
            usual_secs: None,
            over_by_secs: None,
            eta_rough_from: None,
            no_eta_why: None,
            log_bytes: 20_480,
            figures: None,
            cores: Some(2.0),
            mem_bytes: Some(1_500_000_000),
            transfer: None,
        },
        RunningView {
            id: JobId(102),
            owner: owner_b.into(),
            command: "cargo run".into(),
            slot: Some(("run".into(), 0)),
            elapsed_secs: 120.0,
            done_targets: 0,
            total_targets: 0,
            current: None,
            eta_secs: None,
            usual_secs: None,
            over_by_secs: None,
            eta_rough_from: None,
            no_eta_why: Some("first run".into()),
            log_bytes: 512,
            figures: None,
            cores: Some(1.2),
            mem_bytes: Some(400 << 20),
            transfer: None,
        },
    ];
    st.waiting = vec![
        waiter(
            110,
            owner_a,
            "waiting for a compile slot",
            1,
            Some(125.0),
            false,
        ),
        waiter(
            111,
            owner_b,
            "machine pressure: memory stalls 12% over 10%",
            2,
            Some(300.0),
            true,
        ),
    ];
    st.waiting.push(WaiterView {
        on_peer: Some("nixos".into()),
        position: 0,
        start_estimate_secs: None,
        rough: true,
        ..waiter(
            113,
            owner_c,
            "waiting on nixos: its compile queue",
            0,
            None,
            true,
        )
    });
    st.waiting_more = 1;
    st.slots = vec![
        SlotGroupView {
            kind: "compile".into(),
            total: 2,
            free: 0,
            busy: vec![
                SlotView {
                    index: 0,
                    job: Some(JobId(101)),
                    external: false,
                    holder: None,
                },
                SlotView {
                    index: 1,
                    job: None,
                    external: true,
                    holder: Some("pid 4242 cargo build (origin job 9)".into()),
                },
            ],
        },
        SlotGroupView {
            kind: "run".into(),
            total: 2,
            free: 1,
            busy: vec![SlotView {
                index: 0,
                job: Some(JobId(102)),
                external: false,
                holder: None,
            }],
        },
        SlotGroupView {
            kind: "offload".into(),
            total: 4,
            free: 3,
            busy: vec![SlotView {
                index: 0,
                job: Some(JobId(103)),
                external: false,
                holder: None,
            }],
        },
    ];
    st.pressure.compile_blocker = Some("memory stalls 12% over 10%".into());
    st.pressure.memory = "warn".into();
    st.pressure.memory_some = Some(12.0);
    st.pressure.cpu = Some(78.0);
    st.alerts = vec![AlertView {
        id: "disk-cache".into(),
        level: "warn".into(),
        message: "/home/demo/.cache/tasks has 280.0 GiB free, under twice its reserve of 150.0 GiB"
            .into(),
        since_ms: NOW_MS - 3_600_000,
    }];
    d
}

/// What the worker `nixos` says it runs: the forward #103 (its job 1496) and the rest of
/// its load.
fn nixos_load(d: &mut Dashboard) {
    for p in &mut d.status.peers {
        if p.name != "nixos" {
            continue;
        }
        if let Some(m) = &mut p.machine {
            m.jobs = vec![tasks_daemon::api::PeerJobUseView {
                id: 1496,
                owner: "hard-errors@laptop".into(),
                cores_milli: Some(4100),
                mem_bytes: Some(3 << 30),
            }];
            // 30% of 16 cores is 4.8: the job and 0.7 outside it
            m.cores_in_use_milli = Some(4800);
        }
    }
}

/// A Mac worker that is awake: no stall percentage, only a level, and CPU from cores in use.
fn mac_peer() -> PeerView {
    let mut p = named_peer("mac", "up", Some(2));
    if let Some(m) = &mut p.machine {
        m.cores = 8;
        m.memory_bytes = 16 << 30;
        m.memory_pct = None;
        m.memory_level = Some("warn".into());
        m.memory_available_bytes = Some(2 << 30);
        m.cpu_busy_pct = None;
        m.io_pct = None;
        m.cpu_pct = Some(60);
        m.slots = vec![PeerSlotView {
            kind: "compile".into(),
            total: 2,
            used: 1,
        }];
        m.running = 1;
        m.power = Some("on AC".into());
        m.jobs = vec![tasks_daemon::api::PeerJobUseView {
            id: 9,
            owner: "pi-ios@laptop".into(),
            cores_milli: Some(3100),
            mem_bytes: Some(2 << 30),
        }];
        m.cores_in_use_milli = Some(5000);
    }
    p
}

/// [`busy`] with the worker's load, plus an awake Mac running a forwarded job and a finished
/// forward whose files came back: three machines, all of them up.
pub fn mac() -> Dashboard {
    let mut d = busy();
    nixos_load(&mut d);
    d.status.peers.push(mac_peer());
    let mut m1 = job(104, "pi-ios@laptop", Phase::Running);
    m1.argv = "./build_core.sh ios"
        .split(' ')
        .map(str::to_string)
        .collect();
    m1.machine = Some("mac".into());
    m1.forwarded_to = Some(("mac".into(), Some(9)));
    m1.remote_phase = Some(Phase::Running);
    m1.placement = Placement::Peer(MachineName("mac".into()));
    m1.log_path = None;
    m1.sync = Some(tasks_daemon::api::SyncView {
        bytes: 742,
        files: 318,
        millis: 400,
        already_there: false,
    });
    let mut m2 = job(94, "pi-ios@laptop", Phase::Passed);
    m2.argv = "xcodebuild -scheme core"
        .split(' ')
        .map(str::to_string)
        .collect();
    m2.machine = Some("mac".into());
    m2.forwarded_to = Some(("mac".into(), Some(7)));
    m2.placement = Placement::Peer(MachineName("mac".into()));
    m2.started_at_ms = Some(NOW_MS - 400_000);
    m2.finished_at_ms = Some(NOW_MS - 240_000);
    m2.exit = Some(0);
    m2.sync = Some(tasks_daemon::api::SyncView {
        bytes: 5 << 20,
        files: 318,
        millis: 2_300,
        already_there: false,
    });
    m2.fetch_paths = vec!["build/core.xcframework".into(), "build/core.dSYM".into()];
    m2.fetch = vec![
        tasks_core::FetchResult {
            path: "build/core.xcframework".into(),
            status: tasks_core::FetchStatus::Fetched {
                bytes: 38 << 20,
                sha256: "ab".into(),
                unchanged: false,
            },
        },
        tasks_core::FetchResult {
            path: "build/core.dSYM".into(),
            status: tasks_core::FetchStatus::Missing,
        },
    ];
    d.jobs.push(m1);
    d.jobs.push(m2);
    d
}

/// [`mac`], with job 104's command over on the Mac and its framework a third of the way back
/// (R-801).
pub fn fetching() -> Dashboard {
    let mut d = mac();
    let t = tasks_core::TransferProgress {
        kind: tasks_core::TransferKind::Fetch,
        peer: "mac".into(),
        path: Some("build/core.xcframework".into()),
        done_bytes: 120 << 20,
        total_bytes: Some(362 << 20),
        rate_bps: Some(f64::from(4u32 << 20)),
        remote_result: Some("command exit 0".into()),
        at_ms: NOW_MS,
    };
    if let Some(j) = d.jobs.iter_mut().find(|j| j.id.0 == 104) {
        j.fetch_paths = vec!["build/core.xcframework".into()];
        j.transfer = Some(t);
    }
    d
}

/// The step names of the real core-split suite's test plan, in submission order.
const PLAN_TESTS: [&str; 39] = [
    "test-calls",
    "test-link",
    "test-ledger",
    "test-sync",
    "test-queue",
    "test-chat_shards",
    "test-policy",
    "test-peer",
    "test-iroh",
    "test-ui",
    "test-cli",
    "test-mcp",
    "test-core",
    "test-daemon",
    "test-place",
    "test-wait",
    "test-fetch",
    "test-audit",
    "test-stalls",
    "test-timings",
    "test-logs",
    "test-grep",
    "test-cancel",
    "test-drain",
    "test-retry",
    "test-forward",
    "test-metrics",
    "test-pressure",
    "test-slots",
    "test-hints",
    "test-split",
    "test-tools",
    "test-notes",
    "test-render",
    "test-args",
    "test-config",
    "test-startup",
    "test-follow",
    "test-e2e",
];

/// Plan 2 of suite `core-split` as the daemon lists it mid-run, beside one ordinary job: the
/// build and 5 tests passed, 3 tests run on nixos, `test-chat_shards` failed, 30 wait.
pub fn with_plan() -> Dashboard {
    let mut d = idle();
    nixos_load(&mut d);
    let owner = "lead-p2p@laptop";
    let age = |secs: i64| NOW_MS - secs * 1000;
    let mut ordinary = job(101, "ui-claude@laptop", Phase::Running);
    ordinary.timeout_secs = Some(3600);
    d.jobs.push(ordinary);
    d.status.running.push(RunningView {
        id: JobId(101),
        owner: "ui-claude@laptop".into(),
        command: "cargo test -p tasks-ui".into(),
        slot: Some(("compile".into(), 0)),
        elapsed_secs: 120.0,
        done_targets: 3,
        total_targets: 10,
        current: Some("test:lifecycle".into()),
        eta_secs: Some(80.0),
        usual_secs: None,
        over_by_secs: None,
        eta_rough_from: None,
        no_eta_why: None,
        log_bytes: 20_480,
        figures: None,
        cores: Some(2.0),
        mem_bytes: Some(1_500_000_000),
        transfer: None,
    });
    let step = |id: i64, name: &str, phase: Phase| {
        let mut j = job(id, owner, phase.clone());
        j.argv = [
            "cargo",
            "test",
            "--release",
            "-p",
            "tasks-daemon",
            "--test",
            name,
        ]
        .map(String::from)
        .to_vec();
        j.plan = Some((2, format!("tests.{name}")));
        j.suite = Some("core-split".into());
        j.machine = Some("nixos".into());
        j.placement = Placement::Peer(MachineName("nixos".into()));
        j.submitted_at_ms = age(900);
        match &phase {
            Phase::Passed | Phase::Failed { .. } => {
                j.started_at_ms = Some(age(850));
                j.finished_at_ms = Some(age(700));
            }
            Phase::Running => {
                j.started_at_ms = Some(age(60));
                j.finished_at_ms = None;
            }
            _ => {
                j.started_at_ms = None;
                j.finished_at_ms = None;
            }
        }
        j.exit = match &phase {
            Phase::Passed => Some(0),
            Phase::Failed { exit } => *exit,
            _ => None,
        };
        j
    };
    let mut build = step(330, "build", Phase::Passed);
    build.plan = Some((2, "tests.build".into()));
    build.argv = ["cargo", "build", "--release", "--tests"]
        .map(String::from)
        .to_vec();
    build.started_at_ms = Some(age(900));
    build.finished_at_ms = Some(age(655));
    d.jobs.push(build);
    let mut waits = Vec::new();
    for (i, name) in PLAN_TESTS.iter().enumerate() {
        let id = 331 + i as i64;
        let phase = match i {
            0..=4 => Phase::Passed,
            5 => Phase::Failed { exit: Some(101) },
            6..=8 => Phase::Running,
            _ => Phase::Waiting(WaitReason::Slot(SlotKind::Run)),
        };
        let mut j = step(id, name, phase.clone());
        if *name == "test-link" || *name == "test-iroh" {
            j.tags = vec!["light".into()];
        }
        if *name == "test-e2e" {
            j.tags = vec!["isolate".into()];
        }
        if *name == "test-chat_shards" {
            j.tags = vec!["retry".into()];
            j.summary = Some("2 tests failed".into());
        }
        // the failed step is placed by its name; keep the order of the suite otherwise
        if matches!(phase, Phase::Waiting(_)) {
            j.wait_text = Some("waiting for a run slot on nixos".into());
            j.started_at_ms = None;
            waits.push(waiter(
                id,
                owner,
                "waiting for a run slot on nixos",
                waits.len() + 1,
                Some(45.0 + 20.0 * waits.len() as f64),
                false,
            ));
        }
        if phase == Phase::Running {
            d.status.running.push(RunningView {
                id: JobId(id),
                owner: owner.into(),
                command: "cargo test".into(),
                slot: Some(("run".into(), 0)),
                elapsed_secs: 60.0,
                done_targets: 0,
                total_targets: 0,
                current: None,
                eta_secs: Some(180.0 + 60.0 * (i % 3) as f64),
                usual_secs: None,
                over_by_secs: None,
                eta_rough_from: None,
                no_eta_why: None,
                log_bytes: 4_096,
                figures: None,
                cores: Some(1.0),
                mem_bytes: Some(600 << 20),
                transfer: None,
            });
        }
        d.jobs.push(j);
    }
    d.status.waiting = waits;
    d
}

/// The samples of the running job #101: two minutes at one reading every two seconds, CPU
/// around three cores ending at 3.2, memory growing to 1.4 GiB, a little disk traffic.
pub fn metrics_running() -> JobMetricsView {
    const STEP_MS: i64 = 2_000;
    let mut samples = Vec::new();
    let (mut cpu, mut rd, mut wr, mut stall) = (0u64, 0u64, 0u64, 0u64);
    for i in 0..60i64 {
        // a ramp-up, a plateau with wobble, and a last reading of exactly 3.2 cores
        let cores = match i {
            0 => 0.0,
            59 => 3.2,
            1..=8 => 0.4 * i as f64,
            _ => 2.6 + ((i * 7) % 11) as f64 / 20.0,
        };
        cpu += (cores * 2_000_000.0) as u64;
        rd += 8 << 20;
        wr += if i % 9 < 3 { 40 << 20 } else { 2 << 20 };
        stall += if i == 59 { 20_000 } else { 4_000 };
        samples.push(MetricSample {
            t_ms: NOW_MS - (59 - i) * STEP_MS,
            cpu_usec: Some(cpu),
            mem_bytes: Some((400 << 20) + (i as u64) * ((1 << 30) / 59)),
            io_read_bytes: Some(rd),
            io_write_bytes: Some(wr),
            io_some_us: Some(stall),
        });
    }
    let g = |name: &str, count, cpu, rss: u64| ProcGroup {
        name: name.into(),
        count,
        cpu_cores: Some(cpu),
        rss_bytes: rss,
        others: false,
    };
    JobMetricsView {
        quality: Some(Attribution::Cgroup),
        samples,
        procs: vec![
            g("rustc", 6, 2.4, 1_181_116_006),
            g("cc", 2, 0.4, 180 << 20),
            g("ld", 1, 0.2, 410 << 20),
            g("cargo", 1, 0.1, 96 << 20),
            ProcGroup {
                name: "5 others".into(),
                count: 4,
                cpu_cores: Some(0.1),
                rss_bytes: 60 << 20,
                others: true,
            },
        ],
        usual_peak_mem_bytes: Some(2_254_857_830),
        note: None,
        source: None,
    }
}

/// The metrics of the forward #104 as the Mac measured them: no cgroup, no history here.
pub fn metrics_forwarded() -> JobMetricsView {
    let mut v = metrics_running();
    v.quality = Some(Attribution::ProcessTree);
    v.source = Some("on mac, remote job #9".into());
    v.usual_peak_mem_bytes = None;
    for s in &mut v.samples {
        s.io_some_us = None;
        s.mem_bytes = s.mem_bytes.map(|m| m * 3 / 2);
    }
    v.procs = vec![
        ProcGroup {
            name: "clang".into(),
            count: 5,
            cpu_cores: Some(2.6),
            rss_bytes: 1 << 30,
            others: false,
        },
        ProcGroup {
            name: "swift-frontend".into(),
            count: 1,
            cpu_cores: Some(0.5),
            rss_bytes: 780 << 20,
            others: false,
        },
        ProcGroup {
            name: "xcodebuild".into(),
            count: 1,
            cpu_cores: Some(0.0),
            rss_bytes: 90 << 20,
            others: false,
        },
    ];
    v
}

/// [`busy`] plus a second worker that is asleep: three machines, one of them away.
pub fn three_machines() -> Dashboard {
    let mut d = busy();
    nixos_load(&mut d);
    d.status
        .peers
        .push(named_peer("air", "offline", Some(4_200)));
    d
}

/// Like [`busy`], with the worker offline for seven minutes.
pub fn offline() -> Dashboard {
    let mut d = busy();
    d.status.peers = vec![peer("offline", Some(420))];
    d
}
