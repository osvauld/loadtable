//! The trait objects the scheduler works over: platform services and the ledger.

use crate::holders::{HolderSource, ProcHolders};
use std::path::Path;
use std::sync::Arc;
use tasks_ledger::Store;
use tasks_platform::{Disk, FlockSlots, Memory, Pressure, Processes, SlotLocks};
use tasks_timing::History;

/// A ledger the daemon can use from many threads: job storage plus timing history.
pub trait Ledger: Store + History + Send + Sync {}
impl<T: Store + History + Send + Sync> Ledger for T {}

/// Everything the daemon asks of the operating system.
#[derive(Clone)]
pub struct Platform {
    /// Memory totals.
    pub memory: Arc<dyn Memory + Send + Sync>,
    /// Pressure readings.
    pub pressure: Arc<dyn Pressure + Send + Sync>,
    /// Process groups.
    pub procs: Arc<dyn Processes + Send + Sync>,
    /// Disk space.
    pub disk: Arc<dyn Disk + Send + Sync>,
    /// Slot locks.
    pub locks: Arc<dyn SlotLocks + Send + Sync>,
    /// Who holds a busy slot.
    pub holders: Arc<dyn HolderSource>,
}

impl Platform {
    /// The real machine, with slot locks under `lock_dir`.
    pub fn native(lock_dir: &Path) -> Platform {
        let native = Arc::new(tasks_platform::native());
        Platform {
            memory: native.clone(),
            pressure: native.clone(),
            procs: native.clone(),
            disk: native,
            locks: Arc::new(FlockSlots::new(lock_dir)),
            holders: Arc::new(ProcHolders),
        }
    }
}
