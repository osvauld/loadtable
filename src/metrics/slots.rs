//! Read-only view of admission slots and their queues. Unlike `tasks
//! admission`, busy state comes from /proc/locks (no probing flock), so a
//! 2-second sampler never takes a slot even for an instant.
use super::{Slot, SlotKind};
use crate::admission::{Note, OFFLOAD_SLOTS};
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq)]
pub struct Waiter {
    /// Admission slot label: `compile`, `run`, `offload` or `device SERIAL`.
    pub label: String,
    pub job: Option<i64>,
    pub who: String,
    pub mtime: SystemTime,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scan {
    pub slots: Vec<Slot>,
    /// Admission FIFO order: wait-file mtime, then path (as `admission` orders).
    pub waiters: Vec<Waiter>,
}

/// `owner:job-ID` (native admission notes) into (job id, owner).
pub fn parse_who(who: &str) -> (Option<i64>, String) {
    match who.rsplit_once(":job-") {
        Some((owner, id)) => match id.parse() {
            Ok(id) => (Some(id), owner.to_string()),
            Err(_) => (None, who.to_string()),
        },
        None => (None, who.to_string()),
    }
}

fn read_note(path: &Path) -> Option<Note> {
    let m = std::fs::symlink_metadata(path).ok()?;
    if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.len() > 16384 {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn note_alive(note: &Note) -> bool {
    crate::worker::identity(note.pid).is_ok_and(|p| {
        note.start_ticks.is_none_or(|s| s == p.start_ticks) && crate::worker::alive(&p)
    })
}

fn kind_of(name: &str) -> Option<SlotKind> {
    if name == "p2p-build.lock" || name.starts_with("p2p-build-") {
        Some(SlotKind::Compile)
    } else if name.starts_with("p2p-run-") {
        Some(SlotKind::Run)
    } else if name.starts_with("p2p-service-") {
        Some(SlotKind::Service)
    } else if name.starts_with("p2p-offload-") {
        Some(SlotKind::Offload)
    } else if name.starts_with("p2p-device-") {
        Some(SlotKind::Device)
    } else {
        None
    }
}

/// Scans one admission lock directory. `held` is the FLOCK set of /proc/locks.
pub fn scan(root: &Path, held: &HashSet<(u32, u32, u64)>) -> Scan {
    let mut names: Vec<String> =
        crate::admission::compile_slot_names(crate::admission::compile_slots())
            .into_iter()
            .chain((0..4).map(|i| format!("p2p-run-{i}.lock")))
            .chain((0..OFFLOAD_SLOTS).map(|i| format!("p2p-offload-{i}.lock")))
            .chain((0..crate::admission::SERVICE_SLOTS).map(|i| format!("p2p-service-{i}.lock")))
            .collect();
    let mut waiters = vec![];
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.take(4096).flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("p2p-device-") && name.ends_with(".lock") {
                names.push(name);
            } else if name.starts_with("p2p-wait-") && name.ends_with(".json") {
                let path = entry.path();
                let Some(note) = read_note(&path).filter(note_alive) else {
                    continue;
                };
                let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
                    continue;
                };
                let (job, who) = parse_who(&note.who);
                waiters.push(Waiter {
                    label: note.slot.clone().unwrap_or_default(),
                    job,
                    who,
                    mtime,
                    path,
                });
            }
        }
    }
    waiters.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.path.cmp(&b.path)));
    let mut slots = vec![];
    for name in names {
        let Some(kind) = kind_of(&name) else { continue };
        let path = root.join(&name);
        let busy = std::fs::symlink_metadata(&path)
            .is_ok_and(|m| crate::metrics::procfs::flock_held(held, &m));
        let holder = busy
            .then(|| read_note(&path.with_extension("holder")))
            .flatten()
            .filter(note_alive);
        let (holder_job, holder_who) = match holder {
            Some(note) => {
                let (job, who) = parse_who(&note.who);
                (job, Some(who))
            }
            None => (None, None),
        };
        // Busy with no live note: name whoever really holds it (bounded /proc
        // scan, only in this rare case, never a probing flock).
        let holder_processes = if busy && holder_who.is_none() {
            crate::admission::holders::holders(&path)
        } else {
            vec![]
        };
        slots.push(Slot {
            kind,
            name: name.trim_end_matches(".lock").to_string(),
            busy,
            holder_job,
            holder_who,
            holder_processes,
        });
    }
    Scan { slots, waiters }
}

/// Admission label of a device slot from its lock-file stem.
pub fn device_label(slot_name: &str) -> Option<String> {
    slot_name
        .strip_prefix("p2p-device-")
        .map(|serial| format!("device {serial}"))
}
