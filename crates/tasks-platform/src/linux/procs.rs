//! `/proc/*/stat` parsing and process-group accounting.

use crate::{GroupId, PlatformError, Usage, groups};
use rustix::param::{clock_ticks_per_second, page_size};

/// The fields of `/proc/PID/stat` that `tasks` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcStat {
    /// Process id.
    pub pid: i32,
    /// State letter (`R`, `S`, `Z`, ...).
    pub state: char,
    /// Process group id.
    pub pgrp: i32,
    /// User CPU time in clock ticks.
    pub utime: u64,
    /// System CPU time in clock ticks.
    pub stime: u64,
    /// Start time in clock ticks since boot (the PID-reuse token).
    pub start_ticks: u64,
    /// Resident set size in pages.
    pub rss_pages: u64,
}

impl ProcStat {
    /// Zombies and dead processes are not alive (R-077).
    pub fn is_alive(&self) -> bool {
        !matches!(self.state, 'Z' | 'X' | 'x')
    }
}

/// Parses one `/proc/PID/stat` line. `comm` may contain spaces, parentheses
/// and newlines, so the split happens at the LAST `)` (R-077).
pub fn parse_proc_stat(text: &str) -> Result<ProcStat, PlatformError> {
    let bad = |d: &str| PlatformError::parse("/proc/PID/stat", d.to_string());
    let (head, tail) = text
        .rsplit_once(')')
        .ok_or_else(|| bad("no closing paren"))?;
    let pid = head
        .split_once('(')
        .and_then(|(p, _)| p.trim().parse::<i32>().ok())
        .ok_or_else(|| bad("bad pid"))?;
    let f: Vec<&str> = tail.split_whitespace().collect();
    let num = |i: usize, what: &str| -> Result<u64, PlatformError> {
        f.get(i)
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| bad(what))
    };
    // After comm: state=0 ppid=1 pgrp=2 ... utime=11 stime=12 ... starttime=19 vsize=20 rss=21
    Ok(ProcStat {
        pid,
        state: f
            .first()
            .and_then(|s| s.chars().next())
            .ok_or_else(|| bad("missing state"))?,
        pgrp: f
            .get(2)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| bad("bad pgrp"))?,
        utime: num(11, "bad utime")?,
        stime: num(12, "bad stime")?,
        start_ticks: num(19, "bad starttime")?,
        rss_pages: f
            .get(21)
            .and_then(|v| v.parse::<i64>().ok())
            .map_or(Err(bad("bad rss")), |v| Ok(u64::try_from(v).unwrap_or(0)))?,
    })
}

fn vanished(e: &std::io::Error) -> bool {
    // ENOENT, or ESRCH when the process exits mid-read.
    e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(3)
}

fn read_stat(pid: i32) -> Result<Option<ProcStat>, PlatformError> {
    let path = format!("/proc/{pid}/stat");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_proc_stat(&text).map(Some),
        Err(e) if vanished(&e) => Ok(None),
        Err(e) => Err(PlatformError::io(path, e)),
    }
}

/// Start time token of a process; `None` when it does not exist.
pub(super) fn start_time(pid: i32) -> Result<Option<u64>, PlatformError> {
    Ok(read_stat(pid)?.map(|s| s.start_ticks))
}

/// Sums live processes whose pgrp equals `pgid` (pure over parsed stats).
pub(super) fn sum_group(stats: &[ProcStat], pgid: i32, ticks: u64, page: u64) -> Usage {
    let mut u = Usage {
        cpu_user_s: 0.0,
        cpu_sys_s: 0.0,
        rss_bytes: 0,
        processes: 0,
    };
    let ticks = ticks.max(1) as f64;
    for s in stats.iter().filter(|s| s.pgrp == pgid && s.is_alive()) {
        u.cpu_user_s += s.utime as f64 / ticks;
        u.cpu_sys_s += s.stime as f64 / ticks;
        u.rss_bytes = u.rss_bytes.saturating_add(s.rss_pages.saturating_mul(page));
        u.processes += 1;
    }
    u
}

/// Usage of the group, after verifying the leader's identity (R-075, R-096).
pub(super) fn usage(group: GroupId) -> Result<Usage, PlatformError> {
    groups::validate_target(group.pgid)?;
    groups::check_leader(group, &start_time)?;
    let dir = std::fs::read_dir("/proc").map_err(|e| PlatformError::io("/proc", e))?;
    let mut stats = Vec::new();
    for entry in dir {
        let entry = entry.map_err(|e| PlatformError::io("/proc", e))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if let Some(s) = read_stat(pid)? {
            stats.push(s);
        }
    }
    Ok(sum_group(
        &stats,
        group.pgid,
        clock_ticks_per_second(),
        page_size() as u64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(
        pid: i32,
        comm: &str,
        state: char,
        pgrp: i32,
        ut: u64,
        st: u64,
        start: u64,
        rss: i64,
    ) -> String {
        format!(
            "{pid} ({comm}) {state} 1 {pgrp} {pgrp} 0 -1 4194560 100 0 0 0 {ut} {st} 0 0 20 0 1 0 {start} 1000000 {rss} 18446744073709551615 0 0\n"
        )
    }

    // R-077
    #[test]
    fn parser_survives_hostile_comm() {
        let s = parse_proc_stat(&line(42, "a b) (c) d", 'S', 40, 7, 3, 999, 12)).unwrap();
        assert_eq!(
            (s.pid, s.pgrp, s.utime, s.stime, s.start_ticks, s.rss_pages),
            (42, 40, 7, 3, 999, 12)
        );
        assert!(s.is_alive());
        let multi = line(5, "x\ny", 'R', 5, 0, 0, 1, 0);
        assert_eq!(parse_proc_stat(&multi).unwrap().pid, 5);
    }

    // R-077
    #[test]
    fn zombie_is_not_alive() {
        let s = parse_proc_stat(&line(9, "z", 'Z', 9, 1, 1, 5, 0)).unwrap();
        assert!(!s.is_alive());
    }

    #[test]
    fn truncated_stat_is_an_error() {
        assert!(parse_proc_stat("").is_err());
        assert!(parse_proc_stat("1 (a) S 1 1").is_err());
        assert!(parse_proc_stat("x (a) S 1 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0").is_err());
    }

    // R-096
    #[test]
    fn group_sum_counts_only_live_members_of_the_group() {
        let stats: Vec<ProcStat> = [
            line(10, "leader", 'S', 10, 100, 50, 1, 10),
            line(11, "kid", 'R', 10, 200, 0, 2, 5),
            line(12, "dead", 'Z', 10, 999, 999, 3, 99),
            line(20, "other", 'S', 20, 5, 5, 4, 1),
        ]
        .iter()
        .map(|l| parse_proc_stat(l).unwrap())
        .collect();
        let u = sum_group(&stats, 10, 100, 4096);
        assert_eq!(u.processes, 2);
        assert_eq!(u.cpu_user_s, 3.0);
        assert_eq!(u.cpu_sys_s, 0.5);
        assert_eq!(u.rss_bytes, 15 * 4096);
        assert_eq!(sum_group(&stats, 99, 100, 4096).processes, 0);
    }

    #[test]
    fn host_self_stat_parses() {
        let pid = std::process::id() as i32;
        let s = read_stat(pid).unwrap().unwrap();
        assert_eq!(s.pid, pid);
        assert!(s.start_ticks > 0);
    }
}
