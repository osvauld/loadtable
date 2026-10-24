//! Attribution of stalls through the real scheduler over fakes (design §9). Names carry the
//! regression ids they pin.

use crate::api::*;
use crate::testkit::Rig;
use std::sync::mpsc;
use std::time::Duration;
use tasks_core::{Attribution, JobFigures, Kind};
use tasks_ledger::{AuditLedger, AuditRow};
use tasks_peer::sync::{SyncGate, SyncPhase};
use tasks_platform::{CgroupName, ProcInfo};

const CANARY: &str = "SECRET-CANARY-7f3a";

fn proc_row(pid: i32, name: &str, rss: u64, io: (u64, u64)) -> ProcInfo {
    ProcInfo {
        pid,
        ppid: 1,
        pgid: pid,
        start: 5,
        name: name.into(),
        rss_bytes: rss,
        cpu_usec: 0,
        io_read: Some(io.0),
        io_write: Some(io.1),
    }
}

fn trips(r: &Rig) -> Vec<(AuditRow, TripDetails)> {
    r.audit_store
        .scan(0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|x| x.action == "pressure.trip")
        .map(|x| {
            let d = serde_json::from_value(x.details_json()).unwrap();
            (x, d)
        })
        .collect()
}

fn advance(r: &Rig, secs: i64) {
    r.clock.advance(secs * 1000);
}

/// A rig whose I/O limits and trip threshold are the Linux defaults whatever OS runs the test.
fn rig() -> Rig {
    Rig::with_policy(
        "[pressure.compile]\nmax_io = 40\n[pressure.transfer]\nmax_io = 40\n\
         [attribution]\ntrip_io_pct = 10\n",
    )
}

/// Two samples a window apart, then a compile job that the I/O gate refuses.
fn io_stall(r: &Rig) {
    *r.table.table.lock().unwrap() = vec![
        proc_row(100, "firefox", 900 << 20, (1 << 30, 1 << 30)),
        proc_row(101, "tasks-ui", 50 << 20, (0, 0)),
    ];
    r.sch.sample_now();
    advance(r, 6);
    *r.table.table.lock().unwrap() = vec![
        proc_row(100, "firefox", 950 << 20, (1 << 30, 26 << 30)),
        proc_row(101, "tasks-ui", 60 << 20, (0, 1 << 20)),
    ];
    r.sch.sample_now();
    r.machine.set_io(90.0);
    let mut spec = r.spec("o", &["make", CANARY], Kind::Compile);
    spec.env = vec![("API_KEY".into(), CANARY.into())];
    r.submit(&spec);
    r.tick();
}

// R-330
#[test]
fn r330_a_gate_trip_names_the_biggest_processes_outside_jobs_and_never_argv_or_env() {
    let r = rig();
    io_stall(&r);
    let t = trips(&r);
    assert_eq!(t.len(), 1, "one row for the refused gate");
    let (row, d) = &t[0];
    assert_eq!((d.resource.as_str(), d.source.as_str()), ("io", "gate"));
    assert!(d.reason.starts_with("I/O stalls"), "{}", d.reason);
    assert_eq!(d.machine.io_pct, Some(90.0));
    assert_eq!(d.window_ms, 6000);
    assert_eq!(d.top_io[0].name, "firefox");
    assert_eq!(d.top_io[0].pid, 100);
    assert_eq!(d.top_io[0].io_bytes, 25 << 30);
    assert_eq!(d.top_io[1].name, "tasks-ui");
    assert_eq!(d.top_rss[0].name, "firefox");
    assert_eq!(row.target.as_deref(), Some("io"));
    // nothing from the job's command line or environment is in the row
    let text = serde_json::to_string(&row.details_json()).unwrap();
    assert!(
        !text.contains(CANARY) && !text.contains("API_KEY"),
        "{text}"
    );
    assert!(text.len() <= 4096, "{}", text.len());
}

// R-330
#[test]
fn r330_the_status_and_the_stalls_view_show_the_trip() {
    let r = rig();
    io_stall(&r);
    let s = r.sch.status();
    assert_eq!(s.trips.len(), 1);
    assert!(
        s.trips[0]
            .summary
            .contains("busiest outside jobs: firefox (pid 100) moved 25 GiB"),
        "{}",
        s.trips[0].summary
    );
    assert_eq!(s.trips[0].resource, "io");
    // the same rows through the API a client reads
    let reply = r.sch.handle(Request::Audit {
        since_ms: None,
        until_ms: None,
        peer: None,
        job: None,
        action: Some(PRESSURE_TRIP_ACTION.into()),
        limit: None,
    });
    let Reply::Ok(Response::Audit { rows, .. }) = reply else {
        panic!("{reply:?}");
    };
    assert_eq!(rows.len(), 1);
}

// R-331
#[test]
fn r331_trips_are_rate_limited_per_resource_and_count_what_they_dropped() {
    let r = rig();
    io_stall(&r);
    for _ in 0..5 {
        advance(&r, 5);
        r.tick();
    }
    assert_eq!(
        trips(&r).len(),
        1,
        "within a minute only the first is written"
    );
    // memory is its own key
    r.machine.set_memory_level(tasks_platform::Level::Warn);
    r.tick();
    assert_eq!(trips(&r).len(), 2);
    advance(&r, 60);
    r.tick();
    let t = trips(&r);
    let io: Vec<_> = t.iter().filter(|(_, d)| d.resource == "io").collect();
    assert_eq!(io.len(), 2, "a new window lets one through");
    assert!(
        io[1].1.suppressed >= 5,
        "the second row counts the dropped ones: {}",
        io[1].1.suppressed
    );
}

// R-331
#[test]
fn r331_machine_pressure_over_the_alert_threshold_is_a_trip_with_no_job_waiting() {
    let r = rig();
    r.machine.set_io(30.0);
    r.tick();
    let t = trips(&r);
    assert_eq!(t.len(), 1);
    assert_eq!(
        (t[0].1.resource.as_str(), t[0].1.source.as_str()),
        ("io", "psi")
    );
    assert!(t[0].1.reason.contains("30%") && t[0].1.reason.contains("10%"));
    // below the threshold nothing is written
    let r2 = rig();
    r2.machine.set_io(9.0);
    r2.tick();
    assert!(trips(&r2).is_empty());
}

// R-332
#[test]
fn r332_a_transfer_waits_for_the_io_gate_and_then_proceeds() {
    let r = rig();
    r.machine.set_io(90.0);
    let gate = r.sch.transfer_gate();
    let (tx, rx) = mpsc::channel();
    let g = gate.clone();
    let th = std::thread::spawn(move || {
        let guard = g.enter(SyncPhase::Snapshot);
        tx.send(()).unwrap();
        drop(guard);
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(400)).is_err(),
        "the sync must not start while I/O stalls"
    );
    let s = r.sch.status();
    assert!(
        s.transfer
            .as_ref()
            .and_then(|t| t.waiting.clone())
            .is_some_and(|w| w.contains("I/O stalls")),
        "{:?}",
        s.transfer
    );
    // the wait is a trip too, from the transfer
    assert!(trips(&r).iter().any(|(_, d)| d.source == "transfer"));
    r.machine.set_io(5.0);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    th.join().unwrap();
    let s = r.sch.status();
    let t = s.transfer.unwrap();
    assert_eq!((t.active, t.runs, t.waiting), (0, 1, None));
}

// R-332
#[test]
fn r332_one_transfer_slot_serialises_syncs_and_shows_as_busy() {
    let r = rig();
    let gate = r.sch.transfer_gate();
    let first = gate.enter(SyncPhase::Pack);
    let s = r.sch.status();
    let g = s.slots.iter().find(|g| g.kind == "transfer").unwrap();
    assert_eq!((g.total, g.free), (1, 0));
    assert_eq!(g.busy[0].holder.as_deref(), Some("checkout sync"));
    let (tx, rx) = mpsc::channel();
    let g2 = gate.clone();
    let th = std::thread::spawn(move || {
        let second = g2.enter(SyncPhase::Apply);
        tx.send(()).unwrap();
        drop(second);
    });
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    drop(first);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    th.join().unwrap();
}

// R-326
#[test]
fn r326_without_delegation_the_status_says_process_tree_and_why() {
    let r = rig();
    let s = r.sch.status();
    assert_eq!(
        s.attribution,
        "attribution: process tree (no cgroup delegation: test rig)"
    );
}

// R-326, R-327
#[test]
fn r327_figures_without_cgroups_are_summed_over_the_process_tree_and_stored_with_the_job() {
    let r = rig();
    r.procs.auto_exit(0);
    let id = r.submit(&r.spec("o", &["make"], Kind::Compile));
    // the fake group's leader is pid 1000
    *r.table.table.lock().unwrap() = vec![
        proc_row(1000, "make", 100, (10, 20)),
        ProcInfo {
            ppid: 1000,
            pgid: 1000,
            ..proc_row(1001, "cc", 900, (1000, 2000))
        },
        proc_row(55, "firefox", 5000, (99, 99)),
    ];
    r.tick();
    assert_eq!(r.until_done(id), tasks_core::Phase::Passed);
    let rec = r.record(id);
    let f = rec.figures.clone().expect("figures stored");
    assert_eq!(f.quality, Attribution::ProcessTree);
    assert_eq!(
        (f.io_read_bytes, f.io_write_bytes),
        (Some(1010), Some(2020))
    );
    assert_eq!(f.mem_peak_bytes, Some(1000));
    // get and list show them
    let v = crate::job_view(&rec);
    assert_eq!(v.figures, Some(f));
}

// R-323, R-329
#[test]
fn r323_a_job_is_placed_in_its_own_cgroup_and_the_cgroup_goes_when_it_ends() {
    let r = Rig::delegated();
    r.procs.auto_exit(0);
    let mut fig = JobFigures::empty(Attribution::Cgroup);
    fig.io_write_bytes = Some(25 << 30);
    fig.mem_peak_bytes = Some(2 << 30);
    let id = r.submit(&r.spec("o", &["make"], Kind::Compile));
    r.cgroups.set_figures(CgroupName::Job(id.0), fig.clone());
    r.tick();
    assert_eq!(r.until_done(id), tasks_core::Phase::Passed);
    let spawned = r.procs.spawned();
    assert_eq!(
        spawned[0].cgroup_procs.as_deref(),
        Some(std::path::Path::new(&format!(
            "/fake/tasks.slice/job-{}/cgroup.procs",
            id.0
        )))
    );
    assert_eq!(r.record(id).figures, Some(fig));
    assert_eq!(r.cgroups.removed(), [CgroupName::Job(id.0)]);
    assert!(
        r.sch
            .status()
            .attribution
            .starts_with("attribution: exact cgroup")
    );
}

// R-710
#[test]
fn r710_a_compile_job_gets_the_low_cpu_weight_and_a_test_job_the_high_one() {
    let r = Rig::delegated();
    r.procs.auto_exit(0);
    let build = r.submit(&r.spec("o", &["make"], Kind::Compile));
    let test = r.submit(&r.spec_in("o", &["make", "check"], Kind::Test, "t"));
    r.tick();
    assert_eq!(r.until_done(build), tasks_core::Phase::Passed);
    assert_eq!(r.until_done(test), tasks_core::Phase::Passed);
    assert_eq!(r.cgroups.cpu_weight(CgroupName::Job(build.0)), Some(50));
    assert_eq!(r.cgroups.cpu_weight(CgroupName::Job(test.0)), Some(200));
    assert!(r.procs.spawned().iter().all(|s| s.nice == 0));
}

// R-711
#[test]
fn r711_without_cgroups_a_compile_job_is_niced_and_a_test_job_is_not() {
    let r = Rig::new();
    r.procs.auto_exit(0);
    let build = r.submit(&r.spec("o", &["make"], Kind::Compile));
    let test = r.submit(&r.spec_in("o", &["make", "check"], Kind::Test, "t"));
    r.tick();
    assert_eq!(r.until_done(build), tasks_core::Phase::Passed);
    assert_eq!(r.until_done(test), tasks_core::Phase::Passed);
    let mut niceness: Vec<(String, i32)> = r
        .procs
        .spawned()
        .iter()
        .map(|s| (s.argv.join(" "), s.nice))
        .collect();
    niceness.sort();
    assert_eq!(
        niceness,
        [("make".to_string(), 10), ("make check".to_string(), 0)]
    );
}

// R-329
#[test]
fn r329_stale_job_cgroups_are_swept_at_startup_and_survivors_are_killed_first() {
    let cg = tasks_platform::fake::FakeCgroups::delegated();
    use tasks_platform::Cgroups;
    cg.create(CgroupName::Job(3)).unwrap();
    cg.create(CgroupName::Job(4)).unwrap();
    cg.set_pids(CgroupName::Job(4), vec![777]);
    cg.create(CgroupName::Transfer).unwrap();
    crate::server::sweep_stale_cgroups(&cg);
    assert_eq!(
        cg.kills(),
        [CgroupName::Job(4)],
        "only a populated one is killed"
    );
    let mut gone = cg.removed();
    gone.sort();
    assert_eq!(gone, [CgroupName::Job(3), CgroupName::Job(4)]);
    assert!(cg.exists(CgroupName::Transfer), "the transfer cgroup stays");
}

// R-335
#[test]
fn r335_a_running_jobs_figures_show_in_the_status() {
    let r = rig();
    let id = r.submit(&r.spec("o", &["make"], Kind::Compile));
    r.tick();
    r.until_running(id);
    let mut f = JobFigures::empty(Attribution::ProcessTree);
    f.mem_peak_bytes = Some(3 << 30);
    // what the runner's events would have delivered
    let rj = crate::scheduler::lock(&r.sch.running)
        .get(&id)
        .cloned()
        .unwrap();
    // the runner keeps sampling and can overwrite ours before the status is read (a loaded
    // machine widens that window), so set and read again until one read sees ours
    r.until("the status shows the job's figures", || {
        *crate::scheduler::lock(&rj.figures) = Some(f.clone());
        let shown = r
            .sch
            .status()
            .running
            .first()
            .and_then(|j| j.figures.clone());
        shown == Some(f.clone())
    });
    r.procs.exit(r.procs.pgid(0), 0);
    r.until_done(id);
}

// R-337
#[test]
fn r337_the_daemons_own_disk_bytes_while_a_transfer_slot_is_held_are_counted() {
    let r = rig();
    let me = i32::try_from(std::process::id()).unwrap();
    *r.table.table.lock().unwrap() = vec![proc_row(me, "tasks2d", 1 << 20, (100, 200))];
    let guard = r.sch.transfer_gate().enter(SyncPhase::Snapshot);
    *r.table.table.lock().unwrap() = vec![proc_row(me, "tasks2d", 1 << 20, (1100, 5200))];
    drop(guard);
    let t = r.sch.status().transfer.unwrap();
    assert_eq!((t.inproc_read_bytes, t.inproc_write_bytes), (1000, 5000));
    assert_eq!((t.active, t.runs), (0, 1));
}
