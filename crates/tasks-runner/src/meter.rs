//! Per-job resource figures during a run: from the job's cgroup when there is one, otherwise
//! from the job's process tree. Also owns the cgroup's life (create, kill the escaped, remove).

use std::path::PathBuf;
use std::time::Duration;
use tasks_core::{JobFigures, JobId};
use tasks_platform::{CgroupName, Cgroups, Child, CpuShare, ProcessTable, TreeMeter, Usage, tree_pids};

/// How often figures are sampled while the job runs.
const SAMPLE_EVERY: Duration = Duration::from_secs(2);

/// What the runner needs to attribute a job's cost.
#[derive(Clone, Copy)]
pub struct Attrib<'a> {
    /// Per-job cgroups (or the reason there are none).
    pub cgroups: &'a dyn Cgroups,
    /// The process table, for the process-tree fallback.
    pub table: &'a dyn ProcessTable,
}

/// Samples one job.
pub(crate) struct Meter<'a> {
    attrib: Attrib<'a>,
    name: CgroupName,
    /// The cgroup was created, so the child was told to join it.
    in_cgroup: bool,
    /// The job's processes were seen in the cgroup (or its counters moved).
    placed: bool,
    tree: TreeMeter,
    leader: Option<(i32, i32)>,
    last_at: Option<Duration>,
}

impl<'a> Meter<'a> {
    /// Creates the job's cgroup when possible. The returned path is the `cgroup.procs` file the
    /// child joins before it execs; `None` means process groups only.
    pub(crate) fn new(attrib: Attrib<'a>, job: JobId) -> (Meter<'a>, Option<PathBuf>) {
        let name = CgroupName::Job(job.0);
        let procs = attrib.cgroups.create(name).ok();
        let meter = Meter {
            attrib,
            name,
            in_cgroup: procs.is_some(),
            placed: false,
            tree: TreeMeter::new(),
            leader: None,
            last_at: None,
        };
        (meter, procs)
    }

    /// Gives the job its CPU share and returns the niceness the spawn must add: none when the
    /// cgroup took the weight, the share's niceness when there is no cgroup or no cpu controller.
    pub(crate) fn apply_cpu(&self, share: CpuShare) -> i32 {
        if self.in_cgroup
            && let Some(w) = share.weight
            && matches!(self.attrib.cgroups.set_cpu_weight(self.name, w), Ok(true))
        {
            return 0;
        }
        share.nice
    }

    pub(crate) fn started(&mut self, child: &Child) {
        self.leader = Some((child.pid, child.group.pgid));
    }

    /// True when the job has a cgroup.
    pub(crate) fn has_cgroup(&self) -> bool {
        self.in_cgroup
    }

    /// Takes a sample now.
    pub(crate) fn sample(&mut self, usage: Option<&Usage>) -> Option<JobFigures> {
        if self.in_cgroup {
            if self
                .attrib
                .cgroups
                .pids(self.name)
                .is_ok_and(|p| !p.is_empty())
            {
                self.placed = true;
            }
            if let Ok(f) = self.attrib.cgroups.read(self.name) {
                // counters outlive the processes, so a job that ended between two polls still
                // proves it ran in the cgroup
                let moved = f.cpu_usec.unwrap_or(0) > 0 || f.mem_peak_bytes.unwrap_or(0) > 0;
                if self.placed || moved {
                    self.placed = true;
                    return Some(f);
                }
            }
        }
        // not in a cgroup (or never joined it): sum the process tree
        let (leader, pgid) = self.leader?;
        if let Ok(table) = self.attrib.table.snapshot() {
            let members = tree_pids(&table, leader, pgid);
            let rows: Vec<&_> = table.iter().filter(|p| members.contains(&p.pid)).collect();
            self.tree.observe(&rows);
        }
        let cpu = usage.map(|u| ((u.cpu_user_s + u.cpu_sys_s) * 1e6) as u64);
        Some(self.tree.figures(cpu))
    }

    /// A periodic sample: `Some` when one is due.
    pub(crate) fn tick(&mut self, now: Duration, usage: Option<&Usage>) -> Option<JobFigures> {
        let due = self
            .last_at
            .is_none_or(|t| now.saturating_sub(t) >= SAMPLE_EVERY);
        if !due {
            return None;
        }
        self.last_at = Some(now);
        self.sample(usage)
    }

    /// Pids in the cgroup other than the leader (a zombie leader stays listed until reaped).
    pub(crate) fn escapees(&self, leader: i32) -> Vec<i32> {
        if !self.in_cgroup {
            return Vec::new();
        }
        self.attrib
            .cgroups
            .pids(self.name)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| *p != leader)
            .collect()
    }

    /// `cgroup.kill`: the last resort that reaches children that left the process group.
    pub(crate) fn kill(&self) {
        if self.in_cgroup {
            let _ = self.attrib.cgroups.kill(self.name);
        }
    }

    /// Removes the cgroup if it is empty (twice, since the kernel needs a moment after a kill).
    pub(crate) fn remove(&self) {
        if !self.in_cgroup {
            return;
        }
        for _ in 0..2 {
            if self.attrib.cgroups.remove(self.name).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
