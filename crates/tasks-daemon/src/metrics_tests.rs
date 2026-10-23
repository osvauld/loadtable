//! Job metrics through the real scheduler over fakes. Names carry the regression ids they pin.

use crate::api::*;
use crate::metrics::KEEP_AFTER_END_MS;
use crate::scheduler::lock;
use crate::testkit::Rig;
use tasks_core::{Attribution, JobFigures, JobId, Kind, Phase};
use tasks_platform::{CgroupName, ProcInfo};

fn ok(r: Reply) -> Response {
    match r {
        Reply::Ok(x) => x,
        Reply::Err(e) => panic!("unexpected error {e:?}"),
    }
}

fn metrics(r: &Rig, id: JobId) -> tasks_core::metrics::JobMetricsView {
    match ok(r.sch.handle(Request::JobMetrics { id })) {
        Response::JobMetrics(m) => m,
        other => panic!("{other:?}"),
    }
}

fn row(pid: i32, ppid: i32, name: &str, rss: u64, cpu: u64) -> ProcInfo {
    ProcInfo {
        pid,
        ppid,
        pgid: if pid == 1000 || ppid == 1000 {
            1000
        } else {
            pid
        },
        start: 5,
        name: name.into(),
        rss_bytes: rss,
        cpu_usec: cpu,
        io_read: None,
        io_write: None,
    }
}

fn running(r: &Rig) -> (JobId, std::sync::Arc<crate::scheduler::RunningJob>) {
    let id = r.submit(&r.spec("o", &["make"], Kind::Compile));
    r.tick();
    r.until_running(id);
    let rj = lock(&r.sch.running).get(&id).cloned().unwrap();
    (id, rj)
}

fn figs(q: Attribution, cpu: u64, mem: u64) -> JobFigures {
    let mut f = JobFigures::empty(q);
    f.cpu_usec = Some(cpu);
    f.mem_now_bytes = Some(mem);
    f.mem_peak_bytes = Some(mem * 2);
    f
}

fn stop(r: &Rig, rj: &crate::scheduler::RunningJob) {
    r.procs.exit(r.procs.pgid(0), 0);
    r.until_done(rj.id);
}

// R-567
#[test]
fn r567_an_unknown_job_is_not_found_and_a_waiting_one_says_it_has_no_samples() {
    let r = Rig::new();
    let e = match r.sch.handle(Request::JobMetrics { id: JobId(99) }) {
        Reply::Err(e) => e,
        Reply::Ok(x) => panic!("{x:?}"),
    };
    assert_eq!(e.code, ErrorCode::NotFound);
    ok(r.sch.handle(Request::Drain {
        cancel: false,
        who: None,
    }));
    let id = r.submit(&r.spec("o", &["make"], Kind::Compile));
    let m = metrics(&r, id);
    assert!(m.samples.is_empty() && m.procs.is_empty());
    assert!(m.note.unwrap().contains("not produced samples"));
}

// R-568
#[test]
fn r568_a_running_jobs_tree_is_grouped_by_program_and_outside_processes_are_left_out() {
    let r = Rig::new();
    let (id, rj) = running(&r);
    let at = |s: i64| r.clock.advance(s * 1000);
    at(3600);
    *r.table.table.lock().unwrap() = vec![
        row(1000, 1, "make", 100, 0),
        row(1001, 1000, "rustc", 400, 0),
        row(1002, 1000, "rustc", 600, 0),
        row(55, 1, "firefox", 9_000, 0),
    ];
    r.sch
        .record_metrics(&rj, &figs(Attribution::ProcessTree, 0, 1_100));
    at(2);
    *r.table.table.lock().unwrap() = vec![
        row(1000, 1, "make", 100, 100_000),
        row(1001, 1000, "rustc", 400, 2_000_000),
        row(1002, 1000, "rustc", 600, 2_000_000),
        row(55, 1, "firefox", 9_000, 9_000_000),
    ];
    r.sch
        .record_metrics(&rj, &figs(Attribution::ProcessTree, 4_100_000, 1_100));
    let m = metrics(&r, id);
    assert_eq!(m.quality, Some(Attribution::ProcessTree));
    let names: Vec<_> = m.procs.iter().map(|g| (g.name.as_str(), g.count)).collect();
    assert_eq!(names, [("rustc", 2), ("make", 1)]);
    assert_eq!(m.procs[0].cpu_cores, Some(2.0));
    assert_eq!(m.procs[0].rss_bytes, 1_000);
    let last = m.samples.last().unwrap();
    assert_eq!(last.mem_bytes, Some(1_100));
    // the same numbers feed the status row
    let s = r.sch.status();
    assert_eq!(s.running[0].cores, Some(2.05));
    assert_eq!(s.running[0].mem_bytes, Some(1_100));
    stop(&r, &rj);
}

// R-569
#[test]
fn r569_a_cgroup_jobs_processes_are_the_cgroups_members_exactly() {
    let r = Rig::delegated();
    let (id, rj) = running(&r);
    // 2001 escaped the process group; 1001 is in the group but not in the cgroup
    r.cgroups.set_pids(CgroupName::Job(id.0), vec![2001]);
    r.clock.advance(3_600_000);
    *r.table.table.lock().unwrap() = vec![
        row(1001, 1000, "stray", 5, 0),
        row(2001, 1, "escaped", 7, 0),
    ];
    r.sch.record_metrics(&rj, &figs(Attribution::Cgroup, 0, 7));
    let m = metrics(&r, id);
    assert_eq!(m.quality, Some(Attribution::Cgroup));
    let names: Vec<_> = m.procs.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, ["escaped"]);
    stop(&r, &rj);
}

// R-570
#[test]
fn r570_a_finished_jobs_samples_are_kept_for_a_while_then_dropped_with_a_note() {
    let r = Rig::new();
    let (id, rj) = running(&r);
    r.sch
        .record_metrics(&rj, &figs(Attribution::ProcessTree, 1, 1));
    stop(&r, &rj);
    until_ended(&r, id);
    let m = metrics(&r, id);
    assert!(!m.samples.is_empty() && m.procs.is_empty());
    assert!(m.note.unwrap().contains("ended"));
    assert_eq!(r.record(id).phase, Phase::Passed);
    r.clock.advance(KEEP_AFTER_END_MS + 1);
    let m = metrics(&r, id);
    assert!(m.samples.is_empty());
    assert!(m.note.unwrap().contains("no longer held"));
    // an ended job shows no live figures in the status either
    assert!(r.sch.status().running.is_empty());
}

fn until_ended(r: &Rig, id: JobId) {
    r.until("history marked ended", || {
        lock(&r.sch.metrics).latest(id).is_none()
    });
}

// R-571
#[test]
fn r571_the_status_machine_cores_and_old_json_without_the_new_fields() {
    let old_run = r#"{"id":4,"owner":"me","command":"c","slot":null,"elapsed_secs":1.0,
        "done_targets":0,"total_targets":0,"current":null,"eta_secs":null,"log_bytes":0}"#;
    let v: RunningView = serde_json::from_str(old_run).unwrap();
    assert_eq!((v.cores, v.mem_bytes), (None, None));
    let old_pressure = r#"{"memory":"ok","memory_some":null,"memory_full":null,"cpu":null,
        "io":null,"available_memory":null}"#;
    let p: PressureView = serde_json::from_str(old_pressure).unwrap();
    assert_eq!(p.cores_in_use, None);
    // a figures record written before `mem_now_bytes` existed still reads
    let f: JobFigures = serde_json::from_str(r#"{"quality":"cgroup","cpu_usec":5}"#).unwrap();
    assert_eq!(f.mem_now_bytes, None);
}

// R-572
#[test]
fn r572_the_usual_peak_is_the_median_of_past_runs_on_this_machine() {
    let r = Rig::new();
    r.procs.auto_exit(0);
    for (n, peak) in [2u64 << 30, 4 << 30, 9 << 30].into_iter().enumerate() {
        // each fake group's leader is the next pid from 1000; the tree meter peaks at its memory
        let leader = 1000 + i32::try_from(n).unwrap();
        let mut p = row(leader, 1, "make", peak, 0);
        p.pgid = leader;
        *r.table.table.lock().unwrap() = vec![p];
        let id = r.submit(&r.spec("o", &["make", "all"], Kind::Compile));
        r.tick();
        assert_eq!(r.until_done(id), Phase::Passed);
    }
    let id = r.submit(&r.spec("o", &["make", "all"], Kind::Compile));
    assert_eq!(metrics(&r, id).usual_peak_mem_bytes, Some(4 << 30));
    // a command never run here has no usual peak
    let other = r.submit(&r.spec("o", &["ninja"], Kind::Compile));
    assert_eq!(metrics(&r, other).usual_peak_mem_bytes, None);
}
