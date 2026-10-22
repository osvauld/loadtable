//! Slot locks implemented with `flock`, shared by both operating systems.

use crate::{PlatformError, SlotLocks};
use rustix::fs::{FlockOperation, flock};
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
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
}

fn kind_name(k: SlotKind) -> &'static str {
    match k {
        SlotKind::Compile => "compile",
        SlotKind::Run => "run",
        SlotKind::Offload => "offload",
        SlotKind::Device => "device",
    }
}

impl SlotLocks for FlockSlots {
    fn try_acquire(&self, kind: SlotKind, index: u32) -> Result<Option<SlotGuard>, PlatformError> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{}-{index}.lock", kind_name(kind)));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(SlotGuard { _file: file })),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
