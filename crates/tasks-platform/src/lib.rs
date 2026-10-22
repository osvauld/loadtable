//! OS abstraction for `tasks`: traits for memory, pressure, processes, slot locks,
//! services and disk, with Linux and macOS implementations and test fakes.

mod error;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
mod slots;
mod traits;
mod types;

pub use error::PlatformError;
pub use slots::{FlockSlots, SlotGuard};
pub use traits::{Disk, Memory, Pressure, Processes, Service, SlotLocks};
pub use types::{
    Child, GroupId, Level, MemorySnapshot, PressureSnapshot, ServiceStatus, ServiceUnit, Signal,
    SpawnSpec, Usage,
};

#[cfg(target_os = "linux")]
pub use linux::Native;
#[cfg(target_os = "macos")]
pub use macos::Native;

/// Returns the implementation for the current OS.
pub fn native() -> Native {
    Native
}
