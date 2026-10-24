//! State transitions shared by every store, so both implement exactly one rule set.

use crate::record::{CancelRequest, dep_failed};
use crate::request::{CancelOutcome, Finish, NewJob, PlanContext, StartInfo};
use crate::{JobRecord, LedgerError, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tasks_core::plan::Plan;
use tasks_core::{JobId, JobSpec, Phase, Priority, WaitReason};
use tasks_timing::Sample;

/// Builds the queued record for a new job.
pub(crate) fn new_record(id: JobId, job: &NewJob, now: i64) -> JobRecord {
    JobRecord {
        id,
        spec: job.spec.clone(),
        hints: job.hints.clone(),
        plan_id: job.step.as_ref().map(|(p, _)| *p),
        step_name: job.step.as_ref().map(|(_, n)| n.clone()),
        phase: Phase::Queued,
        wait_text: None,
        submitted_at_ms: now,
        started_at_ms: None,
        finished_at_ms: None,
        machine: None,
        exit_code: None,
        signal: None,
        timeout: None,
        summary: None,
        lease: None,
        log_path: None,
        forward: None,
        origin: job.origin.clone(),
        remote_phase: None,
        cancel: None,
        figures: None,
    }
}

/// Checks a dependency list against the phases of existing jobs: every dependency must exist
/// and must not already have ended without passing (R-179). A new job cannot depend on itself
/// because its id does not exist yet; cycles are refused by plan validation.
pub(crate) fn check_deps(after: &[JobId], phase_of: impl Fn(JobId) -> Option<Phase>) -> Result<()> {
    for d in after {
        match phase_of(*d) {
            None => return Err(LedgerError::UnknownDependency(*d)),
            Some(p) if dep_failed(&p) => return Err(LedgerError::DependencyNotPassable(*d)),
            Some(_) => {}
        }
    }
    Ok(())
}

fn illegal(r: &JobRecord, to: Phase) -> LedgerError {
    LedgerError::IllegalTransition {
        from: r.phase.clone(),
        to,
    }
}

/// Queued or waiting -> waiting, with the full wait text.
pub(crate) fn apply_wait(
    r: &mut JobRecord,
    reason: WaitReason,
    text: Option<String>,
) -> Result<()> {
    let next = Phase::Waiting(reason);
    if !r.phase.can_move_to(&next) {
        return Err(illegal(r, next));
    }
    r.phase = next;
    r.wait_text = text;
    Ok(())
}

/// Changes the class of a job that has not started, validating the class with its reason.
/// Returns the class it had. A job that is running or over is refused: nothing is preempted.
pub(crate) fn apply_priority(
    r: &mut JobRecord,
    priority: Priority,
    reason: Option<&str>,
) -> Result<Priority> {
    if !matches!(r.phase, Phase::Queued | Phase::Waiting(_)) {
        return Err(LedgerError::NotQueued {
            id: r.id,
            phase: r.phase.clone(),
        });
    }
    tasks_core::check_reason(priority, reason)?;
    let before = r.spec.priority;
    r.spec.priority = priority;
    r.spec.priority_reason = reason
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Ok(before)
}

/// Queued or waiting -> running, recording machine, lease and log path together.
pub(crate) fn apply_start(r: &mut JobRecord, info: Option<&StartInfo>, now: i64) -> Result<()> {
    if !r.phase.can_move_to(&Phase::Running) || r.cancel_requested() {
        return Err(illegal(r, Phase::Running));
    }
    r.phase = Phase::Running;
    r.wait_text = None;
    r.started_at_ms = Some(now);
    if let Some(i) = info {
        r.machine = Some(i.machine.clone());
        r.lease = Some(i.lease);
        r.log_path = i.log_path.clone().or_else(|| r.log_path.take());
    }
    Ok(())
}

/// Running (or, for cancel, waiting) -> terminal. A cancel committed before a passed/failed
/// write wins: the phase becomes `Cancelled` and the exit code is kept for the record (R-006).
/// A terminal row is immutable.
pub(crate) fn apply_finish(r: &mut JobRecord, f: &Finish, now: i64) -> Result<()> {
    if !f.phase.is_terminal() || !r.phase.can_move_to(&f.phase) {
        return Err(illegal(r, f.phase.clone()));
    }
    let mut phase = f.phase.clone();
    if r.cancel_requested() && matches!(phase, Phase::Passed | Phase::Failed { .. }) {
        phase = Phase::Cancelled;
    }
    r.exit_code = match (&f.phase, f.exit_code) {
        (Phase::Passed, _) => Some(0),
        (Phase::Failed { exit } | Phase::TimedOut { exit }, c) => exit.or(c),
        (Phase::Interrupted, _) => None,
        (_, c) => c,
    };
    r.phase = phase;
    r.wait_text = None;
    r.signal = f.signal;
    r.timeout = f.timeout;
    r.summary = f.summary.clone();
    r.finished_at_ms = Some(now);
    Ok(())
}

/// Records a cancel request. A job that has not started is cancelled at once and never takes
/// admission; a running job only gets the flag. The first requester is kept.
pub(crate) fn apply_cancel(
    r: &mut JobRecord,
    by: &str,
    reason: Option<&str>,
    now: i64,
) -> CancelOutcome {
    if r.phase.is_terminal() {
        return CancelOutcome::AlreadyFinished(r.phase.clone());
    }
    if r.cancel.is_none() {
        r.cancel = Some(CancelRequest {
            by: by.to_string(),
            at_ms: now,
            reason: reason.map(str::to_string),
        });
    }
    if r.phase == Phase::Running {
        return CancelOutcome::Requested;
    }
    r.phase = Phase::Cancelled;
    r.wait_text = None;
    r.exit_code = None;
    r.summary = Some(match reason {
        Some(why) => format!("cancelled before start by {by}: {why}"),
        None => format!("cancelled before start by {by}"),
    });
    r.finished_at_ms = Some(now);
    CancelOutcome::Cancelled
}

/// Expands a validated plan into jobs in dependency order, calling `insert` for each. Returns
/// (step name, job id) in the plan's declaration order. `insert` must validate and store the job;
/// the caller owns atomicity (all steps or none).
pub(crate) fn plan_jobs(
    ctx: &PlanContext,
    plan: &Plan,
    plan_id: i64,
    mut insert: impl FnMut(&NewJob) -> Result<JobId>,
) -> Result<Vec<(String, JobId)>> {
    let mut ids: BTreeMap<&str, JobId> = BTreeMap::new();
    for i in plan.order() {
        let Some(step) = plan.steps.get(i) else {
            continue;
        };
        let after = step
            .after
            .iter()
            .filter_map(|d| ids.get(d.as_str()).copied())
            .collect();
        let (priority, priority_reason) = ctx.step_priority(&step.hints);
        let job = NewJob {
            spec: JobSpec {
                owner: ctx.owner.clone(),
                argv: step.argv.clone(),
                cwd: ctx.cwd.clone(),
                env: ctx.env.clone(),
                kind: step.kind,
                requirements: step.requirements.clone(),
                placement: ctx.placement.clone(),
                after,
                // the step's own hint fills the spec's limit (R-095's sibling)
                timeout_secs: step.hints.timeout_secs,
                broad_test_escape: ctx.broad_test_escape,
                priority,
                priority_reason,
                fetch: step.hints.fetch.clone(),
                isolate: step.hints.isolate,
                light: step.hints.light,
                retry_alone: step.hints.retry_alone,
            },
            hints: step.hints.clone(),
            step: Some((plan_id, step.name.clone())),
            origin: None,
        };
        let id = insert(&job)?;
        ids.insert(step.name.as_str(), id);
    }
    Ok(plan
        .steps
        .iter()
        .filter_map(|s| ids.get(s.name.as_str()).map(|id| (s.name.clone(), *id)))
        .collect())
}

/// Sets the log path.
pub(crate) fn apply_log_path(r: &mut JobRecord, path: PathBuf) {
    r.log_path = Some(path);
}

/// Rejects samples that would corrupt estimates: non-finite, negative or absurdly large.
pub(crate) fn validate_sample(s: &Sample) -> Result<()> {
    if !s.secs.is_finite() {
        return Err(LedgerError::InvalidSample("duration is not finite"));
    }
    if s.secs < 0.0 {
        return Err(LedgerError::InvalidSample("duration is negative"));
    }
    if i64::try_from(s.at_ms).is_err() {
        return Err(LedgerError::InvalidSample("timestamp out of range"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::tests::rec;
    use crate::record::{LeaseRecord, TimeoutReason};
    use tasks_core::MachineName;
    use tasks_timing::{Cache, Outcome};

    fn start_info() -> StartInfo {
        StartInfo {
            machine: MachineName("m".into()),
            lease: LeaseRecord {
                pgid: 7,
                leader_start: 9,
            },
            log_path: Some("/l".into()),
        }
    }

    #[test]
    fn start_records_machine_lease_and_clears_wait() {
        let mut r = rec(1, Phase::Waiting(WaitReason::Disk));
        r.wait_text = Some("disk".into());
        apply_start(&mut r, Some(&start_info()), 50).unwrap();
        assert_eq!(r.phase, Phase::Running);
        assert_eq!(r.started_at_ms, Some(50));
        assert!(r.wait_text.is_none() && r.lease.is_some() && r.machine.is_some());
        assert!(apply_start(&mut r, Some(&start_info()), 60).is_err());
    }

    #[test]
    fn finish_normalises_exit_and_cancel_wins() {
        let mut r = rec(1, Phase::Running);
        apply_finish(&mut r, &Finish::from_phase(Phase::Passed), 5).unwrap();
        assert_eq!(r.exit_code, Some(0));
        let again = apply_finish(
            &mut r,
            &Finish::from_phase(Phase::Failed { exit: Some(1) }),
            6,
        );
        assert!(matches!(again, Err(LedgerError::IllegalTransition { .. })));

        let mut r = rec(2, Phase::Running);
        assert_eq!(apply_cancel(&mut r, "u", None, 3), CancelOutcome::Requested);
        let mut f = Finish::from_phase(Phase::Failed { exit: Some(3) });
        f.timeout = Some(TimeoutReason::Wall);
        apply_finish(&mut r, &f, 4).unwrap();
        assert_eq!(r.phase, Phase::Cancelled);
        assert_eq!(r.exit_code, Some(3));

        let mut r = rec(3, Phase::Running);
        let mut f = Finish::from_phase(Phase::Interrupted);
        f.exit_code = Some(0);
        apply_finish(&mut r, &f, 4).unwrap();
        assert_eq!(r.exit_code, None);
        assert!(
            apply_finish(
                &mut rec(4, Phase::Running),
                &Finish::from_phase(Phase::Running),
                1
            )
            .is_err()
        );
    }

    #[test]
    fn cancel_keeps_first_requester_and_is_noop_when_finished() {
        let mut r = rec(1, Phase::Running);
        apply_cancel(&mut r, "first", Some("a"), 1);
        apply_cancel(&mut r, "second", Some("b"), 2);
        assert_eq!(r.cancel.as_ref().map(|c| c.by.as_str()), Some("first"));
        // the reason belongs to the request that was kept
        assert_eq!(
            r.cancel.as_ref().and_then(|c| c.reason.as_deref()),
            Some("a")
        );
        let mut q = rec(2, Phase::Waiting(WaitReason::Disk));
        assert_eq!(apply_cancel(&mut q, "u", None, 9), CancelOutcome::Cancelled);
        assert_eq!(q.finished_at_ms, Some(9));
        assert_eq!(
            apply_cancel(&mut q, "v", None, 10),
            CancelOutcome::AlreadyFinished(Phase::Cancelled)
        );
        assert_eq!(q.cancel.as_ref().map(|c| c.by.as_str()), Some("u"));
    }

    #[test]
    fn sample_validation() {
        let ok = Sample {
            secs: 1.0,
            cache: Cache::Warm,
            outcome: Outcome::Passed,
            at_ms: 1,
        };
        assert!(validate_sample(&ok).is_ok());
        for secs in [f64::NAN, f64::INFINITY, -1.0] {
            assert!(validate_sample(&Sample { secs, ..ok }).is_err());
        }
        assert!(
            validate_sample(&Sample {
                at_ms: u64::MAX,
                ..ok
            })
            .is_err()
        );
    }

    #[test]
    fn deps_must_exist_and_not_be_dead() {
        let phase = |id: JobId| match id.0 {
            1 => Some(Phase::Passed),
            2 => Some(Phase::Queued),
            3 => Some(Phase::Cancelled),
            _ => None,
        };
        assert!(check_deps(&[JobId(1), JobId(2)], phase).is_ok());
        assert!(matches!(
            check_deps(&[JobId(9)], phase),
            Err(LedgerError::UnknownDependency(_))
        ));
        assert!(matches!(
            check_deps(&[JobId(3)], phase),
            Err(LedgerError::DependencyNotPassable(_))
        ));
    }
}
