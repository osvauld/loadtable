//! Runner tests: fake groups for the logic, a few short real `sh` groups.

use super::*;
use crate::testkit::{FakeProcs, ManualClock, RecEvents, TempDir};
use crate::{CancelToken, SystemClock};
use std::rc::Rc;
use tasks_core::Phase;
use tasks_platform::native;

const MS: fn(u64) -> Duration = Duration::from_millis;

fn policy() -> Policy {
    Policy {
        cancel_grace: MS(300),
        wall_timeout: None,
        idle_timeout: None,
        cancel_on_client_loss: false,
        poll: MS(50),
    }
}

fn env_policy() -> EnvPolicy {
    EnvPolicy {
        default_cargo_jobs: Some(2),
    }
}

fn job(dir: &TempDir, argv: &[&str]) -> JobStart {
    JobStart {
        job: JobId(5),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: dir.path().to_path_buf(),
        env: vec![],
        log_path: dir.path().join("job.log"),
        timed_path: Some(dir.path().join("job.timed")),
    }
}

struct Fx {
    dir: TempDir,
    clock: Rc<ManualClock>,
    procs: FakeProcs,
}

fn fx() -> Fx {
    let clock = Rc::new(ManualClock::default());
    Fx {
        dir: TempDir::new(),
        procs: FakeProcs::new(clock.clone()),
        clock,
    }
}

fn run_fake(f: &Fx, p: Policy, cancel: &CancelToken) -> (Outcome, RecEvents) {
    let runner = Runner::new(&f.procs, &*f.clock, p, env_policy(), vec![]);
    let mut ev = RecEvents::default();
    let out = runner
        .run(&job(&f.dir, &["true"]), cancel, &mut ev)
        .unwrap();
    (out, ev)
}

#[test]
fn spawn_spec_uses_allowlisted_env_and_job_id() {
    let dir = TempDir::new();
    let parent = vec![
        ("PATH".to_string(), "/bin".to_string()),
        ("AWS_SECRET".to_string(), "x".to_string()),
    ];
    let spec = build_spawn_spec(&job(&dir, &["true"]), parent, &env_policy()).unwrap();
    let names: Vec<&str> = spec.env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["CARGO_BUILD_JOBS", "PATH", "TASKS_JOB_ID"]);
    let mut empty = job(&dir, &["true"]);
    empty.argv.clear();
    assert!(matches!(
        build_spawn_spec(&empty, vec![], &env_policy()),
        Err(RunnerError::EmptyArgv)
    ));
}

#[test]
fn normal_exit_reports_pass_lease_usage_and_lines() {
    let mut f = fx();
    f.procs.exit_at = Some((MS(200), 0));
    f.procs
        .output
        .lock()
        .unwrap()
        .push((MS(100), b"hello\nwor".to_vec()));
    f.procs
        .output
        .lock()
        .unwrap()
        .push((MS(150), b"ld\n".to_vec()));
    let (out, ev) = run_fake(&f, policy(), &CancelToken::new());
    assert_eq!(out.end, End::Exited);
    assert_eq!(out.phase(), Phase::Passed);
    assert!(Phase::Running.can_move_to(&out.phase()));
    assert_eq!(ev.started.len(), 1);
    assert_eq!(ev.started[0], out.lease);
    assert_eq!(ev.finished.len(), 1);
    let texts: Vec<&str> = ev.lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["hello", "world"]);
    assert!(ev.lines[1].offset >= ev.lines[0].offset);
    assert!(out.usage.is_some());
    let timed = std::fs::read_to_string(f.dir.path().join("job.timed")).unwrap();
    assert_eq!(timed.lines().count(), 2);
    assert!(timed.lines().all(|l| l.split('\t').count() == 3));
}

#[test]
fn nonzero_exit_is_failed_with_the_real_code() {
    let mut f = fx();
    f.procs.exit_at = Some((MS(50), 3));
    let (out, _) = run_fake(&f, policy(), &CancelToken::new());
    assert_eq!(out.phase(), Phase::Failed { exit: Some(3) });
    assert_eq!(out.describe(), "command exit 3");
}

#[test]
fn r078_cancel_sends_term_and_a_cooperative_leader_is_not_killed() {
    let f = fx();
    let cancel = CancelToken::new();
    cancel.cancel();
    cancel.cancel(); // idempotent
    let (out, _) = run_fake(&f, policy(), &cancel);
    assert_eq!(f.procs.sent(), [Signal::Term]);
    assert_eq!(out.end, End::Cancelled);
    assert_eq!(out.phase(), Phase::Cancelled);
    assert_eq!(out.signal, Some(15));
    assert!(
        f.clock.now() < MS(300),
        "early exit once the leader is gone"
    );
}

#[test]
fn r078_resistant_leader_gets_the_grace_then_kill() {
    let mut f = fx();
    f.procs.ignores_term = true;
    let cancel = CancelToken::new();
    cancel.cancel();
    let (out, _) = run_fake(&f, policy(), &cancel);
    assert_eq!(f.procs.sent(), [Signal::Term, Signal::Kill]);
    assert!(f.clock.now() >= MS(300));
    assert_eq!(out.signal, Some(9));
    assert_eq!(out.phase(), Phase::Cancelled);
}

#[test]
fn r078_grace_is_a_policy_value_not_a_constant() {
    let mut f = fx();
    f.procs.ignores_term = true;
    let cancel = CancelToken::new();
    cancel.cancel();
    let p = Policy {
        cancel_grace: Duration::from_secs(5),
        ..policy()
    };
    run_fake(&f, p, &cancel);
    assert!(f.clock.now() >= Duration::from_secs(5));
}

#[test]
fn r079_normal_exit_cleans_descendants_before_reaping_and_keeps_status() {
    let mut f = fx();
    f.procs.exit_at = Some((MS(50), 3));
    f.procs.strays = 2;
    let (out, _) = run_fake(&f, policy(), &CancelToken::new());
    assert_eq!(f.procs.sent(), [Signal::Term]);
    assert_eq!(out.phase(), Phase::Failed { exit: Some(3) });
    assert_eq!(out.signal, None);
}

#[test]
fn r079_term_exit_of_leader_does_not_strand_a_resistant_descendant() {
    let mut f = fx();
    f.procs.strays = 1;
    f.procs.strays_ignore_term = true;
    let cancel = CancelToken::new();
    cancel.cancel();
    let (out, _) = run_fake(&f, policy(), &cancel);
    // Term stops the leader; the stray needs a second Term's grace, then Kill.
    assert_eq!(f.procs.sent(), [Signal::Term, Signal::Term, Signal::Kill]);
    assert_eq!(out.signal, Some(15));
    assert_eq!(out.phase(), Phase::Cancelled);
}

#[test]
fn cancel_after_exit_is_safe_and_sends_nothing() {
    let mut f = fx();
    f.procs.exit_at = Some((MS(0), 0));
    let cancel = CancelToken::new();
    cancel.cancel();
    let (out, _) = run_fake(&f, policy(), &cancel);
    assert!(f.procs.sent().is_empty());
    assert_eq!(out.end, End::Exited);
    assert_eq!(out.phase(), Phase::Passed);
}

#[test]
fn gone_group_on_cancel_is_not_an_error() {
    let mut f = fx();
    f.procs.gone = true;
    f.procs.exit_at = Some((MS(60), 143));
    let cancel = CancelToken::new();
    cancel.cancel();
    let (out, _) = run_fake(&f, policy(), &cancel);
    assert_eq!(out.end, End::Cancelled);
    assert_eq!(out.exit_code, Some(143));
}

#[test]
fn r080_lost_reaping_ownership_is_interrupted_and_never_signals() {
    let mut f = fx();
    f.procs.lost_wait = true;
    let cancel = CancelToken::new();
    cancel.cancel();
    let (out, _) = run_fake(&f, policy(), &cancel);
    assert_eq!(out.end, End::Lost);
    assert_eq!(out.phase(), Phase::Interrupted);
    assert_eq!(out.exit_code, None);
    assert!(f.procs.sent().is_empty());
    assert_eq!(out.describe(), "command (result lost) exit -");
}

#[test]
fn r099_r104_client_loss_does_not_kill_unless_asked() {
    let mut f = fx();
    f.procs.exit_at = Some((MS(500), 0));
    let cancel = CancelToken::new();
    cancel.client_lost();
    let (out, _) = run_fake(&f, policy(), &cancel);
    assert!(f.procs.sent().is_empty());
    assert_eq!(out.phase(), Phase::Passed);
}

#[test]
fn r104_client_loss_cancels_a_job_that_asked_for_it() {
    let f = fx();
    let cancel = CancelToken::new();
    cancel.client_lost();
    let p = Policy {
        cancel_on_client_loss: true,
        ..policy()
    };
    let (out, _) = run_fake(&f, p, &cancel);
    assert_eq!(out.end, End::ClientLost);
    assert_eq!(out.phase(), Phase::Cancelled);
    // An explicit cancel is not downgraded by a later client loss.
    cancel.cancel();
    cancel.client_lost();
    assert_eq!(cancel.state(), CancelState::Cancel);
}

#[test]
fn r105_client_events_do_not_change_the_recorded_result() {
    let mut f = fx();
    f.procs.exit_at = Some((MS(100), 7));
    let cancel = CancelToken::new();
    cancel.client_lost();
    let (out, _) = run_fake(&f, policy(), &cancel);
    assert_eq!(out.phase(), Phase::Failed { exit: Some(7) });
}

#[test]
fn r100_wall_timeout_stops_the_group_and_fails() {
    let f = fx();
    let p = Policy {
        wall_timeout: Some(Duration::from_secs(2)),
        ..policy()
    };
    let (out, _) = run_fake(&f, p, &CancelToken::new());
    assert_eq!(out.end, End::WallTimeout);
    assert!(out.wall >= Duration::from_secs(2));
    assert_eq!(out.phase(), Phase::TimedOut { exit: Some(143) });
}

#[test]
fn r101_idle_timeout_counts_from_the_last_output_not_the_start() {
    let f = fx();
    f.procs
        .output
        .lock()
        .unwrap()
        .push((Duration::from_secs(1), b"tick\n".to_vec()));
    let p = Policy {
        idle_timeout: Some(Duration::from_secs(2)),
        ..policy()
    };
    let (out, ev) = run_fake(&f, p, &CancelToken::new());
    assert_eq!(out.end, End::IdleTimeout);
    // Output at 1s postponed the idle stop until about 3s.
    assert!(out.wall >= Duration::from_secs(3), "{:?}", out.wall);
    assert_eq!(ev.lines.len(), 1);
}

#[test]
fn timeouts_are_off_unless_configured() {
    let mut f = fx();
    f.procs.exit_at = Some((Duration::from_secs(10_000), 0));
    let (out, _) = run_fake(&f, policy(), &CancelToken::new());
    assert_eq!(out.phase(), Phase::Passed);
    assert!(f.procs.sent().is_empty());
}

#[test]
fn invalid_policy_is_refused_before_spawning() {
    let f = fx();
    let p = Policy {
        poll: Duration::ZERO,
        ..policy()
    };
    let runner = Runner::new(&f.procs, &*f.clock, p, env_policy(), vec![]);
    let r = runner.run(
        &job(&f.dir, &["true"]),
        &CancelToken::new(),
        &mut RecEvents::default(),
    );
    assert!(matches!(r, Err(RunnerError::Policy(_))));
    assert!(!f.dir.path().join("job.log").exists());
}

#[test]
fn lease_still_ours_follows_the_platform() {
    let f = fx();
    let lease = Lease::new(tasks_platform::GroupId {
        pgid: 4242,
        start_time: 7,
    });
    assert!(lease.still_ours(&f.procs).unwrap());
    // R-075: a group whose leader is absent (or reused) is not ours.
    let native = native();
    let absent = Lease::new(tasks_platform::GroupId {
        pgid: 999_999,
        start_time: 1,
    });
    assert!(!absent.still_ours(&native).unwrap());
}

// Real process groups: short, `sh` only.

fn real_policy(grace_ms: u64) -> Policy {
    Policy {
        cancel_grace: MS(grace_ms),
        poll: MS(10),
        ..policy()
    }
}

fn run_real(
    dir: &TempDir,
    script: &str,
    p: Policy,
    cancel: &CancelToken,
) -> (Outcome, RecEvents, Duration) {
    let n = native();
    let clock = SystemClock::new();
    let runner = Runner::new(
        &n,
        &clock,
        p,
        env_policy(),
        vec![("PATH".into(), std::env::var("PATH").unwrap_or_default())],
    );
    let mut ev = RecEvents::default();
    let t = std::time::Instant::now();
    let out = runner
        .run(&job(dir, &["sh", "-c", script]), cancel, &mut ev)
        .unwrap();
    (out, ev, t.elapsed())
}

#[test]
fn real_job_streams_output_and_reports_the_exit_code() {
    let dir = TempDir::new();
    let (out, ev, _) = run_real(
        &dir,
        "printf 'a\\nb'; printf 'c\\n' >&2; exit 4",
        real_policy(300),
        &CancelToken::new(),
    );
    assert_eq!(out.phase(), Phase::Failed { exit: Some(4) });
    let texts: Vec<&str> = ev.lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["a", "bc"]);
    let id = std::fs::read_to_string(dir.path().join("job.log")).unwrap();
    assert_eq!(id, "a\nbc\n");
}

#[test]
fn r074_r107_cancel_stops_the_whole_group_including_children() {
    let dir = TempDir::new();
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(MS(80));
        c2.cancel();
    });
    let (out, _, took) = run_real(&dir, "sleep 5 & sleep 5; true", real_policy(300), &cancel);
    h.join().unwrap();
    assert_eq!(out.phase(), Phase::Cancelled);
    assert_eq!(out.signal, Some(15));
    assert!(took < MS(900), "{took:?}");
    assert!(!out.lease.still_ours(&native()).unwrap());
}

#[test]
fn r079_real_normal_exit_cleans_a_stray_and_keeps_the_status() {
    let dir = TempDir::new();
    let (out, _, took) = run_real(
        &dir,
        "sleep 5 & exit 3",
        real_policy(300),
        &CancelToken::new(),
    );
    assert_eq!(out.phase(), Phase::Failed { exit: Some(3) });
    assert!(took < MS(900), "{took:?}");
    assert!(!out.lease.still_ours(&native()).unwrap());
}

#[test]
fn r078_real_term_resistant_job_is_killed_after_the_grace() {
    let dir = TempDir::new();
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(MS(80));
        c2.cancel();
    });
    let (out, _, took) = run_real(
        &dir,
        "trap '' TERM; while :; do sleep 0.02; done",
        real_policy(150),
        &cancel,
    );
    h.join().unwrap();
    assert_eq!(out.phase(), Phase::Cancelled);
    assert_eq!(out.signal, Some(9));
    assert!(took >= MS(230), "{took:?}");
}

#[test]
fn r090_real_job_sees_only_allowlisted_env() {
    let dir = TempDir::new();
    let (out, ev, _) = run_real(
        &dir,
        "printf '%s|%s|%s\\n' \"$TASKS_JOB_ID\" \"${HOME-unset}\" \"$CARGO_BUILD_JOBS\"",
        real_policy(300),
        &CancelToken::new(),
    );
    assert_eq!(out.phase(), Phase::Passed);
    assert_eq!(ev.lines[0].text, "5|unset|2");
}

// ---- attribution (design §9)

mod attribution {
    use super::*;
    use crate::Attrib;
    use tasks_core::{Attribution, JobFigures};
    use tasks_platform::fake::{FakeCgroups, FakePlatform};
    use tasks_platform::{CgroupName, ProcInfo};

    const JOB: CgroupName = CgroupName::Job(5);

    fn run_with(f: &Fx, p: Policy, cg: &FakeCgroups, plat: &FakePlatform) -> (Outcome, RecEvents) {
        let runner =
            Runner::new(&f.procs, &*f.clock, p, env_policy(), vec![]).with_attribution(Attrib {
                cgroups: cg,
                table: plat,
            });
        let mut ev = RecEvents::default();
        let out = runner
            .run(&job(&f.dir, &["true"]), &CancelToken::new(), &mut ev)
            .unwrap();
        (out, ev)
    }

    fn cgroup_figures() -> JobFigures {
        let mut fig = JobFigures::empty(Attribution::Cgroup);
        fig.io_write_bytes = Some(25 << 30);
        fig.cpu_usec = Some(3_000_000);
        fig
    }

    // R-323
    #[test]
    fn r323_the_child_is_told_to_join_the_jobs_cgroup() {
        let mut f = fx();
        f.procs.exit_at = Some((MS(100), 0));
        let cg = FakeCgroups::delegated();
        cg.set_figures(JOB, cgroup_figures());
        let plat = FakePlatform::default();
        let (out, _) = run_with(&f, policy(), &cg, &plat);
        assert_eq!(
            f.procs.joined.lock().unwrap().as_deref(),
            Some(std::path::Path::new("/fake/tasks.slice/job-5/cgroup.procs"))
        );
        let fig = out.figures.unwrap();
        assert_eq!(fig.quality, Attribution::Cgroup);
        assert_eq!(fig.io_write_bytes, Some(25 << 30));
    }

    // R-329
    #[test]
    fn r329_the_jobs_cgroup_is_removed_when_it_ends() {
        let mut f = fx();
        f.procs.exit_at = Some((MS(100), 0));
        let cg = FakeCgroups::delegated();
        cg.set_figures(JOB, cgroup_figures());
        let plat = FakePlatform::default();
        run_with(&f, policy(), &cg, &plat);
        assert_eq!(cg.removed(), [JOB]);
        assert!(!cg.exists(JOB));
    }

    // R-328
    #[test]
    fn r328_cgroup_kill_follows_the_group_kill_when_a_child_escaped_the_group() {
        let mut f = fx();
        f.procs.exit_at = Some((MS(100), 0));
        let cg = FakeCgroups::delegated();
        cg.set_figures(JOB, cgroup_figures());
        // the zombie leader and a daemonised child that left the process group
        cg.set_pids(JOB, vec![4242, 9001]);
        let plat = FakePlatform::default();
        let (out, _) = run_with(&f, policy(), &cg, &plat);
        assert_eq!(cg.kills(), [JOB], "the cgroup is the last resort");
        assert_eq!(
            out.phase(),
            Phase::Passed,
            "the command's own status is kept"
        );
        assert_eq!(cg.removed(), [JOB], "empty after the kill, so removed");
    }

    // R-328
    #[test]
    fn r328_no_cgroup_kill_when_only_the_leader_remains() {
        let mut f = fx();
        f.procs.exit_at = Some((MS(100), 0));
        let cg = FakeCgroups::delegated();
        cg.set_figures(JOB, cgroup_figures());
        cg.set_pids(JOB, vec![4242]);
        let plat = FakePlatform::default();
        run_with(&f, policy(), &cg, &plat);
        assert!(cg.kills().is_empty());
    }

    // R-326
    #[test]
    fn r326_without_delegation_the_job_runs_in_a_process_group_and_says_so() {
        let mut f = fx();
        f.procs.exit_at = Some((MS(2500), 0));
        let cg = FakeCgroups::unavailable("not running under systemd");
        let plat = FakePlatform::default();
        *plat.table.lock().unwrap() = vec![
            ProcInfo {
                pid: 4242,
                ppid: 1,
                pgid: 4242,
                start: 7,
                name: "cargo".into(),
                rss_bytes: 100,
                cpu_usec: 0,
                io_read: Some(10),
                io_write: Some(20),
            },
            ProcInfo {
                pid: 4300,
                ppid: 4242,
                pgid: 4242,
                start: 8,
                name: "rustc".into(),
                rss_bytes: 900,
                cpu_usec: 0,
                io_read: Some(1000),
                io_write: Some(2000),
            },
            ProcInfo {
                pid: 77,
                ppid: 1,
                pgid: 77,
                start: 3,
                name: "firefox".into(),
                rss_bytes: 5000,
                cpu_usec: 0,
                io_read: Some(999_999),
                io_write: Some(999_999),
            },
        ];
        let (out, ev) = run_with(&f, policy(), &cg, &plat);
        assert_eq!(*f.procs.joined.lock().unwrap(), None);
        let fig = out.figures.unwrap();
        assert_eq!(fig.quality, Attribution::ProcessTree);
        assert_eq!(fig.io_read_bytes, Some(1010));
        assert_eq!(fig.io_write_bytes, Some(2020));
        assert_eq!(
            fig.mem_peak_bytes,
            Some(1000),
            "firefox is not part of the job"
        );
        assert_eq!(fig.io_some_us, None, "stall times need a cgroup");
        assert!(cg.removed().is_empty());
        assert!(ev.finished.len() == 1);
    }
}
