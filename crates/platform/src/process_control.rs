//! Shared signalling and target-selected pre-exec protection, separate from identity reads.
use super::process::{ProcessIdentity, identity};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, getpgid};
use std::io;

/// Caller must own the stage child and keep its leader unreaped through escalation.
/// Identity validation plus killpg is not atomic. Linux can observe zombie identity;
/// Darwin may return ESRCH for it, causing this helper to do nothing. For newly spawned
/// direct groups requiring zombie-leader cleanup, use owned_group::OwnedChildGroup.
/// Never signal PID 0/1 or wrapped PIDs.
pub fn signal_owned_group(expected: &ProcessIdentity, signal: i32) -> io::Result<()> {
    if expected.pid <= 1 || expected.pid > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid managed group PID",
        ));
    }
    if !identity(expected.pid).is_ok_and(|p| p == *expected) {
        return Ok(());
    }
    let pid = Pid::from_raw(expected.pid as i32);
    if getpgid(Some(pid)).map_err(io::Error::from)? != pid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to signal a group not owned by stage child",
        ));
    }
    let signal = Signal::try_from(signal).map_err(io::Error::from)?;
    match killpg(pid, signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Arm Linux's parent-death contract in Command::pre_exec. Unported hosts reject,
/// never silently omit protection. Uses only async-signal-safe operations.
pub fn arm_parent_death_signal() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl changes only the calling pre-exec child.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    Err(io::Error::from_raw_os_error(libc::ENOTSUP))
}

/// Mark descriptors above stderr close-on-exec in a pre-exec child. Preserve Linux's
/// close_range/fcntl fallback. Other hosts reject rather than leak slot leases.
/// Only async-signal-safe syscalls, with no allocation or FD enumeration.
pub fn mark_inherited_fds_cloexec() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
        // SAFETY: called in a pre-exec child; descriptors are not closed.
        unsafe {
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, CLOSE_RANGE_CLOEXEC) == 0 {
                return Ok(());
            }
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            let max = if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
                limit.rlim_cur.min(1 << 16) as i32
            } else {
                1024
            };
            for fd in 3..max {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    Err(io::Error::from_raw_os_error(libc::ENOTSUP))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Command, Stdio};

    #[test]
    fn mismatched_identity_never_signals_our_own_group() {
        let mut wrong = identity(std::process::id()).unwrap();
        wrong.start_ticks = wrong.start_ticks.wrapping_add(1);
        signal_owned_group(&wrong, libc::SIGTERM).unwrap();
    }

    #[test]
    fn special_pids_never_become_group_queries() {
        for pid in [0, 1, u32::MAX] {
            assert_eq!(
                signal_owned_group(
                    &ProcessIdentity {
                        pid,
                        start_ticks: 0
                    },
                    libc::SIGTERM
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn refuses_child_that_does_not_lead_its_group() {
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let result = identity(child.id()).and_then(|id| signal_owned_group(&id, libc::SIGTERM));
        let _ = child.kill();
        child.wait().unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn signals_only_its_separate_child_group() {
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let result = identity(child.id()).and_then(|id| signal_owned_group(&id, libc::SIGTERM));
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        result.unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }
}
