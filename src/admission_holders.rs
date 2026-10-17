//! Names the process that actually holds an admission lock when no live
//! holder note does (feedback lease-leak: a detached sccache server inherited
//! a finished job's compile lease and showed as "an untracked holder").
//!
//! Read-only: this never takes, releases, steals or replaces a lock, and never
//! signals the holder. A flock is held by an open file description, so it is
//! released only when every descriptor sharing it closes; tasks cannot release
//! it on another process's behalf without killing a process it does not own or
//! moving the lock to a new inode (which would break the stable-inode protocol
//! shared with legacy clients). Diagnosis is therefore the safe recovery aid.
//!
//! A holder is a process whose `/proc/PID/fdinfo/FD` for a descriptor on the
//! lock inode lists a `FLOCK` lock: the kernel prints only locks owned by THAT
//! open file description, so blocked or polling waiters (which merely have the
//! file open) are not reported. Only processes this user may inspect are seen.
use crate::metrics::HolderProcess;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const MAX_PROCS: usize = 32768;
const MAX_FDS: usize = 4096;
const MAX_HOLDERS: usize = 4;
const MAX_CMD: usize = 160;
const MAX_ENVIRON: usize = 256 * 1024;

/// Processes holding a flock on `lock` (by device and inode), bounded.
pub fn holders(lock: &Path) -> Vec<HolderProcess> {
    holders_in(Path::new("/proc"), lock)
}

fn holders_in(proc_root: &Path, lock: &Path) -> Vec<HolderProcess> {
    let Ok(target) = std::fs::metadata(lock) else {
        return vec![];
    };
    let want = (target.dev(), target.ino());
    let Some(name) = lock.file_name() else {
        return vec![];
    };
    let me = std::process::id();
    let mut out = vec![];
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return out;
    };
    for entry in entries.take(MAX_PROCS).flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let dir = entry.path();
        let Ok(fds) = std::fs::read_dir(dir.join("fd")) else {
            continue;
        };
        let holds = fds.take(MAX_FDS).flatten().any(|fd| {
            let path = fd.path();
            // Cheap name filter before the stat that confirms the inode.
            std::fs::read_link(&path).is_ok_and(|l| l.file_name() == Some(name))
                && std::fs::metadata(&path).is_ok_and(|m| (m.dev(), m.ino()) == want)
                && std::fs::read_to_string(dir.join("fdinfo").join(fd.file_name()))
                    .is_ok_and(|info| fdinfo_has_flock(&info))
        });
        if holds {
            out.push(describe(pid, &dir));
            if out.len() >= MAX_HOLDERS {
                break;
            }
        }
    }
    out.sort_by_key(|h| h.pid);
    out
}

/// `lock:\t1: FLOCK  ADVISORY  WRITE 2298 00:2a:1234 0 EOF` lines in fdinfo.
pub fn fdinfo_has_flock(info: &str) -> bool {
    info.lines()
        .filter_map(|l| l.strip_prefix("lock:"))
        .any(|l| l.split_whitespace().nth(1) == Some("FLOCK"))
}

/// Command line (NUL-separated argv joined by spaces, or `[comm]`), bounded.
pub fn cmdline_text(raw: &[u8], comm: &str) -> String {
    let text = raw
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ");
    let text = if text.is_empty() {
        format!("[{}]", comm.trim())
    } else {
        text
    };
    if text.chars().count() > MAX_CMD {
        let cut: String = text.chars().take(MAX_CMD - 1).collect();
        format!("{cut}…")
    } else {
        text
    }
}

/// The `TASKS_JOB_ID` a worker sets for every admitted command, if present
/// in a NUL-separated environ block. No other variable is read or kept.
pub fn tasks_job_in_environ(raw: &[u8]) -> Option<i64> {
    raw.split(|b| *b == 0)
        .find_map(|v| v.strip_prefix(b"TASKS_JOB_ID="))
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|v| v.parse().ok())
}

fn describe(pid: u32, dir: &Path) -> HolderProcess {
    let read = |name: &str, cap: usize| -> Vec<u8> {
        use std::io::Read;
        let mut buf = vec![];
        if let Ok(f) = std::fs::File::open(dir.join(name)) {
            let _ = f.take(cap as u64).read_to_end(&mut buf);
        }
        buf
    };
    let comm = String::from_utf8_lossy(&read("comm", 64)).into_owned();
    HolderProcess {
        pid,
        cmd: cmdline_text(&read("cmdline", 4096), &comm),
        tasks_job: tasks_job_in_environ(&read("environ", MAX_ENVIRON)),
    }
}

/// One-line description, e.g. `pid 2298 (sccache) from job #154`.
pub fn describe_text(h: &HolderProcess) -> String {
    match h.tasks_job {
        Some(j) => format!("pid {} ({}) from job #{j}", h.pid, h.cmd),
        None => format!("pid {} ({}), not started by a tasks job", h.pid, h.cmd),
    }
}

#[cfg(test)]
#[path = "admission_holders_tests.rs"]
pub(crate) mod tests;
