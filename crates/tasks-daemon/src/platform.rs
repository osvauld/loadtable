//! The trait objects the scheduler works over: platform services and the ledger.

use crate::holders::{HolderSource, ProcHolders};
use std::path::Path;
use std::sync::Arc;
use tasks_ledger::Store;
use tasks_platform::{
    Cgroups, Disk, FlockSlots, Memory, PowerSource, Pressure, ProcessTable, Processes, SleepGuard,
    SlotLocks,
};
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
    /// Per-job cgroups, or the reason there are none (design §9).
    pub cgroups: Arc<dyn Cgroups>,
    /// The process table: per-job figures without cgroups, and the top processes outside jobs.
    pub table: Arc<dyn ProcessTable>,
    /// Mains or battery.
    pub power: Arc<dyn PowerSource + Send + Sync>,
    /// The hold that keeps the machine from idle sleep.
    pub sleep: Arc<dyn SleepGuard + Send + Sync>,
}

impl Platform {
    /// The real machine, with slot locks under `lock_dir`.
    pub fn native(lock_dir: &Path) -> Platform {
        let native = Arc::new(tasks_platform::native());
        Platform {
            memory: native.clone(),
            pressure: native.clone(),
            procs: native.clone(),
            disk: native.clone(),
            locks: Arc::new(FlockSlots::new(lock_dir)),
            holders: Arc::new(ProcHolders),
            // On Linux this moves the daemon into its `daemon` leaf cgroup when the service
            // unit delegated one; otherwise it says why it did not.
            cgroups: Arc::from(tasks_platform::native_cgroups()),
            table: native.clone(),
            power: native.clone(),
            sleep: native,
        }
    }
}
