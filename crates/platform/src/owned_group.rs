//! Process-group ownership pinned by a private, unreaped direct child, not a
//! libproc lookup of a zombie. Shared Linux/macOS machinery; no daemon/ledger.
//!
//! The host must not auto-reap children (SIGCHLD=SIG_IGN/SA_NOCLDWAIT) or use an
//! independent waitpid(-1) reaper. Like std::process::Child, this type requires
//! exclusive reaping ownership. No Child/try_wait handle is exposed.
//!
//! Drop provides best-effort cleanup on ordinary scope exit/unwind, NOT process
//! death. SIGKILL/crash supervision, escaped sessions and lease auditing require
//! additional machinery before this may be integrated into a Mac worker.
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus};
use std::time::{Duration, Instant};

#[must_use = "retain group ownership until wait/terminate; dropping kills the group"]
pub struct OwnedChildGroup {
    child: Option<Child>,
}

impl OwnedChildGroup {
    /// Spawn a dedicated process group and retain its direct leader. Trusted
    /// pre_exec hooks must not undo process_group(0). Programs can escape groups;
    /// this is cleanup ownership, not a sandbox or complete descendant tracker.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let mut disposition = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        // SAFETY: query only; no process-global signal disposition is changed.
        if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), disposition.as_mut_ptr()) }
            != 0
        {
            return Err(io::Error::last_os_error());
        }
        let disposition = unsafe { disposition.assume_init() };
        if disposition.sa_sigaction != libc::SIG_DFL
            || disposition.sa_flags & libc::SA_NOCLDWAIT != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "owned groups require default SIGCHLD and exclusive reaping",
            ));
        }
        let child = command.process_group(0).spawn()?;
        Ok(Self { child: Some(child) })
    }

    pub fn id(&self) -> u32 {
        self.child.as_ref().expect("group already reaped").id()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    /// Observe exit without releasing the PID/PGID lifetime pin. ECHILD is an
    /// error, never proof of ownership or permission to signal a numeric group.
    pub fn has_exited(&self) -> io::Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: writable siginfo; WNOWAIT deliberately retains the direct child.
        loop {
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(unsafe { info.si_pid() } != 0)
    }

    fn signal(&self, signal: Signal) -> io::Result<()> {
        // With exclusive reaping, either a live direct child or its retained
        // zombie pins this PGID. Unlike native identity reads, waitid works for
        // our unreaped zombie on Darwin. Refuse if the ownership pin was lost.
        self.has_exited()?;
        match killpg(Pid::from_raw(self.id() as i32), signal) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn reap(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.as_mut().expect("group already reaped").wait()?;
        self.child = None;
        Ok(status)
    }

    /// Wait for the leader, kill remaining group members BEFORE reaping, and
    /// return the leader's original status. Drain piped output concurrently.
    /// No timeout is implied; use terminate for explicit cancellation.
    pub fn wait(mut self) -> io::Result<ExitStatus> {
        while !self.has_exited()? {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.signal(Signal::SIGKILL)?;
        self.reap()
    }

    /// TERM then KILL after the grace period OR leader exit, whichever occurs
    /// first. Retain the leader through escalation, including when already dead.
    /// Any error still invokes best-effort Drop cleanup of this owned group.
    pub fn terminate(mut self, grace: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now().checked_add(grace).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "termination deadline overflow")
        })?;
        self.signal(Signal::SIGTERM)?;
        while Instant::now() < deadline && !self.has_exited()? {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.signal(Signal::SIGKILL)?;
        self.reap()
    }
}

impl Drop for OwnedChildGroup {
    fn drop(&mut self) {
        if self.child.is_some() && self.signal(Signal::SIGKILL).is_ok() {
            let _ = self.reap();
        }
    }
}
