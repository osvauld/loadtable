//! Synthetic dashboards for tests and screenshots. Identities and paths are made up.

use crate::feed::Dashboard;
use std::path::PathBuf;
use tasks_core::{JobId, Kind, MachineName, Placement, SlotKind, WaitReason};
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
        log_path: Some(PathBuf::from(format!(
            "/home/demo/.local/state/tasks/logs/{id}.log"
        ))),
        figures: None,
        priority: Default::default(),
        priority_reason: None,
        remote_wait: None,
        fetch_paths: Vec::new(),
        fetch: Vec::new(),
    }
}

fn pressure() -> PressureView {
    PressureView {
        memory: "normal".into(),
        memory_some: Some(1.5),
        memory_full: Some(0.0),
        cpu: Some(12.0),
        io: Some(3.0),
        available_memory: Some(8 << 30),
        memory_total: Some(16 << 30),
        cpu_busy_pct: Some(23.0),
        cores_in_use: Some(3.7),
        swap_total: Some(8 << 30),
        swap_used: Some(1 << 30),
        swap_in_per_s: Some(0),
        swap_out_per_s: Some(0),
        cache_free: Some(120 << 30),
        state_free: Some(300 << 30),
        compile_blocker: None,
        warning: None,
    }
}

fn peer(state: &str, ago: Option<u64>) -> PeerView {
    PeerView {
        name: "nixos".into(),
        node_id: "node-nixos".into(),
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
            io_pct: Some(1),
            pressure_summary: String::new(),
            cpu_busy_pct: Some(30),
            swap_total: Some(4 << 30),
            swap_used: Some(0),
            swap_in_per_s: Some(0),
            swap_out_per_s: Some(0),
            jobs: Vec::new(),
            cores_in_use_milli: None,
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
    let owner_a = "claude@laptop";
    let owner_b = "v2-core@laptop";
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
        "codex@laptop",
        Phase::Waiting(WaitReason::Slot(SlotKind::Compile)),
    );
    q3.log_path = None;

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

    d.jobs = vec![f1, f2, f3, r1, r2, r3, q1, q2, q3];
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
            log_bytes: 20_480,
            figures: None,
            cores: Some(2.0),
            mem_bytes: None,
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
            log_bytes: 512,
            figures: None,
            cores: None,
            mem_bytes: None,
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
        id: "disk".into(),
        level: "warn".into(),
        message: "cache disk below 150 GiB".into(),
        since_ms: NOW_MS - 3_600_000,
    }];
    d
}

/// Like [`busy`], with the worker offline for seven minutes.
pub fn offline() -> Dashboard {
    let mut d = busy();
    d.status.peers = vec![peer("offline", Some(420))];
    d
}
