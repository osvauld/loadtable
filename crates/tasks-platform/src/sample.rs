//! Process-table sampling shared by both operating systems: the types, the per-job process-tree
//! meter that backs the fallback when there is no cgroup, and the sampler that finds the biggest
//! disk and memory users outside every job.
//!
//! Only a program name is ever kept for a process: never its arguments or environment.

use crate::PlatformError;
use std::collections::{BTreeMap, BTreeSet};
use tasks_core::{Attribution, JobFigures};

/// How many processes a window report lists per ranking.
pub const TOP_N: usize = 10;
/// Longest program name kept, in characters.
const MAX_NAME: usize = 48;

/// One process at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    /// Process id.
    pub pid: i32,
    /// Parent process id.
    pub ppid: i32,
    /// Process group id.
    pub pgid: i32,
    /// Opaque start time; (pid, start) identifies a process even after pid reuse.
    pub start: u64,
    /// The program's base name (see [`clean_program_name`]).
    pub name: String,
    /// Resident memory in bytes.
    pub rss_bytes: u64,
    /// CPU time (user + system) in microseconds.
    pub cpu_usec: u64,
    /// Bytes this process has read from storage since it started, when the OS lets us see it
    /// (Linux: only processes of the same user).
    pub io_read: Option<u64>,
    /// Bytes written to storage since start.
    pub io_write: Option<u64>,
}

/// Lists the processes of the machine.
pub trait ProcessTable: Send + Sync {
    /// All processes visible to us, zombies excluded.
    fn snapshot(&self) -> Result<Vec<ProcInfo>, PlatformError>;

    /// Storage bytes `(read, written)` one process has moved since it started; `None` when it is
    /// gone or the OS hides them. The default scans [`ProcessTable::snapshot`]; Linux reads the
    /// one file.
    fn io_of(&self, pid: i32) -> Option<(u64, u64)> {
        let rows = self.snapshot().ok()?;
        let p = rows.iter().find(|p| p.pid == pid)?;
        Some((p.io_read?, p.io_write?))
    }
}

/// Reduces a program path or command name to a short, printable base name. Arguments are never
/// passed in; callers hand only the first word of the command line.
pub fn clean_program_name(raw: &str) -> String {
    let base = raw.rsplit('/').next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    if cleaned.is_empty() {
        "?".to_string()
    } else {
        cleaned
    }
}

/// The pids of the job's process tree: everyone in its process group plus every descendant of the
/// leader (children that called `setsid` left the group but are still its descendants).
pub fn tree_pids(table: &[ProcInfo], leader: i32, pgid: i32) -> BTreeSet<i32> {
    let mut members: BTreeSet<i32> = table
        .iter()
        .filter(|p| p.pgid == pgid)
        .map(|p| p.pid)
        .collect();
    members.insert(leader);
    // grow by parent links until no process is added
    loop {
        let before = members.len();
        for p in table {
            if members.contains(&p.ppid) {
                members.insert(p.pid);
            }
        }
        if members.len() == before {
            return members;
        }
    }
}

/// Sums a job's process tree over repeated samples. Bytes are kept per (pid, start) at their
/// highest value seen, so a process that exited between two samples still counts what it had
/// done by the last sample that saw it; what it did after that is lost (this is the
/// approximation the `process_tree` quality names).
#[derive(Debug, Default)]
pub struct TreeMeter {
    seen: BTreeMap<(i32, u64), (u64, u64)>,
    peak_rss: u64,
    /// Summed RSS of the latest sample.
    last_rss: u64,
    any_io: bool,
}

impl TreeMeter {
    /// An empty meter.
    pub fn new() -> TreeMeter {
        TreeMeter::default()
    }

    /// Folds in one sample of the job's tree.
    pub fn observe(&mut self, members: &[&ProcInfo]) {
        let mut rss = 0u64;
        for p in members {
            rss = rss.saturating_add(p.rss_bytes);
            if p.io_read.is_some() || p.io_write.is_some() {
                self.any_io = true;
            }
            let e = self.seen.entry((p.pid, p.start)).or_insert((0, 0));
            e.0 = e.0.max(p.io_read.unwrap_or(0));
            e.1 = e.1.max(p.io_write.unwrap_or(0));
        }
        self.peak_rss = self.peak_rss.max(rss);
        self.last_rss = rss;
    }

    /// The figures so far. CPU comes from the caller (the runner's usage sample), because exited
    /// processes drop out of the table.
    pub fn figures(&self, cpu_usec: Option<u64>) -> JobFigures {
        let mut f = JobFigures::empty(Attribution::ProcessTree);
        if self.any_io {
            f.io_read_bytes = Some(self.seen.values().map(|v| v.0).sum());
            f.io_write_bytes = Some(self.seen.values().map(|v| v.1).sum());
        }
        if self.peak_rss > 0 {
            f.mem_peak_bytes = Some(self.peak_rss);
        }
        if self.last_rss > 0 {
            f.mem_now_bytes = Some(self.last_rss);
        }
        f.cpu_usec = cpu_usec;
        f
    }
}

/// A process in a window report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopProc {
    /// Process id.
    pub pid: i32,
    /// Program base name.
    pub name: String,
    /// Bytes it read plus wrote during the window (a process born in the window counts all it
    /// did since it started).
    pub io_bytes: u64,
    /// Resident memory at the end of the window.
    pub rss_bytes: u64,
}

/// The biggest users outside every job over the last sample window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TopOutside {
    /// Window length in milliseconds; 0 for the first sample, which has nothing to compare to.
    pub window_ms: u64,
    /// By disk bytes in the window, biggest first, at most [`TOP_N`].
    pub by_io: Vec<TopProc>,
    /// By resident memory, biggest first, at most [`TOP_N`].
    pub by_rss: Vec<TopProc>,
    /// Processes whose I/O we could not read (other users' processes on Linux).
    pub unreadable: usize,
}

/// Keeps the previous sample and reports what changed since.
#[derive(Debug, Default)]
pub struct OutsideSampler {
    prev: BTreeMap<(i32, u64), u64>,
    prev_at_ms: Option<i64>,
}

impl OutsideSampler {
    /// A sampler that has seen nothing.
    pub fn new() -> OutsideSampler {
        OutsideSampler::default()
    }

    /// Takes a sample at `now_ms`. Processes in `in_jobs` (the pids of every running job's tree)
    /// are left out of the report but still remembered.
    pub fn sample(
        &mut self,
        now_ms: i64,
        table: &[ProcInfo],
        in_jobs: &BTreeSet<i32>,
    ) -> TopOutside {
        let had_prev = self.prev_at_ms.is_some();
        let window_ms = self
            .prev_at_ms
            .map_or(0, |t| u64::try_from(now_ms.saturating_sub(t)).unwrap_or(0));
        let mut next = BTreeMap::new();
        let mut rows: Vec<TopProc> = Vec::new();
        let mut unreadable = 0;
        for p in table {
            let total = match (p.io_read, p.io_write) {
                (None, None) => None,
                (r, w) => Some(r.unwrap_or(0).saturating_add(w.unwrap_or(0))),
            };
            if let Some(t) = total {
                next.insert((p.pid, p.start), t);
            }
            if in_jobs.contains(&p.pid) {
                continue;
            }
            let io_bytes = match (total, had_prev) {
                (Some(t), true) => match self.prev.get(&(p.pid, p.start)) {
                    Some(before) => t.saturating_sub(*before),
                    // new in this window: everything it did happened inside it
                    None => t,
                },
                (Some(_), false) => 0,
                (None, _) => {
                    unreadable += 1;
                    0
                }
            };
            rows.push(TopProc {
                pid: p.pid,
                name: p.name.clone(),
                io_bytes,
                rss_bytes: p.rss_bytes,
            });
        }
        self.prev = next;
        self.prev_at_ms = Some(now_ms);
        let mut by_io: Vec<TopProc> = rows.iter().filter(|r| r.io_bytes > 0).cloned().collect();
        by_io.sort_by(|a, b| b.io_bytes.cmp(&a.io_bytes).then(a.pid.cmp(&b.pid)));
        by_io.truncate(TOP_N);
        rows.sort_by(|a, b| b.rss_bytes.cmp(&a.rss_bytes).then(a.pid.cmp(&b.pid)));
        rows.truncate(TOP_N);
        TopOutside {
            window_ms,
            by_io,
            by_rss: rows,
            unreadable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, pgid: i32, name: &str, rss: u64, io: Option<(u64, u64)>) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            pgid,
            start: 1,
            name: name.into(),
            rss_bytes: rss,
            cpu_usec: 0,
            io_read: io.map(|x| x.0),
            io_write: io.map(|x| x.1),
        }
    }

    #[test]
    fn program_names_are_base_names_without_control_characters() {
        assert_eq!(clean_program_name("/usr/lib/firefox/firefox"), "firefox");
        assert_eq!(clean_program_name("a\nb"), "ab");
        assert_eq!(clean_program_name(""), "?");
        assert_eq!(clean_program_name(&"x".repeat(200)).chars().count(), 48);
    }

    #[test]
    fn the_tree_includes_the_group_and_setsid_descendants() {
        let table = [
            p(10, 1, 10, "sh", 0, None),
            p(11, 10, 10, "cc", 0, None),
            p(12, 11, 12, "escaped", 0, None), // setsid: own group, still a descendant
            p(20, 1, 20, "other", 0, None),
        ];
        let t = tree_pids(&table, 10, 10);
        assert_eq!(t.into_iter().collect::<Vec<_>>(), [10, 11, 12]);
    }

    // R-327
    #[test]
    fn r327_the_tree_meter_keeps_what_exited_processes_had_done() {
        let mut m = TreeMeter::new();
        let a = p(10, 1, 10, "sh", 100, Some((10, 20)));
        let b = p(11, 10, 10, "cc", 300, Some((1000, 5000)));
        m.observe(&[&a, &b]);
        // b exits; a grows
        let a2 = p(10, 1, 10, "sh", 150, Some((15, 25)));
        m.observe(&[&a2]);
        let f = m.figures(Some(2_000_000));
        assert_eq!(f.quality, Attribution::ProcessTree);
        assert_eq!(f.io_read_bytes, Some(1015));
        assert_eq!(f.io_write_bytes, Some(5025));
        assert_eq!(f.mem_peak_bytes, Some(400));
        // the level now is the latest sample's, not the peak
        assert_eq!(f.mem_now_bytes, Some(150));
        assert_eq!(f.cpu_usec, Some(2_000_000));
        // never an exact-looking number for what the OS did not give
        assert_eq!(f.io_some_us, None);
    }

    // R-327
    #[test]
    fn r327_without_per_process_io_the_bytes_stay_unmeasured() {
        let mut m = TreeMeter::new();
        m.observe(&[&p(10, 1, 10, "sh", 5, None)]);
        let f = m.figures(None);
        assert_eq!((f.io_read_bytes, f.io_write_bytes), (None, None));
        assert_eq!(f.mem_peak_bytes, Some(5));
    }

    // R-330
    #[test]
    fn r330_outside_processes_are_ranked_by_window_io_and_rss_and_jobs_are_excluded() {
        let mut s = OutsideSampler::new();
        let t0 = [
            p(1, 0, 1, "firefox", 900, Some((100, 100))),
            p(2, 0, 2, "tasks-ui", 50, Some((0, 0))),
            p(3, 0, 3, "cc", 10, Some((0, 0))),
        ];
        let first = s.sample(1_000, &t0, &BTreeSet::from([3]));
        assert_eq!(first.window_ms, 0);
        assert!(first.by_io.is_empty());
        let t1 = [
            p(1, 0, 1, "firefox", 950, Some((100, 300))),
            p(2, 0, 2, "tasks-ui", 60, Some((0, 30_000_000_000))),
            p(3, 0, 3, "cc", 10, Some((0, 99_000_000_000))),
            p(4, 0, 4, "new", 5, Some((7, 0))),
            p(5, 0, 5, "root-owned", 700, None),
        ];
        let top = s.sample(6_000, &t1, &BTreeSet::from([3]));
        assert_eq!(top.window_ms, 5_000);
        let names: Vec<_> = top.by_io.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            ["tasks-ui", "firefox", "new"],
            "job process 3 is excluded"
        );
        assert_eq!(top.by_io[0].io_bytes, 30_000_000_000);
        assert_eq!(top.by_io[1].io_bytes, 200);
        let rss: Vec<_> = top.by_rss.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(rss, ["firefox", "root-owned", "tasks-ui", "new"]);
        assert_eq!(top.unreadable, 1);
    }

    #[test]
    fn rankings_stop_at_ten() {
        let mut s = OutsideSampler::new();
        let table: Vec<ProcInfo> = (1..=30)
            .map(|i| p(i, 0, i, "x", u64::try_from(i).unwrap(), None))
            .collect();
        let top = s.sample(1, &table, &BTreeSet::new());
        assert_eq!(top.by_rss.len(), TOP_N);
        assert_eq!(top.by_rss[0].pid, 30);
    }
}
