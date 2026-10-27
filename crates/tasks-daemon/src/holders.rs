//! Naming who holds a slot lock (R-047, R-048, R-049, R-050, R-021).
//!
//! On Linux the owners are found by a read-only scan of `/proc/<pid>/fdinfo`: a process counts
//! when one of its descriptors carries a granted `FLOCK` entry on the lock file's inode, which
//! names flock owners (including a daemonized child that inherited the lock) and never mere
//! openers or waiters. The inode alone is compared, not the device, because a btrfs subvolume
//! reports an anonymous device number (R-021). Nothing here takes the lock it inspects.
//! Other systems report no names; the caller then says "held by unknown work".

use std::path::Path;
use tasks_core::JobId;

/// A process holding a slot lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// Process id.
    pub pid: u32,
    /// Command line, shortened.
    pub command: String,
    /// The job that started it, from its `TASKS_JOB_ID`.
    pub origin_job: Option<JobId>,
}

impl Holder {
    /// `pid 123 \`cargo build\` from job #7`.
    pub fn describe(&self) -> String {
        let origin = self
            .origin_job
            .map_or_else(String::new, |j| format!(" from job #{}", j.0));
        format!("pid {} `{}`{origin}", self.pid, self.command)
    }
}

/// Finds the holders of a lock file.
pub trait HolderSource: Send + Sync {
    /// The processes holding the flock on `lock_file`; empty when unknown.
    fn holders(&self, lock_file: &Path) -> Vec<Holder>;
}

/// One parsed lock line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockLine {
    /// Owner pid as the kernel records it.
    pub pid: u32,
    /// Inode of the locked file.
    pub inode: u64,
    /// True for a waiter (`->`), false for a granted lock.
    pub waiting: bool,
}

/// Parses a `/proc/locks` line or the `lock:` line of `fdinfo`; `None` for anything that is
/// not an flock entry.
pub fn parse_lock_line(line: &str) -> Option<LockLine> {
    let line = line.trim();
    let line = line.strip_prefix("lock:").map_or(line, str::trim);
    let mut words = line.split_whitespace();
    let _index = words.next()?;
    let mut next = words.next()?;
    let waiting = next == "->";
    if waiting {
        next = words.next()?;
    }
    if next != "FLOCK" {
        return None;
    }
    let _advisory = words.next()?;
    let _mode = words.next()?;
    let pid: u32 = words.next()?.parse().ok()?;
    let dev_ino = words.next()?;
    let inode: u64 = dev_ino.rsplit(':').next()?.parse().ok()?;
    Some(LockLine {
        pid,
        inode,
        waiting,
    })
}

/// Pids with a granted flock on `inode` in `/proc/locks` text.
pub fn flock_owners(text: &str, inode: u64) -> Vec<u32> {
    let mut out: Vec<u32> = text
        .lines()
        .filter_map(parse_lock_line)
        .filter(|l| !l.waiting && l.inode == inode)
        .map(|l| l.pid)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// True when `fdinfo` text carries a granted flock on `inode`.
pub fn fdinfo_holds_flock(fdinfo: &str, inode: u64) -> bool {
    fdinfo
        .lines()
        .filter(|l| l.starts_with("lock:"))
        .filter_map(parse_lock_line)
        .any(|l| !l.waiting && l.inode == inode)
}

/// Turns a NUL-separated `cmdline` into a short readable command.
pub fn parse_cmdline(raw: &[u8]) -> String {
    let text: String = String::from_utf8_lossy(raw)
        .split('\0')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() > 120 {
        let cut: String = text.chars().take(117).collect();
        format!("{cut}...")
    } else {
        text
    }
}

/// The `LOADTABLE_JOB_ID` (or, from a job started before the rename, `TASKS_JOB_ID`) of a
/// NUL-separated `environ`.
pub fn parse_job_env(raw: &[u8]) -> Option<JobId> {
    String::from_utf8_lossy(raw)
        .split('\0')
        .find_map(|kv| {
            kv.strip_prefix("LOADTABLE_JOB_ID=")
                .or_else(|| kv.strip_prefix("TASKS_JOB_ID="))
        })
        .and_then(|v| v.parse::<i64>().ok())
        .map(JobId)
}

/// The real `/proc` scan (Linux); no names elsewhere.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcHolders;

#[cfg(target_os = "linux")]
impl HolderSource for ProcHolders {
    fn holders(&self, lock_file: &Path) -> Vec<Holder> {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::metadata(lock_file) else {
            return Vec::new();
        };
        let inode = meta.ino();
        let Ok(procs) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in procs.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fdinfo")) else {
                continue;
            };
            let holds = fds
                .flatten()
                .filter_map(|fd| std::fs::read_to_string(fd.path()).ok())
                .any(|text| fdinfo_holds_flock(&text, inode));
            if holds {
                let command = std::fs::read(format!("/proc/{pid}/cmdline"))
                    .map(|b| parse_cmdline(&b))
                    .unwrap_or_default();
                let origin_job = std::fs::read(format!("/proc/{pid}/environ"))
                    .ok()
                    .and_then(|b| parse_job_env(&b));
                out.push(Holder {
                    pid,
                    command,
                    origin_job,
                });
            }
        }
        out
    }
}

#[cfg(not(target_os = "linux"))]
impl HolderSource for ProcHolders {
    fn holders(&self, _lock_file: &Path) -> Vec<Holder> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCKS: &str = "\
1: FLOCK  ADVISORY  WRITE 4242 00:2a:131075 0 EOF
2: -> FLOCK  ADVISORY  WRITE 4343 00:2a:131075 0 EOF
3: POSIX  ADVISORY  WRITE 4444 fd:01:999 0 EOF
4: FLOCK  ADVISORY  WRITE 5555 fd:01:777 0 EOF
";

    // R-021: matched by inode, whatever the (anonymous) device says.
    #[test]
    fn r021_a_flock_on_an_anonymous_device_is_found_by_inode() {
        assert_eq!(flock_owners(LOCKS, 131075), vec![4242]);
        assert_eq!(flock_owners(LOCKS, 777), vec![5555]);
        assert!(
            flock_owners(LOCKS, 999).is_empty(),
            "POSIX locks are not slot locks"
        );
    }

    // R-047
    #[test]
    fn r047_locks_parse_holders_and_skip_waiters() {
        let l = parse_lock_line("2: -> FLOCK  ADVISORY  WRITE 4343 00:2a:131075 0 EOF").unwrap();
        assert!(l.waiting);
        assert_eq!(flock_owners(LOCKS, 131075), vec![4242]);
        assert!(parse_lock_line("garbage").is_none());
        let fdinfo =
            "pos:\t0\nflags:\t02\nlock:\t1: FLOCK  ADVISORY  WRITE 4242 00:2a:131075 0 EOF\n";
        assert!(fdinfo_holds_flock(fdinfo, 131075));
        assert!(!fdinfo_holds_flock(fdinfo, 5));
        // a descriptor that merely has the file open carries no lock line
        assert!(!fdinfo_holds_flock("pos:\t0\nflags:\t02\n", 131075));
        let waiter = "lock:\t1: -> FLOCK  ADVISORY  WRITE 4242 00:2a:131075 0 EOF\n";
        assert!(!fdinfo_holds_flock(waiter, 131075));
    }

    // R-047, R-048
    #[test]
    fn r047_cmdline_and_origin_job_are_parsed() {
        assert_eq!(
            parse_cmdline(b"cargo\0build\0--release\0"),
            "cargo build --release"
        );
        assert!(parse_cmdline(&vec![b'a'; 500]).ends_with("..."));
        assert_eq!(
            parse_job_env(b"PATH=/bin\0TASKS_JOB_ID=4271\0HOME=/h\0"),
            Some(JobId(4271))
        );
        assert_eq!(parse_job_env(b"PATH=/bin\0"), None);
        let h = Holder {
            pid: 7,
            command: "sleep 99".into(),
            origin_job: Some(JobId(12)),
        };
        assert_eq!(h.describe(), "pid 7 `sleep 99` from job #12");
    }

    // R-020: the scan only reads; it never takes the lock it looks at.
    #[cfg(target_os = "linux")]
    #[test]
    fn r020_r048_the_real_scan_names_us_as_holder_without_taking_the_lock() {
        use tasks_core::SlotKind;
        use tasks_platform::{FlockSlots, SlotLocks};
        let dir = std::env::temp_dir().join(format!("tasks-holders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let slots = FlockSlots::new(&dir);
        let guard = slots.try_acquire(SlotKind::Compile, 0).unwrap().unwrap();
        let found = ProcHolders.holders(&dir.join("compile-0.lock"));
        assert!(
            found.iter().any(|h| h.pid == std::process::id()),
            "{found:?}"
        );
        // scanning left the lock held by us only
        assert!(slots.try_acquire(SlotKind::Compile, 0).unwrap().is_none());
        drop(guard);
        assert!(ProcHolders.holders(&dir.join("compile-0.lock")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
