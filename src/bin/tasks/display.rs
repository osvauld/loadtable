//! Human-facing CLI presentation: compact job tables, the submit line, shared-cwd
//! warnings and live lock holders. Display only: the stored and executed argv, the
//! ledger and every `--json` response are left exactly as they are.
use tinline_task_manager::local_command::{self, Recipe, Resource};
use tinline_task_manager::{Job, Phase};

/// `NAME=value` arguments with a longer value (typically PATH) are abbreviated.
pub const ENV_VALUE_MAX: usize = 40;
const ENV_VALUE_KEEP: usize = 24;
const TABLE_CWD: usize = 36;
const TABLE_COMMAND: usize = 60;
const SUBMIT_COMMAND: usize = 200;

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// Keeps the first `max - 1` characters and marks the cut with `…`.
pub fn truncate_end(s: &str, max: usize) -> String {
    if chars(s) <= max {
        return s.into();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Keeps the last `max - 1` characters (the distinctive end of a path).
pub fn truncate_start(s: &str, max: usize) -> String {
    let n = chars(s);
    if n <= max {
        return s.into();
    }
    let mut out = String::from("…");
    out.extend(s.chars().skip(n - max.saturating_sub(1)));
    out
}

/// `PATH=/a:/b…(+900)` style abbreviation (count of omitted characters) for long
/// environment assignments only; it contains no whitespace, so it needs no quoting.
pub fn abbreviate_arg(arg: &str) -> String {
    let Some((name, value)) = arg.split_once('=') else {
        return arg.into();
    };
    let is_name = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let n = chars(value);
    if !is_name || n <= ENV_VALUE_MAX {
        return arg.into();
    }
    let kept: String = value.chars().take(ENV_VALUE_KEEP).collect();
    format!("{name}={kept}…(+{})", n - ENV_VALUE_KEEP)
}

fn quote(arg: String) -> String {
    if arg.is_empty()
        || arg
            .chars()
            .any(|c| c.is_whitespace() || c == '\'' || c == '"')
    {
        format!("'{}'", arg.replace('\'', r"'\''"))
    } else {
        arg
    }
}

fn recipe(job: &Job) -> Option<Recipe> {
    (job.spec.suite == local_command::SUITE)
        .then(|| serde_json::from_str(&job.spec.configuration).ok())
        .flatten()
}

/// One-line command for display, with long env values abbreviated.
pub fn command_summary(job: &Job) -> String {
    match recipe(job) {
        Some(recipe) => recipe
            .argv
            .iter()
            .map(|arg| quote(abbreviate_arg(arg)))
            .collect::<Vec<_>>()
            .join(" "),
        None => format!("{} {}", job.spec.suite, job.spec.configuration),
    }
}

pub fn resource_label(job: &Job) -> String {
    match recipe(job).map(|r| r.resource) {
        Some(Resource::Compile) => "compile".into(),
        Some(Resource::Run) => "run".into(),
        Some(Resource::Offload) => "offload".into(),
        Some(Resource::Device(serial)) => format!("device:{serial}"),
        None if job.spec.suite == tinline_task_manager::catalog::ANDROID_SUITE => "android".into(),
        None => "suite".into(),
    }
}

/// Replaces the home prefix with `~` so paths stay readable in narrow columns.
pub fn home_relative(path: &str, home: Option<&str>) -> String {
    match home.filter(|h| !h.is_empty() && *h != "/") {
        Some(home) if path == home => "~".into(),
        Some(home) => match path.strip_prefix(home) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.into(),
        },
        None => path.into(),
    }
}

pub fn duration(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn phase_name(phase: &Phase) -> String {
    phase.as_db().unwrap_or_else(|_| format!("{phase:?}"))
}

/// Exit code shown for a finished job (`Phase::reported_exit_code`): 0 only when passed,
/// else the last stage's nonzero code or signal; `-` otherwise (an interrupted job never
/// shows 0, even when an earlier stage succeeded).
pub fn exit_label(job: &Job) -> String {
    if !job.phase.terminal() {
        return "-".into();
    }
    let stages = job
        .execution
        .as_ref()
        .map_or(&[][..], |e| e.stages.as_slice());
    if let Some(code) = job.phase.reported_exit_code(stages) {
        return code.to_string();
    }
    stages
        .last()
        .and_then(|s| s.signal.map(|s| format!("sig{s}")))
        .unwrap_or_else(|| "-".into())
}

/// (age, admission wait, run) as display strings. Wait and run come from the stages
/// the worker has recorded; a stage still in progress is not yet included, so
/// active jobs show `+` after the recorded part.
pub fn timing(job: &Job, now: i64) -> (String, String, String) {
    let end = job
        .execution
        .as_ref()
        .and_then(|e| e.finished_at)
        .filter(|_| job.phase.terminal())
        .unwrap_or(now);
    let age = duration((end - job.submitted_at) as f64);
    let stages = job
        .execution
        .as_ref()
        .map(|e| e.stages.as_slice())
        .unwrap_or(&[]);
    let more = if job.phase.terminal() { "" } else { "+" };
    if stages.is_empty() {
        let none = if job.phase.terminal() { "-" } else { "…" };
        return (age, none.into(), none.into());
    }
    let wait: f64 = stages.iter().map(|s| s.wait_seconds).sum();
    let run: f64 = stages.iter().filter_map(|s| s.seconds).sum();
    (
        age,
        format!("{}{more}", duration(wait)),
        format!("{}{more}", duration(run)),
    )
}

pub fn job_table(jobs: &[Job], now: i64, home: Option<&str>) -> String {
    let mut out = format!(
        "{:>5} {:<13} {:<4} {:<20} {:<12} {:>7} {:>8} {:>8} {:<TABLE_CWD$} COMMAND\n",
        "ID", "PHASE", "EXIT", "OWNER", "RESOURCE", "AGE", "WAIT", "RUN", "CWD"
    );
    for job in jobs {
        let (age, wait, run) = timing(job, now);
        out.push_str(&format!(
            "{:>5} {:<13} {:<4} {:<20} {:<12} {:>7} {:>8} {:>8} {:<TABLE_CWD$} {}\n",
            job.id,
            phase_name(&job.phase),
            exit_label(job),
            truncate_end(&job.spec.owner, 20),
            truncate_end(&resource_label(job), 12),
            age,
            wait,
            run,
            truncate_start(&home_relative(&job.spec.source, home), TABLE_CWD),
            truncate_end(&command_summary(job), TABLE_COMMAND),
        ));
    }
    if jobs.is_empty() {
        out.push_str("(no matching jobs)\n");
    }
    out
}

pub const TABLE_LEGEND: &str = "AGE = since submission (to finish when done); WAIT/RUN = admission wait and \
     stage run time recorded so far (+ = a stage may still be in progress). \
     Env values over 40 chars are abbreviated here only; tasks list --json has the full argv.";

/// Jobs selected by `tasks list` options. `limit` keeps the most recent matches.
pub fn select(jobs: Vec<Job>, who: Option<&str>, active: bool, limit: Option<usize>) -> Vec<Job> {
    let mut jobs: Vec<Job> = jobs
        .into_iter()
        .filter(|j| who.is_none_or(|w| j.spec.owner == w))
        .filter(|j| !active || !j.phase.terminal())
        .collect();
    if let Some(limit) = limit {
        let skip = jobs.len().saturating_sub(limit);
        jobs.drain(..skip);
    }
    jobs
}

/// The first client line; the `tasks: submitted job N;` prefix is parsed by scripts.
pub fn submit_line(id: i64, cwd: &str, job: Option<&Job>) -> String {
    let command = job
        .map(|j| {
            format!(
                " argv {};",
                truncate_end(&command_summary(j), SUBMIT_COMMAND)
            )
        })
        .unwrap_or_default();
    format!(
        "tasks: submitted job {id}; cwd {cwd};{command} Ctrl-C detaches, tasks cancel {id} cancels"
    )
}

/// Unfinished jobs of OTHER owners in the same live cwd as job `id`.
pub fn shared_cwd(jobs: &[Job], id: i64) -> Vec<&Job> {
    let Some(new) = jobs.iter().find(|j| j.id == id) else {
        return vec![];
    };
    jobs.iter()
        .filter(|j| {
            j.id != id
                && !j.phase.terminal()
                && j.spec.source == new.spec.source
                && j.spec.owner != new.spec.owner
        })
        .collect()
}

pub fn shared_cwd_warning(jobs: &[Job], id: i64) -> Option<String> {
    let others = shared_cwd(jobs, id);
    let first = others.first()?;
    let list = others
        .iter()
        .map(|j| format!("{} ({}, {})", j.id, j.spec.owner, phase_name(&j.phase)))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "tasks: warning: other owners have unfinished jobs in this same live cwd {}: {list}. \
         They build and edit the same checkout; coordinate before relying on results \
         (not blocking).",
        first.spec.source
    ))
}

/// A live lock observation from `tasks admission`, reduced to what the table needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Holder {
    /// Lock slot, e.g. `compile`, `run-0`, `device 9BWAY…`; `waiting` for waiters.
    pub slot: String,
    pub who: String,
    /// None when a lock is busy but its holder note is missing or stale.
    pub pid: Option<u32>,
    /// Unix seconds when the note was written.
    pub since: f64,
}

/// `p2p-build.lock` -> `compile`, `p2p-run-2.lock` -> `run-2`, `p2p-device-X.lock` -> `device X`.
pub fn slot_name(file_name: &str) -> String {
    let stem = file_name.strip_suffix(".lock").unwrap_or(file_name);
    let stem = stem.strip_prefix("p2p-").unwrap_or(stem);
    if stem == "build" {
        "compile".into()
    } else if let Some(serial) = stem.strip_prefix("device-") {
        format!("device {serial}")
    } else {
        stem.into()
    }
}

/// Where a holder note comes from: a listed durable job (`OWNER:job-ID`), a native
/// job of another state directory, or a legacy/external client.
/// Job ids named by native holder notes (`owner:job-N`), each once.
pub fn holder_job_ids<'a>(holders: impl Iterator<Item = &'a Holder>) -> Vec<i64> {
    let mut ids: Vec<i64> = holders
        .filter_map(|h| h.who.rsplit_once(":job-")?.1.parse().ok())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn origin(who: &str, jobs: &[Job]) -> String {
    let native = who
        .rsplit_once(":job-")
        .and_then(|(owner, id)| Some((owner, id.parse::<i64>().ok()?)));
    match native {
        Some((owner, id)) if jobs.iter().any(|j| j.id == id && j.spec.owner == owner) => {
            format!("job {id}")
        }
        Some(_) => "native job not in this ledger (other state dir)".into(),
        None => "not in ledger (legacy/external client)".into(),
    }
}

/// Live lock holders and waiters. Holders without a durable job are legacy/external
/// clients sharing the same flock files: shown, never turned into ledger rows.
pub fn holders_section(holders: &[Holder], waiting: &[Holder], jobs: &[Job], now: i64) -> String {
    let mut out = String::from("live admission (from tasks admission; advisory holder notes):\n");
    if holders.is_empty() && waiting.is_empty() {
        out.push_str("  no lock is held and nobody is waiting\n");
        return out;
    }
    for (state, notes) in [("holds", holders), ("waits", waiting)] {
        for h in notes {
            let origin = origin(&h.who, jobs);
            out.push_str(&format!(
                "  {:<16} {state} {:<32} pid {:<8} for {:>8}  {origin}\n",
                h.slot,
                truncate_end(&h.who, 32),
                h.pid.map_or("?".into(), |p| p.to_string()),
                duration(now as f64 - h.since),
            ));
        }
    }
    out
}

/// The one line `tasks start-worker` / `tasks stop-worker` print (not the ledger).
pub fn worker_switch_line(start: bool, running: bool, jobs: &[Job]) -> String {
    let approved = jobs.iter().filter(|j| j.phase == Phase::Approved).count();
    let active = jobs.iter().filter(|j| j.phase.active()).count();
    if start {
        format!(
            "tasks: execution enabled; worker {}; {approved} approved job(s) waiting, {active} running. \
             Stays enabled across daemon restarts until tasks stop-worker",
            if running {
                "running"
            } else {
                "not running yet (it exits when idle; tasks admission shows blockers)"
            }
        )
    } else {
        format!(
            "tasks: execution stopped; the worker claims no new jobs ({active} running job(s) finish, nothing cancelled); \
             {approved} approved job(s) wait. Stays stopped across daemon restarts until tasks start-worker"
        )
    }
}

/// Human note for replies whose JSON carries `execution_enabled`: that field says
/// whether a worker process was alive when the daemon replied, not whether a job ran.
pub fn worker_note(execution_enabled: bool) -> &'static str {
    if execution_enabled {
        "worker process running at reply time (JSON execution_enabled=true)"
    } else {
        "no worker process at reply time (JSON execution_enabled=false; this does not mean the \
         job did not run — see its phase; tasks start-worker starts one)"
    }
}

/// One line describing a job's state, for stderr after `logs`/`approve-*`.
pub fn job_status_line(job: &Job, execution_enabled: bool) -> String {
    let exit = if job.phase.terminal() {
        format!(" (exit {})", exit_label(job))
    } else {
        String::new()
    };
    format!(
        "tasks: job {} {}{exit} owner={}; {}{}",
        job.id,
        phase_name(&job.phase),
        job.spec.owner,
        worker_note(execution_enabled),
        remote_note(job)
    )
}

/// `; remote job 152 on abe@host passed (exit 0) [terminal]` for an interrupted offload whose
/// remote half was reconciled; the job's own phase/exit stay as they are.
pub fn remote_note(job: &Job) -> String {
    job.execution
        .as_ref()
        .and_then(|e| e.remote_result.as_ref())
        .map(|r| {
            format!(
                "\ntasks: job {} is {} here; its {} [{}]",
                job.id,
                phase_name(&job.phase),
                r.summary(),
                r.state
            )
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "display_tests.rs"]
mod tests;
