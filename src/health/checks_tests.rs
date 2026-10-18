//! Health checks against fixtures: cluster snapshot files, fake /proc/locks text, synthetic
//! disk numbers, pressure samples and peer views. No live /proc or daemon.
use super::*;
use crate::health::alert::AlertBook;
use crate::health::disk::ReserveClass;
use crate::peer_link::{STREAM_OFFLINE, STREAM_ONLINE};

const GIB: u64 = 1024 * 1024 * 1024;
const NOW: f64 = 1_792_216_000.0;

fn fixture(name: &str) -> Snapshot {
    let path = format!(
        "{}/tests/fixtures/snapshot/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn cfg() -> HealthConfig {
    HealthConfig::default()
}

fn disk(free: u64, reserve: u64) -> DiskStatus {
    DiskStatus {
        filesystem: "/home/u/build-cache".into(),
        roots: vec!["build-cache=/home/u/build-cache".into()],
        total_bytes: 500 * GIB,
        free_bytes: free,
        reserve_bytes: reserve,
        reserve_class: ReserveClass::Cache,
        growth_bytes_per_h: None,
        time_to_full_s: None,
        top: vec![Consumer {
            path: "/home/u/build-cache/p2p_chat/tasks/checkouts/abc".into(),
            bytes: 80 * GIB,
            checkout: Some("/home/u/p2p_chat".into()),
            jobs: vec![301],
            partial: false,
        }],
        scanned_at: Some(NOW),
        scan_truncated: false,
    }
}

#[test]
fn disk_thresholds_warn_below_twice_the_reserve_and_go_critical_below_it() {
    let kinds = |d: DiskStatus| -> Vec<(String, Severity)> {
        disks("laptop", &[d], &["#302 waits for disk".into()], &cfg())
            .into_iter()
            .map(|o| (o.kind, o.severity.unwrap()))
            .collect()
    };
    assert!(
        kinds(disk(40 * GIB, 20 * GIB)).is_empty(),
        "exactly 2x is fine"
    );
    assert_eq!(
        kinds(disk(40 * GIB - 1, 20 * GIB)),
        [("disk_low".to_string(), Severity::Warn)]
    );
    let critical = disks(
        "laptop",
        &[disk(19 * GIB, 20 * GIB)],
        &["#302 waits".into()],
        &cfg(),
    );
    assert_eq!(critical.len(), 1);
    let o = &critical[0];
    assert_eq!(o.kind, "disk_reserve");
    assert_eq!(o.severity, Some(Severity::Critical));
    assert_eq!(o.id(), "disk_reserve:laptop:/home/u/build-cache");
    assert!(
        o.summary.contains("19.0 GiB free, reserve 20.0 GiB"),
        "{}",
        o.summary
    );
    assert!(
        o.evidence
            .iter()
            .any(|e| e.contains("checkout /home/u/p2p_chat")),
        "{:?}",
        o.evidence
    );
    assert!(o.evidence.iter().any(|e| e == "waiting: #302 waits"));
    assert!(o.needs_human);
    // Time to full: under 6 h warns, under 1 h is critical.
    let mut filling = disk(100 * GIB, 20 * GIB);
    filling.growth_bytes_per_h = Some(20.0 * GIB as f64);
    filling.time_to_full_s = Some(5.0 * 3600.0);
    assert_eq!(
        kinds(filling.clone()),
        [("disk_filling".to_string(), Severity::Warn)]
    );
    filling.time_to_full_s = Some(1800.0);
    assert_eq!(
        kinds(filling.clone()),
        [("disk_filling".to_string(), Severity::Critical)]
    );
    filling.time_to_full_s = Some(7.0 * 3600.0);
    assert!(kinds(filling).is_empty());
}

#[test]
fn large_checkout_outputs_are_named_with_their_checkout() {
    let t = disk(100 * GIB, 20 * GIB).top;
    let o = targets("laptop", &t, &cfg());
    assert_eq!(o.len(), 1, "80 GiB is over the 60 GiB default");
    assert!(
        o[0].summary.contains("/home/u/p2p_chat is 80.0 GiB"),
        "{}",
        o[0].summary
    );
    let mut c = cfg();
    c.target_max_bytes = 100 * GIB;
    assert!(targets("laptop", &t, &c).is_empty());
}

/// The worker case: the compile flock held by the daemon itself, no job in the ledger.
#[test]
fn a_slot_held_with_no_live_job_names_the_holder_after_the_grace_period() {
    let mut snap = fixture("metrics-idle");
    for s in &mut snap.machine.slots {
        if s.name == "p2p-build" {
            s.busy = true;
            s.holder_job = None;
            s.holder_who = None;
        }
    }
    let locks = crate::health::procs::parse_locks(
        "1: FLOCK  ADVISORY  WRITE 4242 00:1b:678 0 EOF\n\
         2: FLOCK  ADVISORY  WRITE 5151 00:1b:999 0 EOF\n",
    );
    let files = vec![
        ("p2p-build".to_string(), (0, 0x1b, 678)),
        ("p2p-run-0".to_string(), (0, 0x1b, 700)),
    ];
    let cmd = |pid: u32| (pid == 4242).then(|| "tasks daemon".to_string());
    let holders = |_: &str| vec![];
    let no_env = |_: u32| None;
    let inputs = SlotInputs {
        locks: &locks,
        files: &files,
        cmdline: &cmd,
        fd_holders: &holders,
        job_env: &no_env,
    };
    let mut since = Since::default();
    let c = cfg();
    assert!(
        slots("nixos", &snap, &inputs, &mut since, NOW, &c).is_empty(),
        "grace period"
    );
    assert!(slots("nixos", &snap, &inputs, &mut since, NOW + 119.0, &c).is_empty());
    let o = slots("nixos", &snap, &inputs, &mut since, NOW + 121.0, &c);
    assert_eq!(o.len(), 1);
    assert_eq!(o[0].id(), "slot-holder:nixos:p2p-build.lock");
    assert!(
        o[0].summary.contains("compile slot p2p-build held for 2m"),
        "{}",
        o[0].summary
    );
    assert!(
        o[0].evidence
            .iter()
            .any(|e| e == "/proc/locks: FLOCK held by pid 4242 (tasks daemon)"),
        "{:?}",
        o[0].evidence
    );
    // A job that then builds explains the slot: the condition and its timer reset.
    let mut building = snap.clone();
    let mut job = fixture("metrics-laptop-busy")
        .jobs
        .into_iter()
        .find(|j| j.id == 301)
        .unwrap();
    job.phase = Phase::Building;
    building.jobs.push(job);
    assert!(slots("nixos", &building, &inputs, &mut since, NOW + 130.0, &c).is_empty());
    assert!(
        slots("nixos", &snap, &inputs, &mut since, NOW + 131.0, &c).is_empty(),
        "timer restarted"
    );
}

#[test]
fn holder_notes_and_executing_jobs_explain_busy_slots() {
    let snap = fixture("metrics-laptop-busy");
    // Compile slot: note names live job 301; offload: 303; the device slot's note names job
    // 299, which is not in the ledger's live jobs: only that one is unexplained.
    let names: Vec<&str> = unexplained_slots(&snap.machine.slots, &snap.jobs)
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(names, ["p2p-device-TESTSERIAL01"]);
    // Without notes an executing job of the kind still explains one busy slot.
    let mut no_notes = snap.clone();
    for s in &mut no_notes.machine.slots {
        s.holder_job = None;
    }
    let names: Vec<&str> = unexplained_slots(&no_notes.machine.slots, &no_notes.jobs)
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(names, ["p2p-device-TESTSERIAL01"]);
}

#[test]
fn memory_pressure_must_last_and_names_the_largest_processes() {
    let mut m = fixture("metrics-laptop-busy").machine;
    let top = || {
        vec![ProcMemory {
            pid: 77,
            name: "tasks".into(),
            rss_bytes: GIB,
            swap_bytes: 5 * GIB,
            cmdline: Some("tasks ui".into()),
        }]
    };
    let mut since = Since::default();
    let c = cfg();
    let min = 8 * GIB;
    assert!(
        memory("laptop", &m, min, &top, &mut since, NOW, &c).is_empty(),
        "no pressure"
    );
    m.pressure.as_mut().unwrap().memory_some_avg10 = 12.0;
    assert!(
        memory("laptop", &m, min, &top, &mut since, NOW, &c).is_empty(),
        "starts now"
    );
    assert!(memory("laptop", &m, min, &top, &mut since, NOW + 299.0, &c).is_empty());
    let o = memory("laptop", &m, min, &top, &mut since, NOW + 301.0, &c);
    assert_eq!(o.len(), 1);
    assert_eq!(o[0].id(), "memory_pressure:laptop");
    assert!(
        o[0].summary.contains("memory PSI some 12.0%"),
        "{}",
        o[0].summary
    );
    assert!(
        o[0].evidence
            .iter()
            .any(|e| e.contains("pid 77 tasks: 1.0 GiB RSS + 5.0 GiB swap (tasks ui)")),
        "{:?}",
        o[0].evidence
    );
    // Low available memory alone also counts; relief resets the timer.
    m.pressure.as_mut().unwrap().memory_some_avg10 = 0.0;
    assert!(memory("laptop", &m, min, &top, &mut since, NOW + 302.0, &c).is_empty());
    m.mem_available_bytes = Some(GIB);
    assert!(
        memory("laptop", &m, min, &top, &mut since, NOW + 303.0, &c).is_empty(),
        "restarted"
    );
    let o = memory("laptop", &m, min, &top, &mut since, NOW + 700.0, &c);
    assert!(
        o[0].summary
            .contains("available memory 1.0 GiB below 8.0 GiB"),
        "{}",
        o[0].summary
    );
}

fn peer_view(name: &str, state: &str, stale: bool, age: Option<f64>) -> PeerSnapshot {
    PeerSnapshot {
        name: name.into(),
        role: "worker".into(),
        state: state.into(),
        since: NOW,
        error: (state == STREAM_OFFLINE).then(|| "connect timed out".into()),
        received_at: age.map(|a| NOW - a),
        age_s: age,
        stale,
        stale_after_s: 16.0,
        snapshot: None,
    }
}

#[test]
fn stale_and_offline_peers_alert_after_the_grace_period() {
    let mut since = Since::default();
    let c = cfg();
    let fresh = peer_view("worker", STREAM_ONLINE, false, Some(1.0));
    let stale = peer_view("worker", STREAM_ONLINE, true, Some(95.0));
    let offline = peer_view("worker", STREAM_OFFLINE, true, Some(400.0));
    assert!(peers(std::slice::from_ref(&fresh), &mut since, NOW, &c).is_empty());
    assert!(peers(std::slice::from_ref(&stale), &mut since, NOW, &c).is_empty());
    let o = peers(std::slice::from_ref(&stale), &mut since, NOW + 61.0, &c);
    assert_eq!(o[0].id(), "peer_stale:worker");
    assert!(
        o[0].summary.contains("last snapshot 2m ago"),
        "{}",
        o[0].summary
    );
    // Going offline is a different condition with its own grace period.
    assert!(peers(std::slice::from_ref(&offline), &mut since, NOW + 62.0, &c).is_empty());
    let o = peers(&[offline], &mut since, NOW + 130.0, &c);
    assert_eq!(o[0].id(), "peer_offline:worker");
    assert!(o[0].evidence[0].contains("connect timed out"));
    assert!(peers(&[fresh], &mut since, NOW + 131.0, &c).is_empty());
}

#[test]
fn long_waits_name_their_reason() {
    let snap = fixture("metrics-laptop-busy");
    let submitted = snap.jobs.iter().find(|j| j.id == 302).unwrap().submitted_at as f64;
    let c = cfg();
    assert!(waiting("laptop", &snap.jobs, submitted + 60.0, &c).is_empty());
    let o = waiting("laptop", &snap.jobs, submitted + 3600.0, &c);
    let ids: Vec<String> = o.iter().map(|o| o.id()).collect();
    assert!(
        ids.contains(&"job_waiting:laptop:302".to_string()),
        "{ids:?}"
    );
    let o302 = o
        .iter()
        .find(|o| o.subject.as_deref() == Some("302"))
        .unwrap();
    assert!(
        o302.summary.contains("compile slot held by #301"),
        "{}",
        o302.summary
    );
    // A proposal waiting for approval is not an admission wait.
    assert!(!ids.contains(&"job_waiting:laptop:305".to_string()));
}

fn worker_peer() -> crate::placement::Peer {
    let mut worker = fixture("metrics-worker-busy");
    worker.machine.blocker = None;
    worker.machine.cargo_jobs_max = Some(6);
    for s in &mut worker.machine.slots {
        s.busy = false;
        s.holder_job = None;
    }
    worker.jobs.retain(|j| j.phase.terminal());
    crate::placement::Peer {
        name: "worker".into(),
        role: "worker".into(),
        state: STREAM_ONLINE.into(),
        age_s: Some(1.0),
        snapshot: Some(worker),
        ..Default::default()
    }
}

#[test]
fn queued_compile_jobs_with_an_idle_peer_are_reported_with_placements_reasoning() {
    let mut local = fixture("metrics-laptop-busy");
    local.machine.blocker = Some("memory PSI some avg10 above 1%".into());
    let submitted = local
        .jobs
        .iter()
        .find(|j| j.id == 302)
        .unwrap()
        .submitted_at as f64;
    let c = cfg();
    let peers = vec![worker_peer()];
    assert!(
        idle_peer("laptop", &local, &peers, submitted + 60.0, &c).is_empty(),
        "too recent"
    );
    let o = idle_peer("laptop", &local, &peers, submitted + 600.0, &c);
    assert_eq!(o.len(), 1, "{o:?}");
    assert_eq!(o[0].id(), "idle_peer:laptop:worker");
    assert_eq!(o[0].severity, Some(Severity::Warn), "pressured laptop");
    assert!(
        o[0].summary
            .contains("1 compile job(s) queued on laptop (blocked: memory PSI"),
        "{}",
        o[0].summary
    );
    assert!(
        o[0].evidence.iter().any(|e| e.starts_with("#302 ")),
        "{:?}",
        o[0].evidence
    );
    assert!(
        o[0].evidence
            .iter()
            .any(|e| e.starts_with("placement anywhere -> worker")),
        "{:?}",
        o[0].evidence
    );
    // A busy worker is not idle.
    let mut busy = worker_peer();
    if let Some(s) = busy.snapshot.as_mut() {
        s.machine.blocker = Some("CPU PSI some avg10 above 40%".into());
    }
    assert!(idle_peer("laptop", &local, &[busy], submitted + 600.0, &c).is_empty());
    assert!(idle_peer("laptop", &local, &[], submitted + 600.0, &c).is_empty());
}

#[test]
fn high_cpu_jobs_name_the_agent_and_command() {
    let mut snap = fixture("metrics-laptop-busy");
    let job = snap.jobs.iter_mut().find(|j| j.id == 284).unwrap();
    let finished = job.finished_at.unwrap() as f64;
    let r = job.result.as_mut().unwrap();
    r.cpu_user_s = Some(17_000.0);
    r.cpu_sys_s = Some(1_000.0);
    let o = high_cpu("laptop", &snap.jobs, finished + 60.0, &cfg());
    assert_eq!(o.len(), 1);
    assert_eq!(o[0].severity, Some(Severity::Info));
    assert!(o[0].summary.starts_with("job #284 by "), "{}", o[0].summary);
    assert!(
        o[0].summary.contains("used 5.0 CPU-hours"),
        "{}",
        o[0].summary
    );
    assert!(
        high_cpu("laptop", &snap.jobs, finished + 7.0 * 3600.0, &cfg()).is_empty(),
        "old"
    );
}

#[test]
fn dedupe_and_auto_clear_through_the_book() {
    let mut book = AlertBook::new("laptop");
    let low = |free| disks("laptop", &[disk(free, 20 * GIB)], &[], &cfg());
    let raised = book.observe("disk", low(10 * GIB), NOW);
    assert_eq!(raised.len(), 1);
    // Repeats update, never duplicate.
    for i in 1..5 {
        assert!(
            book.observe("disk", low(9 * GIB), NOW + i as f64)
                .is_empty()
        );
    }
    assert_eq!(book.active.len(), 1);
    let a = book.active.values().next().unwrap();
    assert_eq!(a.count, 5);
    assert_eq!(a.first_seen, NOW);
    assert_eq!(a.last_seen, NOW + 4.0);
    assert!(
        a.summary.contains("9.0 GiB free"),
        "latest observation wins"
    );
    // Another check's run does not clear it.
    book.observe("peers", vec![], NOW + 5.0);
    assert_eq!(book.active.len(), 1);
    // Condition gone: cleared and kept in recent.
    book.observe("disk", low(100 * GIB), NOW + 6.0);
    assert!(book.active.is_empty());
    assert_eq!(book.recent.back().unwrap().cleared_at, Some(NOW + 6.0));
    // Back within the flap window: same alert continues (first_seen kept).
    let raised = book.observe("disk", low(10 * GIB), NOW + 60.0);
    assert_eq!(raised.len(), 1);
    assert_eq!(raised[0].first_seen, NOW);
    assert!(book.recent.is_empty());
    // Capability marks become alerts about the marked node.
    let mark = crate::health::capability::CapabilityMark {
        node: "nixos".into(),
        capability: "libclang".into(),
        family: "libclang".into(),
        job: 456,
        remote_job: None,
        kind_key: "cargo build".into(),
        line: "Unable to find libclang: \"couldn't find any valid shared libraries matching: ['libclang.so', ...".into(),
        marked_at: NOW,
        expires_at: NOW + 86400.0,
        source: "job log".into(),
    };
    let o = capabilities(&[mark]);
    assert_eq!(o[0].id(), "missing_capability:nixos:libclang");
    assert_eq!(o[0].summary, "nixos lacks libclang (bindgen)");
    assert!(o[0].evidence[0].starts_with("job #456: Unable to find libclang"));
}

/// 2026-10-17: an sccache server spawned inside jobs 154/155 inherited the p2p-build.lock
/// lease; the locking process exited. The alert names the fd holder, its command line and the
/// job it was spawned in, and the fix names the compiler-cache service.
#[test]
fn an_inherited_lease_names_the_sccache_holder_and_its_origin_job() {
    let mut snap = fixture("metrics-idle");
    for s in &mut snap.machine.slots {
        if s.name == "p2p-build" {
            s.busy = true;
            s.holder_job = None;
            s.holder_who = None;
        }
    }
    let locks =
        crate::health::procs::parse_locks("1: FLOCK  ADVISORY  WRITE 4242 00:1b:678 0 EOF\n");
    let files = vec![("p2p-build".to_string(), (0, 0x1b, 678))];
    // 4242 (the job's command) is gone; 777 is the sccache server holding the fd.
    let cmd = |pid: u32| (pid == 777).then(|| "/home/u/.cargo/bin/sccache".to_string());
    let holders = |name: &str| {
        if name == "p2p-build" {
            vec![777]
        } else {
            vec![]
        }
    };
    let env = |pid: u32| (pid == 777).then(|| "154".to_string());
    let inputs = SlotInputs {
        locks: &locks,
        files: &files,
        cmdline: &cmd,
        fd_holders: &holders,
        job_env: &env,
    };
    let mut since = Since::default();
    let c = cfg();
    assert!(slots("nixos", &snap, &inputs, &mut since, NOW, &c).is_empty());
    let o = slots("nixos", &snap, &inputs, &mut since, NOW + 121.0, &c);
    assert_eq!(o.len(), 1);
    assert_eq!(o[0].id(), "slot-holder:nixos:p2p-build.lock");
    let ev = o[0].evidence.join("\n");
    assert!(
        ev.contains("pid 4242 (exited; a child inherited the descriptor)"),
        "{ev}"
    );
    assert!(
        ev.contains("open by pid 777 (/home/u/.cargo/bin/sccache), TASKS_JOB_ID=154: spawned inside job #154"),
        "{ev}"
    );
    assert!(o[0].fix.contains("compiler-cache server"), "{}", o[0].fix);
}

#[test]
fn environ_parsing_is_exact_and_bounded() {
    let env = b"PATH=/bin\0TASKS_JOB_ID=155\0TASKS_JOB_ID_X=1\0";
    assert_eq!(
        crate::health::procs::parse_environ(env, "TASKS_JOB_ID").as_deref(),
        Some("155")
    );
    assert_eq!(crate::health::procs::parse_environ(env, "HOME"), None);
}

/// The cache reserve is `TASKS_MIN_CACHE_FREE_GIB` in GiB (default 20); no memory/MiB setting
/// (e.g. the worker's `TASKS_MIN_AVAILABLE_MEMORY_MIB=1024`) feeds it. 219 GiB free against
/// the real 20 GiB reserve renders as such and raises no disk alert. (The 65536 GiB in the
/// sample and hermetic tests is set on purpose there to force the alert.)
#[test]
fn disk_reserve_unit_is_gib_and_ample_free_space_raises_nothing() {
    use crate::health::disk::{History, Measured, ReserveClass, statuses};
    const GIB: u64 = 1024 * 1024 * 1024;
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| std::ffi::OsString::from(v))
        }
    };
    let t = crate::admission::Thresholds::from_lookup(env(&[(
        "TASKS_MIN_AVAILABLE_MEMORY_MIB",
        "1024",
    )]))
    .unwrap();
    assert_eq!(t.min_cache_free_bytes, 20 * GIB);
    assert_eq!(t.min_available_memory_bytes, 1024 * 1024 * 1024);
    let explicit = crate::admission::Thresholds::from_lookup(env(&[
        ("TASKS_MIN_CACHE_FREE_GIB", "20"),
        ("TASKS_MIN_AVAILABLE_MEMORY_MIB", "1024"),
    ]))
    .unwrap();
    assert_eq!(explicit.min_cache_free_bytes, 20 * GIB);
    let measured = Measured {
        filesystems: vec![(
            "/home/u/build-cache".into(),
            vec!["build-cache=/home/u/build-cache".into()],
            428 * GIB,
            219 * GIB,
            ReserveClass::Cache,
        )],
        keys: vec![],
    };
    let s = statuses(&measured, &explicit, &History::default(), None, 0.0);
    assert_eq!(s[0].reserve_bytes, 20 * GIB);
    assert!(
        s[0].line()
            .contains("219.0 GiB free of 428.0 GiB (reserve 20.0 GiB)"),
        "{}",
        s[0].line()
    );
    assert!(disks("nixos", &s, &[], &cfg()).is_empty());
}
