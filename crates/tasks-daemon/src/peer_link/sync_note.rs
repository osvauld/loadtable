//! What the sync before a forwarded run did, kept next to the job's log so the status can say
//! `synced 742 B in 0.4 s` (R-648). Like the fetch results, it is a small sidecar file: the
//! audit log has the bytes, but not how long the sync took.

use crate::api::SyncView;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;
use tasks_peer::sync::{SyncDriver, SyncError, SyncJob, SyncReport};
use tasks_peer::transport::Connection;

/// Where a job's sync note lives: next to its log.
pub fn sidecar_path(log: &Path) -> PathBuf {
    let mut p = log.as_os_str().to_os_string();
    p.push(".sync.json");
    PathBuf::from(p)
}

/// The recorded sync of a job, when one finished.
pub fn read(log: &Path) -> Option<SyncView> {
    std::fs::read(sidecar_path(log))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

/// Records `note` next to `log`; a failure to write loses only the display line.
pub fn write(log: &Path, note: &SyncView) {
    let path = sidecar_path(log);
    let tmp = path.with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec(note)
        && std::fs::write(&tmp, bytes).is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// A sync driver that times the one it wraps and remembers the last good result.
pub struct TimedSync<'a> {
    inner: &'a dyn SyncDriver,
    last: Mutex<Option<SyncView>>,
}

impl<'a> TimedSync<'a> {
    /// Times `inner`.
    pub fn new(inner: &'a dyn SyncDriver) -> TimedSync<'a> {
        TimedSync {
            inner,
            last: Mutex::new(None),
        }
    }

    /// The sync finished since the last call, once.
    pub fn take(&self) -> Option<SyncView> {
        self.last.lock().ok().and_then(|mut l| l.take())
    }
}

/// The view of a finished sync that took `elapsed`; a sync cannot take longer than a u64
/// count of milliseconds, but the conversion says what happens if it does.
pub fn view_of(report: &SyncReport, elapsed: std::time::Duration) -> SyncView {
    SyncView {
        bytes: report.pack_bytes_sent,
        files: report.files,
        millis: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        already_there: report.already_there,
    }
}

impl SyncDriver for TimedSync<'_> {
    fn push(&self, conn: &mut dyn Connection, job: &SyncJob<'_>) -> Result<SyncReport, SyncError> {
        let t = Instant::now();
        let rep = self.inner.push(conn, job)?;
        if let Ok(mut l) = self.last.lock() {
            *l = Some(view_of(&rep, t.elapsed()));
        }
        Ok(rep)
    }

    fn tree_of(&self, job: &SyncJob<'_>) -> Result<tasks_peer::locks::CopyTree, SyncError> {
        self.inner.tree_of(job)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // R-648
    #[test]
    fn r648_the_sync_note_is_kept_next_to_the_log_and_read_back() {
        let dir = std::env::temp_dir().join(format!("tasks-sync-note-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("7.log");
        assert_eq!(read(&log), None, "no note before a sync");
        let note = SyncView {
            bytes: 742,
            files: 120,
            millis: 400,
            already_there: false,
        };
        write(&log, &note);
        assert_eq!(read(&log), Some(note));
        assert!(sidecar_path(&log).ends_with("7.log.sync.json"));
        // a later sync (a retry) replaces the earlier note
        let again = SyncView { millis: 90, ..note };
        write(&log, &again);
        assert_eq!(read(&log), Some(again));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
