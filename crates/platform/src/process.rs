//! Native process identity plus shared Unix process-group signalling.
use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, getpgid};
use std::io;

#[cfg(target_os = "linux")]
#[path = "process_linux.rs"]
mod native;
#[cfg(target_os = "macos")]
#[path = "process_macos.rs"]
mod native;

// Exercise the Linux token parser with synthetic records on macOS too.
#[cfg(all(test, not(target_os = "linux")))]
#[path = "process_linux.rs"]
mod linux_parser_tests;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub pid: u32,
    /// Linux: /proc start ticks (unchanged). macOS: start time in microseconds since epoch.
    /// Host-local token only, never compare identities from different machines/OSes.
    pub start_ticks: u64,
}

pub struct Observation {
    pub identity: Identity,
    pub zombie: bool,
}

pub fn observe(pid: u32) -> io::Result<Observation> {
    if pid <= 1 || pid > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid managed process PID",
        ));
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return native::observe(pid);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native process identity unavailable",
    ))
}

pub fn identity(pid: u32) -> io::Result<Identity> {
    observe(pid).map(|p| p.identity)
}

pub fn alive(expected: &Identity) -> bool {
    observe(expected.pid).is_ok_and(|p| p.identity == *expected && !p.zombie)
}

/// The caller must own this stage child and keep its leader unreaped through escalation.
/// A matching PID/start token is necessary but is not a kernel-atomic signal capability.
/// Zombie leaders are allowed here so descendants can still be killed before reaping.
pub fn signal_owned_group(expected: &Identity, signal: i32) -> io::Result<()> {
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

/// Arm the Linux admission child's parent-death contract in `Command::pre_exec`.
/// macOS requires a different supervision design; never silently omit this protection.
pub fn arm_parent_death_signal() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl changes only the calling process; suitable for the pre-exec hook.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    Err(io::Error::from_raw_os_error(libc::ENOTSUP))
}

/// Mark descriptors above stderr close-on-exec in a pre-exec child. Preserve Linux's
/// close_range/fcntl fallback. Other hosts reject rather than leak inherited slot leases.
/// This function uses only async-signal-safe syscalls, with no allocation or FD enumeration.
pub fn mark_inherited_fds_cloexec() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
        // SAFETY: this is called only in the pre-exec child; descriptors are not closed.
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
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[test]
    fn native_identity_is_stable_and_rejects_reused_token() {
        let current = identity(std::process::id()).unwrap();
        assert_eq!(identity(current.pid).unwrap(), current);
        assert!(alive(&current));
        let wrong = Identity {
            start_ticks: current.start_ticks.wrapping_add(1),
            ..current
        };
        assert!(!alive(&wrong));
        // A mismatched token must not signal our own process group.
        signal_owned_group(&wrong, libc::SIGTERM).unwrap();
        for invalid in [0, 1, u32::MAX] {
            assert!(identity(invalid).is_err());
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
        // Always reap our own child, including on a failed assertion.
        let result = (|| -> io::Result<()> {
            let child_identity = identity(child.id())?;
            signal_owned_group(&child_identity, libc::SIGTERM)
        })();
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        result.unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }
}
