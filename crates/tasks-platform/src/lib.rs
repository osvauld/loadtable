//! OS abstraction for `tasks`: traits for memory, pressure, processes, slot locks,
//! services and disk, with Linux and macOS implementations and test fakes.

mod disk;
mod error;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
mod fds;
mod groups;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
// The macOS parsers are pure, so they are also compiled under test on Linux.
#[cfg(any(test, target_os = "macos"))]
#[path = "macos/parse.rs"]
mod macos_parse;
mod service;
mod slots;
mod traits;
mod types;

pub use error::PlatformError;
pub use fds::close_on_exec_inherited;
pub use service::{render_launchd_plist, render_systemd_unit};
pub use slots::{FlockSlots, SlotGuard, slot_lock_path};
pub use traits::{Disk, Memory, Pressure, Processes, Service, SlotLocks};
pub use types::{
    Child, GroupId, InstallPlan, Level, MemorySnapshot, PressureSnapshot, PressureThresholds,
    ServiceStatus, ServiceUnit, Signal, SpawnSpec, Usage,
};

#[cfg(target_os = "linux")]
pub use linux::Native;
#[cfg(target_os = "macos")]
pub use macos::Native;

/// Returns the implementation for the current OS.
pub fn native() -> Native {
    Native
}
