//! Controller-loss guard for a separately launched guardian process.
//!
//! The guardian owns the direct child/group; the controller owns the sole peer
//! endpoint of a UnixStream. EOF cancels and cleans the group before returning.
//! Streams must remain close-on-exec and the workload must not inherit the peer
//! endpoint. Duplicating/passing that endpoint elsewhere invalidates liveness.
//!
//! This is NOT a complete crash supervisor: guardian SIGKILL, a stuck spawn/exec,
//! escaped sessions, admission and ledger recovery require additional protection.
//! Observing an open channel does not atomically authorize a later exec. The
//! worker is deliberately not wired to this primitive.
use crate::owned_group::OwnedChildGroup;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, ExitStatus};
use std::time::Duration;

/// Controller sends this single byte to grant startup; then keeps its endpoint
/// alive without further bytes. Closing it withdraws authority. No PID lookup.
pub const START: u8 = b'S';
const POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    ControllerLostBeforeStart,
    ControllerLost,
    LeaderExited,
}

#[derive(Debug)]
pub struct Outcome {
    pub reason: Reason,
    /// None means nothing was spawned. A status alone is not a job success:
    /// ControllerLost remains an interruption even if the leader exited zero.
    pub status: Option<ExitStatus>,
}

pub(crate) enum ControlRead {
    Closed,
    Pending,
    Byte(u8),
}

pub(crate) fn byte(control: &mut UnixStream) -> io::Result<ControlRead> {
    let mut buf = [0];
    loop {
        match control.read(&mut buf) {
            Ok(0) => return Ok(ControlRead::Closed),
            Ok(_) => return Ok(ControlRead::Byte(buf[0])),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(ControlRead::Pending),
            Err(e) => return Err(e),
        }
    }
}

fn lost(control: &mut UnixStream) -> io::Result<bool> {
    match byte(control)? {
        ControlRead::Closed => Ok(true),
        ControlRead::Pending => Ok(false),
        ControlRead::Byte(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected control byte after start grant",
        )),
    }
}

pub(crate) fn prepare_control(control: &UnixStream) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(control.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::FD_CLOEXEC == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "guard control endpoint must be close-on-exec",
        ));
    }
    control.set_nonblocking(true)
}

/// Run in the separate guardian, not a thread in the controller. No signal
/// handlers or process-global reaper are installed. OwnedChildGroup's exclusive
/// reaping/default-SIGCHLD contract applies. Call with explicit stdio and leases.
/// This does not acquire resources, approve commands, detach, or write state.
/// No hard timeout is implied for spawn or reaping. Unexpected protocol/I/O
/// errors cause best-effort group Drop cleanup, then propagate as errors.
pub fn supervise(
    command: &mut Command,
    mut control: UnixStream,
    grace: Duration,
) -> io::Result<Outcome> {
    prepare_control(&control)?;
    loop {
        match byte(&mut control)? {
            ControlRead::Byte(START) => break,
            ControlRead::Closed => {
                return Ok(Outcome {
                    reason: Reason::ControllerLostBeforeStart,
                    status: None,
                });
            }
            ControlRead::Byte(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid start grant",
                ));
            }
            ControlRead::Pending => std::thread::sleep(POLL),
        }
    }
    if lost(&mut control)? {
        return Ok(Outcome {
            reason: Reason::ControllerLostBeforeStart,
            status: None,
        });
    }
    let group = OwnedChildGroup::spawn(command)?;
    loop {
        if lost(&mut control)? {
            return Ok(Outcome {
                reason: Reason::ControllerLost,
                status: Some(group.terminate(grace)?),
            });
        }
        if group.has_exited()? {
            let status = group.wait()?; // Clean descendants while still pinning the leader.
            let reason = if lost(&mut control)? {
                Reason::ControllerLost
            } else {
                Reason::LeaderExited
            };
            return Ok(Outcome {
                reason,
                status: Some(status),
            });
        }
        std::thread::sleep(POLL);
    }
}
