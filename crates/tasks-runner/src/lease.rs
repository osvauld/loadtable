//! The lease: the identity of a running group that survives a daemon restart.

use tasks_platform::{GroupId, PlatformError, Processes};

/// Ownership record of a running process group: the group id plus the start
/// time of its leader, so a restarted daemon can tell its group from a reused pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Lease {
    /// The group identity to persist.
    pub group: GroupId,
}

impl Lease {
    /// Wraps a group identity.
    pub fn new(group: GroupId) -> Lease {
        Lease { group }
    }

    /// True while the group is alive and its leader is the one recorded.
    /// `Gone` (exited or pid reused) is `Ok(false)`; other errors propagate.
    pub fn still_ours(&self, procs: &dyn Processes) -> Result<bool, PlatformError> {
        match procs.usage(self.group) {
            Ok(_) => Ok(true),
            Err(PlatformError::Gone { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }
}
