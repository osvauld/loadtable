//! Structured submission: a plan of named steps with `after` dependencies, run as ordinary jobs.
//!
//! A step is one `tasks run` job, so admission, placement, ETA, logs and cancellation apply to it
//! exactly as to any other job; nothing here touches the ledger or the daemon protocol. This module
//! is the pure part: parsing and validating a plan, the [`Scheduler`] that says which steps may
//! start, and the summary text. The driver that spawns the jobs is `src/bin/tasks/steps.rs`.
//!
//! * Independent steps start together (their jobs queue in admission like any others), so a long
//!   test target can run beside the short ones instead of behind them.
//! * By default the first failure stops new steps from starting ([`OnFailure::Stop`]); steps that
//!   already run finish. With [`OnFailure::Continue`] only the steps that depend on a failed one
//!   are skipped.
//! * The failed step is named in the summary with its job id, so nobody searches a merged log.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const MAX_STEPS: usize = 64;
pub const MAX_NAME: usize = 64;
pub const MAX_ARGS: usize = 256;
pub const MAX_ARG_BYTES: usize = 4096;

/// A plan as written by the caller (JSON file, stdin or an MCP field).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Unique within the plan: letters, digits, `_`, `-` and `.`.
    pub name: String,
    pub argv: Vec<String>,
    /// Steps that must pass before this one starts.
    #[serde(default)]
    pub after: Vec<String>,
    /// Use the run admission class (already-built commands that need no compile slot).
    #[serde(default)]
    pub run: bool,
    /// Same meaning as `tasks run --broad-test` for this step.
    #[serde(default)]
    pub broad_test: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnFailure {
    /// No new step starts after a failure.
    Stop,
    /// Only steps depending on a failed or skipped step are skipped.
    Continue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Running,
    Passed,
    Failed,
    /// Never started: a step it needs failed, or the plan stopped at a failure.
    Skipped,
}

impl State {
    pub fn finished(self) -> bool {
        matches!(self, Self::Passed | Self::Failed | Self::Skipped)
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

impl Plan {
    pub fn parse(text: &str) -> Result<Self, String> {
        let plan: Plan = serde_json::from_str(text).map_err(|e| format!("plan: {e}"))?;
        plan.validate()?;
        Ok(plan)
    }

    /// Every rule a plan must meet before anything is submitted.
    pub fn validate(&self) -> Result<(), String> {
        if self.steps.is_empty() {
            return Err("plan: needs at least one step".into());
        }
        if self.steps.len() > MAX_STEPS {
            return Err(format!("plan: at most {MAX_STEPS} steps"));
        }
        let mut seen = HashSet::new();
        for step in &self.steps {
            if !valid_name(&step.name) {
                return Err(format!(
                    "plan: step name {:?} must be 1..={MAX_NAME} letters, digits, _ - or .",
                    step.name
                ));
            }
            if !seen.insert(step.name.as_str()) {
                return Err(format!("plan: step {:?} is listed twice", step.name));
            }
            if step.argv.is_empty() || step.argv[0].is_empty() {
                return Err(format!("plan: step {:?} has no command", step.name));
            }
            if step.argv.len() > MAX_ARGS || step.argv.iter().any(|a| a.len() > MAX_ARG_BYTES) {
                return Err(format!(
                    "plan: step {:?} has more than {MAX_ARGS} arguments or one over {MAX_ARG_BYTES} bytes",
                    step.name
                ));
            }
            if step.argv.iter().any(|a| a.contains('\0')) {
                return Err(format!(
                    "plan: step {:?} has a NUL in its command",
                    step.name
                ));
            }
        }
        for step in &self.steps {
            let mut deps = HashSet::new();
            for dep in &step.after {
                if dep == &step.name {
                    return Err(format!("plan: step {:?} waits for itself", step.name));
                }
                if !seen.contains(dep.as_str()) {
                    return Err(format!(
                        "plan: step {:?} comes after {dep:?}, which is not a step",
                        step.name
                    ));
                }
                if !deps.insert(dep.as_str()) {
                    return Err(format!(
                        "plan: step {:?} lists {dep:?} twice in after",
                        step.name
                    ));
                }
            }
        }
        if let Some(name) = self.cycle() {
            return Err(format!(
                "plan: steps depend on each other in a loop (at {name:?})"
            ));
        }
        Ok(())
    }

    /// A step on a dependency cycle, if there is one (Kahn's algorithm).
    fn cycle(&self) -> Option<&str> {
        let index: HashMap<&str, usize> = self
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.as_str(), i))
            .collect();
        let mut waiting: Vec<usize> = self.steps.iter().map(|s| s.after.len()).collect();
        let mut queue: Vec<usize> = (0..self.steps.len()).filter(|&i| waiting[i] == 0).collect();
        let mut done = 0;
        while let Some(i) = queue.pop() {
            done += 1;
            for (j, other) in self.steps.iter().enumerate() {
                if other.after.iter().any(|d| index[d.as_str()] == i) {
                    waiting[j] -= 1;
                    if waiting[j] == 0 {
                        queue.push(j);
                    }
                }
            }
        }
        (done < self.steps.len()).then(|| {
            let stuck = (0..self.steps.len()).find(|&i| waiting[i] > 0).unwrap_or(0);
            self.steps[stuck].name.as_str()
        })
    }

    /// Advice for the caller before submitting; never a refusal.
    pub fn notes(&self) -> Vec<String> {
        let mut out = vec![];
        for step in &self.steps {
            let shell = matches!(step.argv[0].rsplit('/').next(), Some("sh" | "bash" | "zsh"))
                && step.argv.iter().any(|a| a == "-c" || a == "-lc");
            let text = step.argv.join(" ");
            if shell && text.contains("cargo") {
                out.push(format!(
                    "step {:?} wraps cargo in a shell: its flags (-j, --test) and its progress are \
                     invisible to the queue, and a pipe such as `| grep` hides the compile lines. \
                     Give cargo as the command itself, one step per target",
                    step.name
                ));
            }
        }
        out
    }
}

/// What the driver reports back about a step.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub job: Option<i64>,
    pub seconds: Option<f64>,
}

/// Which steps may start, given what has finished. Pure: the driver feeds it events.
#[derive(Debug)]
pub struct Scheduler {
    plan: Plan,
    on_failure: OnFailure,
    states: Vec<State>,
    reports: Vec<Report>,
    failed_once: bool,
}

impl Scheduler {
    pub fn new(plan: Plan, on_failure: OnFailure) -> Self {
        let n = plan.steps.len();
        Self {
            plan,
            on_failure,
            states: vec![State::Pending; n],
            reports: vec![Report::default(); n],
            failed_once: false,
        }
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    pub fn state(&self, i: usize) -> State {
        self.states[i]
    }

    pub fn report(&self, i: usize) -> &Report {
        &self.reports[i]
    }

    fn index(&self, name: &str) -> usize {
        self.plan
            .steps
            .iter()
            .position(|s| s.name == name)
            .unwrap_or(0)
    }

    /// Marks steps that can never run as skipped, then returns the pending steps whose
    /// dependencies all passed. Each returned step is marked running.
    pub fn start_ready(&mut self) -> Vec<usize> {
        loop {
            let mut changed = false;
            for i in 0..self.states.len() {
                if self.states[i] != State::Pending {
                    continue;
                }
                let blocked = self.plan.steps[i]
                    .after
                    .iter()
                    .any(|d| matches!(self.states[self.index(d)], State::Failed | State::Skipped));
                let stopped = self.failed_once && self.on_failure == OnFailure::Stop;
                if blocked || stopped {
                    self.states[i] = State::Skipped;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut started = vec![];
        for i in 0..self.states.len() {
            if self.states[i] == State::Pending
                && self.plan.steps[i]
                    .after
                    .iter()
                    .all(|d| self.states[self.index(d)] == State::Passed)
            {
                self.states[i] = State::Running;
                started.push(i);
            }
        }
        started
    }

    pub fn finish(&mut self, i: usize, passed: bool, report: Report) {
        self.states[i] = if passed { State::Passed } else { State::Failed };
        self.reports[i] = report;
        if !passed {
            self.failed_once = true;
        }
    }

    /// Nothing is running and nothing can start.
    pub fn done(&self) -> bool {
        self.states.iter().all(|s| s.finished())
    }

    pub fn passed(&self) -> bool {
        self.states.iter().all(|s| *s == State::Passed)
    }

    /// Counts for a progress line: (finished, total).
    pub fn progress(&self) -> (usize, usize) {
        (
            self.states.iter().filter(|s| s.finished()).count(),
            self.states.len(),
        )
    }

    /// The table and the named failures printed when the plan ends.
    pub fn summary(&self) -> String {
        let width = self
            .plan
            .steps
            .iter()
            .map(|s| s.name.len())
            .max()
            .unwrap_or(4)
            .max(4);
        let mut out = format!(
            "{:<width$}  {:<8}  {:>6}  {:>8}  AFTER\n",
            "STEP", "STATE", "JOB", "TIME"
        );
        for (i, step) in self.plan.steps.iter().enumerate() {
            let r = &self.reports[i];
            out.push_str(&format!(
                "{:<width$}  {:<8}  {:>6}  {:>8}  {}\n",
                step.name,
                state_name(self.states[i]),
                r.job.map_or("-".into(), |j| j.to_string()),
                r.seconds.map_or("-".into(), seconds_text),
                if step.after.is_empty() {
                    "-".to_string()
                } else {
                    step.after.join(",")
                },
            ));
        }
        let failed: Vec<String> = self
            .plan
            .steps
            .iter()
            .enumerate()
            .filter(|(i, _)| self.states[*i] == State::Failed)
            .map(|(i, s)| match self.reports[i].job {
                Some(job) => format!("{} (job {job}; tasks logs {job})", s.name),
                None => s.name.clone(),
            })
            .collect();
        let skipped: Vec<&str> = self
            .plan
            .steps
            .iter()
            .enumerate()
            .filter(|(i, _)| self.states[*i] == State::Skipped)
            .map(|(_, s)| s.name.as_str())
            .collect();
        if !failed.is_empty() {
            out.push_str(&format!("failed: {}\n", failed.join("; ")));
        }
        if !skipped.is_empty() {
            out.push_str(&format!("not run: {}\n", skipped.join(", ")));
        }
        let (done, total) = self.progress();
        let passed = self.states.iter().filter(|s| **s == State::Passed).count();
        out.push_str(&format!(
            "steps: {passed}/{total} passed ({done} finished)\n"
        ));
        out
    }
}

pub fn state_name(state: State) -> &'static str {
    match state {
        State::Pending => "pending",
        State::Running => "running",
        State::Passed => "passed",
        State::Failed => "failed",
        State::Skipped => "skipped",
    }
}

pub fn seconds_text(s: f64) -> String {
    let s = s.round() as u64;
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, s) => format!("{h}h{m:02}m{s:02}s"),
    }
}

/// The job id in a `tasks run` client's output: the line that says the job stays live if the
/// client stops, or the final `job N passed|failed ...` line. The last one seen wins.
pub fn job_id_from_line(line: &str) -> Option<i64> {
    let line = line.trim();
    let rest = line
        .strip_prefix("tasks: if this client stops or times out, job ")
        .or_else(|| line.strip_prefix("job "))?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
#[path = "steps_tests.rs"]
mod tests;
