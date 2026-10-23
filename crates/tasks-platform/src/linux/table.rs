//! The Linux process table: `/proc/PID/{stat,io,cmdline,comm}`.

use super::procs::read_stat;
use crate::sample::clean_program_name;
use crate::{PlatformError, ProcInfo, ProcessTable};
use rustix::param::{clock_ticks_per_second, page_size};
use std::io::Read;

/// Parses `/proc/PID/io`; the storage-layer `read_bytes` and `write_bytes` lines are what reached
/// a disk (`rchar`/`wchar` include page-cache hits and pipes).
pub fn parse_proc_io(text: &str) -> Option<(u64, u64)> {
    let mut read = None;
    let mut write = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("read_bytes:") {
            read = v.trim().parse::<u64>().ok();
        } else if let Some(v) = line.strip_prefix("write_bytes:") {
            write = v.trim().parse::<u64>().ok();
        }
    }
    Some((read?, write?))
}

/// The program name: the base name of the first word of the command line, else `comm`. Only
/// the first word is read; the rest of the command line is never kept.
fn program_name(pid: i32) -> String {
    let first = std::fs::File::open(format!("/proc/{pid}/cmdline"))
        .ok()
        .and_then(|f| {
            let mut buf = Vec::new();
            f.take(512).read_to_end(&mut buf).ok()?;
            let word = buf.split(|b| *b == 0).next().unwrap_or_default().to_vec();
            String::from_utf8(word).ok().filter(|w| !w.is_empty())
        });
    match first {
        Some(w) => clean_program_name(&w),
        None => clean_program_name(
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .unwrap_or_default()
                .trim(),
        ),
    }
}

/// Reads the live process table.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcFsTable;

impl ProcessTable for ProcFsTable {
    fn io_of(&self, pid: i32) -> Option<(u64, u64)> {
        std::fs::read_to_string(format!("/proc/{pid}/io"))
            .ok()
            .and_then(|t| parse_proc_io(&t))
    }

    fn snapshot(&self) -> Result<Vec<ProcInfo>, PlatformError> {
        let dir = std::fs::read_dir("/proc").map_err(|e| PlatformError::io("/proc", e))?;
        let ticks = clock_ticks_per_second().max(1);
        let page = page_size() as u64;
        let mut out = Vec::new();
        for entry in dir.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
            else {
                continue;
            };
            // a process that exits mid-read simply drops out
            let Ok(Some(stat)) = read_stat(pid) else {
                continue;
            };
            if !stat.is_alive() {
                continue;
            }
            let io = std::fs::read_to_string(format!("/proc/{pid}/io"))
                .ok()
                .and_then(|t| parse_proc_io(&t));
            out.push(ProcInfo {
                pid,
                ppid: stat.ppid,
                pgid: stat.pgrp,
                start: stat.start_ticks,
                name: program_name(pid),
                rss_bytes: stat.rss_pages.saturating_mul(page),
                cpu_usec: (stat.utime + stat.stime).saturating_mul(1_000_000) / ticks,
                io_read: io.map(|x| x.0),
                io_write: io.map(|x| x.1),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_file_gives_storage_bytes_not_chars() {
        let text = "rchar: 9999\nwchar: 8888\nsyscr: 1\nsyscw: 2\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";
        assert_eq!(parse_proc_io(text), Some((4096, 8192)));
        assert_eq!(parse_proc_io("rchar: 1\n"), None);
    }

    // R-330: only the first word of the command line is ever read, never its arguments.
    #[test]
    fn r330_the_table_keeps_a_program_name_and_never_the_arguments() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 5; : SECRET_ARGUMENT_9f3 --token=hunter2"])
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let t = ProcFsTable.snapshot().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        let row = t.iter().find(|p| p.pid == pid).unwrap();
        assert_eq!(row.name, "sh");
        let dump = format!("{row:?}");
        assert!(
            !dump.contains("SECRET_ARGUMENT") && !dump.contains("hunter2"),
            "{dump}"
        );
    }

    #[test]
    fn the_live_table_lists_this_process_with_a_program_name_only() {
        let me = i32::try_from(std::process::id()).unwrap();
        let t = ProcFsTable.snapshot().unwrap();
        let mine = t.iter().find(|p| p.pid == me).unwrap();
        assert!(!mine.name.is_empty() && !mine.name.contains(' ') && !mine.name.contains('/'));
        assert!(mine.rss_bytes > 0);
        // our own io file is always readable
        assert!(mine.io_read.is_some() && mine.io_write.is_some());
    }
}
