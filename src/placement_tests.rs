use super::*;
use crate::metrics::{SlotKind, Snapshot};
use crate::peer_link::{STREAM_CONNECTING, STREAM_OFFLINE, STREAM_ONLINE};

fn fixture(name: &str) -> Snapshot {
    let path = format!(
        "{}/tests/fixtures/snapshot/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

/// The laptop fixture with its Cargo cap (2), as a current daemon reports it.
fn laptop(name: &str) -> Snapshot {
    let mut s = fixture(name);
    s.machine.cargo_jobs_max = Some(2);
    s
}

/// The worker fixture without its PSI blocker, with its Cargo cap (6).
fn open_worker() -> Snapshot {
    let mut worker = fixture("metrics-worker-busy");
    worker.machine.blocker = None;
    worker.machine.cargo_jobs_max = Some(6);
    worker
}

fn peer(snapshot: Snapshot) -> Peer {
    Peer {
        name: "worker".into(),
        role: "worker".into(),
        state: STREAM_ONLINE.into(),
        age_s: Some(1.0),
        snapshot: Some(snapshot),
        ..Default::default()
    }
}

fn anywhere(argv: &str, host: &Snapshot, peers: &[Peer]) -> Decision {
    decide(
        &words(argv),
        &Class::Compile,
        &Request::Anywhere,
        host,
        2,
        peers,
    )
    .unwrap()
}

fn score<'a>(d: &'a Decision, target: &Target) -> &'a Score {
    d.scores.iter().find(|s| &s.target == target).unwrap()
}

fn worker_target() -> Target {
    Target::Peer("worker".into())
}

#[test]
fn strip_jobs_drops_every_cargo_parallelism_form_only_before_harness_args() {
    for (input, want) in [
        ("cargo test -j 8 --release", "cargo test --release"),
        ("cargo build -j2 -p desktop", "cargo build -p desktop"),
        ("cargo test --jobs 4 -p video", "cargo test -p video"),
        ("cargo test --jobs=4 -p video", "cargo test -p video"),
        (
            "env CARGO_BUILD_JOBS=3 VPX_DIR=/x cargo test -j 8 -- -j 3",
            "env VPX_DIR=/x cargo test -- -j 3",
        ),
        ("env CARGO_BUILD_JOBS=3 cargo check", "cargo check"),
        (
            "cargo test -jobs-like-test-name",
            "cargo test -jobs-like-test-name",
        ),
        ("sh -c cargo-test-j-8", "sh -c cargo-test-j-8"),
        ("./gradlew -j 8", "./gradlew -j 8"),
    ] {
        assert_eq!(strip_jobs(&words(input)), words(want), "{input}");
    }
}

#[test]
fn apply_jobs_sets_the_cap_in_an_env_prefix_and_passes_other_commands_through() {
    for (input, jobs, want) in [
        (
            "cargo test -j 2 -p video",
            6,
            "env CARGO_BUILD_JOBS=6 cargo test -p video",
        ),
        (
            "env VPX_DIR=/x cargo build --jobs=8",
            2,
            "env CARGO_BUILD_JOBS=2 VPX_DIR=/x cargo build",
        ),
        (
            "env -- CARGO_BUILD_JOBS=9 cargo check",
            6,
            "env CARGO_BUILD_JOBS=6 cargo check",
        ),
        (
            "env -- A=1 cargo check -j 2",
            6,
            "env -- CARGO_BUILD_JOBS=6 A=1 cargo check",
        ),
        (
            "/usr/bin/cargo test -- -j 3",
            6,
            "env CARGO_BUILD_JOBS=6 /usr/bin/cargo test -- -j 3",
        ),
        ("sh -c 'cargo build -j 8'", 6, "sh -c 'cargo build -j 8'"),
        ("./gradlew assembleDebug", 6, "./gradlew assembleDebug"),
        ("make -j 8", 6, "make -j 8"),
    ] {
        assert_eq!(apply_jobs(&words(input), jobs), words(want), "{input}");
    }
    // The rewritten command passes the receiving machine's own validation at its cap.
    crate::local_command::Recipe {
        broad_test: false,
        argv: apply_jobs(&words("cargo test -j 2"), 6),
        resource: crate::local_command::Resource::Compile,
    }
    .validate_with_max(6)
    .unwrap();
    assert!(
        crate::local_command::Recipe {
            broad_test: false,
            argv: apply_jobs(&words("cargo test"), 6),
            resource: crate::local_command::Resource::Compile,
        }
        .validate_with_max(2)
        .is_err()
    );
}

#[test]
fn request_parses_local_anywhere_and_peer_names() {
    assert_eq!(Request::parse("local").unwrap(), Request::Local);
    assert_eq!(Request::parse("anywhere").unwrap(), Request::Anywhere);
    assert_eq!(
        Request::parse("worker").unwrap(),
        Request::Peer("worker".into())
    );
    assert!(Request::parse("bad name").is_err());
    assert!(Request::parse("").is_err());
}

#[test]
fn busy_laptop_sends_work_to_an_open_worker_with_caps_6_and_2() {
    let host = laptop("metrics-laptop-busy");
    let d = anywhere(
        "cargo test -j 2 -p video --release",
        &host,
        &[peer(open_worker())],
    );
    // Laptop: compile slot held by #301 (235.75 s left) + #302 queued (48 s); median of the
    // 426/90.5/100 passed runs here = 100 s.
    let here = score(&d, &Target::Local);
    assert_eq!(here.wait_s, 235.75 + 48.0);
    assert_eq!(here.run_s, 100.0);
    assert_eq!(here.cargo_jobs, Some(2));
    // Worker: #94 + #95 queued for its compile slot (48 + 100); no history of this kind
    // there, so the laptop's median scaled by the ACTUAL caps, 2/6 (not 8).
    let there = score(&d, &worker_target());
    assert_eq!(there.wait_s, 148.0);
    assert!((there.run_s - 100.0 * 2.0 / 6.0).abs() < 1e-9, "{there:?}");
    assert_eq!(there.cargo_jobs, Some(6));
    assert_eq!(d.target, worker_target());
    assert_eq!(d.machine_id.as_deref(), Some("worker"));
    // The worker's cap goes into the command; the user's -j is gone.
    assert_eq!(d.cargo_jobs, Some(6));
    assert_eq!(
        d.argv,
        words("env CARGO_BUILD_JOBS=6 cargo test -p video --release")
    );
    assert!(
        d.line().starts_with("placement anywhere -> worker"),
        "{}",
        d.line()
    );

    // Laptop chosen: its own cap (2) is applied.
    let d = anywhere(
        "cargo clippy -j 1 --all-targets -- -D warnings",
        &laptop("metrics-idle"),
        &[peer(open_worker())],
    );
    assert_eq!(d.target, Target::Local, "{}", d.line());
    assert_eq!(d.cargo_jobs, Some(2));
    assert_eq!(
        d.argv,
        words("env CARGO_BUILD_JOBS=2 cargo clippy --all-targets -- -D warnings")
    );
}

#[test]
fn idle_laptop_wins_when_the_worker_queue_is_longer() {
    let d = anywhere(
        "cargo clippy --all-targets -- -D warnings",
        &laptop("metrics-idle"),
        &[peer(open_worker())],
    );
    let here = score(&d, &Target::Local);
    assert_eq!((here.wait_s, here.run_s), (0.0, 48.0));
    let there = score(&d, &worker_target());
    assert_eq!((there.wait_s, there.run_s), (148.0, 48.0));
    assert_eq!(d.target, Target::Local);
    assert_eq!(d.reason, "laptop sooner");
}

#[test]
fn a_blocked_machine_counts_as_extra_wait_not_skipped() {
    let host = laptop("metrics-laptop-busy");
    let mut blocked = fixture("metrics-worker-busy");
    blocked.machine.cargo_jobs_max = Some(6);
    let d = anywhere("cargo test -p video --release", &host, &[peer(blocked)]);
    // Still scored: 148 s queued + the blocker's 600 s.
    assert!(d.skipped.is_empty(), "{:?}", d.skipped);
    let there = score(&d, &worker_target());
    assert_eq!(there.wait_s, 148.0 + BLOCKED_S);
    assert!(
        there.wait_basis.contains("blocked: memory PSI"),
        "{there:?}"
    );
    assert_eq!(d.target, Target::Local, "{}", d.line());

    // A blocked laptop against an open, idle worker: the worker.
    let mut host = laptop("metrics-idle");
    host.machine.blocker = Some("CPU PSI some avg10 above 40%".into());
    let mut idle = open_worker();
    idle.jobs.clear();
    let d = anywhere("cargo build -p never-seen", &host, &[peer(idle.clone())]);
    assert_eq!(score(&d, &Target::Local).wait_s, BLOCKED_S);
    assert_eq!(d.target, worker_target());
    // Both blocked: both scored, equal, so the tie goes to the worker.
    idle.machine.blocker = Some("memory PSI full avg10 above 0.5%".into());
    let d = anywhere("cargo build -p never-seen", &host, &[peer(idle)]);
    assert_eq!(d.scores.len(), 2);
    assert_eq!(d.scores[0].total_s, d.scores[1].total_s);
    assert_eq!(d.target, worker_target());
}

#[test]
fn offline_connecting_and_stale_peers_are_skipped_and_fall_back_to_local() {
    let host = laptop("metrics-laptop-busy");
    let offline = Peer {
        state: STREAM_OFFLINE.into(),
        error: Some("connect timed out".into()),
        ..peer(open_worker())
    };
    let stale = Peer {
        name: "second".into(),
        stale: true,
        age_s: Some(95.0),
        ..peer(open_worker())
    };
    let connecting = Peer {
        name: "third".into(),
        state: STREAM_CONNECTING.into(),
        snapshot: None,
        age_s: None,
        ..peer(open_worker())
    };
    let d = anywhere(
        "cargo test -j 2 -p video",
        &host,
        &[offline, stale, connecting],
    );
    assert_eq!(d.target, Target::Local);
    assert_eq!(d.scores.len(), 1);
    let skipped: Vec<_> = d
        .skipped
        .iter()
        .map(|s| format!("{}: {}", s.machine, s.why))
        .collect();
    assert_eq!(
        skipped,
        [
            "worker: offline: connect timed out",
            "second: stale snapshot (95s old)",
            "third: no status from it yet (connecting)",
        ]
    );
    assert!(
        d.reason.starts_with("no usable worker peer"),
        "{}",
        d.reason
    );
    assert!(d.notes.iter().any(|n| n.contains("fell back to local")));
    // Falling back still applies this machine's cap.
    assert_eq!(d.argv, words("env CARGO_BUILD_JOBS=2 cargo test -p video"));

    // An explicit offline peer is an error (nothing to submit); unknown/host roles too.
    let offline = Peer {
        state: STREAM_OFFLINE.into(),
        error: Some("connect timed out".into()),
        ..peer(open_worker())
    };
    let host_role = Peer {
        name: "upstream".into(),
        role: "host".into(),
        ..peer(open_worker())
    };
    let explicit = |name: &str| {
        decide(
            &words("cargo check"),
            &Class::Compile,
            &Request::Peer(name.into()),
            &host,
            2,
            &[offline.clone(), host_role.clone()],
        )
    };
    let e = explicit("worker").unwrap_err();
    assert!(
        e.contains("offline") && e.contains("nothing was submitted"),
        "{e}"
    );
    assert!(explicit("nope").unwrap_err().contains("unknown peer"));
    assert!(explicit("upstream").unwrap_err().contains("role host"));
}

#[test]
fn device_always_local_run_local_unless_named_and_no_peers_is_local() {
    let host = laptop("metrics-idle");
    let mut idle = open_worker();
    idle.jobs.clear();
    let peers = [peer(idle)];
    let d = decide(
        &words("adb -s emulator-5554 install -r app.apk"),
        &Class::Device("emulator-5554".into()),
        &Request::Anywhere,
        &host,
        2,
        &peers,
    )
    .unwrap();
    assert_eq!(d.target, Target::Local);
    assert!(d.reason.contains("device class always stays local"));
    let d = decide(
        &words("cargo test -j 2"),
        &Class::Run,
        &Request::Anywhere,
        &host,
        2,
        &peers,
    )
    .unwrap();
    assert_eq!(d.target, Target::Local);
    assert!(d.reason.contains("run class stays local"));
    // Untouched: no peer decision was made.
    assert_eq!(d.argv, words("cargo test -j 2"));
    // A named peer takes the run class, with its cap applied.
    let d = decide(
        &words("cargo test -j 2"),
        &Class::Run,
        &Request::Peer("worker".into()),
        &host,
        2,
        &peers,
    )
    .unwrap();
    assert_eq!(d.target, worker_target());
    assert_eq!(d.argv, words("env CARGO_BUILD_JOBS=6 cargo test"));
    assert!(d.reason.contains("named explicitly"));
    // No peers configured: anywhere is local, unchanged.
    let d = anywhere("cargo test -j 2", &host, &[]);
    assert_eq!(d.target, Target::Local);
    assert_eq!(d.argv, words("cargo test -j 2"));
    // Explicit local never rewrites.
    let d = decide(
        &words("cargo test -j 1"),
        &Class::Compile,
        &Request::Local,
        &host,
        2,
        &peers,
    )
    .unwrap();
    assert_eq!(
        (d.target, d.argv),
        (Target::Local, words("cargo test -j 1"))
    );
}

#[test]
fn ties_and_unknowns_go_to_the_worker_and_non_cargo_passes_through() {
    let mut host = laptop("metrics-idle");
    host.jobs.clear();
    let mut idle = open_worker();
    idle.jobs.clear();
    let d = anywhere("cargo build -p never-seen", &host, &[peer(idle.clone())]);
    assert_eq!(d.scores[0].total_s, d.scores[1].total_s);
    assert_eq!(d.scores[0].run_s, UNKNOWN_S);
    assert_eq!(d.target, worker_target(), "{}", d.line());
    assert!(d.reason.starts_with("tie"), "{}", d.reason);
    // A non-Cargo command is placed but never rewritten.
    let d = anywhere("/bin/sh -c true", &host, &[peer(idle.clone())]);
    assert_eq!(d.target, worker_target());
    assert_eq!(d.argv, words("/bin/sh -c true"));
    assert_eq!(d.cargo_jobs, None);
    // A peer whose snapshot has no cap: placed, ratio assumes 2, command left unchanged.
    let mut old = idle;
    old.machine.cargo_jobs_max = None;
    let d = anywhere("cargo build -j 2 -p never-seen", &host, &[peer(old)]);
    assert_eq!(d.target, worker_target());
    assert_eq!(d.cargo_jobs, None);
    assert_eq!(d.argv, words("cargo build -j 2 -p never-seen"));
    assert!(d.notes.iter().any(|n| n.contains("cap is unknown")));
}

#[test]
fn laptop_forwarded_history_estimates_the_worker_and_full_offload_slots_add_wait() {
    let mut host = laptop("metrics-idle");
    let mut forwarded = host.jobs[0].clone();
    forwarded.id = 999;
    forwarded.resource = SlotKind::Offload;
    forwarded.kind_key = "[offload] cargo build -p app".into();
    forwarded.result.as_mut().unwrap().wall_s = Some(30.0);
    host.jobs.push(forwarded);
    for slot in &mut host.machine.slots {
        if slot.kind == SlotKind::Offload {
            slot.busy = true;
        }
    }
    let mut worker = open_worker();
    worker.jobs.clear();
    let d = anywhere("cargo build -p app", &host, &[peer(worker)]);
    let there = score(&d, &worker_target());
    assert_eq!(there.run_s, 30.0);
    assert_eq!(there.run_basis, "median of 1 forwarded runs");
    let offload_slots = host
        .machine
        .slots
        .iter()
        .any(|s| s.kind == SlotKind::Offload);
    assert_eq!(there.wait_s, if offload_slots { UNKNOWN_S } else { 0.0 });
    // The laptop has no runs of its own: the worker's estimate scaled 6/2.
    assert_eq!(score(&d, &Target::Local).run_s, 90.0);
}

#[test]
fn idle_laptop_beats_a_held_worker_slot_for_a_small_unknown_crate() {
    // Worker: compile slot held, ~10 min left, nothing queued. Laptop idle.
    // No history of this kind anywhere. Ties go to the worker only when equal.
    let mut host = laptop("metrics-idle");
    host.jobs.clear();
    let mut worker = open_worker();
    worker.jobs.retain(|j| j.id == 93);
    let holder = &mut worker.jobs[0];
    holder.phase = Phase::Building;
    holder.estimate.as_mut().unwrap().remaining_s = 600.0;
    let d = anywhere("cargo check -j 2 --bin tasks", &host, &[peer(worker)]);
    let (here, there) = (score(&d, &Target::Local), score(&d, &worker_target()));
    assert_eq!(here.wait_s, 0.0);
    assert_eq!(there.wait_s, 600.0);
    assert_eq!(here.run_s, there.run_s);
    assert_eq!(d.target, Target::Local, "{}", d.line());
    assert_eq!(d.reason, "laptop sooner");
}

#[test]
fn a_held_slot_without_a_known_holder_waits_the_unknown_eta() {
    // What a fixture sees when something outside the ledger holds the compile flock.
    let mut host = laptop("metrics-idle");
    host.jobs.clear();
    for slot in &mut host.machine.slots {
        if slot.kind == SlotKind::Compile {
            slot.busy = true;
        }
    }
    if !host
        .machine
        .slots
        .iter()
        .any(|s| s.kind == SlotKind::Compile)
    {
        host.machine.slots.push(crate::metrics::types::Slot {
            kind: SlotKind::Compile,
            name: "p2p-build".into(),
            busy: true,
            holder_job: None,
            holder_who: None,
            holder_processes: Vec::new(),
        });
    }
    let mut idle = open_worker();
    idle.jobs.clear();
    let d = anywhere("/bin/sh -c true", &host, &[peer(idle)]);
    let here = score(&d, &Target::Local);
    assert_eq!(here.wait_s, UNKNOWN_S);
    assert_eq!(here.wait_basis, "slot held, ETA unknown");
    assert_eq!(d.target, worker_target());
}

#[test]
fn peer_from_report_reads_the_runtime_view() {
    // The runtime keeps peer snapshots without their history rings.
    let mut snapshot = serde_json::to_value(open_worker()).unwrap();
    snapshot["machine"]
        .as_object_mut()
        .unwrap()
        .remove("history");
    for job in snapshot["jobs"].as_array_mut().unwrap() {
        if let Some(live) = job["live"].as_object_mut() {
            live.remove("history");
        }
    }
    let mut report = crate::peer_link::PeerSnapshot {
        name: "worker".into(),
        role: "worker".into(),
        state: STREAM_ONLINE.into(),
        received_at: Some(100.0),
        stale_after_s: 16.0,
        snapshot: Some(snapshot),
        ..Default::default()
    };
    report.refresh(105.0);
    let p = Peer::from_report(&report);
    assert_eq!((p.age_s, p.stale), (Some(5.0), false));
    assert!(p.unusable().is_none());
    report.refresh(130.0);
    assert!(
        Peer::from_report(&report)
            .unusable()
            .unwrap()
            .contains("stale")
    );
    report.snapshot = Some(serde_json::json!({"version": 1}));
    report.refresh(105.0);
    assert!(
        Peer::from_report(&report)
            .unusable()
            .unwrap()
            .contains("unreadable")
    );
}

// ---- feature/health-alerts: missing-capability marks ----

fn libclang_mark(node: &str) -> crate::health::capability::CapabilityMark {
    crate::health::capability::CapabilityMark {
        node: node.into(),
        capability: "libclang".into(),
        family: "libclang".into(),
        job: 456,
        remote_job: Some(77),
        kind_key: "cargo build -p media".into(),
        line: "Unable to find libclang: \"couldn't find any valid shared libraries\"".into(),
        marked_at: 1.0,
        expires_at: f64::MAX,
        source: "forwarded job log".into(),
    }
}

#[test]
fn anywhere_skips_a_peer_lacking_a_capability_the_command_needs_and_says_why() {
    let host = laptop("metrics-laptop-busy");
    let mut worker = peer(open_worker());
    worker.missing = marks_for(
        &worker,
        &[libclang_mark("worker"), libclang_mark("elsewhere")],
    );
    assert_eq!(worker.missing.len(), 1, "only marks about this peer");
    // Without the mark the busy laptop sends a Cargo build to the worker.
    let clean = anywhere("cargo build -j 8", &host, &[peer(open_worker())]);
    assert_eq!(clean.target, worker_target());
    let d = anywhere("cargo build -j 8", &host, std::slice::from_ref(&worker));
    assert_eq!(d.target, Target::Local, "{}", d.line());
    assert_eq!(d.skipped.len(), 1);
    assert!(
        d.skipped[0].why.contains("lacks libclang (bindgen)"),
        "{:?}",
        d.skipped
    );
    assert!(
        d.skipped[0].why.contains("job #456 (remote #77 on worker)"),
        "{:?}",
        d.skipped
    );
    assert!(
        d.line().contains("worker skipped: lacks libclang"),
        "{}",
        d.line()
    );
    // A command that does not need libclang still goes there.
    let d = anywhere("sh -c true", &host, std::slice::from_ref(&worker));
    assert_eq!(d.target, worker_target(), "{}", d.line());
    // retain_needed drops the mark for a non-Cargo command before deciding.
    let mut peers = vec![worker.clone()];
    retain_needed(&mut peers, &words("make all"), None);
    assert!(peers[0].missing.is_empty());
    // Named explicitly: forwarded anyway, with a note.
    let d = decide(
        &words("cargo build"),
        &Class::Compile,
        &Request::Peer("worker".into()),
        &host,
        2,
        std::slice::from_ref(&worker),
    )
    .unwrap();
    assert_eq!(d.target, worker_target());
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("lacks libclang") && n.contains("named")),
        "{:?}",
        d.notes
    );
}

#[test]
fn marks_for_reads_the_peers_own_published_marks_by_machine_id() {
    let mut snapshot = open_worker();
    let id = snapshot.machine.id.clone();
    snapshot.health = Some(crate::health::NodeHealth {
        node: id.clone(),
        checked_at: 1.0,
        capabilities: vec![libclang_mark(&id)],
        ..Default::default()
    });
    let worker = peer(snapshot);
    let marks = marks_for(&worker, &[]);
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].node, id);
    // The controller's own mark wins; one per capability.
    let marks = marks_for(&worker, &[libclang_mark("worker")]);
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].node, "worker");
}

#[test]
fn only_log_only_cargo_commands_are_placed_by_default() {
    let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    for yes in [
        "cargo test -p runtime",
        "cargo check --workspace --tests",
        "cargo +nightly --offline clippy --all-targets",
        "env RUST_LOG=debug cargo nextest run",
        "/home/x/.cargo/bin/cargo test",
        "./android/gradlew -p android --no-daemon :app:compileDebugKotlin :app:testDebugUnitTest",
        "gradle lint",
    ] {
        assert!(placed_by_default(&v(yes)), "{yes}");
    }
    for no in [
        "cargo build -p shell2 --release",
        "cargo run -p app",
        "cargo --color always test",
        "sh -c cargo test",
        "npm run build",
        "cargo",
        "./android/gradlew -p android :app:assembleDebug",
        "./gradlew compileDebugKotlin bundleRelease",
        "./gradlew -p android",
        "gradlew",
    ] {
        assert!(!placed_by_default(&v(no)), "{no}");
    }
}

#[test]
fn a_second_compile_slot_that_is_free_means_no_wait_on_the_holder() {
    let mut host = laptop("metrics-idle");
    host.jobs.clear();
    host.machine.slots.retain(|s| s.kind != SlotKind::Compile);
    for (i, busy) in [true, false].into_iter().enumerate() {
        host.machine.slots.push(crate::metrics::types::Slot {
            kind: SlotKind::Compile,
            name: format!("p2p-build-{i}"),
            busy,
            holder_job: None,
            holder_who: None,
            holder_processes: Vec::new(),
        });
    }
    assert_eq!(compile_wait(&host).0, 0.0);
    host.machine.slots.last_mut().unwrap().busy = true;
    assert_eq!(compile_wait(&host).0, UNKNOWN_S);
}

#[test]
fn a_path_that_exists_only_on_this_machine_is_found_in_the_argv() {
    let dir = std::env::temp_dir().join(format!("tasks-local-only-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("exp.sh");
    std::fs::write(&script, "true").unwrap();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let path = script.to_str().unwrap();
    assert_eq!(local_only_path(&s(&["sh", path])).as_deref(), Some(path));
    let env = format!("SIM_BASE={path}");
    assert_eq!(
        local_only_path(&s(&["env", &env, "x"])).as_deref(),
        Some(path)
    );
    // Missing files, relative paths, system paths and the program itself do not count.
    assert_eq!(
        local_only_path(&s(&["sh", "/tmp/definitely-not-here-4821"])),
        None
    );
    assert_eq!(local_only_path(&s(&["cargo", "test", "-p", "video"])), None);
    assert_eq!(local_only_path(&s(&["/bin/sh", "-c", "true"])), None);
    assert_eq!(local_only_path(&s(&[path, "x"])), None);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn gradle_tasks_are_read_past_options_and_project_dirs() {
    let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    assert_eq!(
        gradle_tasks(&v(
            "./android/gradlew -p android --no-daemon -Pabis=x86 :app:assembleDebug test"
        )),
        Some(vec!["assembleDebug".to_string(), "test".to_string()])
    );
    assert_eq!(gradle_tasks(&v("cargo test")), None);
    assert!(gradle_builds_artifact(&v("./gradlew :app:installDebug")));
    assert!(!gradle_builds_artifact(&v(
        "./gradlew :app:compileDebugKotlin"
    )));
    assert!(!gradle_builds_artifact(&v("cargo build")));
}

#[test]
fn a_tie_goes_to_the_machine_with_more_compile_slots_free() {
    let mut host = laptop("metrics-idle");
    host.jobs.clear();
    let mut worker = open_worker();
    worker.jobs.clear();
    for s in [&mut host, &mut worker] {
        s.machine
            .slots
            .iter_mut()
            .filter(|slot| slot.kind == SlotKind::Compile)
            .for_each(|slot| slot.busy = false);
    }
    // Both free: the worker still wins the tie.
    let d = anywhere("cargo build -p never-seen", &host, &[peer(worker.clone())]);
    assert_eq!(d.target, worker_target(), "{}", d.line());
    // The worker has a second compile slot and holds one (its wait stays 0): the idle laptop
    // wins, so it builds up its own history.
    let mut held = worker
        .machine
        .slots
        .iter()
        .find(|slot| slot.kind == SlotKind::Compile)
        .unwrap()
        .clone();
    held.busy = true;
    worker.machine.slots.push(held);
    let d = anywhere("cargo build -p never-seen", &host, &[peer(worker)]);
    assert_eq!(
        score(&d, &worker_target()).total_s,
        score(&d, &Target::Local).total_s
    );
    assert_eq!(d.target, Target::Local, "{}", d.line());
    assert!(d.reason.contains("more compile slots free"), "{}", d.reason);
}
