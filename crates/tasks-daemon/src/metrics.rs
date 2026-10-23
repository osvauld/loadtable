//! Live metrics of running jobs: a bounded in-memory sample history per job, the job's live
//! processes grouped by program, and the request that serves both. Nothing here touches the
//! ledger; the history is dropped a few minutes after the job ends.
//!
//! Samples come from the runner's own two-second figures (no second poller). Each tick also
//! takes one process-table snapshot of the job's members so the groups can show CPU rates.

use crate::scheduler::{RunningJob, Scheduler, lock};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use tasks_core::metrics::{JobMetricsView, MAX_SAMPLES, MetricSample, ProcGroup, rates};
use tasks_core::{Attribution, JobFigures, JobId};
use tasks_platform::{CgroupName, ProcInfo, tree_pids};

/// How long a finished job's history stays readable.
pub(crate) const KEEP_AFTER_END_MS: i64 = 5 * 60 * 1000;
/// Program groups listed before the rest are summed into an `others` row.
pub(crate) const MAX_GROUPS: usize = 12;

/// One live process, names only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcRow {
    pub pid: i32,
    /// Distinguishes a reused pid.
    pub start: u64,
    pub name: String,
    pub rss_bytes: u64,
    pub cpu_usec: u64,
}

/// The job's processes at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ProcSnap {
    pub at_ms: i64,
    pub rows: Vec<ProcRow>,
}

impl ProcSnap {
    pub(crate) fn of(at_ms: i64, members: &[&ProcInfo]) -> ProcSnap {
        ProcSnap {
            at_ms,
            rows: members
                .iter()
                .map(|p| ProcRow {
                    pid: p.pid,
                    start: p.start,
                    name: p.name.clone(),
                    rss_bytes: p.rss_bytes,
                    cpu_usec: p.cpu_usec,
                })
                .collect(),
        }
    }
}

/// Groups `cur` by program name with CPU rates against `prev`. A process is matched across the
/// two snapshots by (pid, start), so a reused pid is a new process; a process new since `prev`
/// counts all its CPU; a counter that went backwards counts nothing. Without `prev` the CPU is
/// unknown. Sorted by cpu, then memory, then name; groups past [`MAX_GROUPS`] are summed.
pub(crate) fn group_procs(prev: Option<&ProcSnap>, cur: &ProcSnap) -> Vec<ProcGroup> {
    let dt_ms = prev.map_or(0, |p| cur.at_ms.saturating_sub(p.at_ms));
    let before: BTreeMap<(i32, u64), u64> = prev
        .map(|p| {
            p.rows
                .iter()
                .map(|r| ((r.pid, r.start), r.cpu_usec))
                .collect()
        })
        .unwrap_or_default();
    // name -> (count, cpu delta usec, rss)
    let mut by_name: BTreeMap<&str, (u32, u64, u64)> = BTreeMap::new();
    for r in &cur.rows {
        let delta = match before.get(&(r.pid, r.start)) {
            Some(old) => r.cpu_usec.saturating_sub(*old),
            None => r.cpu_usec,
        };
        let e = by_name.entry(r.name.as_str()).or_insert((0, 0, 0));
        e.0 = e.0.saturating_add(1);
        e.1 = e.1.saturating_add(delta);
        e.2 = e.2.saturating_add(r.rss_bytes);
    }
    let known = dt_ms > 0;
    let cores = |usec: u64| known.then(|| usec as f64 / (dt_ms as f64 * 1000.0));
    let mut groups: Vec<(u64, ProcGroup)> = by_name
        .into_iter()
        .map(|(name, (count, usec, rss))| {
            (
                usec,
                ProcGroup {
                    name: name.to_string(),
                    count,
                    cpu_cores: cores(usec),
                    rss_bytes: rss,
                    others: false,
                },
            )
        })
        .collect();
    groups.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.rss_bytes.cmp(&a.1.rss_bytes))
            .then(a.1.name.cmp(&b.1.name))
    });
    let rest = groups.split_off(groups.len().min(MAX_GROUPS));
    let mut out: Vec<ProcGroup> = groups.into_iter().map(|g| g.1).collect();
    if !rest.is_empty() {
        let usec = rest.iter().fold(0u64, |a, g| a.saturating_add(g.0));
        out.push(ProcGroup {
            name: format!("{} other programs", rest.len()),
            count: rest.iter().fold(0u32, |a, g| a.saturating_add(g.1.count)),
            cpu_cores: cores(usec),
            rss_bytes: rest
                .iter()
                .fold(0u64, |a, g| a.saturating_add(g.1.rss_bytes)),
            others: true,
        });
    }
    out
}

/// What the figures of one tick say, as a sample.
pub(crate) fn sample_of(at_ms: i64, f: &JobFigures) -> MetricSample {
    MetricSample {
        t_ms: at_ms,
        cpu_usec: f.cpu_usec,
        mem_bytes: f.mem_now_bytes.or(f.mem_peak_bytes),
        io_read_bytes: f.io_read_bytes,
        io_write_bytes: f.io_write_bytes,
        io_some_us: f.io_some_us,
    }
}

struct JobHistory {
    quality: Option<Attribution>,
    samples: VecDeque<MetricSample>,
    prev: Option<ProcSnap>,
    cur: Option<ProcSnap>,
    ended_ms: Option<i64>,
}

/// The sample histories of the jobs this daemon runs or ran a moment ago.
#[derive(Default)]
pub(crate) struct MetricsStore {
    jobs: BTreeMap<JobId, JobHistory>,
}

/// The newest reading of a job: its level and rates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Latest {
    pub cores: Option<f64>,
    pub mem_bytes: Option<u64>,
}

impl MetricsStore {
    /// Adds one tick; keeps the newest [`MAX_SAMPLES`]. `snap` is `None` when the process table
    /// could not be read (the last good snapshots stay).
    pub(crate) fn record(
        &mut self,
        id: JobId,
        now_ms: i64,
        quality: Attribution,
        sample: MetricSample,
        snap: Option<ProcSnap>,
    ) {
        self.expire(now_ms);
        let h = self.jobs.entry(id).or_insert_with(|| JobHistory {
            quality: None,
            samples: VecDeque::new(),
            prev: None,
            cur: None,
            ended_ms: None,
        });
        h.quality = Some(quality);
        h.ended_ms = None;
        if h.samples.len() >= MAX_SAMPLES {
            h.samples.pop_front();
        }
        h.samples.push_back(sample);
        if let Some(s) = snap {
            h.prev = h.cur.take();
            h.cur = Some(s);
        }
    }

    /// The job ended: keep its samples for [`KEEP_AFTER_END_MS`], drop the live processes.
    pub(crate) fn end(&mut self, id: JobId, now_ms: i64) {
        if let Some(h) = self.jobs.get_mut(&id) {
            h.ended_ms = Some(now_ms);
            h.prev = None;
            h.cur = None;
        }
        self.expire(now_ms);
    }

    pub(crate) fn expire(&mut self, now_ms: i64) {
        self.jobs.retain(|_, h| {
            h.ended_ms
                .is_none_or(|e| now_ms.saturating_sub(e) < KEEP_AFTER_END_MS)
        });
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.jobs.len()
    }

    /// The held history of `id`, if any.
    pub(crate) fn view(&mut self, id: JobId, now_ms: i64) -> Option<JobMetricsView> {
        self.expire(now_ms);
        let h = self.jobs.get(&id)?;
        Some(JobMetricsView {
            quality: h.quality,
            samples: h.samples.iter().copied().collect(),
            procs: h
                .cur
                .as_ref()
                .map(|c| group_procs(h.prev.as_ref(), c))
                .unwrap_or_default(),
            usual_peak_mem_bytes: None,
            note: h
                .ended_ms
                .map(|_| "the job has ended; these are its last samples".to_string()),
        })
    }

    /// Current cores and memory of a running job.
    pub(crate) fn latest(&self, id: JobId) -> Option<Latest> {
        let h = self.jobs.get(&id).filter(|h| h.ended_ms.is_none())?;
        let last = h.samples.back()?;
        let cores = h
            .samples
            .iter()
            .rev()
            .nth(1)
            .and_then(|p| rates(p, last).cores);
        Some(Latest {
            cores,
            mem_bytes: last.mem_bytes,
        })
    }
}

impl Scheduler {
    /// Records one sample of a running job from the runner's figures.
    pub(crate) fn record_metrics(&self, rj: &RunningJob, f: &JobFigures) {
        let now = self.clock.now_ms();
        let snap = self.job_procs(rj, f, now);
        lock(&self.metrics).record(rj.id, now, f.quality, sample_of(now, f), snap);
    }

    /// The job ended; its history is kept for a few minutes.
    pub(crate) fn end_metrics(&self, id: JobId) {
        lock(&self.metrics).end(id, self.clock.now_ms());
    }

    /// The job's live processes: the cgroup's exact members, else its process tree.
    fn job_procs(&self, rj: &RunningJob, f: &JobFigures, now: i64) -> Option<ProcSnap> {
        let table = self.platform.table.snapshot().ok()?;
        let members: BTreeSet<i32> = match f.quality {
            Attribution::Cgroup => self
                .platform
                .cgroups
                .pids(CgroupName::Job(rj.id.0))
                .ok()?
                .into_iter()
                .collect(),
            Attribution::ProcessTree => {
                let pgid = lock(&rj.group).as_ref()?.pgid;
                tree_pids(&table, pgid, pgid)
            }
        };
        let rows: Vec<&ProcInfo> = table.iter().filter(|p| members.contains(&p.pid)).collect();
        Some(ProcSnap::of(now, &rows))
    }

    /// Answers `Request::JobMetrics`.
    pub(crate) fn job_metrics(&self, rec: &tasks_ledger::JobRecord) -> JobMetricsView {
        let now = self.clock.now_ms();
        let held = lock(&self.metrics).view(rec.id, now);
        let mut v = held.unwrap_or_else(|| JobMetricsView {
            quality: None,
            samples: Vec::new(),
            procs: Vec::new(),
            usual_peak_mem_bytes: None,
            note: Some(if rec.phase.is_terminal() {
                "the job has finished and its samples are no longer held".to_string()
            } else {
                "the job has not produced samples on this machine yet".to_string()
            }),
        });
        v.usual_peak_mem_bytes = self.usual_peak_mem(rec);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: i32, start: u64, name: &str, rss: u64, cpu: u64) -> ProcRow {
        ProcRow {
            pid,
            start,
            name: name.into(),
            rss_bytes: rss,
            cpu_usec: cpu,
        }
    }

    fn snap(at_ms: i64, rows: Vec<ProcRow>) -> ProcSnap {
        ProcSnap { at_ms, rows }
    }

    fn sample(t: i64) -> MetricSample {
        MetricSample {
            t_ms: t,
            cpu_usec: Some(t as u64),
            mem_bytes: Some(1),
            io_read_bytes: None,
            io_write_bytes: None,
            io_some_us: None,
        }
    }

    // R-560
    #[test]
    fn r560_the_ring_keeps_the_newest_900_samples() {
        let mut m = MetricsStore::default();
        let id = JobId(1);
        for t in 0..(MAX_SAMPLES as i64 + 50) {
            m.record(id, t, Attribution::Cgroup, sample(t), None);
        }
        let v = m.view(id, 0).unwrap();
        assert_eq!(v.samples.len(), MAX_SAMPLES);
        assert_eq!(v.samples.first().map(|s| s.t_ms), Some(50));
        assert_eq!(
            v.samples.last().map(|s| s.t_ms),
            Some(MAX_SAMPLES as i64 + 49)
        );
    }

    // R-561
    #[test]
    fn r561_an_ended_jobs_history_expires_and_a_running_ones_does_not() {
        let mut m = MetricsStore::default();
        m.record(JobId(1), 0, Attribution::Cgroup, sample(0), None);
        m.record(JobId(2), 0, Attribution::Cgroup, sample(0), None);
        m.end(JobId(1), 1_000);
        let v = m.view(JobId(1), 1_000 + KEEP_AFTER_END_MS - 1).unwrap();
        assert!(v.note.is_some_and(|n| n.contains("ended")));
        assert!(v.procs.is_empty());
        assert!(m.view(JobId(1), 1_000 + KEEP_AFTER_END_MS).is_none());
        assert!(m.view(JobId(2), 10 * KEEP_AFTER_END_MS).is_some());
        assert_eq!(m.len(), 1);
        assert!(m.latest(JobId(2)).is_some() && m.latest(JobId(1)).is_none());
    }

    // R-562
    #[test]
    fn r562_cores_come_from_the_last_two_samples_and_a_reset_is_unknown() {
        let mut m = MetricsStore::default();
        let id = JobId(1);
        let mut a = sample(0);
        a.cpu_usec = Some(0);
        let mut b = sample(2_000);
        b.cpu_usec = Some(3_000_000);
        m.record(id, 0, Attribution::Cgroup, a, None);
        assert_eq!(m.latest(id).map(|l| l.cores), Some(None));
        m.record(id, 2_000, Attribution::Cgroup, b, None);
        assert_eq!(m.latest(id).and_then(|l| l.cores), Some(1.5));
        let mut c = sample(4_000);
        c.cpu_usec = Some(10);
        m.record(id, 4_000, Attribution::Cgroup, c, None);
        assert_eq!(m.latest(id).map(|l| l.cores), Some(None));
    }

    // R-563
    #[test]
    fn r563_groups_sum_by_program_and_sort_by_cpu_then_memory() {
        let prev = snap(
            0,
            vec![
                row(1, 1, "rustc", 10, 0),
                row(2, 1, "rustc", 10, 0),
                row(3, 1, "ld", 500, 0),
                row(4, 1, "sh", 900, 0),
            ],
        );
        let cur = snap(
            2_000,
            vec![
                row(1, 1, "rustc", 100, 2_000_000),
                row(2, 1, "rustc", 200, 1_000_000),
                row(3, 1, "ld", 500, 1_000_000),
                row(4, 1, "sh", 900, 0),
            ],
        );
        let g = group_procs(Some(&prev), &cur);
        let names: Vec<_> = g.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["rustc", "ld", "sh"]);
        assert_eq!(
            (g[0].count, g[0].rss_bytes, g[0].cpu_cores),
            (2, 300, Some(1.5))
        );
        assert_eq!(g[1].cpu_cores, Some(0.5));
        assert!(g.iter().all(|g| !g.others));
    }

    // R-563
    #[test]
    fn r563_ties_on_cpu_break_by_memory_then_name() {
        let cur = snap(
            2_000,
            vec![
                row(1, 1, "b", 5, 0),
                row(2, 1, "a", 5, 0),
                row(3, 1, "c", 9, 0),
            ],
        );
        let prev = snap(0, vec![]);
        let g = group_procs(Some(&prev), &cur);
        let names: Vec<_> = g.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["c", "a", "b"]);
    }

    // R-564
    #[test]
    fn r564_a_reused_pid_is_a_new_process_and_a_counter_reset_counts_nothing() {
        let prev = snap(
            0,
            vec![
                row(7, 1, "old", 1, 9_000_000),
                row(8, 1, "same", 1, 5_000_000),
            ],
        );
        // pid 7 now belongs to a younger process; pid 8's counter went backwards
        let cur = snap(
            1_000,
            vec![row(7, 2, "new", 1, 400_000), row(8, 1, "same", 1, 100)],
        );
        let g = group_procs(Some(&prev), &cur);
        let by = |n: &str| g.iter().find(|g| g.name == n).and_then(|g| g.cpu_cores);
        assert_eq!(by("new"), Some(0.4));
        assert_eq!(by("same"), Some(0.0));
        assert!(by("old").is_none(), "gone processes are not listed");
    }

    // R-564
    #[test]
    fn r564_without_a_previous_snapshot_or_time_the_cpu_is_unknown() {
        let cur = snap(1_000, vec![row(1, 1, "x", 4, 100)]);
        assert_eq!(group_procs(None, &cur)[0].cpu_cores, None);
        let same_time = snap(1_000, vec![]);
        assert_eq!(group_procs(Some(&same_time), &cur)[0].cpu_cores, None);
    }

    // R-565
    #[test]
    fn r565_beyond_twelve_groups_the_rest_are_summed_into_an_others_row() {
        let prev = snap(0, vec![]);
        let rows: Vec<ProcRow> = (0..15)
            .map(|i| {
                row(
                    i + 1,
                    1,
                    &format!("p{i:02}"),
                    10,
                    1_000_000 * (20 - i as u64),
                )
            })
            .collect();
        let g = group_procs(Some(&prev), &snap(1_000, rows));
        assert_eq!(g.len(), MAX_GROUPS + 1);
        let last = g.last().unwrap();
        assert!(last.others);
        assert_eq!((last.count, last.rss_bytes), (3, 30));
        assert_eq!(last.name, "3 other programs");
        assert_eq!(g[0].name, "p00");
        // the last three spent 8 + 7 + 6 CPU seconds in the one second between snapshots
        assert_eq!(last.cpu_cores, Some(21.0));
    }

    // R-566
    #[test]
    fn r566_a_sample_uses_the_current_level_and_falls_back_to_the_peak() {
        let mut f = JobFigures::empty(Attribution::Cgroup);
        f.mem_peak_bytes = Some(9);
        assert_eq!(sample_of(5, &f).mem_bytes, Some(9));
        f.mem_now_bytes = Some(3);
        assert_eq!(sample_of(5, &f).mem_bytes, Some(3));
    }
}
