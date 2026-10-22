//! Admission control: decides whether a job may start now, given pressure, memory,
//! disk and slot availability (via `&dyn Pressure`, `&dyn Memory`, `&dyn Disk`,
//! `&dyn SlotLocks`). The decision logic will live here; it does no I/O itself.

use tasks_core::WaitReason;
use tasks_platform::SlotGuard;

/// Outcome of an admission check.
#[derive(Debug)]
pub enum Decision {
    /// The job may start; the handle keeps its slot held.
    Admit(SlotGuard),
    /// The job must wait for the given reason.
    Wait(WaitReason),
}
