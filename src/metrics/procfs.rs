//! Pure /proc text parsers and the job process-tree rule. Reading lives in
//! `sampler`; everything here is testable with fixture text.
use std::collections::{HashMap, HashSet};

/// Aggregate `cpu ` line of /proc/stat, in clock ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

/// Parses the aggregate `cpu` line and counts `cpuN` lines (logical CPUs).
/// busy = total - idle - iowait; guest time is already inside user/nice.
pub fn parse_stat(text: &str) -> Option<(CpuTimes, u32)> {
    let mut times = None;
    let mut cpus = 0;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("cpu") => {
                let v: Vec<u64> = fields.take(8).filter_map(|f| f.parse().ok()).collect();
                if v.len() < 5 {
                    return None;
                }
                let total: u64 = v.iter().sum();
                let idle = v[3] + v[4];
                times = Some(CpuTimes {
                    busy: total.saturating_sub(idle),
                    total,
                });
            }
            Some(name) if name.starts_with("cpu") => cpus += 1,
            _ => (),
        }
    }
    Some((times?, cpus))
}

/// Busy cores between two aggregate samples.
pub fn busy_cores(before: CpuTimes, after: CpuTimes, logical_cpus: u32) -> Option<f64> {
    let total = after.total.checked_sub(before.total)?;
    let busy = after.busy.checked_sub(before.busy)?;
    if total == 0 {
        return None;
    }
    Some((busy as f64 / total as f64 * logical_cpus as f64).min(logical_cpus as f64))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut values: HashMap<&str, u64> = HashMap::new();
    for line in text.lines() {
        if let Some((name, rest)) = line.split_once(':')
            && let Some(kb) = rest.split_whitespace().next().and_then(|v| v.parse().ok())
        {
            values.insert(name, kb);
        }
    }
    let kib = |name: &str| values.get(name).map(|v| v * 1024);
    Some(MemInfo {
        total: kib("MemTotal")?,
        available: kib("MemAvailable")?,
        swap_total: kib("SwapTotal").unwrap_or(0),
        swap_free: kib("SwapFree").unwrap_or(0),
    })
}

/// One `/proc/PID/stat` record (the fields this module uses).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: u32,
    pub ppid: u32,
    pub pgrp: u32,
    pub session: u32,
    pub state: char,
    /// utime + stime of the process itself.
    pub cpu_ticks: u64,
    /// cutime + cstime: reaped (waited-for) descendants.
    pub child_ticks: u64,
    pub start_ticks: u64,
    pub rss_pages: u64,
    /// The kernel's short command name (`comm`, at most 15 bytes): a cheap filter
    /// before reading a member's cmdline for compile attribution.
    pub comm: String,
}

/// Parses `/proc/PID/stat`. comm may hold spaces and parentheses, so fields are
/// counted after the LAST ')': state is field 3 of the man page.
pub fn parse_proc_stat(text: &str) -> Option<ProcStat> {
    let (head, rest) = text.rsplit_once(')')?;
    let pid = head.split_whitespace().next()?.parse().ok()?;
    let comm = head
        .split_once('(')
        .map(|(_, c)| c.to_owned())
        .unwrap_or_default();
    let f: Vec<&str> = rest.split_whitespace().collect();
    let num = |i: usize| f.get(i).and_then(|v| v.parse::<i64>().ok());
    Some(ProcStat {
        pid,
        state: f.first()?.chars().next()?,
        ppid: num(1)? as u32,
        pgrp: num(2)? as u32,
        session: num(3)? as u32,
        cpu_ticks: (num(11)? + num(12)?).max(0) as u64,
        child_ticks: (num(13)? + num(14)?).max(0) as u64,
        start_ticks: num(19)? as u64,
        rss_pages: num(21)?.max(0) as u64,
        comm,
    })
}

/// The processes that belong to one job, given its stage supervisor (the
/// `tasks __stage` process the worker records in the ledger).
///
/// Rule: every descendant of the supervisor by parent pid, plus every process
/// in a process group of the supervisor or of one of those descendants. The
/// group term keeps processes that were orphaned (reparented to init or a
/// subreaper) but stayed in the job's admission process group. A process that
/// called `setsid`/`setpgid` AND is no longer a descendant (a double-forked
/// daemon such as an adb or sccache server) is excluded: it outlives the job
/// and serves other jobs, so its CPU/memory is not this job's.
pub fn job_members(procs: &[ProcStat], supervisor: u32) -> Vec<usize> {
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut by_pid = HashMap::new();
    for (i, p) in procs.iter().enumerate() {
        children.entry(p.ppid).or_default().push(i);
        by_pid.insert(p.pid, i);
    }
    let Some(&root) = by_pid.get(&supervisor) else {
        return vec![];
    };
    let mut members: HashSet<usize> = HashSet::from([root]);
    let mut stack = vec![supervisor];
    while let Some(pid) = stack.pop() {
        for &child in children.get(&pid).map(Vec::as_slice).unwrap_or(&[]) {
            if members.insert(child) {
                stack.push(procs[child].pid);
            }
        }
    }
    let groups: HashSet<u32> = members.iter().map(|&i| procs[i].pgrp).collect();
    for (i, p) in procs.iter().enumerate() {
        // Pid 0/1 groups would only arise from a malformed record; never adopt them.
        if p.pgrp > 1 && groups.contains(&p.pgrp) {
            members.insert(i);
        }
    }
    let mut out: Vec<usize> = members.into_iter().collect();
    out.sort_unstable();
    out
}

/// Whether `meta`'s file is flocked, per `held` (from [`parse_flocks`]). /proc/locks names a
/// file by its superblock's device, but `stat` on a btrfs subvolume reports the subvolume's own
/// anonymous device, so the pair never matches there. For such a file (device major 0) the
/// inode number alone is compared against the other major-0 entries.
pub fn flock_held(held: &HashSet<(u32, u32, u64)>, meta: &std::fs::Metadata) -> bool {
    let (major, minor, inode) = task_manager_platform::fs::file_key(meta);
    held.contains(&(major, minor, inode))
        || (major == 0 && held.iter().any(|&(m, _, i)| m == 0 && i == inode))
}

/// FLOCK entries of /proc/locks as (major, minor, inode). Blocked waiters
/// (`->` lines) are not holders and are skipped.
pub fn parse_flocks(text: &str) -> HashSet<(u32, u32, u64)> {
    let mut held = HashSet::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.get(1) == Some(&"->") || fields.get(1) != Some(&"FLOCK") {
            continue;
        }
        let Some(id) = fields.get(5) else { continue };
        let mut parts = id.split(':');
        let (Some(major), Some(minor), Some(inode)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let (Ok(major), Ok(minor), Ok(inode)) = (
            u32::from_str_radix(major, 16),
            u32::from_str_radix(minor, 16),
            inode.parse(),
        ) {
            held.insert((major, minor, inode));
        }
    }
    held
}
