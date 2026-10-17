//! Example snapshots in tests/fixtures/snapshot/metrics-*.json are generated
//! from these types by `assemble` (synthetic hosts, owners and paths):
//!   cargo test --lib metrics::tests::fixture_tests::write_fixtures -- --ignored
//! The normal tests check that the files still equal what the code produces
//! and that every object has exactly the contract's field names.
use super::super::slots::{Scan, Waiter};
use super::super::snapshot::{Blockers, Inputs, JobExtras, assemble};
use super::super::warmth::StartWarmth;
use super::super::*;
use super::{job, measured, stage};
use crate::execution::StageTiming;
use crate::local_command::Resource;
use crate::{Job, Phase};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, UNIX_EPOCH};

const NOW: f64 = 1_792_216_000.25;
const GIB: u64 = 1 << 30;
const LAPTOP_CHECKOUT: &str = "/home/dev/src/app/.claude/worktrees/view-step-up";

const VIDEO_TEST: &[&str] = &[
    "cargo",
    "test",
    "--offline",
    "--locked",
    "-j",
    "2",
    "-p",
    "video",
    "--release",
];
const CLIPPY: &[&str] = &[
    "cargo",
    "clippy",
    "--offline",
    "--locked",
    "-j",
    "2",
    "--all-targets",
    "--",
    "-D",
    "warnings",
];
const OFFLOAD: &[&str] = &[
    "tasks-offload",
    "--worker",
    "dev@192.0.2.7",
    "--who",
    "rate-recovery",
    "--ssh-key",
    "/home/dev/.ssh/id_worker",
    "--",
    "cargo",
    "test",
    "--locked",
    "-j",
    "8",
    "-p",
    "video",
    "--release",
    "--lib",
    "--",
    "sim::",
];
const WORKER_TEST: &[&str] = &[
    "cargo",
    "test",
    "--locked",
    "-j",
    "8",
    "-p",
    "video",
    "--release",
    "--lib",
    "--",
    "sim::",
];
const INSTALL: &[&str] = &[
    "adb",
    "-s",
    "TESTSERIAL01",
    "install",
    "-r",
    "/home/dev/apks/app-debug.apk",
];

fn series(n: usize, base: f64, swing: f64) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let x = base + swing * ((i as f64) / 9.0).sin();
            (x * 100.0).round() / 100.0
        })
        .collect()
}
fn bytes(n: usize, base: u64, swing: u64) -> Vec<u64> {
    series(n, 0.0, 1.0)
        .into_iter()
        .map(|x| base + (swing as f64 * (x + 1.0) / 2.0) as u64)
        .collect()
}

fn live(cpu: f64, rss: u64, processes: u32, points: usize) -> Live {
    Live {
        cpu_cores: Some(cpu),
        rss_bytes: rss,
        processes,
        history: LiveHistory {
            interval_s: SAMPLE_INTERVAL_S,
            cpu_cores: series(points, cpu, cpu * 0.2),
            rss_bytes: bytes(points, rss / 2, rss / 2),
        },
    }
}

fn slot(kind: SlotKind, name: &str, holder: Option<(i64, &str)>) -> Slot {
    Slot {
        kind,
        name: name.into(),
        busy: holder.is_some(),
        holder_job: holder.map(|h| h.0),
        holder_who: holder.map(|h| h.1.to_string()),
    }
}

fn idle_slots() -> Vec<Slot> {
    let mut slots = vec![slot(SlotKind::Compile, "p2p-build", None)];
    slots.extend((0..4).map(|i| slot(SlotKind::Run, &format!("p2p-run-{i}"), None)));
    slots.extend((0..4).map(|i| slot(SlotKind::Offload, &format!("p2p-offload-{i}"), None)));
    slots
}

#[allow(clippy::too_many_arguments)]
fn machine(
    id: &str,
    role: Role,
    cpus: u32,
    busy: Option<f64>,
    total: u64,
    available: u64,
    blocker: Option<&str>,
    slots: Vec<Slot>,
    points: usize,
) -> Machine {
    Machine {
        id: id.into(),
        role,
        logical_cpus: cpus,
        cpu_busy_cores: busy,
        mem_total_bytes: Some(total),
        mem_available_bytes: Some(available),
        swap_total_bytes: Some(8 * GIB),
        swap_used_bytes: Some(256 << 20),
        pressure: Some(Pressure {
            cpu_some_avg10: if busy.unwrap_or(0.0) > 4.0 { 18.5 } else { 0.4 },
            memory_some_avg10: if blocker.is_some() { 3.1 } else { 0.0 },
            memory_full_avg10: if blocker.is_some() { 0.9 } else { 0.0 },
            io_some_avg10: 1.2,
        }),
        blocker: blocker.map(str::to_string),
        slots,
        history: MachineHistory {
            interval_s: SAMPLE_INTERVAL_S,
            cpu_busy_cores: series(points, busy.unwrap_or(0.2), 1.0)
                .into_iter()
                .map(|v| v.clamp(0.0, cpus as f64))
                .collect(),
            mem_used_bytes: bytes(points, total - available - GIB, 2 * GIB),
        },
        cargo_jobs_max: None,
    }
}

fn waiter(label: &str, job: i64, who: &str, at: u64) -> Waiter {
    Waiter {
        label: label.into(),
        job: Some(job),
        who: who.into(),
        mtime: UNIX_EPOCH + Duration::from_secs(at),
        path: format!("/tmp/locks/p2p-wait-{job}.json").into(),
    }
}

fn start(warmth: Warmth, reason: &str) -> Option<StartWarmth> {
    Some(StartWarmth {
        warmth,
        reason: reason.into(),
    })
}

fn with_owner(mut j: Job, owner: &str, source: &str) -> Job {
    j.spec.owner = owner.into();
    j.spec.source = source.into();
    j
}

/// Earlier finished runs on one machine: the history ETAs come from.
fn history_runs(owner: &str, source: &str, test: &[&str], first: i64) -> Vec<Job> {
    let mut jobs = vec![
        job(
            first,
            test,
            Resource::Compile,
            Phase::Passed,
            vec![stage("build", 395.0), stage("run", 31.0)],
            measured(Warmth::Cold, 310),
        ),
        job(
            first + 1,
            test,
            Resource::Compile,
            Phase::Passed,
            vec![stage("build", 61.0), stage("run", 29.5)],
            measured(Warmth::Warm, 4),
        ),
        job(
            first + 2,
            CLIPPY,
            Resource::Compile,
            Phase::Passed,
            vec![stage("build", 48.0)],
            measured(Warmth::Warm, 6),
        ),
        job(
            first + 3,
            test,
            Resource::Compile,
            Phase::Failed,
            vec![
                stage("build", 58.0),
                StageTiming {
                    exit_code: Some(101),
                    ..stage("run", 12.0)
                },
            ],
            measured(Warmth::Warm, 3),
        ),
        // A row from before metrics existed (backfill: unknown warmth).
        job(
            first + 4,
            test,
            Resource::Compile,
            Phase::Passed,
            vec![stage("build", 70.0), stage("run", 30.0)],
            None,
        ),
    ];
    for j in &mut jobs {
        *j = with_owner(j.clone(), owner, source);
        let wall = super::super::history::exec_seconds(j);
        if let Some(m) = j.execution.as_mut().and_then(|e| e.metrics.as_mut()) {
            m.wall_s = wall;
            m.cpu_user_s = wall.map(|w| (w * 1.7 * 10.0).round() / 10.0);
            m.cpu_sys_s = wall.map(|w| (w * 0.2 * 10.0).round() / 10.0);
            m.peak_rss_bytes = Some(3 * GIB + 512 * (1 << 20));
            m.warmth_reason = match m.warmth {
                Warmth::Cold => "no build output for this checkout on this machine (target/release has no fingerprints)".into(),
                _ => format!("compiled {} crates, at most 20% of a full build (310)", m.crates_compiled.unwrap_or(0)),
            };
        }
    }
    jobs
}

fn running(mut j: Job, supervisor: bool) -> Job {
    if let Some(e) = j.execution.as_mut() {
        e.finished_at = None;
        if supervisor {
            e.supervisor = Some(crate::execution::ProcessIdentity {
                pid: 4242,
                start_ticks: 1,
            });
        }
    }
    j
}

struct Scenario {
    name: &'static str,
    machine: Machine,
    jobs: Vec<Job>,
    extras: HashMap<i64, JobExtras>,
    scan: Scan,
    blockers: Blockers,
}

fn extras(checkout: &str, name: &str) -> JobExtras {
    JobExtras {
        checkout: (checkout.into(), name.into()),
        ..Default::default()
    }
}

fn laptop_busy() -> Scenario {
    let me = "view-step-up@laptop";
    let src = LAPTOP_CHECKOUT;
    let mut jobs = history_runs(me, src, VIDEO_TEST, 280);
    // #301: cold build, building; #302: clippy waiting for the compile slot;
    // #303: offload client on an offload slot; #304: device install waiting;
    // #305: proposal awaiting approval.
    jobs.push(with_owner(
        running(
            job(
                301,
                VIDEO_TEST,
                Resource::Compile,
                Phase::Building,
                vec![],
                None,
            ),
            true,
        ),
        me,
        src,
    ));
    jobs.push(with_owner(
        job(
            302,
            CLIPPY,
            Resource::Compile,
            Phase::WaitingBuild,
            vec![],
            None,
        ),
        "fix-cli@laptop",
        "/home/dev/src/tasks-wt/fix-cli",
    ));
    jobs.push(with_owner(
        running(
            job(
                303,
                OFFLOAD,
                Resource::Offload,
                Phase::Running,
                vec![],
                None,
            ),
            true,
        ),
        "rate-recovery",
        "/home/dev/src/app/.claude/worktrees/rate-recovery",
    ));
    jobs.push(with_owner(
        job(
            304,
            INSTALL,
            Resource::Device("TESTSERIAL01".into()),
            Phase::WaitingRun,
            vec![],
            None,
        ),
        "phone-agent",
        src,
    ));
    jobs.push(with_owner(
        job(
            305,
            &["cargo", "build", "--release", "-j", "2"],
            Resource::Compile,
            Phase::Queued,
            vec![],
            None,
        ),
        "late-agent",
        "/home/dev/src/other",
    ));
    let mut slots = idle_slots();
    slots[0] = slot(SlotKind::Compile, "p2p-build", Some((301, me)));
    slots[5] = slot(
        SlotKind::Offload,
        "p2p-offload-0",
        Some((303, "rate-recovery")),
    );
    slots.push(slot(
        SlotKind::Device,
        "p2p-device-TESTSERIAL01",
        Some((299, "other-phone-agent")),
    ));
    let mut extras_map: HashMap<i64, JobExtras> = jobs
        .iter()
        .map(|j| {
            let name = j
                .spec
                .source
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            (j.id, extras(&j.spec.source, &name))
        })
        .collect();
    let e = extras_map.get_mut(&301).unwrap();
    e.live = Some(live(1.9, 2 * GIB + 300 * (1 << 20), 14, 95));
    e.crates_so_far = Some(212);
    e.start = start(
        Warmth::Cold,
        "no build output for this checkout on this machine (target/release has no fingerprints)",
    );
    e.admitted_at = Some(NOW as i64 - 190);
    let e = extras_map.get_mut(&302).unwrap();
    e.crates_so_far = Some(0);
    e.start = start(
        Warmth::Unknown,
        "build output exists (target/debug: 412 fingerprints); refined from Compiling lines",
    );
    let e = extras_map.get_mut(&303).unwrap();
    e.live = Some(live(0.1, 80 << 20, 3, 40));
    e.start = start(Warmth::Unknown, "offloaded: the build runs on the worker");
    e.admitted_at = Some(NOW as i64 - 80);
    let e = extras_map.get_mut(&304).unwrap();
    e.start = start(Warmth::Unknown, "not a Cargo command");
    Scenario {
        name: "metrics-laptop-busy",
        machine: machine(
            "laptop",
            Role::Host,
            8,
            Some(5.4),
            31 * GIB,
            12 * GIB,
            None,
            slots,
            150,
        ),
        jobs,
        extras: extras_map,
        scan: Scan {
            slots: vec![],
            waiters: vec![
                waiter("compile", 302, "fix-cli@laptop", 1_792_215_900),
                waiter("device TESTSERIAL01", 304, "phone-agent", 1_792_215_950),
            ],
        },
        blockers: Blockers::default(),
    }
}

fn worker_busy() -> Scenario {
    let owner = "rate-recovery@laptop";
    let worker_src = "/home/dev/offload/checkouts/5f2a9c0d1e3b4a67";
    let laptop = "/home/dev/src/app/.claude/worktrees/rate-recovery";
    let mut jobs = history_runs(owner, worker_src, WORKER_TEST, 60);
    jobs.push(with_owner(
        running(
            job(
                93,
                WORKER_TEST,
                Resource::Compile,
                Phase::Running,
                vec![stage("build", 64.0)],
                None,
            ),
            true,
        ),
        owner,
        worker_src,
    ));
    jobs.push(with_owner(
        job(
            94,
            CLIPPY,
            Resource::Compile,
            Phase::WaitingBuild,
            vec![],
            None,
        ),
        "fix-ops@laptop",
        "/home/dev/offload/checkouts/0a1b2c3d4e5f6071",
    ));
    jobs.push(with_owner(
        job(
            95,
            WORKER_TEST,
            Resource::Compile,
            Phase::WaitingBuild,
            vec![],
            None,
        ),
        "l2p@laptop",
        "/home/dev/offload/checkouts/9e8d7c6b5a493827",
    ));
    let mut slots = idle_slots();
    slots[1] = slot(SlotKind::Run, "p2p-run-0", Some((93, owner)));
    let mut extras_map: HashMap<i64, JobExtras> = HashMap::new();
    for j in &jobs {
        let (checkout, name) = match j.id {
            94 => ("/home/dev/src/tasks-wt/fix-ops", "fix-ops"),
            95 => ("/home/dev/src/app/.claude/worktrees/l2p", "l2p"),
            _ => (laptop, "rate-recovery"),
        };
        extras_map.insert(j.id, extras(checkout, name));
    }
    let e = extras_map.get_mut(&93).unwrap();
    e.live = Some(live(7.8, 5 * GIB, 22, 150));
    e.crates_so_far = Some(4);
    e.start = start(
        Warmth::Unknown,
        "build output exists (target/release: 640 fingerprints); refined from Compiling lines",
    );
    e.admitted_at = Some(NOW as i64 - 21);
    let e = extras_map.get_mut(&94).unwrap();
    e.crates_so_far = Some(0);
    e.start = start(
        Warmth::Cold,
        "no build output for this checkout on this machine (target/debug has no fingerprints)",
    );
    let e = extras_map.get_mut(&95).unwrap();
    e.crates_so_far = Some(0);
    e.start = start(
        Warmth::Unknown,
        "build output exists (target/release: 610 fingerprints); refined from Compiling lines",
    );
    let blocker = "memory PSI some avg10 above 1%";
    Scenario {
        name: "metrics-worker-busy",
        machine: machine(
            "worker",
            Role::Worker,
            16,
            Some(14.2),
            62 * GIB,
            9 * GIB,
            Some(blocker),
            slots,
            150,
        ),
        jobs,
        extras: extras_map,
        scan: Scan {
            slots: vec![],
            waiters: vec![
                waiter("compile", 94, "fix-ops@laptop", 1_792_215_970),
                waiter("compile", 95, "l2p@laptop", 1_792_215_980),
            ],
        },
        blockers: Blockers {
            compile: Some(blocker.into()),
            run: Some(blocker.into()),
            offload: None,
        },
    }
}

fn idle() -> Scenario {
    let me = "view-step-up@laptop";
    let jobs = history_runs(me, LAPTOP_CHECKOUT, VIDEO_TEST, 400);
    let extras_map = jobs
        .iter()
        .map(|j| (j.id, extras(LAPTOP_CHECKOUT, "view-step-up")))
        .collect();
    Scenario {
        name: "metrics-idle",
        machine: machine(
            "laptop",
            Role::Host,
            8,
            Some(0.3),
            31 * GIB,
            26 * GIB,
            None,
            idle_slots(),
            40,
        ),
        jobs,
        extras: extras_map,
        scan: Scan::default(),
        blockers: Blockers::default(),
    }
}

fn build(s: &Scenario) -> Snapshot {
    let selected = super::super::snapshot::select(&s.jobs);
    let mut machine = s.machine.clone();
    machine.slots = s.machine.slots.clone();
    let scan = Scan {
        slots: s.machine.slots.clone(),
        waiters: s.scan.waiters.clone(),
    };
    assemble(Inputs {
        now: NOW,
        machine,
        jobs: &selected,
        all_jobs: &s.jobs,
        extras: &s.extras,
        scan: &scan,
        blockers: &s.blockers,
        worker_running: true,
    })
}

fn scenarios() -> Vec<Scenario> {
    vec![laptop_busy(), worker_busy(), idle()]
}

fn path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/snapshot")
        .join(format!("{name}.json"))
}

#[test]
#[ignore = "writes tests/fixtures/snapshot/metrics-*.json"]
fn write_fixtures() {
    for s in scenarios() {
        let text = serde_json::to_string_pretty(&build(&s)).unwrap();
        std::fs::create_dir_all(path(s.name).parent().unwrap()).unwrap();
        std::fs::write(path(s.name), text + "\n").unwrap();
    }
}

#[test]
fn fixture_files_match_what_the_code_produces() {
    for s in scenarios() {
        let file: Snapshot = serde_json::from_slice(&std::fs::read(path(s.name)).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", s.name));
        assert_eq!(file, build(&s), "{} is stale: rerun write_fixtures", s.name);
    }
}

fn keys(v: &serde_json::Value) -> BTreeSet<String> {
    v.as_object().unwrap().keys().cloned().collect()
}
fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn snapshot_json_has_exactly_the_contract_field_names() {
    let mut snapshot = build(&laptop_busy());
    // Offload linkage is filled by feature/status-stream; check its shape too.
    snapshot.jobs[0].offload = Some(OffloadLink {
        worker: "dev@192.0.2.7".into(),
        remote_job_id: Some(93),
        transport: "ssh".into(),
        sync_s: Some(1.0),
        fetch_s: None,
    });
    let v = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(keys(&v), set(&["version", "taken_at", "machine", "jobs"]));
    assert_eq!(v["version"], 1);
    let m = &v["machine"];
    assert_eq!(
        keys(m),
        set(&[
            "id",
            "role",
            "logical_cpus",
            "cpu_busy_cores",
            "mem_total_bytes",
            "mem_available_bytes",
            "swap_total_bytes",
            "swap_used_bytes",
            "pressure",
            "blocker",
            "slots",
            "history"
        ])
    );
    assert_eq!(
        keys(&m["pressure"]),
        set(&[
            "cpu_some_avg10",
            "memory_some_avg10",
            "memory_full_avg10",
            "io_some_avg10"
        ])
    );
    assert_eq!(
        keys(&m["history"]),
        set(&["interval_s", "cpu_busy_cores", "mem_used_bytes"])
    );
    assert!(m["history"]["cpu_busy_cores"].as_array().unwrap().len() <= HISTORY_POINTS);
    for s in m["slots"].as_array().unwrap() {
        assert_eq!(
            keys(s),
            set(&["kind", "name", "busy", "holder_job", "holder_who"])
        );
        assert!(["compile", "run", "offload", "device"].contains(&s["kind"].as_str().unwrap()));
    }
    assert!(["host", "worker"].contains(&m["role"].as_str().unwrap()));
    let job_keys = set(&[
        "id",
        "owner",
        "phase",
        "resource",
        "device",
        "checkout",
        "checkout_name",
        "command",
        "kind_key",
        "argv",
        "submitted_at",
        "started_at",
        "finished_at",
        "queue",
        "live",
        "progress",
        "warmth",
        "warmth_reason",
        "estimate",
        "result",
        "offload",
    ]);
    let mut seen = BTreeSet::new();
    for j in v["jobs"].as_array().unwrap() {
        assert_eq!(keys(j), job_keys);
        assert!(
            [
                "queued",
                "approved",
                "preparing",
                "waiting_build",
                "building",
                "waiting_run",
                "running",
                "passed",
                "failed",
                "interrupted",
                "cancelled"
            ]
            .contains(&j["phase"].as_str().unwrap())
        );
        assert!(["compile", "run", "offload", "device"].contains(&j["resource"].as_str().unwrap()));
        assert!(["cold", "warm", "unknown"].contains(&j["warmth"].as_str().unwrap()));
        for (field, expected) in [
            ("queue", set(&["position", "waiting_for"])),
            (
                "live",
                set(&["cpu_cores", "rss_bytes", "processes", "history"]),
            ),
            ("progress", set(&["crates_compiled", "crates_expected"])),
            (
                "estimate",
                set(&["total_s", "remaining_s", "samples", "basis"]),
            ),
            (
                "result",
                set(&[
                    "exit_code",
                    "wall_s",
                    "cpu_user_s",
                    "cpu_sys_s",
                    "peak_rss_bytes",
                    "crates_compiled",
                    "warmth",
                ]),
            ),
            (
                "offload",
                set(&["worker", "remote_job_id", "transport", "sync_s", "fetch_s"]),
            ),
        ] {
            if !j[field].is_null() {
                assert_eq!(keys(&j[field]), expected, "{field}");
                seen.insert(field);
            }
        }
        if !j["live"].is_null() {
            assert_eq!(
                keys(&j["live"]["history"]),
                set(&["interval_s", "cpu_cores", "rss_bytes"])
            );
        }
    }
    assert_eq!(
        seen.len(),
        6,
        "every optional object appears at least once: {seen:?}"
    );
}

#[test]
fn laptop_snapshot_explains_queue_progress_and_eta() {
    let s = build(&laptop_busy());
    let job = |id| s.jobs.iter().find(|j| j.id == id).unwrap();
    let building = job(301);
    assert_eq!(building.command, "cargo test -p video --release");
    assert_eq!(building.warmth, Warmth::Cold);
    assert_eq!(
        building.progress,
        Some(Progress {
            crates_compiled: 212,
            crates_expected: Some(310),
            test_targets_built: None,
        })
    );
    let eta = building.estimate.as_ref().unwrap();
    assert_eq!(
        (eta.total_s, eta.remaining_s, eta.samples),
        (426.0, 235.75, 1)
    );
    assert!(building.live.is_some() && building.queue.is_none());
    let waiting = job(302);
    let q = waiting.queue.as_ref().unwrap();
    assert_eq!(q.position, Some(1));
    assert_eq!(q.waiting_for.as_deref(), Some("compile slot held by #301"));
    assert!(
        waiting.estimate.as_ref().unwrap().basis.contains("unknown"),
        "{:?}",
        waiting.estimate
    );
    let offload = job(303);
    assert_eq!(
        offload.command,
        "[offload] cargo test -p video --release --lib -- sim::"
    );
    assert_eq!(offload.resource, SlotKind::Offload);
    assert!(offload.progress.is_none() && offload.offload.is_none());
    let device = job(304);
    assert_eq!(device.device.as_deref(), Some("TESTSERIAL01"));
    assert_eq!(
        device.queue.as_ref().unwrap().waiting_for.as_deref(),
        Some("device TESTSERIAL01 slot held by #299")
    );
    assert!(
        job(305)
            .queue
            .as_ref()
            .unwrap()
            .waiting_for
            .as_ref()
            .unwrap()
            .starts_with("approval")
    );
    // History: finished jobs carry results; the pre-metrics row has unknown warmth.
    let old = job(284);
    assert_eq!(old.result.as_ref().unwrap().wall_s, Some(100.0));
    assert_eq!(old.warmth, Warmth::Unknown);
    assert_eq!(job(283).result.as_ref().unwrap().exit_code, Some(101));
}

#[test]
fn worker_snapshot_names_the_pressure_blocker_and_queue_order() {
    let s = build(&worker_busy());
    assert_eq!(
        s.machine.blocker.as_deref(),
        Some("memory PSI some avg10 above 1%")
    );
    let job = |id| s.jobs.iter().find(|j| j.id == id).unwrap();
    assert_eq!(job(94).queue.as_ref().unwrap().position, Some(1));
    assert_eq!(job(95).queue.as_ref().unwrap().position, Some(2));
    assert_eq!(
        job(95).queue.as_ref().unwrap().waiting_for.as_deref(),
        Some("memory PSI some avg10 above 1%")
    );
    let run = job(93);
    assert_eq!(
        run.checkout,
        "/home/dev/src/app/.claude/worktrees/rate-recovery"
    );
    // Past its build stage with 4 crates against a 310-crate cold build: warm.
    assert_eq!(run.warmth, Warmth::Warm, "{}", run.warmth_reason);
    let eta = run.estimate.as_ref().unwrap();
    assert_eq!(eta.total_s, 90.5);
    assert_eq!(eta.remaining_s, 5.25);
}
