//! Slot locks implemented with `flock`, shared by both operating systems.

use crate::{PlatformError, SlotLocks};
use rustix::fs::{FlockOperation, flock};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use tasks_core::SlotKind;

/// A held slot; the lock is released when dropped.
#[derive(Debug)]
pub struct SlotGuard {
    _file: File,
}

/// Slot locks stored as files in a directory.
#[derive(Debug, Clone)]
pub struct FlockSlots {
    dir: PathBuf,
}

impl FlockSlots {
    /// Creates slot locks under `dir` (created on first use).
    pub fn new(dir: impl Into<PathBuf>) -> FlockSlots {
        FlockSlots { dir: dir.into() }
    }

    /// The lock file of slot `index` of `kind`. Short-lane slots are just high indices
    /// (`compile-1000.lock`); the index is used as given.
    pub fn lock_path(&self, kind: SlotKind, index: u32) -> PathBuf {
        slot_lock_path(&self.dir, kind, index)
    }
}

/// The lock file of a slot under `dir`: `<kind>-<index>.lock`. The one place that names slot
/// files; holders, audits and the flock itself all go through it.
pub fn slot_lock_path(dir: &Path, kind: SlotKind, index: u32) -> PathBuf {
    dir.join(format!("{}-{index}.lock", kind_name(kind)))
}

fn kind_name(k: SlotKind) -> &'static str {
    match k {
        SlotKind::Compile => "compile",
        SlotKind::Run => "run",
        SlotKind::Offload => "offload",
        SlotKind::Device => "device",
        SlotKind::Transfer => "transfer",
    }
}

impl SlotLocks for FlockSlots {
    fn try_acquire(&self, kind: SlotKind, index: u32) -> Result<Option<SlotGuard>, PlatformError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| PlatformError::io(&self.dir, e))?;
        let path = self.lock_path(kind, index);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| PlatformError::io(&path, e))?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(SlotGuard { _file: file })),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(source) => Err(PlatformError::Syscall {
                syscall: "flock",
                path: Some(path),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_names_the_file_the_flock_takes() {
        let dir = std::env::temp_dir().join(format!("tasks-slots-path-{}", std::process::id()));
        let slots = FlockSlots::new(&dir);
        assert_eq!(
            slots.lock_path(SlotKind::Offload, 3),
            dir.join("offload-3.lock")
        );
        // the short lane is a high index, passed through unchanged
        assert_eq!(
            slot_lock_path(&dir, SlotKind::Compile, 1000),
            dir.join("compile-1000.lock")
        );
        let _held = slots.try_acquire(SlotKind::Run, 1).unwrap();
        assert!(slots.lock_path(SlotKind::Run, 1).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_acquire_returns_none() {
        let dir = std::env::temp_dir().join(format!(
            "tasks-slots-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let slots = FlockSlots::new(&dir);
        let first = slots.try_acquire(SlotKind::Compile, 0).unwrap();
        assert!(first.is_some());
        assert!(slots.try_acquire(SlotKind::Compile, 0).unwrap().is_none());
        assert!(slots.try_acquire(SlotKind::Compile, 1).unwrap().is_some());
        drop(first);
        assert!(slots.try_acquire(SlotKind::Compile, 0).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
