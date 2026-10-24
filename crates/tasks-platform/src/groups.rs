//! Process-group spawning, signalling and reaping shared by Linux and macOS.
//! Only the leader start-time lookup is OS-specific and is passed in.

use crate::{Child, GroupId, PlatformError, Signal, SpawnSpec, place_in_cgroup};
use rustix::process::{Pid, WaitId, WaitIdOptions, WaitOptions};
use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// Looks up a process start time; `None` when the process does not exist.
pub(crate) type StartTime<'a> = &'a dyn Fn(i32) -> Result<Option<u64>, PlatformError>;

fn pid_of(raw: i32) -> Result<Pid, PlatformError> {
    Pid::from_raw(raw).ok_or(PlatformError::InvalidGroup(raw))
}

/// Starts `spec` in its own process group with null stdin and output appended
/// to the log file. Rust opens every descriptor close-on-exec, so the child
/// inherits only 0..=2 and never a slot lock (R-087).
pub(crate) fn spawn(spec: &SpawnSpec, start_time: StartTime) -> Result<Child, PlatformError> {
    if spec.argv.is_empty() {
        return Err(PlatformError::InvalidSpawn("empty argv".into()));
    }
    let placed;
    let argv = match &spec.cgroup_procs {
        Some(procs) => {
            placed = place_in_cgroup(procs, &spec.argv);
            &placed
        }
        None => &spec.argv,
    };
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| PlatformError::InvalidSpawn("empty argv".into()))?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.log_path)
        .map_err(|e| PlatformError::io(&spec.log_path, e))?;
    let log_err = log
        .try_clone()
        .map_err(|e| PlatformError::io(&spec.log_path, e))?;
    let child = Command::new(program)
        .args(args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .spawn()
        .map_err(|source| PlatformError::Spawn {
            program: program.clone(),
            source,
        })?;
    let pid = i32::try_from(child.id())
        .map_err(|_| PlatformError::InvalidSpawn("pid out of range".into()))?;
    // Dropping std's handle neither kills nor reaps; `try_wait` reaps by pid.
    drop(child);
    // The child must lead its own group; otherwise we could never signal it safely.
    let leads = rustix::process::getpgid(Some(pid_of(pid)?))
        .map(|g| g.as_raw_pid() == pid)
        .unwrap_or(false);
    if !leads {
        return Err(PlatformError::InvalidSpawn(format!(
            "child {pid} does not lead its process group"
        )));
    }
    if spec.nice != 0 {
        apply_nice(pid, spec.nice);
    }
    let start = start_time(pid)?.ok_or(PlatformError::Gone { pgid: pid })?;
    Ok(Child {
        pid,
        group: GroupId {
            pgid: pid,
            start_time: start,
        },
    })
}

/// Lowers the priority of the whole group `pgid` leads. A failure is ignored: the job runs at
/// the normal priority, which is only slower for the others, never wrong. Children forked in
/// the instant before this call keep the old niceness; the leader is still starting then.
fn apply_nice(pgid: i32, nice: i32) {
    if let Some(p) = Pid::from_raw(pgid) {
        let _ = rustix::process::setpriority_pgrp(Some(p), nice);
    }
}

/// Fails with `Gone` unless the group leader still has the recorded start time.
pub(crate) fn check_leader(group: GroupId, start_time: StartTime) -> Result<(), PlatformError> {
    match start_time(group.pgid)? {
        Some(t) if t == group.start_time => Ok(()),
        _ => Err(PlatformError::Gone { pgid: group.pgid }),
    }
}

/// Refuses ids that must never be signalled: 0, 1, negatives and our own group.
pub(crate) fn validate_target(pgid: i32) -> Result<Pid, PlatformError> {
    if pgid <= 1 {
        return Err(PlatformError::InvalidGroup(pgid));
    }
    if rustix::process::getpgrp().as_raw_pid() == pgid {
        return Err(PlatformError::OwnGroup(pgid));
    }
    pid_of(pgid)
}

/// Signals exactly one verified group (R-074, R-075, R-076).
pub(crate) fn signal(
    group: GroupId,
    sig: Signal,
    start_time: StartTime,
) -> Result<(), PlatformError> {
    let pid = validate_target(group.pgid)?;
    check_leader(group, start_time)?;
    let sig = match sig {
        Signal::Term => rustix::process::Signal::TERM,
        Signal::Kill => rustix::process::Signal::KILL,
        Signal::Stop => rustix::process::Signal::STOP,
        Signal::Cont => rustix::process::Signal::CONT,
    };
    match rustix::process::kill_process_group(pid, sig) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::SRCH) => Err(PlatformError::Gone { pgid: group.pgid }),
        Err(source) => Err(PlatformError::Syscall {
            syscall: "killpg",
            path: None,
            source,
        }),
    }
}

/// Non-blocking reap of the leader; a lost child (ECHILD) is an error, never
/// authority to signal by number (R-080).
pub(crate) fn try_wait(child: &Child) -> Result<Option<i32>, PlatformError> {
    let pid = pid_of(child.pid)?;
    match rustix::process::waitpid(Some(pid), WaitOptions::NOHANG) {
        Ok(None) => Ok(None),
        Ok(Some((_, status))) => Ok(Some(code_of(
            status.exit_status(),
            status.terminating_signal(),
        ))),
        Err(source) => Err(PlatformError::Syscall {
            syscall: "waitpid",
            path: None,
            source,
        }),
    }
}

/// Like [`try_wait`] but leaves the leader a zombie (`WNOWAIT`), so its pid and
/// group id cannot be reused while descendants are cleaned (R-079).
pub(crate) fn peek_exit(child: &Child) -> Result<Option<i32>, PlatformError> {
    let pid = pid_of(child.pid)?;
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    match rustix::process::waitid(WaitId::Pid(pid), options) {
        Ok(None) => Ok(None),
        Ok(Some(status)) => Ok(Some(code_of(
            status.exit_status(),
            status.terminating_signal(),
        ))),
        Err(source) => Err(PlatformError::Syscall {
            syscall: "waitid",
            path: None,
            source,
        }),
    }
}

/// Exit code, or 128 + signal when killed; -1 when neither is known.
fn code_of(exit: Option<i32>, signal: Option<i32>) -> i32 {
    match (exit, signal) {
        (Some(c), _) => c,
        (None, Some(s)) => 128 + s,
        (None, None) => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FlockSlots, Processes, SlotLocks};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use tasks_core::SlotKind;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tasks-grp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn spec(dir: &Path, script: &str) -> SpawnSpec {
        SpawnSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: dir.to_path_buf(),
            env: vec![
                ("FOO".into(), "bar".into()),
                ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
            ],
            log_path: dir.join("log"),
            cgroup_procs: None,
            nice: 0,
        }
    }

    fn wait_exit(p: &impl Processes, c: &Child) -> i32 {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if let Some(code) = p.try_wait(c).unwrap() {
                return code;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("child did not exit");
    }

    #[test]
    fn spawn_appends_log_clears_env_and_nulls_stdin() {
        let dir = tmpdir("spawn");
        std::fs::write(dir.join("log"), "old\n").unwrap();
        let p = crate::native();
        let c = p
            .spawn_group(&spec(
                &dir,
                "cat; echo \"[$HOME][$FOO]\"; echo err >&2; exit 3",
            ))
            .unwrap();
        assert_eq!(wait_exit(&p, &c), 3);
        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        assert!(log.starts_with("old\n"), "{log}");
        assert!(log.contains("[][bar]"), "{log}");
        assert!(log.contains("err"), "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R-711
    #[test]
    fn r711_a_spawn_with_nice_lowers_the_priority_of_the_group_leader() {
        let dir = tmpdir("nice");
        let p = crate::native();
        let mut sp = spec(&dir, "sleep 5");
        sp.nice = 7;
        let c = p.spawn_group(&sp).unwrap();
        let pid = Pid::from_raw(c.pid);
        // applied before spawn_group returns, so there is nothing to wait for
        let ni = rustix::process::getpriority_process(pid).unwrap();
        assert_eq!(ni, 7);
        let plain = p.spawn_group(&spec(&dir, "sleep 5")).unwrap();
        let ni = rustix::process::getpriority_process(Pid::from_raw(plain.pid)).unwrap();
        assert!(
            ni < 7,
            "a spawn without nice keeps the daemon's niceness: {ni}"
        );
        p.signal_group(c.group, Signal::Kill).unwrap();
        p.signal_group(plain.group, Signal::Kill).unwrap();
        wait_exit(&p, &c);
        wait_exit(&p, &plain);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R-076
    #[test]
    fn child_leads_its_own_group() {
        let dir = tmpdir("lead");
        let p = crate::native();
        let c = p.spawn_group(&spec(&dir, "exit 0")).unwrap();
        assert_eq!(c.group.pgid, c.pid);
        assert_ne!(c.pid, rustix::process::getpgrp().as_raw_pid());
        wait_exit(&p, &c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R-079
    #[test]
    fn peek_exit_keeps_the_leader_until_reaped() {
        let dir = tmpdir("peek");
        let p = crate::native();
        let c = p
            .spawn_group(&spec(&dir, "sleep 30 & exec sh -c 'exit 4'"))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let code = loop {
            if let Some(code) = p.peek_exit(&c).unwrap() {
                break code;
            }
            assert!(Instant::now() < deadline, "leader never exited");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(code, 4);
        assert_eq!(p.peek_exit(&c).unwrap(), Some(4));
        // The zombie leader still guards the group, so the stray is reachable.
        assert_eq!(p.usage(c.group).unwrap().processes, 1);
        p.signal_group(c.group, Signal::Kill).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while p.usage(c.group).unwrap().processes > 0 {
            assert!(Instant::now() < deadline, "stray survived Kill");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(p.try_wait(&c).unwrap(), Some(4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R-521
    #[test]
    fn r521_a_zombie_leader_still_pins_its_group_and_is_not_counted() {
        let dir = tmpdir("zombie");
        let p = crate::native();
        let c = p.spawn_group(&spec(&dir, "sleep 30 & exit 0")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while p.peek_exit(&c).unwrap().is_none() {
            assert!(Instant::now() < deadline, "leader never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
        // the leader is a zombie now: the group is found, with only the stray counted
        let u = p.usage(c.group).unwrap();
        assert_eq!(u.processes, 1, "{u:?}");
        p.signal_group(c.group, Signal::Kill).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while p.usage(c.group).unwrap().processes > 0 {
            assert!(Instant::now() < deadline, "stray survived Kill");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(p.try_wait(&c).unwrap(), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R-076, R-074
    #[test]
    fn special_and_own_groups_are_never_signalled() {
        let p = crate::native();
        for bad in [0, 1, -1, -5] {
            let g = GroupId {
                pgid: bad,
                start_time: 0,
            };
            assert!(matches!(
                p.signal_group(g, Signal::Kill),
                Err(PlatformError::InvalidGroup(_))
            ));
        }
        let own = rustix::process::getpgrp().as_raw_pid();
        let g = GroupId {
            pgid: own,
            start_time: 0,
        };
        assert!(matches!(
            p.signal_group(g, Signal::Kill),
            Err(PlatformError::OwnGroup(_))
        ));
    }

    // R-075, R-074
    #[test]
    fn reused_pid_is_gone_and_not_signalled() {
        let dir = tmpdir("reuse");
        let p = crate::native();
        let c = p.spawn_group(&spec(&dir, "sleep 30")).unwrap();
        let wrong = GroupId {
            start_time: c.group.start_time + 1,
            ..c.group
        };
        assert!(matches!(
            p.signal_group(wrong, Signal::Kill),
            Err(PlatformError::Gone { .. })
        ));
        assert!(matches!(p.usage(wrong), Err(PlatformError::Gone { .. })));
        assert!(
            p.try_wait(&c).unwrap().is_none(),
            "child must still be alive"
        );
        p.signal_group(c.group, Signal::Kill).unwrap();
        assert_eq!(wait_exit(&p, &c), 137);
        // Leader reaped: the identity no longer exists.
        assert!(matches!(p.usage(c.group), Err(PlatformError::Gone { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R-074, R-079: the group signal reaches descendants; a zombie leader pins identity.
    #[test]
    fn group_signal_reaches_descendants_and_zombies_are_not_counted() {
        let dir = tmpdir("tree");
        let p = crate::native();
        let c = p
            .spawn_group(&spec(&dir, "sleep 60 & sleep 60 & wait"))
            .unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        let mut seen = 0;
        while Instant::now() < end && seen < 3 {
            seen = p.usage(c.group).unwrap().processes;
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            seen >= 3,
            "saw {seen} processes; leader stat: {:?}",
            std::fs::read_to_string(format!("/proc/{}/stat", c.pid))
        );
        p.signal_group(c.group, Signal::Kill).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        // Leader is unreaped (zombie): still verifiable, but no live processes.
        while Instant::now() < end && p.usage(c.group).unwrap().processes != 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(p.usage(c.group).unwrap().processes, 0);
        assert_eq!(wait_exit(&p, &c), 137);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R-087, R-089: a spawned helper never keeps a slot lock alive.
    #[test]
    fn child_does_not_inherit_slot_lock() {
        let dir = tmpdir("lease");
        let p = crate::native();
        let slots = FlockSlots::new(dir.join("slots"));
        let guard = slots.try_acquire(SlotKind::Compile, 0).unwrap().unwrap();
        let c = p.spawn_group(&spec(&dir, "sleep 30")).unwrap();
        drop(guard);
        // Concurrent tests may fork between open and exec, briefly holding a copy
        // of the descriptor; a leaked lease would persist for the child's 30 s.
        let end = Instant::now() + Duration::from_secs(3);
        let mut reacquired = false;
        while !reacquired && Instant::now() < end {
            reacquired = slots.try_acquire(SlotKind::Compile, 0).unwrap().is_some();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(reacquired, "child kept the slot lock");
        p.signal_group(c.group, Signal::Kill).unwrap();
        wait_exit(&p, &c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R-323: the child writes its own pid into cgroup.procs before it execs, so the pid, the
    // group leadership and the start time are the command's own.
    #[test]
    fn r323_a_child_joins_its_cgroup_before_it_execs() {
        let dir = tmpdir("place");
        let procs = dir.join("cgroup.procs");
        std::fs::write(&procs, "").unwrap();
        let p = crate::native();
        let mut s = spec(&dir, "echo ran $FOO; exit 5");
        s.cgroup_procs = Some(procs.clone());
        let c = p.spawn_group(&s).unwrap();
        assert_eq!(wait_exit(&p, &c), 5);
        assert_eq!(
            std::fs::read_to_string(&procs).unwrap().trim(),
            c.pid.to_string()
        );
        assert_eq!(c.group.pgid, c.pid);
        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        assert!(log.contains("ran bar"), "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R-323: an unwritable cgroup.procs does not stop the command; it is reported by the
    // missing membership, not by a failed job.
    #[test]
    fn r323_a_failed_placement_still_runs_the_command_quietly() {
        let dir = tmpdir("noplace");
        let p = crate::native();
        let mut s = spec(&dir, "echo still-ran");
        s.cgroup_procs = Some(dir.join("missing/cgroup.procs"));
        let c = p.spawn_group(&s).unwrap();
        assert_eq!(wait_exit(&p, &c), 0);
        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        assert_eq!(log, "still-ran\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_argv_and_missing_program_are_typed_errors() {
        let dir = tmpdir("bad");
        let p = crate::native();
        let mut s = spec(&dir, "");
        s.argv.clear();
        assert!(matches!(
            p.spawn_group(&s),
            Err(PlatformError::InvalidSpawn(_))
        ));
        s.argv = vec!["/nonexistent/prog".into()];
        assert!(matches!(
            p.spawn_group(&s),
            Err(PlatformError::Spawn { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
