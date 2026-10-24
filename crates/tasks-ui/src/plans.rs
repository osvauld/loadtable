//! Plans in the job list: one parent row per plan with its steps underneath (R-740..R-746).
//! Pure: the grouping, the texts and the order of the visible rows are decided here.

use crate::format;
use crate::model::{self, JobRow, PhaseClass, Section, StateCell, Tab, ViewModel};
use std::collections::BTreeMap;
use tasks_daemon::api::{RunningView, WaiterView};
use tasks_daemon::plan_view::{self, PlanState, PlanSummary, StepState};

/// A plan with up to this many steps opens by itself while it runs.
pub const OPEN_WHILE_RUNNING_UP_TO: usize = 6;

/// The selection key of a plan's parent row. Job ids are positive, so a plan takes the
/// negative of its id and the one `selected` slot holds either.
pub fn plan_key(plan_id: i64) -> i64 {
    -plan_id
}

/// The plan a selection key stands for, if it is one.
pub fn plan_of_key(key: i64) -> Option<i64> {
    (key < 0).then_some(-key)
}

/// One plan as a parent row.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRow {
    /// The plan id.
    pub plan_id: i64,
    /// The table the plan sits in, by its overall state.
    pub section: Section,
    /// The counts and times.
    pub summary: PlanSummary,
    /// `suite core-split · plan 2`, or `plan 2`.
    pub title: String,
    /// `12 of 40 done · 3 running · 1 failed`.
    pub progress_text: String,
    /// Owner up to `@`.
    pub agent: String,
    /// Full owner, for the tooltip.
    pub owner: String,
    /// The agent's hue index.
    pub hue: usize,
    /// The coding-agent product the name mentions.
    pub vendor: Option<&'static str>,
    /// `nixos` or `nixos +1`.
    pub machine: String,
    /// Steps done over steps, for the bar.
    pub progress: f32,
    /// The parent's state cell.
    pub state: StateCell,
    /// Seconds since the plan finished, once it has.
    pub finished_ago_secs: Option<u64>,
    /// Whether it opens without being asked: a failed step, or a short plan that runs.
    pub open_by_default: bool,
    /// The steps: failed first, then running, waiting, cancelled, passed; by id within.
    pub steps: Vec<JobRow>,
    /// Orders the plan among the rows of its table.
    order: i64,
}

/// One entry of a table: an ordinary job or a plan.
#[derive(Debug, Clone, Copy)]
pub enum Entry<'a> {
    /// A job that belongs to no plan.
    Job(&'a JobRow),
    /// A plan with its steps.
    Plan(&'a PlanRow),
}

pub use tasks_daemon::plan_view::step_label;

fn section_of_state(s: PlanState) -> Section {
    match s {
        PlanState::Running => Section::Running,
        PlanState::Waiting => Section::Queued,
        PlanState::Finished => Section::Recent,
    }
}

/// The latest start or finish the daemon expects among the open steps; `None` as soon as one
/// open step has no estimate, because then the plan's end is not known.
fn plan_eta(
    steps: &[JobRow],
    running: &BTreeMap<i64, &RunningView>,
    waiting: &BTreeMap<i64, &WaiterView>,
) -> Option<f64> {
    let mut worst: Option<f64> = None;
    for s in steps {
        let e = match s.section {
            Section::Running => running.get(&s.id).and_then(|r| r.eta_secs),
            Section::Queued => waiting.get(&s.id).and_then(|w| w.start_estimate_secs),
            Section::Recent => continue,
        }?;
        worst = Some(worst.map_or(e, |w| w.max(e)));
    }
    worst
}

fn secs(from: i64, to: i64) -> f64 {
    ((to - from).max(0) as f64) / 1000.0
}

fn state_cell(
    sum: &PlanSummary,
    steps: &[JobRow],
    now_ms: i64,
    eta: Option<f64>,
    progress: f32,
) -> StateCell {
    match sum.state {
        PlanState::Running => {
            let elapsed = format::duration(secs(sum.started_at_ms.unwrap_or(now_ms), now_ms));
            StateCell {
                bar: Some(progress),
                head: String::new(),
                tone: PhaseClass::Running,
                left: format!("{} running", sum.running),
                right: eta.map_or(elapsed, |e| format!("~{} left", format::coarse(e))),
            }
        }
        PlanState::Waiting => {
            let first = steps
                .iter()
                .filter(|s| s.section == Section::Queued)
                .min_by_key(|s| (s.position.unwrap_or(usize::MAX), s.id));
            let why = first
                .and_then(|s| s.wait.as_deref())
                .map(model::wait_phrase)
                .unwrap_or_default();
            StateCell {
                bar: None,
                head: why,
                tone: PhaseClass::Waiting,
                left: format!("{} waiting", sum.waiting),
                right: format::duration(secs(sum.submitted_at_ms, now_ms)),
            }
        }
        PlanState::Finished => {
            let end = sum.finished_at_ms.unwrap_or(now_ms);
            let ran = format::duration(secs(sum.started_at_ms.unwrap_or(sum.submitted_at_ms), end));
            let (head, tone, left) = if sum.failed > 0 {
                let names: Vec<&str> = steps
                    .iter()
                    .filter(|s| StepState::of(&s.job) == StepState::Failed)
                    .filter_map(|s| s.step.as_deref())
                    .take(2)
                    .collect();
                (
                    format!("Failed {} of {}", sum.failed, sum.total),
                    PhaseClass::Failed,
                    names.join(", "),
                )
            } else if sum.cancelled > 0 {
                ("Cancelled".to_string(), PhaseClass::Neutral, String::new())
            } else {
                (
                    format!("Passed {}/{} in {ran}", sum.passed, sum.total),
                    PhaseClass::Passed,
                    String::new(),
                )
            };
            StateCell {
                bar: None,
                head,
                tone,
                left,
                right: format::ago_words(secs(end, now_ms) as u64),
            }
        }
    }
}

fn progress_text(sum: &PlanSummary) -> String {
    let mut parts = vec![format!("{} of {} done", sum.done(), sum.total)];
    for (n, word) in [
        (sum.running, "running"),
        (sum.waiting, "waiting"),
        (sum.failed, "failed"),
        (sum.cancelled, "cancelled"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    parts.join(" · ")
}

/// Groups the job rows that belong to plans into plan rows, newest-relevant first as the
/// tables will order them.
pub fn build(
    rows: &[JobRow],
    running: &BTreeMap<i64, &RunningView>,
    waiting: &BTreeMap<i64, &WaiterView>,
    now_ms: i64,
) -> Vec<PlanRow> {
    let jobs: Vec<_> = rows.iter().map(|r| r.job.clone()).collect();
    let mut out = Vec::new();
    for pid in plan_view::plan_ids(&jobs) {
        let mut steps: Vec<JobRow> = rows
            .iter()
            .filter(|r| r.job.plan.as_ref().is_some_and(|(p, _)| *p == pid))
            .cloned()
            .collect();
        let Some(sum) = plan_view::summarise(pid, steps.iter().map(|r| &r.job)) else {
            continue;
        };
        steps.sort_by_key(|s| (StepState::of(&s.job).rank(), s.id));
        let section = section_of_state(sum.state);
        let progress = (sum.done() as f32 / sum.total.max(1) as f32).clamp(0.0, 1.0);
        let eta = (sum.state == PlanState::Running)
            .then(|| plan_eta(&steps, running, waiting))
            .flatten();
        let agent = format::agent_of(&sum.owner).to_string();
        let order = match section {
            Section::Running => sum.started_at_ms.unwrap_or(i64::MAX),
            Section::Queued => steps
                .iter()
                .filter_map(|s| s.position)
                .min()
                .map_or(i64::MAX, |p| p as i64),
            Section::Recent => -sum.finished_at_ms.unwrap_or(sum.submitted_at_ms),
        };
        out.push(PlanRow {
            plan_id: pid,
            section,
            title: match &sum.suite {
                Some(s) => format!("suite {s} · plan {pid}"),
                None => format!("plan {pid}"),
            },
            progress_text: progress_text(&sum),
            hue: format::agent_hue(&agent),
            vendor: format::vendor_of(&agent),
            machine: sum.machine_text(),
            progress,
            state: state_cell(&sum, &steps, now_ms, eta, progress),
            finished_ago_secs: sum.finished_at_ms.map(|f| secs(f, now_ms).max(0.0) as u64),
            open_by_default: sum.failed > 0
                || (sum.state == PlanState::Running && sum.total <= OPEN_WHILE_RUNNING_UP_TO),
            owner: sum.owner.clone(),
            agent,
            steps,
            order,
            summary: sum,
        });
    }
    out
}

impl PlanRow {
    /// The compact step list of the side panel.
    pub fn step_lines(&self, now_ms: i64) -> Vec<StepLine> {
        self.steps
            .iter()
            .map(|s| {
                let (state, tone, time) = match StepState::of(&s.job) {
                    StepState::Passed => ("passed", PhaseClass::Passed, ran(s, now_ms)),
                    StepState::Failed => ("failed", PhaseClass::Failed, ran(s, now_ms)),
                    StepState::Cancelled => ("cancelled", PhaseClass::Neutral, String::new()),
                    StepState::Running => ("running", PhaseClass::Running, ran(s, now_ms)),
                    StepState::Waiting => (
                        "waiting",
                        PhaseClass::Waiting,
                        format::duration(secs(s.job.submitted_at_ms, now_ms)),
                    ),
                };
                StepLine {
                    id: s.id,
                    name: s.step.clone().unwrap_or_else(|| s.command.clone()),
                    state: state.to_string(),
                    tone,
                    time,
                }
            })
            .collect()
    }

    /// The facts of the side panel: label and value.
    pub fn facts(&self, now_ms: i64) -> Vec<(&'static str, String)> {
        let s = &self.summary;
        let mut facts = vec![
            (
                "Suite",
                s.suite.clone().unwrap_or_else(|| "not recorded".into()),
            ),
            ("Owner", self.owner.clone()),
            ("Steps", s.counts_text()),
        ];
        if !self.machine.is_empty() {
            facts.push(("Machine", self.machine.clone()));
        }
        let wall = match (s.state, s.started_at_ms) {
            (PlanState::Finished, started) => {
                let from = started.unwrap_or(s.submitted_at_ms);
                format!(
                    "{} in total",
                    format::duration(secs(from, s.finished_at_ms.unwrap_or(now_ms)))
                )
            }
            (_, Some(started)) => format!("{} so far", format::duration(secs(started, now_ms))),
            (_, None) => format!(
                "not started, queued {}",
                format::duration(secs(s.submitted_at_ms, now_ms))
            ),
        };
        facts.push(("Wall time", wall));
        facts
    }
}

fn ran(s: &JobRow, now_ms: i64) -> String {
    match s.job.started_at_ms {
        Some(st) => format::duration(secs(st, s.job.finished_at_ms.unwrap_or(now_ms))),
        None => String::new(),
    }
}

/// What a step row says on its one line: the state in words, its colour and the time at the
/// right. Short, because a plan can have hundreds of them.
pub fn child_state(row: &JobRow) -> (String, PhaseClass, String) {
    let st = &row.state;
    match row.section {
        Section::Running => ("Running".to_string(), PhaseClass::Running, st.right.clone()),
        // the reason is the plan's and the step's detail; here only the place in line
        Section::Queued => (
            match row.position {
                Some(n) => format!("Waiting · {}", format::ordinal(n)),
                None => "Waiting".to_string(),
            },
            PhaseClass::Waiting,
            st.right.clone(),
        ),
        // when it ended is the plan's news, and the outcome needs the room
        Section::Recent => (st.head.clone(), st.tone, String::new()),
    }
}

/// One line of the plan's step list.
#[derive(Debug, Clone, PartialEq)]
pub struct StepLine {
    /// The step's job id.
    pub id: i64,
    /// The step name.
    pub name: String,
    /// `failed`, `running`, `waiting`, `passed`, `cancelled`.
    pub state: String,
    /// Its colour class.
    pub tone: PhaseClass,
    /// How long it ran or has waited.
    pub time: String,
}

impl ViewModel {
    /// The plan with this id.
    pub fn plan(&self, plan_id: i64) -> Option<&PlanRow> {
        self.plans.iter().find(|p| p.plan_id == plan_id)
    }

    /// The plan a job is a step of.
    pub fn plan_of_job(&self, job: i64) -> Option<&PlanRow> {
        self.plans
            .iter()
            .find(|p| p.steps.iter().any(|s| s.id == job))
    }

    /// What `section` lists on `tab`: ordinary jobs and plans in one order. Steps of plans are
    /// not entries of their own; they hang under their plan.
    pub fn entries(&self, tab: Tab, section: Section) -> Vec<Entry<'_>> {
        let recent_only = |ago: Option<u64>| {
            tab == Tab::History || ago.is_some_and(|s| s <= model::FINISHED_WINDOW_SECS)
        };
        let jobs: &[JobRow] = match section {
            Section::Running => &self.running,
            Section::Queued => &self.queued,
            Section::Recent => &self.recent,
        };
        let mut out: Vec<(i64, i64, Entry<'_>)> = Vec::new();
        for r in jobs.iter().filter(|r| r.job.plan.is_none()) {
            if section == Section::Recent && !recent_only(r.finished_ago_secs) {
                continue;
            }
            let key = match section {
                Section::Running => r.job.started_at_ms.unwrap_or(i64::MAX),
                Section::Queued => r.position.map_or(i64::MAX, |p| p as i64),
                Section::Recent => -r.job.finished_at_ms.unwrap_or(r.job.submitted_at_ms),
            };
            out.push((key, r.id, Entry::Job(r)));
        }
        for p in self.plans.iter().filter(|p| p.section == section) {
            if section == Section::Recent && !recent_only(p.finished_ago_secs) {
                continue;
            }
            out.push((p.order, plan_key(p.plan_id), Entry::Plan(p)));
        }
        out.sort_by_key(|(k, id, _)| (*k, *id));
        out.into_iter().map(|(_, _, e)| e).collect()
    }
}

/// The ids of the rows on screen from top to bottom for the arrow keys: a plan's key, then its
/// steps when it is open.
pub fn visible_ids(vm: &ViewModel, tab: Tab, is_open: &dyn Fn(&PlanRow) -> bool) -> Vec<i64> {
    let sections: &[Section] = if tab == Tab::History {
        &[Section::Recent]
    } else {
        &[Section::Running, Section::Queued, Section::Recent]
    };
    let mut ids = Vec::new();
    for s in sections {
        for e in vm.entries(tab, *s) {
            match e {
                Entry::Job(r) => ids.push(r.id),
                Entry::Plan(p) => {
                    ids.push(plan_key(p.plan_id));
                    if is_open(p) {
                        ids.extend(p.steps.iter().map(|s| s.id));
                    }
                }
            }
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use tasks_core::Phase;

    fn vm() -> ViewModel {
        model::build(&fixtures::with_plan(), 0)
    }

    // R-740
    #[test]
    fn r740_a_plan_is_one_row_in_the_section_of_its_overall_state() {
        let vm = vm();
        let p = vm.plan(2).unwrap();
        assert_eq!(p.section, Section::Running);
        assert_eq!(p.steps.len(), 40);
        let running = vm.entries(Tab::Queue, Section::Running);
        let plans = running
            .iter()
            .filter(|e| matches!(e, Entry::Plan(_)))
            .count();
        assert_eq!(plans, 1);
        // the Queue tab's number counts the plan once
        assert_eq!(vm.open_jobs(), 2);
        // none of its 40 steps is an entry of any table
        for s in [Section::Running, Section::Queued, Section::Recent] {
            for e in vm.entries(Tab::Queue, s) {
                if let Entry::Job(r) = e {
                    assert!(r.job.plan.is_none(), "#{} is a step", r.id);
                }
            }
        }
    }

    // R-741
    #[test]
    fn r741_the_parent_row_says_what_the_plan_is_and_how_far_it_got() {
        let vm = vm();
        let p = vm.plan(2).unwrap();
        assert_eq!(p.title, "suite core-split · plan 2");
        assert_eq!(p.machine, "nixos");
        assert_eq!(
            p.progress_text,
            "7 of 40 done · 3 running · 30 waiting · 1 failed"
        );
        assert!((p.progress - 7.0 / 40.0).abs() < 1e-6);
        assert_eq!(p.state.left, "3 running");
        assert!(p.state.bar.is_some());
    }

    // R-741
    #[test]
    fn r741_a_plan_without_a_suite_name_is_titled_by_its_id() {
        let mut d = fixtures::with_plan();
        for j in &mut d.jobs {
            j.suite = None;
        }
        let vm = model::build(&d, 0);
        assert_eq!(vm.plan(2).unwrap().title, "plan 2");
    }

    // R-741
    #[test]
    fn r741_finished_plans_say_passed_failed_or_cancelled() {
        let mut d = fixtures::with_plan();
        for j in &mut d.jobs {
            if j.plan.is_some() {
                j.phase = Phase::Passed;
                j.started_at_ms = Some(fixtures::NOW_MS - 1_000_000);
                j.finished_at_ms = Some(fixtures::NOW_MS - 120_000);
            }
        }
        let vm = model::build(&d, 0);
        let p = vm.plan(2).unwrap();
        assert_eq!(p.section, Section::Recent);
        assert_eq!(p.state.head, "Passed 40/40 in 14m40s");
        assert_eq!(p.state.tone, PhaseClass::Passed);
        let mut d2 = d.clone();
        if let Some(j) = d2.jobs.iter_mut().find(|j| j.plan.is_some()) {
            j.phase = Phase::Failed { exit: Some(101) };
        }
        let vm2 = model::build(&d2, 0);
        let p2 = vm2.plan(2).unwrap();
        assert_eq!(p2.state.head, "Failed 1 of 40");
        assert_eq!(p2.state.tone, PhaseClass::Failed);
        let mut d3 = d;
        if let Some(j) = d3.jobs.iter_mut().find(|j| j.plan.is_some()) {
            j.phase = Phase::Cancelled;
        }
        let vm3 = model::build(&d3, 0);
        assert_eq!(vm3.plan(2).unwrap().state.head, "Cancelled");
    }

    // R-742
    #[test]
    fn r742_steps_show_their_name_without_the_plan_prefix_and_their_flags() {
        let vm = vm();
        let p = vm.plan(2).unwrap();
        let build = p
            .steps
            .iter()
            .find(|s| s.step.as_deref() == Some("build"))
            .unwrap();
        assert_eq!(
            build.job.plan.as_ref().map(|(_, n)| n.as_str()),
            Some("tests.build")
        );
        let light = p
            .steps
            .iter()
            .find(|s| s.step.as_deref() == Some("test-link"))
            .unwrap();
        assert_eq!(light.tags, vec!["light".to_string()]);
        let waiting = p
            .steps
            .iter()
            .find(|s| s.section == Section::Queued)
            .unwrap();
        let (head, tone, _) = child_state(waiting);
        assert!(head.starts_with("Waiting"), "{head}");
        assert_eq!(tone, PhaseClass::Waiting);
        let failed = p.steps.first().unwrap();
        let (head, tone, right) = child_state(failed);
        assert!(head.starts_with("Failed"), "{head}");
        assert_eq!((tone, right.as_str()), (PhaseClass::Failed, ""));
        assert_eq!(step_label("tests.test-calls"), "test-calls");
    }

    // R-742
    #[test]
    fn r742_a_job_without_a_plan_stays_an_ordinary_row() {
        let vm = vm();
        let rows = vm.entries(Tab::Queue, Section::Running);
        assert!(
            rows.iter()
                .any(|e| matches!(e, Entry::Job(r) if r.job.plan.is_none()))
        );
    }

    // R-742
    #[test]
    fn r742_a_waiting_plan_takes_its_first_steps_wait_reason() {
        let mut d = fixtures::with_plan();
        for j in &mut d.jobs {
            if j.plan.is_some() && j.phase == Phase::Running {
                j.phase = Phase::Passed;
                j.finished_at_ms = Some(fixtures::NOW_MS - 60_000);
            }
        }
        let vm = model::build(&d, 0);
        let p = vm.plan(2).unwrap();
        assert_eq!(p.section, Section::Queued);
        assert_eq!(p.state.tone, PhaseClass::Waiting);
        assert!(!p.state.head.is_empty());
    }

    // R-743
    #[test]
    fn r743_a_plan_opens_by_default_with_a_failed_step_or_short_and_running() {
        let vm = vm();
        // plan 2 has a failed step
        assert!(vm.plan(2).unwrap().open_by_default);
        let mut d = fixtures::with_plan();
        for j in &mut d.jobs {
            if j.phase.word() == "failed" {
                j.phase = Phase::Passed;
            }
        }
        let vm = model::build(&d, 0);
        assert!(!vm.plan(2).unwrap().open_by_default, "40 steps stay shut");
        // keep only 4 steps
        d.jobs
            .retain(|j| j.plan.is_none() || [330, 331, 337].contains(&j.id.0));
        let vm = model::build(&d, 0);
        let p = vm.plan(2).unwrap();
        assert!(p.summary.total <= OPEN_WHILE_RUNNING_UP_TO);
        assert!(p.open_by_default);
    }

    // R-744
    #[test]
    fn r744_arrow_keys_walk_only_the_rows_on_screen() {
        let vm = vm();
        let closed = visible_ids(&vm, Tab::Queue, &|_| false);
        let open = visible_ids(&vm, Tab::Queue, &|_| true);
        assert!(closed.contains(&plan_key(2)));
        assert!(!closed.contains(&330));
        assert_eq!(open.len(), closed.len() + 40);
        let at = open.iter().position(|i| *i == plan_key(2)).unwrap();
        let steps = &open[at + 1..at + 41];
        let mut want: Vec<i64> = vm.plan(2).unwrap().steps.iter().map(|s| s.id).collect();
        assert_eq!(steps, want.as_slice());
        want.sort();
        assert_eq!(want.len(), 40);
        // stepping down from the parent lands on its first step, from the last step leaves it
        assert_eq!(
            model::step_selection(&open, Some(plan_key(2)), 1),
            open.get(at + 1).copied()
        );
        assert_eq!(
            model::step_selection(&closed, Some(plan_key(2)), 1),
            closed.get(at + 1).copied()
        );
    }

    // R-745
    #[test]
    fn r745_the_plan_panel_lists_failed_then_running_then_waiting_then_passed() {
        let vm = vm();
        let p = vm.plan(2).unwrap();
        let lines = p.step_lines(vm.now_ms);
        let rank = |s: &str| {
            ["failed", "running", "waiting", "cancelled", "passed"]
                .iter()
                .position(|w| *w == s)
                .unwrap()
        };
        assert!(
            lines
                .windows(2)
                .all(|w| rank(&w[0].state) <= rank(&w[1].state))
        );
        assert_eq!(lines[0].name, "test-chat_shards");
        assert_eq!(lines[0].state, "failed");
        let facts = p.facts(vm.now_ms);
        assert!(
            facts
                .iter()
                .any(|(k, v)| *k == "Suite" && v == "core-split")
        );
        assert!(facts.iter().any(|(k, _)| *k == "Wall time"));
    }

    // R-745
    #[test]
    fn r745_a_plan_key_is_not_a_job_id() {
        assert_eq!(plan_of_key(plan_key(7)), Some(7));
        assert_eq!(plan_of_key(7), None);
    }

    // R-746
    #[test]
    fn r746_the_history_tab_lists_finished_plans_once() {
        let mut d = fixtures::with_plan();
        for j in &mut d.jobs {
            if j.plan.is_some() {
                j.phase = Phase::Passed;
                j.started_at_ms = Some(fixtures::NOW_MS - 90_000_000);
                j.finished_at_ms = Some(fixtures::NOW_MS - 86_400_000);
            }
        }
        let vm = model::build(&d, 0);
        // a day old: not in the Queue tab, in History
        assert!(
            vm.entries(Tab::Queue, Section::Recent)
                .iter()
                .all(|e| !matches!(e, Entry::Plan(_)))
        );
        let hist = vm.entries(Tab::History, Section::Recent);
        assert_eq!(
            hist.iter().filter(|e| matches!(e, Entry::Plan(_))).count(),
            1
        );
    }
}
