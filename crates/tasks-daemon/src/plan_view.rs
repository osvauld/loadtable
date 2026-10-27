//! One plan as one thing: the counts, the overall state and the times that the dashboard and
//! `loadtable list` show instead of one row per step (R-740).

use crate::api::JobView;
use tasks_core::Phase;

/// Where a plan stands as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanState {
    /// At least one step runs.
    Running,
    /// Nothing runs, at least one step waits or is queued.
    Waiting,
    /// Every step has ended.
    Finished,
}

/// What a step is doing, coarsely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    /// Executing here or on a peer that is running it.
    Running,
    /// Queued, waiting, or held by a peer's own gates.
    Waiting,
    /// Finished well.
    Passed,
    /// Failed, timed out or never started.
    Failed,
    /// Cancelled or interrupted.
    Cancelled,
}

impl StepState {
    /// The state of one step job.
    pub fn of(j: &JobView) -> StepState {
        match &j.phase {
            Phase::Running if j.remote_wait.is_some() => StepState::Waiting,
            Phase::Running => StepState::Running,
            Phase::Queued | Phase::Waiting(_) => StepState::Waiting,
            Phase::Passed => StepState::Passed,
            Phase::Failed { .. } | Phase::TimedOut { .. } | Phase::NotStarted { .. } => {
                StepState::Failed
            }
            Phase::Cancelled | Phase::Interrupted => StepState::Cancelled,
        }
    }

    /// Sort rank for a step list: failed first, then running, waiting, cancelled, passed.
    pub fn rank(self) -> u8 {
        match self {
            StepState::Failed => 0,
            StepState::Running => 1,
            StepState::Waiting => 2,
            StepState::Cancelled => 3,
            StepState::Passed => 4,
        }
    }
}

/// The totals of one plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanSummary {
    /// The plan id.
    pub plan_id: i64,
    /// The suite it was expanded from, when known.
    pub suite: Option<String>,
    /// Who submitted it (first step's owner).
    pub owner: String,
    /// Steps in the plan (as many as the job list holds).
    pub total: usize,
    /// Steps that passed.
    pub passed: usize,
    /// Steps that failed, timed out or never started.
    pub failed: usize,
    /// Steps running now.
    pub running: usize,
    /// Steps queued or waiting.
    pub waiting: usize,
    /// Steps cancelled or interrupted.
    pub cancelled: usize,
    /// The overall state.
    pub state: PlanState,
    /// Distinct machines of the steps that have one, in first-seen order.
    pub machines: Vec<String>,
    /// When the first step was submitted, ms since the epoch.
    pub submitted_at_ms: i64,
    /// When the first step started, if any did.
    pub started_at_ms: Option<i64>,
    /// When the last step ended, once every step has.
    pub finished_at_ms: Option<i64>,
}

/// The step's name without its group prefix: `tests.test-calls` is `test-calls`.
pub fn step_label(name: &str) -> String {
    match name.split_once('.') {
        Some((_, rest)) if !rest.is_empty() => rest.to_string(),
        _ => name.to_string(),
    }
}

/// Plan ids in the order their first step appears in `jobs`.
pub fn plan_ids(jobs: &[JobView]) -> Vec<i64> {
    let mut ids: Vec<i64> = Vec::new();
    for (p, _) in jobs.iter().filter_map(|j| j.plan.as_ref()) {
        if !ids.contains(p) {
            ids.push(*p);
        }
    }
    ids
}

/// The summary of the steps of one plan; `None` when `steps` is empty.
pub fn summarise<'a>(
    plan_id: i64,
    steps: impl IntoIterator<Item = &'a JobView>,
) -> Option<PlanSummary> {
    let mut s = PlanSummary {
        plan_id,
        suite: None,
        owner: String::new(),
        total: 0,
        passed: 0,
        failed: 0,
        running: 0,
        waiting: 0,
        cancelled: 0,
        state: PlanState::Finished,
        machines: Vec::new(),
        submitted_at_ms: i64::MAX,
        started_at_ms: None,
        finished_at_ms: None,
    };
    let mut last_end: Option<i64> = None;
    for j in steps {
        if s.total == 0 {
            s.owner = j.owner.clone();
        }
        if s.suite.is_none() {
            s.suite = j.suite.clone();
        }
        s.total += 1;
        match StepState::of(j) {
            StepState::Running => s.running += 1,
            StepState::Waiting => s.waiting += 1,
            StepState::Passed => s.passed += 1,
            StepState::Failed => s.failed += 1,
            StepState::Cancelled => s.cancelled += 1,
        }
        if let Some(m) = &j.machine
            && !s.machines.contains(m)
        {
            s.machines.push(m.clone());
        }
        s.submitted_at_ms = s.submitted_at_ms.min(j.submitted_at_ms);
        s.started_at_ms = match (s.started_at_ms, j.started_at_ms) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let Some(f) = j.finished_at_ms {
            last_end = Some(last_end.map_or(f, |l| l.max(f)));
        }
    }
    if s.total == 0 {
        return None;
    }
    s.state = if s.running > 0 {
        PlanState::Running
    } else if s.waiting > 0 {
        PlanState::Waiting
    } else {
        PlanState::Finished
    };
    if s.state == PlanState::Finished {
        s.finished_at_ms = last_end;
    }
    Some(s)
}

impl PlanSummary {
    /// Steps that have ended one way or another.
    pub fn done(&self) -> usize {
        self.passed + self.failed + self.cancelled
    }

    /// `12/40 done, 3 running, 1 failed`: only the counts that are not zero.
    pub fn counts_text(&self) -> String {
        let mut parts = vec![format!("{}/{} done", self.done(), self.total)];
        for (n, word) in [
            (self.running, "running"),
            (self.waiting, "waiting"),
            (self.failed, "failed"),
            (self.cancelled, "cancelled"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {word}"));
            }
        }
        parts.join(", ")
    }

    /// `nixos` or `nixos +1`; empty when no step has a machine yet.
    pub fn machine_text(&self) -> String {
        match self.machines.as_slice() {
            [] => String::new(),
            [one] => one.clone(),
            [first, rest @ ..] => format!("{first} +{}", rest.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_core::{JobId, Kind, Placement};

    fn step(id: i64, plan: i64, name: &str, phase: Phase, machine: &str) -> JobView {
        JobView {
            id: JobId(id),
            owner: "lead@laptop".into(),
            argv: vec!["cargo".into()],
            cwd: "/w".into(),
            kind: Kind::Test,
            placement: Placement::Here,
            after: Vec::new(),
            phase,
            wait_text: None,
            exit: None,
            summary: None,
            submitted_at_ms: 1_000 + id,
            started_at_ms: None,
            finished_at_ms: None,
            machine: Some(machine.into()),
            forwarded_to: None,
            cancel_requested_by: None,
            cancel_reason: None,
            timeout_secs: None,
            remote_phase: None,
            plan: Some((plan, name.into())),
            suite: None,
            tags: Vec::new(),
            log_path: None,
            figures: None,
            priority: Default::default(),
            priority_reason: None,
            remote_wait: None,
            fetch_paths: Vec::new(),
            fetch: Vec::new(),
            sync: None,
            transfer: None,
        }
    }

    // R-740
    #[test]
    fn r740_a_plan_with_a_running_step_is_running_and_counts_add_up() {
        let jobs = vec![
            step(1, 2, "build", Phase::Passed, "nixos"),
            step(2, 2, "a", Phase::Running, "nixos"),
            step(3, 2, "b", Phase::Queued, "nixos"),
            step(4, 2, "c", Phase::Failed { exit: Some(1) }, "laptop"),
        ];
        let s = summarise(2, &jobs).unwrap();
        assert_eq!(s.state, PlanState::Running);
        assert_eq!(
            (s.passed, s.running, s.waiting, s.failed, s.total),
            (1, 1, 1, 1, 4)
        );
        assert_eq!(s.counts_text(), "2/4 done, 1 running, 1 waiting, 1 failed");
        assert_eq!(s.machine_text(), "nixos +1");
    }

    // R-740
    #[test]
    fn r740_waiting_when_nothing_runs_and_finished_when_all_ended() {
        let waiting = vec![
            step(1, 2, "a", Phase::Passed, "nixos"),
            step(2, 2, "b", Phase::Queued, "nixos"),
        ];
        assert_eq!(summarise(2, &waiting).unwrap().state, PlanState::Waiting);
        let mut done = vec![step(1, 2, "a", Phase::Passed, "nixos")];
        done[0].finished_at_ms = Some(9_000);
        let s = summarise(2, &done).unwrap();
        assert_eq!(s.state, PlanState::Finished);
        assert_eq!(s.finished_at_ms, Some(9_000));
        assert!(summarise(2, &[]).is_none());
    }

    // R-740
    #[test]
    fn r740_a_step_a_peer_holds_back_counts_as_waiting() {
        let mut j = step(1, 2, "a", Phase::Running, "nixos");
        j.remote_wait = Some("needs a slot".into());
        assert_eq!(StepState::of(&j), StepState::Waiting);
    }

    // R-742
    #[test]
    fn r742_a_step_label_drops_the_group_prefix() {
        assert_eq!(step_label("tests.test-calls"), "test-calls");
        assert_eq!(step_label("lint"), "lint");
        assert_eq!(step_label("a."), "a.");
    }

    // R-740
    #[test]
    fn r740_plan_ids_keep_first_seen_order() {
        let jobs = vec![
            step(1, 5, "a", Phase::Passed, "x"),
            step(2, 3, "a", Phase::Passed, "x"),
            step(3, 5, "b", Phase::Passed, "x"),
        ];
        assert_eq!(plan_ids(&jobs), vec![5, 3]);
    }
}
