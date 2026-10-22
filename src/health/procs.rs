//! Bounded, read-only /proc readers for the health checks: lock holders (/proc/locks), the
//! largest memory users (/proc/PID/status VmRSS + VmSwap) and command lines. The processes
//! that hold a slot's flock through an inherited descriptor are found by fix/lease-leak's
//! `admission::holders` scan (one scanner for admission, the snapshot and health). Parsers are
//! pure and take fixture text; readers take the /proc root so tests can use a fake tree.
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Processes inspected per pass at most.
pub const MAX_PIDS: usize = 8192;
/// File descriptors inspected per process at most (holder search).
const MAX_FDS: usize = 4096;
const MAX_CMDLINE: usize = 240;

/// One /proc/locks entry (holders only; blocked waiters are skipped).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockEntry {
    /// `FLOCK`, `POSIX`, `OFDLCK`...
    pub class: String,
    pub pid: u32,
    pub major: u32,
    pub minor: u32,
    pub inode: u64,
}

/// `1: FLOCK  ADVISORY  WRITE 12345 00:1b:678 0 EOF`.
pub fn parse_locks(text: &str) -> Vec<LockEntry> {
    let mut out = vec![];
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.get(1) == Some(&"->") || fields.len() < 6 {
            continue;
        }
        let (Some(pid), Some(id)) = (fields.get(4), fields.get(5)) else {
            continue;
        };
        let mut parts = id.split(':');
        let (Some(major), Some(minor), Some(inode)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let (Ok(pid), Ok(major), Ok(minor), Ok(inode)) = (
            pid.parse(),
            u32::from_str_radix(major, 16),
            u32::from_str_radix(minor, 16),
            inode.parse(),
        ) {
            out.push(LockEntry {
                class: fields[1].to_string(),
                pid,
                major,
                minor,
                inode,
            });
        }
    }
    out
}

/// (major, minor, inode) of a file as /proc/locks names it.
pub fn file_id(path: &Path) -> Option<(u32, u32, u64)> {
    let m = std::fs::symlink_metadata(path).ok()?;
    Some(task_manager_platform::fs::file_key(&m))
}

/// The NUL-separated command line, joined by spaces and bounded. None for a gone process or
/// a kernel thread.
pub fn cmdline(proc: &Path, pid: u32) -> Option<String> {
    let bytes = std::fs::read(proc.join(pid.to_string()).join("cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let text: String = bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let mut shown: String = text.chars().take(MAX_CMDLINE).collect();
    if text.chars().count() > MAX_CMDLINE {
        shown.push('…');
    }
    Some(shown)
}

/// This user's processes whose `comm` is `name` (bounded by [`MAX_PIDS`]).
pub fn find_by_comm(proc: &Path, name: &str) -> Vec<u32> {
    let me = unsafe { libc::geteuid() };
    let Ok(entries) = std::fs::read_dir(proc) else {
        return vec![];
    };
    entries
        .flatten()
        .take(MAX_PIDS)
        .filter_map(|e| {
            let pid = e.file_name().to_str()?.parse::<u32>().ok()?;
            if !e.metadata().is_ok_and(|m| m.uid() == me) {
                return None;
            }
            let comm = std::fs::read_to_string(e.path().join("comm")).ok()?;
            (comm.trim() == name).then_some(pid)
        })
        .collect()
}

/// Slot-lock files (`p2p-*.lock` in `lock_dir`) the process has open (bounded by [`MAX_FDS`]).
pub fn lock_fds(proc: &Path, pid: u32, lock_dir: &Path) -> Vec<String> {
    let Ok(fds) = std::fs::read_dir(proc.join(pid.to_string()).join("fd")) else {
        return vec![];
    };
    let mut out: Vec<String> = fds
        .flatten()
        .take(MAX_FDS)
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter(|t| {
            t.parent() == Some(lock_dir)
                && t.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("p2p-") && n.ends_with(".lock"))
        })
        .map(|t| t.display().to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// One variable from a process's environment (/proc/PID/environ, readable only for this
/// user's processes), bounded to 256 KiB. None: unreadable or unset.
pub fn env_var(proc: &Path, pid: u32, name: &str) -> Option<String> {
    use std::io::Read;
    let mut bytes = vec![];
    std::fs::File::open(proc.join(pid.to_string()).join("environ"))
        .ok()?
        .take(256 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    parse_environ(&bytes, name)
}

/// `name`'s value in NUL-separated `NAME=value` bytes.
pub fn parse_environ(bytes: &[u8], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    bytes
        .split(|b| *b == 0)
        .find_map(|kv| kv.strip_prefix(prefix.as_bytes()))
        .map(|v| String::from_utf8_lossy(v).chars().take(64).collect())
}

/// One process's memory, from /proc/PID/status.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcMemory {
    pub pid: u32,
    pub name: String,
    pub rss_bytes: u64,
    pub swap_bytes: u64,
    pub cmdline: Option<String>,
}

impl ProcMemory {
    pub fn total(&self) -> u64 {
        self.rss_bytes + self.swap_bytes
    }
    pub fn line(&self) -> String {
        format!(
            "pid {} {}: {} RSS + {} swap{}",
            self.pid,
            self.name,
            gib(self.rss_bytes),
            gib(self.swap_bytes),
            self.cmdline
                .as_deref()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default()
        )
    }
}

pub fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// (name, VmRSS bytes, VmSwap bytes) of a /proc/PID/status text. Kernel threads have no VmRSS.
pub fn parse_status(text: &str) -> Option<(String, u64, u64)> {
    let mut name = None;
    let mut rss = None;
    let mut swap = 0;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let kib = || {
            value
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .map(|v| v * 1024)
        };
        match key {
            "Name" => name = Some(value.trim().to_string()),
            "VmRSS" => rss = kib(),
            "VmSwap" => swap = kib().unwrap_or(0),
            _ => {}
        }
    }
    Some((name?, rss?, swap))
}

/// The `limit` largest processes by RSS + swap (all users; bounded walk).
pub fn top_memory(proc: &Path, limit: usize) -> Vec<ProcMemory> {
    let mut all = vec![];
    let Ok(entries) = std::fs::read_dir(proc) else {
        return all;
    };
    for entry in entries.flatten().take(MAX_PIDS) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        if let Some((name, rss_bytes, swap_bytes)) = parse_status(&text) {
            all.push(ProcMemory {
                pid,
                name,
                rss_bytes,
                swap_bytes,
                cmdline: None,
            });
        }
    }
    all.sort_by(|a, b| b.total().cmp(&a.total()).then(a.pid.cmp(&b.pid)));
    all.truncate(limit);
    for p in &mut all {
        p.cmdline = cmdline(proc, p.pid);
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_parse_holders_and_skip_waiters() {
        let text = "1: FLOCK  ADVISORY  WRITE 4242 00:1b:678 0 EOF\n\
                    1: -> FLOCK  ADVISORY  WRITE 4343 00:1b:678 0 EOF\n\
                    2: POSIX  ADVISORY  READ 77 fd:01:99 0 EOF\n\
                    garbage\n";
        let l = parse_locks(text);
        assert_eq!(l.len(), 2);
        assert_eq!(
            l[0],
            LockEntry {
                class: "FLOCK".into(),
                pid: 4242,
                major: 0,
                minor: 0x1b,
                inode: 678
            }
        );
        assert_eq!((l[1].major, l[1].minor, l[1].pid), (0xfd, 1, 77));
    }

    #[test]
    fn status_and_top_memory_from_a_fake_proc() {
        let root = std::env::temp_dir().join(format!("tasks-procs-{}", std::process::id()));
        let put = |pid: u32, name: &str, rss_kb: Option<u64>, swap_kb: u64, cmd: &str| {
            let dir = root.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let mut status = format!("Name:\t{name}\nState:\tS\n");
            if let Some(rss) = rss_kb {
                status.push_str(&format!("VmRSS:\t{rss} kB\nVmSwap:\t{swap_kb} kB\n"));
            }
            std::fs::write(dir.join("status"), status).unwrap();
            std::fs::write(dir.join("cmdline"), cmd.replace(' ', "\0")).unwrap();
        };
        put(10, "tasks", Some(1024 * 1024), 5 * 1024 * 1024, "tasks ui");
        put(
            11,
            "rustc",
            Some(2 * 1024 * 1024),
            0,
            "rustc --edition 2024",
        );
        put(12, "kthreadd", None, 0, "");
        put(13, "sh", Some(1000), 0, "sh");
        std::fs::create_dir_all(root.join("self")).unwrap();
        let top = top_memory(&root, 2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].pid, 10, "RSS + swap ranks the leaking UI first");
        assert_eq!(top[0].total(), 6 * 1024 * 1024 * 1024);
        assert_eq!(top[0].cmdline.as_deref(), Some("tasks ui"));
        assert!(
            top[0].line().contains("1.0 GiB RSS + 5.0 GiB swap"),
            "{}",
            top[0].line()
        );
        assert_eq!(top[1].pid, 11);
        assert_eq!(parse_status("Name:\tkworker\n"), None);
        assert_eq!(cmdline(&root, 12), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
