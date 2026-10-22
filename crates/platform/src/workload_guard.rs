//! Workload-side guardian-loss experiment, NOT worker integration.
//!
//! Run only in a dedicated, freshly exec'd process-group leader. A watcher is
//! armed before READY; START then permits a command in the SAME group. EOF or
//! protocol failure kills the wrapper's own still-live group, never a stored PID.
//! The wrapper stays alive/armed after reporting the command result: returning
//! early would abandon descendants if the guardian died before its final sweep.
//!
//! The guardian must retain exclusive OwnedChildGroup ownership of this wrapper,
//! record the report, clean the group and reap. A report is not cleanup or ledger
//! completion. The guardian owns the sole peer endpoint; it must not escape to
//! the workload. Captured output must be drained independently.
//!
//! This does not contain escaped sessions, simultaneous guardian+wrapper death,
//! or commands that change credentials/groups. It does not prove every kernel
//! fork/kill interleaving safe, provide hard deadlines for stuck kernel tasks,
//! implement admission/recovery, or atomically revoke a previously sent grant.
use crate::guarded_group::{ControlRead, START, byte, prepare_control};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, SyncSender};
use std::time::Duration;

pub const READY: u8 = b'R';
const POLL: Duration = Duration::from_millis(10);

/// Host-local bounded report, not a cross-host protocol or job success result.
#[derive(Debug)]
pub enum Report {
    Exited(ExitStatus),
    /// Spawn/wait error; zero means the error had no OS errno.
    ExecutionError(i32),
}
impl Report {
    fn frame(&self) -> [u8; 5] {
        let (tag, raw) = match self {
            Self::Exited(status) => (b'D', status.into_raw()),
            Self::ExecutionError(errno) => (b'E', *errno),
        };
        let mut frame = [tag, 0, 0, 0, 0];
        frame[1..].copy_from_slice(&raw.to_be_bytes());
        frame
    }
}

/// Read one five-byte report after READY/START. Caller must provide its own
/// timeout/event-loop policy; this may block. Raw wait status is host-local.
pub fn read_report(control: &mut UnixStream) -> io::Result<Report> {
    let mut frame = [0; 5];
    control.read_exact(&mut frame)?;
    decode_report(frame)
}

pub(crate) fn decode_report(frame: [u8; 5]) -> io::Result<Report> {
    let raw = i32::from_be_bytes(frame[1..].try_into().expect("four bytes"));
    match frame[0] {
        b'D' if (0..=0xffff).contains(&raw) => {
            let status = ExitStatus::from_raw(raw);
            if status.code().is_some() || status.signal().is_some() {
                return Ok(Report::Exited(status));
            }
        }
        b'E' if raw >= 0 => return Ok(Report::ExecutionError(raw)),
        _ => {}
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid workload report",
    ))
}

pub(crate) fn send_control(control: &UnixStream, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // Explicitly suppress SIGPIPE even for a socket imported from another
        // launcher. Report-write failure must not kill only the wrapper and
        // abandon descendants before the watcher can clean them.
        let count = unsafe {
            libc::send(
                control.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[count as usize..];
    }
    Ok(())
}

fn own_group() -> bool {
    // The running process itself pins its ID. Never signal a remembered PGID
    // after its leader has gone; refuse even self-cleanup if moved out of group.
    unsafe { libc::getpid() > 1 && libc::getpgrp() == libc::getpid() }
}
fn stop_group() -> ! {
    if own_group() && unsafe { libc::killpg(libc::getpid(), libc::SIGKILL) } == 0 {
        // Darwin can return before self-directed group SIGKILL is delivered.
        // Do not race it with _exit and discard our live leader pin early.
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    // If signal delivery fails, exit without inventing successful cleanup.
    // Leaked descendant leases still block authoritative admission.
    unsafe { libc::_exit(125) }
}
struct FailStop;
impl Drop for FailStop {
    fn drop(&mut self) {
        stop_group();
    }
}
extern "C" fn retain_on_term(_: libc::c_int) {}

fn watch(mut control: UnixStream, permit: SyncSender<()>) -> ! {
    let _fail_stop = FailStop;
    if send_control(&control, &[READY]).is_err() {
        stop_group();
    }
    let mut started = false;
    loop {
        match byte(&mut control) {
            Ok(ControlRead::Pending) => std::thread::sleep(POLL),
            Ok(ControlRead::Byte(START)) if !started => {
                // A grant already followed by EOF/extra data grants nothing.
                if !matches!(byte(&mut control), Ok(ControlRead::Pending)) {
                    stop_group();
                }
                started = true;
                if permit.send(()).is_err() {
                    stop_group();
                }
            }
            _ => stop_group(),
        }
    }
}

/// Does not return after arming. Startup errors return BEFORE command entry.
///
/// Sole-purpose wrapper only: installs a caught no-op SIGTERM handler, not
/// SIG_IGN. Thus guardian TERM cannot kill the watcher before its final KILL;
/// exec resets the caught handler to default for the actual workload. Trusted
/// pre_exec hooks must be async-signal-safe and MUST NOT change group/session,
/// signal policy or close/overwrite control descriptors. This overrides the
/// command's process_group and stdin: the job joins this group with null stdin,
/// never the wrapper's potentially control-bearing stdin. This is neither a
/// sandbox nor a general thread API. Use pre-opened output pipes/files with an
/// independent drainer, NOT Stdio::piped() whose Child handles stay private.
///
/// The guardian must kill/reap this wrapper even after receiving a report. The
/// watcher remains active while spawn/pre_exec/wait and result handling run.
pub fn serve(command: &mut Command, control: UnixStream) -> io::Result<()> {
    if !own_group() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workload guard requires its own live process-group leader",
        ));
    }
    prepare_control(&control)?;
    let watcher = control.try_clone()?;
    let (permit, granted) = mpsc::sync_channel(1);
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = retain_on_term as *const () as usize;
    action.sa_flags = libc::SA_RESTART;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
    }
    if unsafe { libc::sigaction(libc::SIGTERM, std::ptr::null(), &mut old) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if old.sa_sigaction != libc::SIG_DFL {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workload guard requires default SIGTERM at startup",
        ));
    }
    if unsafe { libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if let Err(error) = std::thread::Builder::new()
        .name("guardian-liveness".into())
        .spawn(move || watch(watcher, permit))
    {
        unsafe {
            libc::sigaction(libc::SIGTERM, &old, std::ptr::null_mut());
        }
        return Err(error);
    }
    let _fail_stop = FailStop;
    if granted.recv().is_err() {
        stop_group();
    }
    command
        .stdin(Stdio::null())
        .process_group(unsafe { libc::getpid() });
    let report = match command.spawn().and_then(|mut child| child.wait()) {
        Ok(status) => Report::Exited(status),
        Err(error) => Report::ExecutionError(error.raw_os_error().unwrap_or(0)),
    };
    if send_control(&control, &report.frame()).is_err() {
        stop_group();
    }
    // NO disarm on command exit. Descendants and leases may still be present.
    loop {
        std::thread::park();
    }
}
