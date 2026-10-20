use super::command::{cargo_invocation, normalize};
use super::history::{History, median};
use super::procfs::*;
use super::ring::Ring;
use super::warmth::{self, CompileCounter, StartWarmth, refine};
use super::*;
use crate::execution::{Execution, StageTiming};
use crate::local_command::{Recipe, Resource};
use crate::{Cost, Job, JobSpec, Phase};

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|s| s.to_string()).collect()
}

// ---- /proc parsing -------------------------------------------------------

const STAT: &str = "cpu  1000 50 300 8000 200 10 5 0 0 0
cpu0 500 25 150 4000 100 5 2 0 0 0
cpu1 500 25 150 4000 100 5 3 0 0 0
intr 12345
ctxt 999
";

#[test]
fn proc_stat_gives_busy_ticks_and_logical_cpus() {
    let (t, cpus) = parse_stat(STAT).unwrap();
    assert_eq!(cpus, 2);
    assert_eq!(t.total, 1000 + 50 + 300 + 8000 + 200 + 10 + 5);
    assert_eq!(t.busy, t.total - 8000 - 200);
    let later = CpuTimes {
        busy: t.busy + 150,
        total: t.total + 200,
    };
    assert_eq!(busy_cores(t, later, 2), Some(1.5));
    assert_eq!(busy_cores(later, t, 2), None, "counter went backwards");
    assert_eq!(busy_cores(t, t, 2), None, "no interval");
    assert!(parse_stat("intr 1\n").is_none());
}

#[test]
fn meminfo_reads_total_available_and_swap_in_bytes() {
    let text = "MemTotal:       32768000 kB\nMemFree:  100 kB\nMemAvailable:   16384000 kB\nSwapTotal:       8192000 kB\nSwapFree:        8000000 kB\n";
    let m = parse_meminfo(text).unwrap();
    assert_eq!(m.total, 32768000 * 1024);
    assert_eq!(m.available, 16384000 * 1024);
    assert_eq!(m.swap_total - m.swap_free, 192000 * 1024);
    assert!(
        parse_meminfo("MemTotal: 1 kB\n").is_none(),
        "no MemAvailable"
    );
}

/// A /proc/PID/stat line; comm may contain spaces and parentheses.
fn stat_line(
    pid: u32,
    comm: &str,
    ppid: u32,
    pgrp: u32,
    session: u32,
    ticks: (u64, u64, u64, u64),
    rss: u64,
) -> String {
    format!(
        "{pid} ({comm}) S {ppid} {pgrp} {session} 0 -1 4194304 10 0 0 0 {} {} {} {} 20 0 1 0 {} 1000000 {rss} 18446744073709551615",
        ticks.0,
        ticks.1,
        ticks.2,
        ticks.3,
        1000 + pid
    )
}

#[test]
fn proc_pid_stat_survives_hostile_comm() {
    let p = parse_proc_stat(&stat_line(42, "a) (b c", 7, 40, 30, (11, 12, 13, 14), 256)).unwrap();
    assert_eq!((p.pid, p.ppid, p.pgrp, p.session), (42, 7, 40, 30));
    assert_eq!(p.state, 'S');
    assert_eq!(p.cpu_ticks, 23);
    assert_eq!(p.child_ticks, 27);
    assert_eq!(p.start_ticks, 1042);
    assert_eq!(p.rss_pages, 256);
    assert!(parse_proc_stat("garbage").is_none());
}

fn proc(pid: u32, ppid: u32, pgrp: u32, session: u32) -> ProcStat {
    ProcStat {
        pid,
        ppid,
        pgrp,
        session,
        state: 'S',
        ..Default::default()
    }
}

#[test]
fn job_tree_follows_parents_and_groups_but_not_escaped_daemons() {
    // worker 10 (own group); supervisor 100 (group 100); admission 101
    // (group 101) -> __command 102 -> cargo 103 -> rustc 104.
    let procs = vec![
        proc(1, 0, 1, 1),
        proc(10, 1, 10, 5),
        proc(100, 10, 100, 5),
        proc(101, 100, 101, 5),
        proc(102, 101, 101, 5),
        proc(103, 102, 101, 5),
        proc(104, 103, 101, 5),
        // Orphan still in the admission group (parent exited): kept.
        proc(105, 1, 101, 5),
        // setsid'ed but still a descendant: kept (by parent walk).
        proc(106, 103, 106, 106),
        proc(107, 106, 106, 106),
        // Double-forked daemon (adb/sccache server): new session, reparented: excluded.
        proc(108, 1, 108, 108),
        // Another job's tree and an unrelated process.
        proc(200, 10, 200, 5),
        proc(201, 200, 201, 5),
        proc(300, 1, 300, 300),
    ];
    let pids = |sup| -> Vec<u32> {
        job_members(&procs, sup)
            .into_iter()
            .map(|i| procs[i].pid)
            .collect()
    };
    assert_eq!(pids(100), vec![100, 101, 102, 103, 104, 105, 106, 107]);
    assert_eq!(pids(200), vec![200, 201]);
    assert!(pids(999).is_empty(), "unknown supervisor");
}

#[test]
fn proc_locks_lists_only_flock_holders() {
    let text = "1: FLOCK  ADVISORY  WRITE 1264778 00:22:38339807 0 EOF
2: POSIX  ADVISORY  WRITE 2888702 00:22:15608284 1073741826 1073742335
3: FLOCK  ADVISORY  WRITE 1211953 103:02:4398045 0 EOF
3: -> FLOCK  ADVISORY  WRITE 1211999 103:02:777 0 EOF
";
    let held = parse_flocks(text);
    assert!(held.contains(&(0, 0x22, 38339807)));
    assert!(held.contains(&(0x103, 2, 4398045)));
    assert!(!held.contains(&(0, 0x22, 15608284)), "POSIX lock");
    assert!(!held.contains(&(0x103, 2, 777)), "blocked waiter");
    assert_eq!(held.len(), 2);
}

#[test]
fn ring_keeps_the_newest_points_in_order() {
    let mut r = Ring::new(3);
    assert!(r.is_empty());
    for v in 1..=5 {
        r.push(v);
    }
    assert_eq!(r.to_vec(), vec![3, 4, 5]);
    assert_eq!(r.len(), 3);
    assert_eq!(r.last(), Some(&5));
    let mut full = Ring::new(HISTORY_POINTS);
    for v in 0..1000 {
        full.push(v);
    }
    assert_eq!(full.len(), 150);
    assert_eq!(full.to_vec()[0], 850);
}

// ---- command normalisation (argv from the real ledger) --------------------

fn cmd(words: &[&str]) -> String {
    let n = normalize(&argv(words));
    assert_eq!(n.command, n.kind_key);
    n.command
}

#[test]
fn normalisation_drops_env_jobs_lockfile_flags_and_paths() {
    assert_eq!(
        cmd(&[
            "cargo",
            "test",
            "--offline",
            "--locked",
            "-j",
            "2",
            "-p",
            "video",
            "--release",
            "--lib"
        ]),
        "cargo test -p video --release --lib"
    );
    assert_eq!(
        cmd(&[
            "env",
            "VPX_DIR=/home/abe/build-cache/p2p_chat/libvpx/install-v1.17.0-host",
            "cargo",
            "test",
            "-j",
            "2",
            "--release",
            "-p",
            "p2pcore",
            "--features",
            "vp8,dev-tools",
            "--test",
            "video_calls"
        ]),
        "cargo test --release -p p2pcore --features vp8,dev-tools --test video_calls"
    );
    // Varying env values do not split a kind.
    let a = cmd(&[
        "env",
        "SIM_TRACE=reports",
        "SIM_FROM=74.9",
        "SIM_TO=76.0",
        "cargo",
        "test",
        "--offline",
        "--locked",
        "-j",
        "2",
        "-p",
        "video",
        "--release",
        "--lib",
        "--",
        "--nocapture",
        "--test-threads=1",
        "sim::tests::wifi_delay_spikes",
    ]);
    let b = cmd(&[
        "env",
        "SIM_TRACE=reports",
        "SIM_FROM=20.6",
        "SIM_TO=21.3",
        "cargo",
        "test",
        "-j2",
        "-p",
        "video",
        "--release",
        "--lib",
        "--",
        "--nocapture",
        "--test-threads=1",
        "sim::tests::wifi_delay_spikes",
    ]);
    assert_eq!(a, b);
    assert_eq!(
        a,
        "cargo test -p video --release --lib -- --nocapture --test-threads=1 sim::tests::wifi_delay_spikes"
    );
    assert_eq!(
        cmd(&["cargo", "check", "--jobs=2", "-p", "desktop"]),
        "cargo check -p desktop"
    );
    // After `--`, -j belongs to the test binary and is kept.
    assert_eq!(
        cmd(&["cargo", "test", "--", "-j", "4"]),
        "cargo test -- -j 4"
    );
    // Feature lists with '/' are not paths.
    assert_eq!(
        cmd(&[
            "cargo",
            "clippy",
            "-p",
            "video",
            "--features",
            "video/vp8,p2pcore/vp8"
        ]),
        "cargo clippy -p video --features video/vp8,p2pcore/vp8"
    );
}

#[test]
fn normalisation_shortens_programs_and_test_binary_hashes() {
    assert_eq!(
        cmd(&[
            "env",
            "SIM_TRACE=reports",
            "SIM_FROM=12",
            "SIM_TO=26",
            "/home/abe/p2p_chat/.claude/worktrees/rate-recovery/target/release/deps/video-0e73e8183febedc5",
            "sim::tests::lan_delay_bursts",
            "--exact",
            "--nocapture"
        ]),
        "video sim::tests::lan_delay_bursts --exact --nocapture"
    );
    assert_eq!(
        cmd(&[
            "target/release/deps/video-0e73e8183febedc5",
            "media::tests::delay_burst"
        ]),
        "video media::tests::delay_burst"
    );
    assert_eq!(
        cmd(&[
            "adb",
            "-s",
            "9BWAY1LSDC",
            "install",
            "-r",
            "/home/abe/tinline-builds/tinline-b83e596.apk"
        ]),
        "adb -s 9BWAY1LSDC install -r tinline-b83e596.apk"
    );
    assert_eq!(
        cmd(&[
            "env",
            "PYTHONDONTWRITEBYTECODE=1",
            "python3",
            "-m",
            "unittest",
            "discover",
            "-s",
            "tests",
            "-p",
            "test_*.py",
            "-v"
        ]),
        "python3 -m unittest discover -s tests -p test_*.py -v"
    );
    assert_eq!(
        cmd(&["./gradlew", ":app:compileDebugKotlin"]),
        "gradlew :app:compileDebugKotlin"
    );
    assert_eq!(cmd(&[]), "");
}

#[test]
fn normalisation_unwraps_offload_clients_with_a_marker() {
    let inner = cmd(&[
        "tasks-offload",
        "--worker",
        "abe@192.168.1.7",
        "--who",
        "rate-recovery",
        "--ssh-key",
        "/home/abe/.ssh/id_ed25519_nixos_worker",
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
        "sim::tests::lan",
        "--",
        "--nocapture",
    ]);
    assert_eq!(
        inner,
        "[offload] cargo test -p video --release --lib sim::tests::lan -- --nocapture"
    );
    // Absolute client path and --fetch flags are dropped the same way.
    assert_eq!(
        cmd(&[
            "/home/abe/.local/bin/tasks-offload",
            "--worker",
            "abe@192.168.1.7",
            "--who",
            "lead-vp8enc",
            "--ssh-key",
            "/k",
            "--fetch",
            "target/release/p2p-desktop",
            "--",
            "env",
            "VPX_DIR=/x",
            "cargo",
            "build",
            "-j",
            "8",
            "--release",
            "-p",
            "desktop"
        ]),
        "[offload] cargo build --release -p desktop"
    );
    assert_eq!(
        cmd(&[
            "tasks-offload",
            "--worker",
            "w",
            "--",
            "sh",
            "-c",
            "cargo test --locked -j 8 && cargo clippy --locked --all-targets -j 8 -- -D warnings"
        ]),
        "[offload] sh -c cargo test && cargo clippy --all-targets -- -D warnings"
    );
    assert_eq!(cmd(&["tasks-offload", "--worker", "w"]), "[offload]");
}

#[test]
fn normalisation_applies_inside_shell_text() {
    assert_eq!(
        cmd(&[
            "sh",
            "-c",
            "cargo test --offline --locked -j 2 && cargo clippy --offline --locked -j 2 --all-targets -- -D warnings"
        ]),
        "sh -c cargo test && cargo clippy --all-targets -- -D warnings"
    );
    assert_eq!(
        cmd(&[
            "sh",
            "-c",
            "SIM_TRACE=1 SIM_FROM=9.8 SIM_TO=12 cargo test --locked -j 8 -p video --release --lib -- sim::tests::bandwidth_drop"
        ]),
        "sh -c cargo test -p video --release --lib -- sim::tests::bandwidth_drop"
    );
}

#[test]
fn cargo_invocations_are_found_directly_in_env_and_in_shell_text() {
    let inv = cargo_invocation(&argv(&[
        "env",
        "CARGO_TARGET_DIR=/t",
        "cargo",
        "test",
        "--release",
    ]))
    .unwrap();
    assert_eq!(inv.profile_dir(), "release");
    assert_eq!(inv.target_dir(), Some("/t"));
    let inv = cargo_invocation(&argv(&[
        "sh",
        "-c",
        "echo hi; RUST_LOG=1 cargo build --profile bench --target x86_64-unknown-linux-gnu && true",
    ]))
    .unwrap();
    assert_eq!(inv.profile_dir(), "release");
    assert_eq!(inv.target_triple(), Some("x86_64-unknown-linux-gnu"));
    assert_eq!(inv.env, vec!["RUST_LOG=1".to_string()]);
    let inv = cargo_invocation(&argv(&["cargo", "build", "--profile=ci-fast"])).unwrap();
    assert_eq!(inv.profile_dir(), "ci-fast");
    assert_eq!(
        cargo_invocation(&argv(&["cargo", "check"]))
            .unwrap()
            .profile_dir(),
        "debug"
    );
    assert!(cargo_invocation(&argv(&["python3", "x.py"])).is_none());
    assert!(cargo_invocation(&argv(&["tasks-offload", "--", "cargo", "test"])).is_none());
}

// ---- warmth ------------------------------------------------------------------

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tm-metrics-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn start_warmth_is_cold_without_profile_fingerprints() {
    let root = temp_dir("warmth");
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"a\"]\n").unwrap();
    std::fs::create_dir_all(root.join("a/src")).unwrap();
    std::fs::write(root.join("a/Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
    let test = argv(&["cargo", "test", "--release", "-p", "a"]);
    let cwd = root.join("a");
    let w = warmth::at_start(&test, &cwd);
    assert_eq!(w.warmth, Warmth::Cold, "{}", w.reason);
    assert!(w.reason.contains("no build output"), "{}", w.reason);
    // Empty fingerprint directory: still cold. The workspace target is used.
    std::fs::create_dir_all(root.join("target/release/.fingerprint")).unwrap();
    assert_eq!(warmth::at_start(&test, &cwd).warmth, Warmth::Cold);
    std::fs::create_dir(root.join("target/release/.fingerprint/a-123")).unwrap();
    let w = warmth::at_start(&test, &cwd);
    assert_eq!(w.warmth, Warmth::Unknown);
    assert!(w.reason.contains("1 fingerprints"), "{}", w.reason);
    // A different profile has no output yet.
    assert_eq!(
        warmth::at_start(&argv(&["cargo", "test"]), &cwd).warmth,
        Warmth::Cold
    );
    assert_eq!(
        warmth::at_start(&argv(&["python3", "x.py"]), &cwd).reason,
        "not a Cargo command"
    );
    assert!(
        warmth::at_start(&argv(&["tasks-offload", "--", "cargo", "test"]), &cwd)
            .reason
            .starts_with("offloaded")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn warmth_refines_by_compiled_crates_against_history() {
    let unknown = StartWarmth {
        warmth: Warmth::Unknown,
        reason: "build output exists".into(),
    };
    let cold = StartWarmth {
        warmth: Warmth::Cold,
        reason: "no build output".into(),
    };
    assert_eq!(refine(&cold, 0, Some(300), true).0, Warmth::Cold);
    // Early, few crates: still unknown; >20% of R: cold at once.
    assert_eq!(refine(&unknown, 10, Some(300), false).0, Warmth::Unknown);
    let (w, why) = refine(&unknown, 61, Some(300), false);
    assert_eq!(w, Warmth::Cold);
    assert!(why.contains("more than 20% of a full build (300)"), "{why}");
    // Finished at <= 20%: warm.
    let (w, why) = refine(&unknown, 60, Some(300), true);
    assert_eq!(w, Warmth::Warm);
    assert!(why.contains("at most 20%"), "{why}");
    // No history: zero recompiled is warm, otherwise unknown.
    assert_eq!(refine(&unknown, 0, None, true).0, Warmth::Warm);
    assert_eq!(refine(&unknown, 5, None, true).0, Warmth::Unknown);
}

#[test]
fn compile_counter_handles_split_lines() {
    let mut c = CompileCounter::default();
    c.feed(b"   Compiling serde v1.0.0\n   Comp");
    c.feed(b"iling foo v0.1.0 (/x)\nwarning: Compiling is a word\n    Finished `dev`\n");
    assert_eq!(c.count, 2);
    assert_eq!(CompileCounter::count_text(b"Compiling a\nCompiling b\n"), 2);
}

// ---- history, estimates, backfill ---------------------------------------

fn stage(name: &str, seconds: f64) -> StageTiming {
    StageTiming {
        stage: name.into(),
        wait_seconds: 1.0,
        seconds: Some(seconds),
        exit_code: Some(0),
        signal: None,
    }
}

pub(super) fn job(
    id: i64,
    words: &[&str],
    resource: Resource,
    phase: Phase,
    stages: Vec<StageTiming>,
    metrics: Option<JobMetrics>,
) -> Job {
    let recipe = Recipe {
        broad_test: false,
        argv: argv(words),
        resource,
    };
    let base = 1_792_216_000 - 10_000 + id * 60;
    let terminal = phase.terminal();
    let started = !matches!(phase, Phase::Queued | Phase::Approved);
    Job {
        id,
        spec: JobSpec {
            suite: crate::local_command::SUITE.into(),
            owner: "view-step-up@kurukkan".into(),
            source: "/home/abe/p2p_chat/.claude/worktrees/view-step-up".into(),
            revision: "live-workspace".into(),
            configuration: serde_json::to_string(&recipe).unwrap(),
            cost: Cost::CpuSimulation,
        },
        phase: phase.clone(),
        submitted_at: base,
        execution: (phase != Phase::Queued).then(|| Execution {
            approved_at: base + 1,
            started_at: started.then_some(base + 2),
            finished_at: terminal
                .then_some(base + 2 + stages.iter().filter_map(|s| s.seconds).sum::<f64>() as i64),
            cancel_requested: false,
            worker: None,
            supervisor: None,
            snapshot: None,
            stages,
            build_artifact: None,
            android_trial: false,
            local_command: true,
            detail: String::new(),
            metrics,
            peer: None,
            reroute: None,
            remote_result: None,
        }),
    }
}

fn measured(warmth: Warmth, crates: u64) -> Option<JobMetrics> {
    Some(JobMetrics {
        warmth,
        warmth_reason: "fixture".into(),
        crates_compiled: Some(crates),
        ..Default::default()
    })
}

const VIDEO: &[&str] = &[
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

#[test]
fn estimate_prefers_same_warmth_then_falls_back_with_a_note() {
    let kind = normalize(&argv(VIDEO)).kind_key;
    let jobs = vec![
        job(
            1,
            VIDEO,
            Resource::Compile,
            Phase::Passed,
            vec![stage("build", 300.0), stage("run", 20.0)],
            measured(Warmth::Cold, 310),
        ),
        job(
            2,
            VIDEO,
            Resource::Compile,
            Phase::Passed,
            vec![stage("build", 40.0), stage("run", 20.0)],
            measured(Warmth::Warm, 3),
        ),
        job(
            3,
            VIDEO,
            Resource::Compile,
            Phase::Passed,
            vec![stage("build", 50.0), stage("run", 20.0)],
            measured(Warmth::Warm, 5),
        ),
        job(
            4,
            VIDEO,
            Resource::Compile,
            Phase::Passed,
            vec![stage("build", 30.0), stage("run", 30.0)],
            measured(Warmth::Warm, 2),
        ),
        // Failed runs never teach durations.
        job(
            5,
            VIDEO,
            Resource::Compile,
            Phase::Failed,
            vec![stage("build", 1.0)],
            measured(Warmth::Warm, 0),
        ),
    ];
    let h = History::from_jobs(&jobs);
    let warm = h.estimate(&kind, Warmth::Warm, 99).unwrap();
    assert_eq!((warm.total_s, warm.samples), (60.0, 3));
    assert_eq!(
        warm.basis,
        "median of 3 warm runs of this kind on this machine"
    );
    let cold = h.estimate(&kind, Warmth::Cold, 99).unwrap();
    assert_eq!((cold.total_s, cold.samples), (320.0, 1));
    assert_eq!(
        cold.basis,
        "median of 1 cold run of this kind on this machine"
    );
    let unknown = h.estimate(&kind, Warmth::Unknown, 99).unwrap();
    assert_eq!(unknown.samples, 4);
    assert!(
        unknown.basis.contains("any warmth (no unknown history)"),
        "{}",
        unknown.basis
    );
    // The job itself is excluded, and unrelated kinds have no estimate.
    assert_eq!(h.estimate(&kind, Warmth::Cold, 1).unwrap().samples, 3);
    assert!(h.estimate("cargo build", Warmth::Warm, 99).is_none());
    assert_eq!(h.crates_expected(&kind, Warmth::Cold, 99), Some(310));
    assert_eq!(
        h.crates_expected(&kind, Warmth::Warm, 99),
        Some(3),
        "passed runs only: 3,5,2"
    );
    assert_eq!(h.reference_crates(&kind, 99), Some(310));
    assert_eq!(
        h.reference_crates(&kind, 1),
        Some(5),
        "no cold run: largest count"
    );
}

#[test]
fn backfill_uses_old_rows_without_metrics_as_unknown_warmth() {
    // Ledger rows written before metrics existed: argv + stage seconds only.
    let jobs: Vec<Job> = (1..=5)
        .map(|i| {
            job(
                i,
                VIDEO,
                Resource::Compile,
                Phase::Passed,
                vec![stage("build", 100.0 + i as f64), stage("run", 10.0)],
                None,
            )
        })
        .collect();
    let h = History::from_jobs(&jobs);
    let kind = normalize(&argv(VIDEO)).kind_key;
    let e = h.estimate(&kind, Warmth::Unknown, 99).unwrap();
    assert_eq!((e.total_s, e.samples), (113.0, 5));
    assert!(e.basis.contains("(warmth unknown)"), "{}", e.basis);
    // A warm job falls back to them with a note; no crate history exists.
    assert!(
        h.estimate(&kind, Warmth::Warm, 99)
            .unwrap()
            .basis
            .contains("any warmth")
    );
    assert_eq!(h.crates_expected(&kind, Warmth::Warm, 99), None);
    assert_eq!(h.reference_crates(&kind, 99), None);
}

#[test]
fn medians() {
    assert_eq!(median(&mut []), None);
    assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
    assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), Some(2.5));
}

// ---- persisted result ------------------------------------------------------

#[test]
fn finished_metrics_merge_into_execution_json_compatibly() {
    use crate::Store;
    use crate::execution::ProcessIdentity;
    let mut store = Store::memory().unwrap();
    let recipe = Recipe {
        broad_test: false,
        argv: argv(&["cargo", "test", "-p", "a"]),
        resource: Resource::Compile,
    };
    let job = store
        .submit(&recipe.spec("agent".into(), "/fixture".into()).unwrap())
        .unwrap();
    store.approve_command(job.id).unwrap();
    store
        .claim(ProcessIdentity {
            pid: 1,
            start_ticks: 1,
        })
        .unwrap()
        .unwrap();
    let metrics = measured(Warmth::Warm, 4).unwrap();
    // Not terminal yet: refused.
    assert!(!sampler::write_metrics(&mut store, job.id, &metrics).unwrap());
    store
        .finish(job.id, Phase::Failed, "fixture failure")
        .unwrap();
    // A field this release does not know survives the merge.
    store
        .0
        .execute(
            "UPDATE executions SET data=json_set(data,'$.future_field',7) WHERE job_id=?1",
            [job.id],
        )
        .unwrap();
    assert!(sampler::write_metrics(&mut store, job.id, &metrics).unwrap());
    assert!(
        !sampler::write_metrics(&mut store, job.id, &JobMetrics::default()).unwrap(),
        "never overwritten"
    );
    let stored = store.job(job.id).unwrap().execution.unwrap();
    assert_eq!(stored.metrics, Some(metrics));
    let raw: String = store
        .0
        .query_row(
            "SELECT data FROM executions WHERE job_id=?1",
            [job.id],
            |r| r.get(0),
        )
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["future_field"], 7);
    // An older release's Execution type (no `metrics`, no deny_unknown_fields) still reads it.
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct OldExecution {
        approved_at: i64,
        started_at: Option<i64>,
        finished_at: Option<i64>,
        cancel_requested: bool,
        worker: Option<ProcessIdentity>,
        supervisor: Option<ProcessIdentity>,
        snapshot: Option<crate::snapshot::Snapshot>,
        stages: Vec<StageTiming>,
        detail: String,
    }
    serde_json::from_str::<OldExecution>(&raw).unwrap();
}

#[test]
fn finalize_records_new_jobs_and_never_rewrites_pre_metrics_history() {
    use crate::Store;
    use crate::execution::ProcessIdentity;
    let state = temp_dir("finalize");
    let mut store = Store::memory().unwrap();
    let finished = |store: &mut Store, words: &[&str]| -> i64 {
        let recipe = Recipe {
            broad_test: false,
            argv: argv(words),
            resource: Resource::Run,
        };
        let job = store
            .submit(&recipe.spec("agent".into(), "/fixture".into()).unwrap())
            .unwrap();
        store.approve_command(job.id).unwrap();
        store
            .claim(ProcessIdentity {
                pid: 1,
                start_ticks: 1,
            })
            .unwrap()
            .unwrap();
        store.finish(job.id, Phase::Failed, "fixture").unwrap();
        job.id
    };
    let old = finished(&mut store, &["true"]);
    let old_new_worker = finished(&mut store, &["true"]);
    let dir = sampler::job_dir(&state, old_new_worker);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("run-usage.json"),
        r#"{"cpu_user_s":1.5,"cpu_sys_s":0.25,"max_rss_bytes":4096}"#,
    )
    .unwrap();
    let sampler = sampler::Sampler::new(state.clone());
    sampler.finalize(&mut store).unwrap();
    let metrics = |store: &Store, id| store.execution(id).unwrap().unwrap().metrics;
    assert_eq!(metrics(&store, old), None, "pre-metrics row untouched");
    let m = metrics(&store, old_new_worker).unwrap();
    assert_eq!((m.cpu_user_s, m.cpu_sys_s), (Some(1.5), Some(0.25)));
    assert_eq!(m.peak_rss_bytes, Some(4096));
    // A job finishing after startup is recorded even if never sampled live.
    let fresh = finished(&mut store, &["true"]);
    sampler.finalize(&mut store).unwrap();
    let m = metrics(&store, fresh).unwrap();
    assert_eq!(m.cpu_user_s, None, "no usage file: unknown, not zero");
    assert_eq!(m.warmth_reason, "start state not recorded");
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn finish_metrics_combine_usage_log_count_and_sampled_peak() {
    let history = History::from_jobs(&[job(
        1,
        VIDEO,
        Resource::Compile,
        Phase::Passed,
        vec![stage("build", 300.0)],
        measured(Warmth::Cold, 300),
    )]);
    let j = job(
        2,
        VIDEO,
        Resource::Compile,
        Phase::Passed,
        vec![stage("build", 30.0), stage("run", 12.5)],
        None,
    );
    let usage = sampler::StageUsage {
        cpu_user_s: 50.0,
        cpu_sys_s: 5.0,
        max_rss_bytes: 900,
    };
    let start = StartWarmth {
        warmth: Warmth::Unknown,
        reason: "exists".into(),
    };
    let m = sampler::finish_metrics(
        &j,
        &history,
        Some(usage.clone()),
        Some(12),
        Some(start),
        Some(4000),
    );
    assert_eq!(m.warmth, Warmth::Warm);
    assert_eq!(m.wall_s, Some(42.5));
    assert_eq!((m.cpu_user_s, m.cpu_sys_s), (Some(50.0), Some(5.0)));
    assert_eq!(
        m.peak_rss_bytes,
        Some(4000),
        "summed sampled peak beats one process"
    );
    assert_eq!(m.crates_compiled, Some(12));
    let m = sampler::finish_metrics(&j, &history, Some(usage), Some(200), None, None);
    assert_eq!(m.warmth, Warmth::Cold);
    assert_eq!(m.peak_rss_bytes, Some(900));
    // Non-cargo commands carry no crate count.
    let py = job(
        3,
        &["python3", "x.py"],
        Resource::Run,
        Phase::Passed,
        vec![stage("run", 1.0)],
        None,
    );
    assert_eq!(
        sampler::finish_metrics(&py, &history, None, Some(3), None, None).crates_compiled,
        None
    );
}

#[test]
fn rusage_delta_never_goes_negative() {
    let a = sampler::StageUsage {
        cpu_user_s: 2.0,
        cpu_sys_s: 1.0,
        max_rss_bytes: 10,
    };
    let b = sampler::StageUsage {
        cpu_user_s: 5.0,
        cpu_sys_s: 0.5,
        max_rss_bytes: 20,
    };
    let d = sampler::usage_delta(&a, &b);
    assert_eq!((d.cpu_user_s, d.cpu_sys_s, d.max_rss_bytes), (3.0, 0.0, 20));
}

#[test]
fn slot_holder_notes_name_job_and_owner() {
    assert_eq!(
        slots::parse_who("view-step-up@kurukkan:job-91"),
        (Some(91), "view-step-up@kurukkan".into())
    );
    assert_eq!(
        slots::parse_who("legacy-python"),
        (None, "legacy-python".into())
    );
    assert_eq!(slots::parse_who("a:job-x"), (None, "a:job-x".into()));
}

#[path = "fixture_tests.rs"]
mod fixture_tests;

/// Thread CPU seconds (RUSAGE_THREAD): what one sampling pass costs.
fn thread_cpu() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut u) };
    let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    s(u.ru_utime) + s(u.ru_stime)
}

/// Measures the sampler's per-tick CPU on this host (prints; asserts a bound).
///   cargo test --lib metrics::tests::sampler_cost -- --ignored --nocapture
#[test]
#[ignore = "measurement; prints CPU ms per tick on this host"]
fn sampler_cost() {
    use crate::Store;
    use crate::execution::ProcessIdentity;
    const N: usize = 50;
    let procs = read_procs_now();
    let t = thread_cpu();
    for _ in 0..N {
        std::hint::black_box(sampler::read_procs());
    }
    let walk = (thread_cpu() - t) / N as f64 * 1000.0;
    let t = thread_cpu();
    let mut tree_len = 0;
    let mut tree_supported = true;
    for _ in 0..N {
        match sampler::read_tree(std::process::id(), &[]) {
            Some(tree) => tree_len = tree.len(),
            None => tree_supported = false,
        }
    }
    let tree = (thread_cpu() - t) / N as f64 * 1000.0;
    // A ledger with history plus N_JOBS active jobs whose supervisor is this
    // test process (its tree: the test binary and its threads' process).
    let state = temp_dir("cost");
    let mut store = Store::open(&state.join("tasks.sqlite")).unwrap();
    for _ in 0..300 {
        let r = Recipe {
            broad_test: false,
            argv: argv(&["cargo", "test", "-j", "2"]),
            resource: Resource::Compile,
        };
        let j = store
            .submit(&r.spec("bench".into(), "/fixture".into()).unwrap())
            .unwrap();
        store.approve_command(j.id).unwrap();
        store
            .claim(ProcessIdentity {
                pid: 1,
                start_ticks: 1,
            })
            .unwrap();
        store.finish(j.id, Phase::Failed, "bench").unwrap();
    }
    let me = crate::worker::identity(std::process::id()).unwrap();
    let mut children = vec![];
    for _ in 0..4 {
        let r = Recipe {
            broad_test: false,
            argv: argv(&["cargo", "test", "-j", "2"]),
            resource: Resource::Compile,
        };
        let j = store
            .submit(&r.spec("bench".into(), "/fixture".into()).unwrap())
            .unwrap();
        store.approve_command(j.id).unwrap();
        let claimed = store
            .claim(ProcessIdentity {
                pid: 1,
                start_ticks: 1,
            })
            .unwrap()
            .unwrap();
        let mut e = claimed.execution.unwrap();
        e.supervisor = Some(me.clone());
        store.update_execution(j.id, Phase::Building, e).unwrap();
        children.push(
            std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        );
    }
    let sampler = sampler::Sampler::new(state.clone());
    sampler.tick(Some(&mut store)).unwrap(); // first tick: startup finalize pass
    let t = thread_cpu();
    for _ in 0..N {
        sampler.tick(Some(&mut store)).unwrap();
    }
    // Includes the periodic full walks (1 in sampler::FULL_WALK_EVERY ticks).
    let tick = (thread_cpu() - t) / N as f64 * 1000.0;
    // Live compile attribution: fake compilers in the jobs' tree (scripts named rustc: a real
    // rustc's comm and cmdline shape), so each tick also reads their cmdlines. The script
    // blocks in the shell's `read` builtin (no grandchild left behind when it is killed).
    let rustc_script = state.join("bin/rustc");
    std::fs::create_dir_all(rustc_script.parent().unwrap()).unwrap();
    std::fs::write(&rustc_script, "#!/bin/sh\nread -r _\n").unwrap();
    std::fs::set_permissions(
        &rustc_script,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    for i in 0..8 {
        children.push(
            std::process::Command::new(&rustc_script)
                .args(["--crate-name", &format!("fake_{i}"), "--crate-type", "lib"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let t = thread_cpu();
    for _ in 0..N {
        sampler.tick(Some(&mut store)).unwrap();
    }
    let tick_attr = (thread_cpu() - t) / N as f64 * 1000.0;
    let jobs = store.jobs().unwrap();
    let attributed: usize = sampler
        .snapshot(&jobs)
        .jobs
        .iter()
        .filter_map(|j| j.compiling.as_ref())
        .map(Vec::len)
        .sum();
    let t = thread_cpu();
    for _ in 0..10 {
        std::hint::black_box(sampler.snapshot(&jobs));
    }
    let snap = (thread_cpu() - t) / 10.0 * 1000.0;
    let t = thread_cpu();
    for _ in 0..10 {
        std::hint::black_box(store.jobs().unwrap());
    }
    let list = (thread_cpu() - t) / 10.0 * 1000.0;
    for mut c in children {
        let _ = c.kill();
        let _ = c.wait();
    }
    eprintln!(
        "sampler cost: /proc walk of {procs} processes {walk:.2} ms; targeted tree read \
         ({tree_len} processes, children files supported: {tree_supported}) {tree:.3} ms; full tick with 4 running jobs \
         + 304-row ledger {tick:.2} ms CPU ({:.3}% of one core at 2 s); status snapshot {snap:.2} ms; \
         ledger listing (Store::jobs) {list:.2} ms; compile attribution: 8 fake rustc in each \
         job tree, {attributed} entries in the snapshot, tick {tick_attr:.2} ms",
        tick / 20.0
    );
    std::fs::remove_dir_all(state).unwrap();
    assert!(tick < 50.0, "tick {tick} ms");
    assert!(tick_attr < 50.0, "tick with attribution {tick_attr} ms");
    assert_eq!(attributed, 32, "8 fake compilers in each of 4 job trees");
}

fn read_procs_now() -> usize {
    sampler::read_procs().len()
}

#[test]
fn a_flock_on_a_btrfs_subvolume_file_is_found_by_inode_despite_the_anonymous_device() {
    use std::os::unix::fs::MetadataExt;
    let path = std::env::temp_dir().join(format!("tasks-flock-held-{}", std::process::id()));
    std::fs::write(&path, "").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let (major, minor) = (libc::major(meta.dev()), libc::minor(meta.dev()));
    let held = |m, n, i| std::collections::HashSet::from([(m, n, i)]);
    assert!(
        flock_held(&held(major, minor, meta.ino()), &meta),
        "exact match"
    );
    assert!(
        !flock_held(&held(major, minor, meta.ino() + 1), &meta),
        "other inode"
    );
    assert!(!flock_held(&Default::default(), &meta));
    // The file's stat device is anonymous (major 0, as on a btrfs subvolume) while
    // /proc/locks names the superblock device: still held.
    if major == 0 {
        assert!(flock_held(&held(0, minor + 1, meta.ino()), &meta));
    }
    std::fs::remove_file(&path).unwrap();
}
