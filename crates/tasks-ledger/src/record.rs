//! Stored job records and list filters.

use std::path::PathBuf;
use tasks_core::{JobId, JobSpec, MachineName, Phase, StepHints};

/// Why a job was stopped by a timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutReason {
    /// The wall-clock limit was reached.
    Wall,
    /// The command produced no output for too long.
    Idle,
}

/// The identity of a running process group, so a restarted daemon can check it is still ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseRecord {
    /// Process group id (the leader's pid).
    pub pgid: i32,
    /// Opaque start time of the group leader; tells our leader from a reused pid.
    pub leader_start: u64,
}

/// A cancel request recorded on a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelRequest {
    /// Who asked (user and pid, "dependency", "daemon restart", ...).
    pub by: String,
    /// When, ms since epoch.
    pub at_ms: i64,
}

/// Where a job was forwarded to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    /// The peer running it.
    pub peer: MachineName,
    /// The job id on that peer, once known; `None` means never submitted remotely.
    pub remote_job: Option<i64>,
}

/// A job as stored in the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    /// Job id.
    pub id: JobId,
    /// The submitted spec.
    pub spec: JobSpec,
    /// Scheduling hints (non-empty for plan steps only).
    pub hints: StepHints,
    /// The plan this job is a step of.
    pub plan_id: Option<i64>,
    /// The step name within its plan.
    pub step_name: Option<String>,
    /// Current phase.
    pub phase: Phase,
    /// Full human wait text beside the coarse reason in `phase`.
    pub wait_text: Option<String>,
    /// Submission (queued) time, ms since epoch.
    pub submitted_at_ms: i64,
    /// Start time, once running.
    pub started_at_ms: Option<i64>,
    /// Finish time, once terminal.
    pub finished_at_ms: Option<i64>,
    /// Machine that ran or runs it.
    pub machine: Option<MachineName>,
    /// The command's exit code; use `shown_exit` for display.
    pub exit_code: Option<i32>,
    /// The signal we sent that ended it.
    pub signal: Option<i32>,
    /// Which timeout stopped it.
    pub timeout: Option<TimeoutReason>,
    /// One line describing the outcome.
    pub summary: Option<String>,
    /// The running group's identity.
    pub lease: Option<LeaseRecord>,
    /// Where the job's log lives.
    pub log_path: Option<PathBuf>,
    /// Forwarding info.
    pub forward: Option<Forward>,
    /// Last known phase on the peer; never changes the local `phase` (R-015).
    pub remote_phase: Option<Phase>,
    /// The cancel request, if one was made.
    pub cancel: Option<CancelRequest>,
}

impl JobRecord {
    /// The exit code every surface shows: 0 only for a passed job (R-009); an interrupted job
    /// shows none, and a non-passed job never shows 0.
    pub fn shown_exit(&self) -> Option<i32> {
        match self.phase {
            Phase::Passed => Some(0),
            Phase::Interrupted => None,
            _ => self.exit_code.filter(|c| *c != 0),
        }
    }

    /// True once a cancel was requested.
    pub fn cancel_requested(&self) -> bool {
        self.cancel.is_some()
    }
}

/// Criteria for `Store::list`; all set fields must match.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Only this owner.
    pub owner: Option<String>,
    /// Only unfinished jobs (queued, waiting, running) (R-026).
    pub active_only: bool,
    /// Only this job (R-023).
    pub id: Option<JobId>,
    /// All unfinished jobs plus only the newest n finished ones (R-024).
    pub recent_finished: Option<usize>,
    /// Maximum rows returned (oldest first).
    pub limit: Option<usize>,
}

impl ListFilter {
    /// True when the record passes the owner, id and activity criteria
    /// (`recent_finished` needs the whole set and is applied by `select`).
    pub fn matches(&self, r: &JobRecord) -> bool {
        self.owner
            .as_deref()
            .is_none_or(|o| r.spec.owner.as_str() == o)
            && self.id.is_none_or(|i| i == r.id)
            && !(self.active_only && r.phase.is_terminal())
    }

    /// Applies every criterion to `all` (ordered by id) and returns the survivors.
    pub(crate) fn select(&self, all: &[JobRecord]) -> Vec<JobRecord> {
        let keep_finished: Option<std::collections::BTreeSet<JobId>> =
            self.recent_finished.map(|n| {
                let mut fin: Vec<&JobRecord> =
                    all.iter().filter(|r| r.phase.is_terminal()).collect();
                fin.sort_by_key(|r| std::cmp::Reverse((r.finished_at_ms, r.id)));
                fin.into_iter().take(n).map(|r| r.id).collect()
            });
        all.iter()
            .filter(|r| self.matches(r))
            .filter(|r| {
                keep_finished
                    .as_ref()
                    .is_none_or(|k| !r.phase.is_terminal() || k.contains(&r.id))
            })
            .take(self.limit.unwrap_or(usize::MAX))
            .cloned()
            .collect()
    }
}

/// A stable tag for a phase, used for indexed queries.
pub(crate) fn phase_tag(p: &Phase) -> &'static str {
    match p {
        Phase::Queued => "queued",
        Phase::Waiting(_) => "waiting",
        Phase::Running => "running",
        Phase::Passed => "passed",
        Phase::Failed { .. } => "failed",
        Phase::TimedOut { .. } => "timed_out",
        Phase::Cancelled => "cancelled",
        Phase::Interrupted => "interrupted",
    }
}

/// True when a dependency in this phase will never pass (single rule for `blocked_by_failure`):
/// it is terminal and not `Passed`.
pub(crate) fn dep_failed(p: &Phase) -> bool {
    p.is_terminal() && *p != Phase::Passed
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tasks_core::{Kind, Owner, Placement, Requirements};

    pub(crate) fn rec(id: i64, phase: Phase) -> JobRecord {
        JobRecord {
            id: JobId(id),
            spec: JobSpec {
                owner: Owner::new("o").unwrap(),
                argv: vec!["x".into()],
                cwd: "/".into(),
                env: vec![],
                kind: Kind::Run,
                requirements: Requirements::default(),
                placement: Placement::Anywhere,
                after: vec![],
            },
            hints: StepHints::default(),
            plan_id: None,
            step_name: None,
            phase,
            wait_text: None,
            submitted_at_ms: 0,
            started_at_ms: None,
            finished_at_ms: Some(id),
            machine: None,
            exit_code: None,
            signal: None,
            timeout: None,
            summary: None,
            lease: None,
            log_path: None,
            forward: None,
            remote_phase: None,
            cancel: None,
        }
    }

    // R-009: only a passed job shows exit 0.
    #[test]
    fn r009_only_passed_reports_exit_zero() {
        let mut r = rec(1, Phase::Passed);
        assert_eq!(r.shown_exit(), Some(0));
        for p in [
            Phase::Failed { exit: Some(0) },
            Phase::Cancelled,
            Phase::TimedOut { exit: Some(0) },
        ] {
            r.phase = p;
            r.exit_code = Some(0);
            assert_eq!(r.shown_exit(), None);
        }
        r.phase = Phase::Failed { exit: Some(2) };
        r.exit_code = Some(2);
        assert_eq!(r.shown_exit(), Some(2));
        r.phase = Phase::Interrupted;
        r.exit_code = Some(3);
        assert_eq!(r.shown_exit(), None);
    }

    #[test]
    fn dep_failed_is_terminal_and_not_passed() {
        for (p, want) in [
            (Phase::Queued, false),
            (Phase::Running, false),
            (Phase::Passed, false),
            (Phase::Failed { exit: None }, true),
            (Phase::TimedOut { exit: None }, true),
            (Phase::Cancelled, true),
            (Phase::Interrupted, true),
        ] {
            assert_eq!(dep_failed(&p), want, "{p:?}");
        }
    }

    #[test]
    fn select_keeps_unfinished_and_newest_finished() {
        let all = vec![
            rec(1, Phase::Passed),
            rec(2, Phase::Passed),
            rec(3, Phase::Queued),
            rec(4, Phase::Passed),
        ];
        let f = ListFilter {
            recent_finished: Some(1),
            ..ListFilter::default()
        };
        let ids: Vec<i64> = f.select(&all).iter().map(|r| r.id.0).collect();
        assert_eq!(ids, vec![3, 4]);
    }
}
