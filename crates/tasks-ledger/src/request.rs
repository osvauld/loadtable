//! Inputs to store operations.

use crate::record::{LeaseRecord, Origin, TimeoutReason};
use std::path::PathBuf;
use std::time::Duration;
use tasks_core::{JobId, JobSpec, MachineName, Owner, Phase, Placement, StepHints};

/// A job to submit: the spec plus ledger-only metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    /// The spec.
    pub spec: JobSpec,
    /// Scheduling hints.
    pub hints: StepHints,
    /// Plan and step name when this job is a step.
    pub step: Option<(i64, String)>,
    /// The peer and key this job was received under; at most one job per pair.
    pub origin: Option<Origin>,
}

impl NewJob {
    /// A plain job with no hints and no plan.
    pub fn new(spec: JobSpec) -> NewJob {
        NewJob {
            spec,
            hints: StepHints::default(),
            step: None,
            origin: None,
        }
    }
}

/// What the plan's steps share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanContext {
    /// Submitting owner.
    pub owner: Owner,
    /// Working directory for every step.
    pub cwd: PathBuf,
    /// Extra environment for every step.
    pub env: Vec<(String, String)>,
    /// Placement for every step.
    pub placement: Placement,
    /// The submitter passed the broad-test escape; every step's spec carries it (R-191).
    pub broad_test_escape: bool,
}

/// The result of submitting a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSubmitted {
    /// The plan's id.
    pub plan_id: i64,
    /// Step name and job id, in the plan's declaration order.
    pub jobs: Vec<(String, JobId)>,
}

/// What the runner knows when it takes a job from waiting to running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartInfo {
    /// Machine running it.
    pub machine: MachineName,
    /// The group identity to persist.
    pub lease: LeaseRecord,
    /// Where the log lives.
    pub log_path: Option<PathBuf>,
}

/// How a job ended. Build one from a runner outcome or a phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finish {
    /// The terminal phase.
    pub phase: Phase,
    /// The command's exit code; a code inside `phase` wins, `Passed` is always 0.
    pub exit_code: Option<i32>,
    /// The signal we sent that ended it.
    pub signal: Option<i32>,
    /// Which timeout stopped it.
    pub timeout: Option<TimeoutReason>,
    /// One line describing the outcome.
    pub summary: Option<String>,
}

impl Finish {
    /// A finish carrying only the phase (and the exit code inside it).
    pub fn from_phase(phase: Phase) -> Finish {
        let exit_code = match &phase {
            Phase::Passed => Some(0),
            Phase::Failed { exit } | Phase::TimedOut { exit } => *exit,
            _ => None,
        };
        Finish {
            phase,
            exit_code,
            signal: None,
            timeout: None,
            summary: None,
        }
    }
}

/// What a cancel request did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The job had not started: it is now cancelled and will never take admission.
    Cancelled,
    /// The job is running: the flag is set and the runner must stop it and finish it.
    Requested,
    /// The job had already finished in this phase; nothing changed.
    AlreadyFinished(Phase),
}

/// Compares two ledger states: changes whenever this or another connection commits (R-025).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeToken(pub u64, pub u64);

/// What to prune. Running and queued jobs are never pruned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Drop finished jobs that ended at least this long ago.
    pub max_job_age: Option<Duration>,
    /// Keep at most this many finished jobs (the newest).
    pub max_finished_jobs: Option<usize>,
    /// Drop samples taken at least this long ago.
    pub max_sample_age: Option<Duration>,
    /// Keep at most this many samples per (recipe, target, machine) (the newest).
    pub max_samples_per_key: Option<usize>,
}

/// What a prune removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Finished jobs removed.
    pub jobs: usize,
    /// Samples removed.
    pub samples: usize,
}

/// One finished job as the retention rule sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PruneRow {
    pub id: i64,
    pub finished_at_ms: i64,
}

/// Picks the finished jobs to remove. `protected` are ids that an unfinished job depends on;
/// `newest` is the highest job id, kept so ids are never reused.
pub(crate) fn select_prune(
    finished: &[PruneRow],
    protected: &std::collections::BTreeSet<i64>,
    newest: i64,
    policy: &RetentionPolicy,
    now_ms: i64,
) -> Vec<i64> {
    let age_ms = |d: Duration| i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
    let mut order: Vec<&PruneRow> = finished.iter().collect();
    order.sort_by_key(|r| std::cmp::Reverse((r.finished_at_ms, r.id)));
    let mut out = vec![];
    for (rank, r) in order.into_iter().enumerate() {
        let too_old = policy
            .max_job_age
            .is_some_and(|a| r.finished_at_ms <= now_ms.saturating_sub(age_ms(a)));
        let too_many = policy.max_finished_jobs.is_some_and(|n| rank >= n);
        if (too_old || too_many) && !protected.contains(&r.id) && r.id != newest {
            out.push(r.id);
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn finish_from_phase_takes_exit_from_the_phase() {
        assert_eq!(Finish::from_phase(Phase::Passed).exit_code, Some(0));
        let f = Finish::from_phase(Phase::Failed { exit: Some(7) });
        assert_eq!(f.exit_code, Some(7));
        assert_eq!(Finish::from_phase(Phase::Cancelled).exit_code, None);
    }

    #[test]
    fn prune_selection_by_age_and_count_with_protection() {
        let rows: Vec<PruneRow> = (1..=5)
            .map(|i| PruneRow {
                id: i,
                finished_at_ms: i * 1000,
            })
            .collect();
        let none = BTreeSet::new();
        let by_age = RetentionPolicy {
            max_job_age: Some(Duration::from_secs(3)),
            ..RetentionPolicy::default()
        };
        // now = 6000: ids finished at <= 3000 go, but 5 is the newest id anyway.
        assert_eq!(select_prune(&rows, &none, 9, &by_age, 6000), vec![1, 2, 3]);
        let by_count = RetentionPolicy {
            max_finished_jobs: Some(2),
            ..RetentionPolicy::default()
        };
        assert_eq!(select_prune(&rows, &none, 9, &by_count, 0), vec![1, 2, 3]);
        assert_eq!(select_prune(&rows, &none, 3, &by_count, 0), vec![1, 2]);
        let protect: BTreeSet<i64> = [2].into();
        assert_eq!(select_prune(&rows, &protect, 9, &by_count, 0), vec![1, 3]);
        assert!(select_prune(&rows, &none, 9, &RetentionPolicy::default(), 0).is_empty());
    }
}
