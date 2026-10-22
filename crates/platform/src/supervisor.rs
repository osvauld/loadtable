//! Composed controller -> guardian -> watched-wrapper lifecycle experiment.
//!
//! Run in a separate guardian. Caller spawns workload_guard::serve in a private
//! OwnedChildGroup with its own Unix socket, holds authoritative leases, and
//! drains output independently. This loop forwards READY, then one START, and
//! retains BOTH channels until owned cleanup finishes. No worker/state changes.
//!
//! An outcome/report is NOT ledger completion or proof that all descendants
//! released leases. In particular cleanup errors (including native EPERM) stay
//! errors. Admission, FD allowlists, holder observation and recovery are still
//! required; arbitrary escapes/simultaneous failures are not contained here.
use crate::guarded_group::{ControlRead, START, byte, prepare_control};
use crate::owned_group::OwnedChildGroup;
use crate::workload_guard::{READY, Report, decode_report, send_control};
use std::io;
use std::os::unix::net::UnixStream;
use std::process::ExitStatus;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Command reported, NOT an authorization to mark a job passed.
    CommandReported,
    ControllerLost,
    WrapperExited,
    ProtocolFailure,
}
#[derive(Debug)]
pub struct Outcome {
    pub reason: Reason,
    pub report: Option<Report>,
    pub error: Option<io::Error>,
    /// Kept separate even when a zero command exit was reported. Never convert
    /// a permission/ownership error into successful cleanup or replay permission.
    pub cleanup: io::Result<ExitStatus>,
}

fn controller_event(control: &mut UnixStream, granted: &mut bool) -> io::Result<bool> {
    match byte(control)? {
        ControlRead::Closed => Ok(true),
        ControlRead::Pending => Ok(false),
        ControlRead::Byte(START) if !*granted => {
            *granted = true;
            Ok(false)
        }
        ControlRead::Byte(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid controller grant",
        )),
    }
}

/// Guardian-side event loop. Send READY to controller only after the wrapper's
/// watcher is armed. Controller sends START once; otherwise retains its sole
/// close-on-exec peer endpoint. Never duplicate that endpoint into descendants.
/// Wrapper socket speaks workload_guard's host-local READY / five-byte report.
///
/// Polls both channels without blocking on a partial report. No hard deadline
/// for kernel spawn/reaping is promised. Retain caller leases through this
/// function AND subsequent native holder checks. A live wrapper is mandatory
/// until guardian cleanup: early wrapper exit remains uncertain, not success.
pub fn supervise(
    group: OwnedChildGroup,
    mut wrapper: UnixStream,
    mut controller: UnixStream,
    grace: Duration,
) -> Outcome {
    let mut report = None;
    let mut failure = None;
    let result = (|| -> io::Result<Reason> {
        prepare_control(&wrapper)?;
        prepare_control(&controller)?;
        let mut ready = false;
        let mut granted = false;
        let mut forwarded = false;
        let mut frame = [0; 5];
        let mut used = 0;
        loop {
            if controller_event(&mut controller, &mut granted)? {
                return Ok(Reason::ControllerLost);
            }
            // Bounded per iteration: a malicious/partial stream cannot starve
            // controller-loss observation. No blocking read_exact here.
            for _ in 0..7 {
                match byte(&mut wrapper)? {
                    ControlRead::Pending => break,
                    ControlRead::Closed => return Ok(Reason::WrapperExited),
                    ControlRead::Byte(READY) if !ready => {
                        ready = true;
                        send_control(&controller, &[READY])?;
                    }
                    ControlRead::Byte(value) if ready && forwarded && used < frame.len() => {
                        frame[used] = value;
                        used += 1;
                    }
                    ControlRead::Byte(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid wrapper frame/order",
                        ));
                    }
                }
            }
            if group.has_exited()? {
                return Ok(Reason::WrapperExited);
            }
            if used == frame.len() {
                report = Some(decode_report(frame)?);
                return Ok(if controller_event(&mut controller, &mut granted)? {
                    Reason::ControllerLost
                } else {
                    Reason::CommandReported
                });
            }
            if ready && granted && !forwarded {
                if controller_event(&mut controller, &mut granted)? {
                    return Ok(Reason::ControllerLost);
                }
                send_control(&wrapper, &[START])?;
                forwarded = true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    let mut reason = match result {
        Ok(reason) => reason,
        Err(error) => {
            failure = Some(error);
            Reason::ProtocolFailure
        }
    };
    // Keep the wrapper endpoint open while TERM/KILL runs; dropping it earlier
    // would race its self-cleanup with our sweep and discard useful ownership.
    let cleanup = group.terminate(grace);
    // A command report cannot launder controller loss during cleanup into a
    // completed lifecycle. Other uncertainty/error reasons never become success.
    if reason == Reason::CommandReported {
        let mut already_granted = true;
        match controller_event(&mut controller, &mut already_granted) {
            Ok(true) => reason = Reason::ControllerLost,
            Ok(false) => {}
            Err(error) => {
                failure = Some(error);
                reason = Reason::ProtocolFailure;
            }
        }
    }
    Outcome {
        reason,
        report,
        error: failure,
        cleanup,
    }
}
