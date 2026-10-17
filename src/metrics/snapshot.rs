//! Assembles the v1 snapshot from ledger jobs plus sampled/derived extras.
//! `assemble` is pure (fixture-testable); file reads happen in the helpers.
use super::history::{self, History};
use super::slots::{Scan, Waiter, device_label};
use super::warmth::{self, StartWarmth};
use super::{
    EtaEstimate, FINISHED_JOBS, JobResult, JobView, Live, Machine, Progress, Queue,
    SNAPSHOT_VERSION, SlotKind, Snapshot, Warmth,
};
use crate::local_command::Resource;
use crate::{Job, Phase};
use std::collections::HashMap;
use std::path::Path;

/// Admission's current pressure blockers per gate (null: that gate admits).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Blockers {
    pub compile: Option<String>,
    pub run: Option<String>,
    pub offload: Option<String>,
}
impl Blockers {
    pub fn all(reason: &str) -> Self {
        Self {
            compile: Some(reason.into()),
            run: Some(reason.into()),
            offload: Some(reason.into()),
        }
    }
}

/// Per-job values that come from the sampler or the job directory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobExtras {
    pub live: Option<Live>,
    /// `Compiling` lines seen so far in the job log (active jobs).
    pub crates_so_far: Option<u64>,
    /// Start warmth (worker file, or the daemon's own first-sight check).
    pub start: Option<StartWarmth>,
    /// Unix time the current stage was admitted (`STAGE-admitted.json`).
    pub admitted_at: Option<i64>,
    /// (checkout, checkout_name) as the owner knows it.
    pub checkout: (String, String),
    /// Peer forwarding link of a forwarded job (`<state>/peer/links/<id>.json`).
    pub offload: Option<super::types::OffloadLink>,
}

pub struct Inputs<'a> {
    pub now: f64,
    pub machine: Machine,
    /// The jobs to show (see [`select`]).
    pub jobs: &'a [&'a Job],
    /// The whole ledger, for history.
    pub all_jobs: &'a [Job],
    pub extras: &'a HashMap<i64, JobExtras>,
    pub scan: &'a Scan,
    pub blockers: &'a Blockers,
    pub worker_running: bool,
}

/// Every non-terminal job plus the newest [`FINISHED_JOBS`] finished ones, by id.
pub fn select(jobs: &[Job]) -> Vec<&Job> {
    let mut finished: Vec<&Job> = jobs.iter().filter(|j| j.phase.terminal()).collect();
    finished.sort_by_key(|j| std::cmp::Reverse(j.id));
    finished.truncate(FINISHED_JOBS);
    let mut out: Vec<&Job> = jobs.iter().filter(|j| !j.phase.terminal()).collect();
    out.extend(finished);
    out.sort_by_key(|j| j.id);
    out
}

/// Laptop checkout for a worker-side tasks-offload path
/// (`.../offload/checkouts/<id>/<subdir>` with a sibling `<id>.identity`
/// holding `{"host":..,"toplevel":..}`), else the path itself. The name is
/// the basename of the Git top level (nearest `.git` entry) or of the path.
pub fn checkout_of(source: &str) -> (String, String) {
    let path = Path::new(source);
    if source.contains("/offload/checkouts/") {
        for dir in path.ancestors() {
            let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
                break;
            };
            if parent.file_name().is_none_or(|n| n != "checkouts") {
                continue;
            }
            let marker = parent.join(format!("{}.identity", name.to_string_lossy()));
            let Some(identity) = std::fs::read(&marker)
                .ok()
                .filter(|b| b.len() <= 16 * 1024)
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            else {
                break;
            };
            let Some(top) = identity["toplevel"].as_str() else {
                break;
            };
            let sub = path.strip_prefix(dir).unwrap_or(Path::new(""));
            let checkout = Path::new(top).join(sub);
            let name = Path::new(top)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| top.to_string());
            return (checkout.to_string_lossy().into_owned(), name);
        }
    }
    let top = path
        .ancestors()
        .find(|d| std::fs::symlink_metadata(d.join(".git")).is_ok())
        .unwrap_or(path);
    let name = top
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.to_string());
    (source.to_string(), name)
}

fn current_stage(phase: &Phase) -> Option<&'static str> {
    match phase {
        Phase::Building => Some("build"),
        Phase::Running => Some("run"),
        _ => None,
    }
}

/// When the job's current stage was admitted, from the worker's marker file.
pub fn admitted_at(state_dir: &Path, job: &Job) -> Option<i64> {
    let stage = current_stage(&job.phase)?;
    let path = super::sampler::job_dir(state_dir, job.id).join(format!("{stage}-admitted.json"));
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    value["at"].as_i64()
}

fn slot_kind(resource: &Resource) -> SlotKind {
    match resource {
        Resource::Compile => SlotKind::Compile,
        Resource::Run => SlotKind::Run,
        Resource::Offload => SlotKind::Offload,
        Resource::Device(_) => SlotKind::Device,
    }
}

/// The admission label a waiting job is queued under.
fn wanted_label(job: &Job, resource: &Resource, waiters: &[Waiter]) -> Option<String> {
    if let Some(w) = waiters.iter().find(|w| w.job == Some(job.id)) {
        return Some(w.label.clone());
    }
    match job.phase {
        Phase::WaitingBuild => Some("compile".into()),
        Phase::WaitingRun => Some(match resource {
            Resource::Offload => "offload".into(),
            _ => "run".into(),
        }),
        _ => None,
    }
}

fn queue(job: &Job, resource: &Resource, inputs: &Inputs) -> Option<Queue> {
    let id = job.id;
    let simple = |text: String| {
        Some(Queue {
            position: None,
            waiting_for: Some(text),
        })
    };
    match job.phase {
        Phase::Queued => return simple(format!("approval (tasks approve-command {id})")),
        Phase::Approved if !inputs.worker_running => {
            return simple("worker not running (tasks start-worker)".into());
        }
        Phase::Approved => return simple("worker pickup".into()),
        Phase::Preparing => {
            let detail = job.execution.as_ref().map(|e| e.detail.as_str());
            return detail
                .filter(|d| d.starts_with("Waiting"))
                .and_then(|d| simple(d.to_string()));
        }
        Phase::WaitingBuild | Phase::WaitingRun => (),
        _ => return None,
    }
    let waiters = &inputs.scan.waiters;
    let label = wanted_label(job, resource, waiters)?;
    let same: Vec<&Waiter> = waiters.iter().filter(|w| w.label == label).collect();
    let mine = same.iter().position(|w| w.job == Some(id));
    let position = mine.map(|i| i as u32 + 1);
    let blocker = match label.as_str() {
        "compile" => inputs.blockers.compile.clone(),
        "run" => inputs.blockers.run.clone(),
        "offload" => inputs.blockers.offload.clone(),
        _ => None,
    };
    let kind = match label.as_str() {
        "compile" => Some(SlotKind::Compile),
        "run" => Some(SlotKind::Run),
        "offload" => Some(SlotKind::Offload),
        _ => None,
    };
    let holders = |kind: Option<SlotKind>| -> Option<String> {
        let slots: Vec<_> = inputs
            .scan
            .slots
            .iter()
            .filter(|s| match kind {
                Some(k) => s.kind == k,
                None => device_label(&s.name).as_deref() == Some(label.as_str()),
            })
            .collect();
        if slots.is_empty() || slots.iter().any(|s| !s.busy) {
            return None;
        }
        let names: Vec<String> = slots
            .iter()
            .map(|s| match (s.holder_job, &s.holder_who) {
                (Some(j), _) => format!("#{j}"),
                (None, Some(who)) => who.clone(),
                (None, None) => "an untracked holder".into(),
            })
            .collect();
        Some(if slots.len() == 1 {
            format!("{label} slot held by {}", names[0])
        } else {
            format!(
                "all {} {label} slots busy ({})",
                slots.len(),
                names.join(", ")
            )
        })
    };
    let ahead = mine
        .and_then(|i| i.checked_sub(1))
        .map(|i| same[i])
        .or_else(|| same.last().copied().filter(|_| mine.is_none()));
    let waiting_for = if let Some(b) = blocker {
        Some(b)
    } else if let Some(text) = holders(kind) {
        Some(text)
    } else if let Some(w) = ahead {
        Some(match w.job {
            Some(j) => format!("behind #{j} in the {label} queue"),
            None => format!("behind {} in the {label} queue", w.who),
        })
    } else {
        Some("admission check".into())
    };
    Some(Queue {
        position,
        waiting_for,
    })
}

fn job_view(job: &Job, inputs: &Inputs, history: &History) -> JobView {
    let extras = inputs.extras.get(&job.id).cloned().unwrap_or_default();
    let argv = history::argv(job);
    let (command, kind_key) = history::names(job);
    let resource = history::resource(job);
    let cargo = super::command::is_cargo(&argv);
    let execution = job.execution.as_ref();
    let metrics = execution.and_then(|e| e.metrics.as_ref());
    let terminal = job.phase.terminal();
    // `Compiling` lines seen so far (live jobs only).
    let crates_now = extras.crates_so_far.unwrap_or(0);
    // Past the build stage of a two-stage recipe, no more crates will compile.
    let build_done = terminal
        || execution.is_some_and(|e| e.stages.iter().any(|s| s.stage == "build"))
            && matches!(job.phase, Phase::WaitingRun | Phase::Running);
    let (warmth, warmth_reason) = if terminal {
        match metrics {
            Some(m) => (m.warmth, m.warmth_reason.clone()),
            None => (
                Warmth::Unknown,
                "finished before metrics were recorded".to_string(),
            ),
        }
    } else {
        let start = extras.start.clone().unwrap_or_else(|| {
            warmth::at_start_unknown("start state not checked yet (job not picked up)")
        });
        if cargo {
            warmth::refine(
                &start,
                crates_now,
                history.reference_crates(&kind_key, job.id),
                build_done,
            )
        } else {
            (start.warmth, start.reason)
        }
    };
    // Live progress only; a finished job's count is in `result.crates_compiled`.
    let progress = (cargo && !terminal).then(|| Progress {
        crates_compiled: crates_now,
        crates_expected: history.crates_expected(&kind_key, warmth, job.id),
    });
    let estimate = (!terminal)
        .then(|| history.estimate(&kind_key, warmth, job.id))
        .flatten()
        .map(|eta| {
            let done: f64 = execution
                .map(|e| e.stages.iter().filter_map(|s| s.seconds).sum())
                .unwrap_or(0.0);
            let current = extras
                .admitted_at
                .map(|at| (inputs.now - at as f64).max(0.0))
                .unwrap_or(0.0);
            EtaEstimate {
                total_s: eta.total_s,
                remaining_s: (eta.total_s - done - current).max(0.0),
                samples: eta.samples,
                basis: eta.basis,
            }
        });
    let result = terminal.then(|| JobResult {
        exit_code: execution
            .and_then(|e| e.stages.last())
            .and_then(|s| s.exit_code),
        wall_s: metrics
            .and_then(|m| m.wall_s)
            .or_else(|| history::exec_seconds(job)),
        cpu_user_s: metrics.and_then(|m| m.cpu_user_s),
        cpu_sys_s: metrics.and_then(|m| m.cpu_sys_s),
        peak_rss_bytes: metrics.and_then(|m| m.peak_rss_bytes),
        crates_compiled: metrics.and_then(|m| m.crates_compiled),
        warmth,
    });
    let device = match &resource {
        Resource::Device(lock) => Some(crate::device::adb_serial_for_lock(lock)),
        _ => None,
    };
    let (checkout, checkout_name) = extras.checkout.clone();
    JobView {
        id: job.id,
        owner: job.spec.owner.clone(),
        phase: job.phase.clone(),
        resource: slot_kind(&resource),
        device,
        checkout,
        checkout_name,
        command,
        kind_key,
        argv,
        submitted_at: job.submitted_at,
        started_at: execution.and_then(|e| e.started_at),
        finished_at: execution.and_then(|e| e.finished_at),
        queue: queue(job, &resource, inputs),
        live: extras.live.filter(|_| !terminal),
        progress,
        warmth,
        warmth_reason,
        estimate,
        result,
        offload: extras.offload.clone(),
    }
}

pub fn assemble(inputs: Inputs) -> Snapshot {
    let history = History::from_jobs(inputs.all_jobs);
    let jobs = inputs
        .jobs
        .iter()
        .map(|j| job_view(j, &inputs, &history))
        .collect();
    Snapshot {
        version: SNAPSHOT_VERSION,
        taken_at: inputs.now,
        machine: inputs.machine.clone(),
        jobs,
        health: None,
    }
}
