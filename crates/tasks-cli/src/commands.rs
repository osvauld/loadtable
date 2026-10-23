//! The commands: each takes a [`Ctx`] (client, output, environment) and returns an exit code.

use crate::args::{
    Command, ExecutionCommand, ListArgs, LogSearchArgs, LogsArgs, PolicyCommand, RunArgs,
    SplitArgs, StatsArgs, StepsArgs, SuiteCommand, SuiteRunArgs,
};
use crate::client::{Client, ClientError, JSON_LIST_FRAME_CAP};
use crate::error::{CliError, EXIT_FAILURE, EXIT_OK, EXIT_PIPE};
use crate::render;
use crate::runtime::{Pacer, Signals};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tasks_core::plan::Plan;
use tasks_core::{DeviceId, JobId, JobSpec, Kind, Owner, Phase, Placement, Priority};
use tasks_daemon::api::{
    ErrorCode, HistoryView, JobView, LogSearchQuery, LogView, MAX_CONTEXT, MAX_TAIL_LINES, Request,
    Response, StatusSnapshot, WaiterView,
};
use tasks_policy::PolicyFs;
use tasks_policy::submit_rules::{self, PlacementClass};
use tasks_timing::{History, RecipeKey};

/// How long `run` waits between log polls.
pub const POLL: Duration = Duration::from_millis(200);
/// Idle polls between checks of a queued job's wait reason.
const WAIT_NOTE_EVERY: u32 = 10;
/// Finished jobs a submit reconciliation looks at.
const RECONCILE_RECENT: usize = 30;
/// A reconciled job must have been submitted no earlier than this long before the attempt.
const RECONCILE_SLACK_MS: i64 = 5_000;
/// Default tail length of `logs`.
const DEFAULT_TAIL: usize = 100;
/// Default number of finished jobs `list` shows.
const DEFAULT_LIST_RECENT: usize = 50;

/// Everything a command needs from the world.
pub struct Ctx<'a> {
    /// The daemon.
    pub client: &'a dyn Client,
    /// Standard output.
    pub out: &'a mut dyn Write,
    /// Standard error.
    pub err: &'a mut dyn Write,
    /// The working directory, resolved.
    pub cwd: PathBuf,
    /// Environment lookup.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Milliseconds since the epoch.
    pub now_ms: &'a dyn Fn() -> i64,
    /// Waits between polls.
    pub pacer: &'a dyn Pacer,
    /// Termination signals.
    pub signals: &'a dyn Signals,
    /// Where this client would look for the slot locks (R-231), for `admission`.
    pub client_lock_dir: Option<PathBuf>,
    /// Reads a file; `-` is standard input.
    pub read_input: &'a dyn Fn(&Path) -> std::io::Result<String>,
    /// The file system the repo run policy is read from.
    pub policy_fs: &'a dyn PolicyFs,
    /// Timing history to use instead of asking the daemon for it (`None` asks the daemon).
    pub history: Option<&'a dyn History>,
}

impl Ctx<'_> {
    fn note(&mut self, text: &str) {
        let _ = writeln!(self.err, "{text}");
    }
}

/// Runs a client command. `daemon`, `ui` and `mcp` are handled by the caller.
pub fn execute(ctx: &mut Ctx<'_>, cmd: &Command) -> i32 {
    match dispatch(ctx, cmd) {
        Ok(code) => code,
        Err(CliError::Pipe) => {
            let _ = writeln!(ctx.err, "tasks: standard output was closed; stopping");
            EXIT_PIPE
        }
        Err(e) => {
            let _ = writeln!(ctx.err, "tasks2: {e}");
            e.exit_code()
        }
    }
}

fn dispatch(ctx: &mut Ctx<'_>, cmd: &Command) -> Result<i32, CliError> {
    match cmd {
        Command::Run(a) => run(ctx, a, !a.detach),
        Command::Enqueue(a) => run(ctx, a, false),
        Command::List(a) => list(ctx, a),
        Command::Logs(a) => logs(ctx, a),
        Command::LogSearch(a) => log_search(ctx, a),
        Command::Get { id, json } => get(ctx, *id, *json),
        Command::Cancel { id, who, reason } => cancel(ctx, *id, who.as_deref(), reason.as_deref()),
        Command::Prioritize {
            id,
            who,
            priority,
            reason,
        } => prioritize(ctx, *id, who.as_deref(), priority, reason.as_deref()),
        Command::Status { json } => status(ctx, *json, false),
        Command::Admission { json } => status(ctx, *json, true),
        Command::Estimate { command } => estimate(ctx, command),
        Command::Drain => drain(ctx, false),
        Command::Resume => drain(ctx, true),
        Command::Execution { action } => execution(ctx, *action),
        Command::Steps(a) => steps(ctx, a),
        Command::Suite { action } => match action {
            SuiteCommand::Run(a) => suite_run(ctx, a),
            SuiteCommand::List => suite_list(ctx),
        },
        Command::Policy { action } => match action {
            PolicyCommand::Show => policy_show(ctx),
            PolicyCommand::Suggest => policy_suggest(ctx),
        },
        Command::Split(a) => split(ctx, a),
        Command::Stats(a) => stats(ctx, a),
        Command::Audit(a) => crate::audit::audit(ctx, a),
        Command::Stalls(a) => crate::audit::stalls(ctx, a),
        Command::Pair { action } => crate::trust::pair(ctx, action),
        Command::Peers { action, json } => crate::trust::peers(ctx, action.as_ref(), *json),
        Command::Identity { .. } => Err(CliError::Failed(
            "internal: identity is read from the state directory, not from the daemon".into(),
        )),
        Command::Daemon { .. } | Command::Cache | Command::Ui { .. } | Command::Mcp { .. } => Err(
            CliError::Failed("internal: this command is handled before the client is built".into()),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// submitting

/// The owner: `--who`, then `$TASKS_WHO`, then `$USER`.
pub fn resolve_owner(ctx: &Ctx<'_>, who: Option<&str>) -> Result<Owner, CliError> {
    let name = who
        .map(str::to_string)
        .or_else(|| (ctx.env)("TASKS_WHO").filter(|v| !v.is_empty()))
        .or_else(|| (ctx.env)("USER").filter(|v| !v.is_empty()))
        .ok_or_else(|| {
            CliError::Usage(
                "no job owner: pass --who NAME or set TASKS_WHO (letters, digits, `.`, `_`, `-`)"
                    .into(),
            )
        })?;
    Owner::new(&name).map_err(|e| {
        CliError::Usage(format!(
            "owner {name:?} is not valid ({e}); pass --who with letters, digits, `.`, `_` or `-`"
        ))
    })
}

/// Notes about where the job will run. The submit rules themselves (broad `cargo test`, denied
/// devices, the Cargo jobs cap) are the daemon's: it answers every front end the same way
/// (R-190, R-191, R-210), and a refusal comes back as the daemon's `refused` error (exit 2).
fn placement_note(ctx: &mut Ctx<'_>, argv: &[String], placement: &Placement) {
    if *placement == Placement::Anywhere
        && let PlacementClass::StaysLocal(why) = submit_rules::placement_class(argv)
    {
        ctx.note(&format!(
            "tasks: note: --anywhere has no effect here, the job stays on this machine: {why}"
        ));
    }
}

/// Builds the job from the arguments, after the shared submit rules (R-190, R-191).
pub fn build_spec(ctx: &mut Ctx<'_>, a: &RunArgs) -> Result<JobSpec, CliError> {
    let placement = a.place.placement(a.device.as_deref())?;
    let owner = resolve_owner(ctx, a.who.as_deref())?;
    placement_note(ctx, &a.command, &placement);
    let kind = if a.device.is_some() {
        Kind::Device
    } else {
        RecipeKey::from_argv(&a.command, &[])
            .first()
            .map_or(Kind::Run, RecipeKey::kind)
    };
    let mut spec = JobSpec {
        owner,
        argv: a.command.clone(),
        cwd: ctx.cwd.clone(),
        env: Vec::new(),
        kind,
        requirements: tasks_core::Requirements::default(),
        placement,
        after: Vec::new(),
        // the daemon enforces the limit; wrapping the command in `timeout` would change the
        // recipe and the ETA history
        timeout_secs: a.timeout,
        broad_test_escape: a.broad_test,
        priority: Priority::Normal,
        priority_reason: None,
        fetch: a.fetch.clone(),
    };
    (spec.priority, spec.priority_reason) = a.class.resolve()?;
    spec.requirements.device = a.device.clone().map(DeviceId);
    spec.validate()
        .map_err(|e| CliError::Refused(format!("invalid job: {e}")))?;
    Ok(spec)
}

/// Unfinished jobs of other owners in the same cwd (R-187).
pub fn shared_cwd_warning(jobs: &[JobView], owner: &str, cwd: &Path) -> Option<String> {
    let others: Vec<&JobView> = jobs
        .iter()
        .filter(|j| !j.phase.is_terminal() && j.cwd == cwd && j.owner != owner)
        .collect();
    others.first()?;
    let list = others
        .iter()
        .map(|j| format!("{} ({}, {})", j.id.0, j.owner, render::phase_tag(&j.phase)))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "tasks: warning: other owners have unfinished jobs in this same cwd {}: {list}. They build and edit the same checkout; coordinate before relying on results (not blocking).",
        cwd.display()
    ))
}

/// Submits `spec`. When the reply is lost, finds the job in the ledger by exact spec match
/// instead of resending (R-099, R-100).
pub fn submit_reconciled(ctx: &mut Ctx<'_>, spec: &JobSpec) -> Result<JobId, CliError> {
    let before = (ctx.now_ms)();
    match ctx.client.call(&Request::Submit { spec: spec.clone() }) {
        Ok(Response::Submitted { id, warnings }) => {
            for w in &warnings {
                ctx.note(&format!("tasks: warning: {w}"));
            }
            Ok(id)
        }
        Ok(other) => Err(CliError::Failed(format!(
            "unexpected reply to submit: {other:?}; do not resubmit, look in `tasks2 list --who {}`",
            spec.owner.as_str()
        ))),
        Err(ClientError::ReplyLost { detail }) => reconcile_lost_submit(ctx, spec, before, &detail),
        Err(e) => Err(e.into()),
    }
}

fn reconcile_lost_submit(
    ctx: &mut Ctx<'_>,
    spec: &JobSpec,
    before_ms: i64,
    lost: &str,
) -> Result<JobId, CliError> {
    let owner = spec.owner.as_str();
    let hint = format!("`tasks2 list --who {owner}`");
    let listed = ctx.client.call(&Request::List {
        owner: Some(owner.to_string()),
        active_only: false,
        recent_finished: Some(RECONCILE_RECENT),
        limit: None,
    });
    let Ok(Response::Jobs { jobs, .. }) = listed else {
        return Err(CliError::Failed(format!(
            "submit was sent but no reply arrived ({lost}) and the ledger could not be read to check. Do NOT resubmit: look in {hint}"
        )));
    };
    let matches: Vec<JobId> = jobs
        .iter()
        .filter(|j| {
            j.owner == owner
                && j.cwd == spec.cwd
                && j.argv == spec.argv
                && j.submitted_at_ms >= before_ms - RECONCILE_SLACK_MS
        })
        .map(|j| j.id)
        .collect();
    match matches.as_slice() {
        [id] => {
            ctx.note(&format!(
                "tasks: the daemon's reply to submit was lost ({lost}); found the submitted job {} in the ledger by exact spec match; not resubmitted",
                id.0
            ));
            Ok(*id)
        }
        [] => Err(CliError::Failed(format!(
            "submit was sent but no reply arrived ({lost}) and no matching job is listed yet; it may still be recorded. Do NOT resubmit: look in {hint}"
        ))),
        many => {
            let ids: Vec<String> = many.iter().map(|i| i.0.to_string()).collect();
            Err(CliError::Failed(format!(
                "submit reply lost ({lost}) and several identical jobs match: {}. One is yours; inspect them with {hint} / `tasks2 logs ID`. Do NOT resubmit",
                ids.join(", ")
            )))
        }
    }
}

fn run(ctx: &mut Ctx<'_>, a: &RunArgs, follow: bool) -> Result<i32, CliError> {
    let spec = build_spec(ctx, a)?;
    if let Ok(Response::Jobs { jobs, .. }) = ctx.client.call(&Request::List {
        owner: None,
        active_only: true,
        recent_finished: None,
        limit: None,
    }) && let Some(w) = shared_cwd_warning(&jobs, spec.owner.as_str(), &spec.cwd)
    {
        ctx.note(&w);
    }
    let id = submit_reconciled(ctx, &spec)?;
    let line = render::submit_line(id.0, &spec.cwd, &spec.argv);
    if writeln!(ctx.err, "{line}").is_err() {
        // stderr gone: keep going, the job is submitted
    }
    if !follow {
        writeln!(ctx.out, "{}", id.0)?;
        return Ok(EXIT_OK);
    }
    let mut state = FollowState::default();
    let end = follow_log(ctx, id, &mut state);
    match end {
        Ok(FollowEnd::Finished(job)) => {
            if job.phase == Phase::Passed && tasks_core::any_failed(&job.fetch) {
                let n = job.fetch.iter().filter(|r| !r.ok()).count();
                ctx.note(&format!(
                    "tasks: job {} passed, but {n} requested file(s) could not be fetched back; see the fetch lines above (exit 1)",
                    id.0
                ));
            }
            // the last stdout line, whatever the job printed (a failed pipe changes no result)
            if writeln!(ctx.out, "{}", render::summary_line(&job)).is_err() {
                return Ok(EXIT_PIPE);
            }
            let _ = ctx.out.flush();
            Ok(render::job_exit_code(&job))
        }
        Ok(FollowEnd::Signal(sig)) => {
            ctx.note(&render::still_live(id.0));
            Ok(128 + sig)
        }
        Err(e) => {
            // R-104/R-105: never cancel, never touch the job; say where it is
            if state.done {
                ctx.note(&format!(
                    "tasks: job {} finished but its final state could not be read ({e}); query it with `tasks2 get {}`",
                    id.0, id.0
                ));
            } else {
                if !matches!(e, CliError::Pipe) {
                    ctx.note(&format!("tasks2: {e}"));
                }
                ctx.note(&render::still_live(id.0));
            }
            Ok(match e {
                CliError::Pipe => EXIT_PIPE,
                other => other.exit_code().max(EXIT_FAILURE),
            })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// following a log

/// Progress of a follow, readable after it failed.
#[derive(Debug, Default)]
pub struct FollowState {
    /// The daemon said the log is complete.
    pub done: bool,
}

/// How a follow ended.
#[derive(Debug)]
pub enum FollowEnd {
    /// The job is over; here is its final state.
    Finished(Box<JobView>),
    /// A termination signal arrived; the job was left alone.
    Signal(i32),
}

fn get_job(ctx: &Ctx<'_>, id: JobId) -> Result<JobView, CliError> {
    match ctx.client.call(&Request::Get { id })? {
        Response::Job { job } => Ok(*job),
        other => Err(CliError::Failed(format!(
            "unexpected reply to get: {other:?}"
        ))),
    }
}

/// Streams the log of `id` to stdout exactly once and in order, emitting lines only at newlines
/// (R-111), until the job ends. Sends only read-only requests (R-105).
pub fn follow_log(
    ctx: &mut Ctx<'_>,
    id: JobId,
    state: &mut FollowState,
) -> Result<FollowEnd, CliError> {
    let mut offset = 0u64;
    let mut pending = String::new();
    let mut idle = 0u32;
    let mut last_wait: Option<String> = None;
    loop {
        if let Some(sig) = ctx.signals.pending() {
            flush_partial(ctx, &mut pending)?;
            return Ok(FollowEnd::Signal(sig));
        }
        let reply = ctx.client.call(&Request::Logs {
            id,
            view: LogView::Follow {
                offset,
                max_bytes: None,
            },
        });
        match reply {
            Ok(Response::LogChunk {
                data,
                next_offset,
                done,
            }) => {
                let got = !data.is_empty();
                pending.push_str(&data);
                emit_lines(ctx, &mut pending)?;
                offset = next_offset;
                if done {
                    state.done = true;
                    flush_partial(ctx, &mut pending)?;
                    return Ok(FollowEnd::Finished(Box::new(get_job(ctx, id)?)));
                }
                if got {
                    idle = 0;
                    continue;
                }
            }
            Ok(other) => {
                return Err(CliError::Failed(format!(
                    "unexpected reply to a log read: {other:?}"
                )));
            }
            // the log of a job that has not started does not exist yet
            Err(ClientError::Daemon(e)) if e.code == ErrorCode::NotFound => {
                let job = get_job(ctx, id)?;
                if job.phase.is_terminal() {
                    state.done = true;
                    return Ok(FollowEnd::Finished(Box::new(job)));
                }
            }
            Err(e) => return Err(e.into()),
        }
        idle += 1;
        if idle % WAIT_NOTE_EVERY == 1 {
            let job = get_job(ctx, id)?;
            if job.phase.is_terminal() && !state.done {
                // the daemon will say `done` next; read once more to drain
                ctx.pacer.pause(POLL);
                continue;
            }
            if matches!(job.phase, Phase::Queued | Phase::Waiting(_)) && job.wait_text != last_wait
            {
                if let Some(t) = &job.wait_text {
                    ctx.note(&format!("tasks: job {} waiting: {t}", id.0));
                }
                last_wait = job.wait_text;
            }
        }
        ctx.pacer.pause(POLL);
    }
}

fn emit_lines(ctx: &mut Ctx<'_>, pending: &mut String) -> Result<(), CliError> {
    if let Some(end) = pending.rfind('\n') {
        let complete: String = pending.drain(..=end).collect();
        ctx.out.write_all(complete.as_bytes())?;
        ctx.out.flush()?;
    }
    Ok(())
}

fn flush_partial(ctx: &mut Ctx<'_>, pending: &mut String) -> Result<(), CliError> {
    if !pending.is_empty() {
        let rest = std::mem::take(pending);
        ctx.out.write_all(rest.as_bytes())?;
        ctx.out.write_all(b"\n")?;
        ctx.out.flush()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// reading

fn as_jobs(resp: Response) -> Result<(Vec<JobView>, bool), CliError> {
    match resp {
        Response::Jobs { jobs, truncated } => Ok((jobs, truncated)),
        other => Err(CliError::Failed(format!(
            "unexpected reply to list: {other:?}"
        ))),
    }
}

/// Client-side selection: owner, activity and the most recent `limit` (R-185).
pub fn select(
    jobs: Vec<JobView>,
    who: Option<&str>,
    active: bool,
    limit: Option<usize>,
) -> Vec<JobView> {
    let mut jobs: Vec<JobView> = jobs
        .into_iter()
        .filter(|j| who.is_none_or(|w| j.owner == w))
        .filter(|j| !active || !j.phase.is_terminal())
        .collect();
    if let Some(limit) = limit {
        let skip = jobs.len().saturating_sub(limit);
        jobs.drain(..skip);
    }
    jobs
}

fn waiters(ctx: &Ctx<'_>) -> Vec<WaiterView> {
    match ctx.client.call(&Request::Status) {
        Ok(Response::Status(s)) => s.waiting,
        _ => Vec::new(),
    }
}

fn list(ctx: &mut Ctx<'_>, a: &ListArgs) -> Result<i32, CliError> {
    let req = Request::List {
        owner: a.who.clone(),
        active_only: a.active,
        recent_finished: (!a.active).then_some(a.limit.unwrap_or(DEFAULT_LIST_RECENT)),
        limit: None,
    };
    let resp = if a.json {
        ctx.client.call_capped(&req, JSON_LIST_FRAME_CAP)?
    } else {
        ctx.client.call(&req)?
    };
    let (jobs, truncated) = as_jobs(resp)?;
    let jobs = select(jobs, a.who.as_deref(), a.active, a.limit);
    if a.json {
        let v = serde_json::json!({ "jobs": jobs, "truncated": truncated });
        writeln!(ctx.out, "{}", pretty(&v)?)?;
        return Ok(EXIT_OK);
    }
    let w = if !a.brief
        && jobs
            .iter()
            .any(|j| matches!(j.phase, Phase::Queued | Phase::Waiting(_)))
    {
        waiters(ctx)
    } else {
        Vec::new()
    };
    let home = (ctx.env)("HOME");
    let table = render::job_table(
        &jobs,
        (ctx.now_ms)(),
        home.as_deref(),
        &w,
        render::TableOptions { brief: a.brief },
    );
    ctx.out.write_all(table.as_bytes())?;
    if truncated {
        ctx.note("tasks: the daemon cut the list at its row limit; narrow it with --who, --active or --limit");
    }
    Ok(EXIT_OK)
}

fn pretty<T: serde::Serialize>(v: &T) -> Result<String, CliError> {
    serde_json::to_string_pretty(v)
        .map_err(|e| CliError::Failed(format!("cannot encode JSON: {e}")))
}

fn get(ctx: &mut Ctx<'_>, id: i64, json: bool) -> Result<i32, CliError> {
    let job = get_job(ctx, JobId(id))?;
    if json {
        writeln!(ctx.out, "{}", pretty(&job)?)?;
        return Ok(EXIT_OK);
    }
    let w = if matches!(job.phase, Phase::Queued | Phase::Waiting(_)) {
        waiters(ctx).into_iter().find(|w| w.id == job.id)
    } else {
        None
    };
    let text = render::job_detail(&job, (ctx.now_ms)(), w.as_ref());
    ctx.out.write_all(text.as_bytes())?;
    Ok(EXIT_OK)
}

fn logs(ctx: &mut Ctx<'_>, a: &LogsArgs) -> Result<i32, CliError> {
    let id = JobId(a.id);
    if a.follow {
        let mut state = FollowState::default();
        return match follow_log(ctx, id, &mut state)? {
            FollowEnd::Finished(_) => Ok(EXIT_OK),
            FollowEnd::Signal(sig) => Ok(128 + sig),
        };
    }
    if let Some(pattern) = &a.grep {
        if a.context > MAX_CONTEXT {
            return Err(CliError::Usage(format!(
                "-C {} is too large; at most {MAX_CONTEXT} lines of context",
                a.context
            )));
        }
        let resp = ctx.client.call(&Request::Logs {
            id,
            view: LogView::Grep {
                pattern: pattern.clone(),
                context: a.context,
                regex: !a.fixed,
                max_matches: None,
            },
        })?;
        let Response::LogGrep {
            groups,
            matches,
            truncated,
        } = resp
        else {
            return Err(CliError::Failed(format!(
                "unexpected reply to a log read: {resp:?}"
            )));
        };
        ctx.out.write_all(render::grep_groups(&groups).as_bytes())?;
        if matches == 0 {
            ctx.note("tasks: no matching lines");
        }
        if truncated {
            ctx.note("tasks: the search stopped at its match limit; narrow the pattern");
        }
        return Ok(EXIT_OK);
    }
    let lines = a.tail.unwrap_or(DEFAULT_TAIL);
    if lines == 0 || lines > MAX_TAIL_LINES {
        return Err(CliError::Usage(format!(
            "--tail {lines} is out of range; use 1 to {MAX_TAIL_LINES}"
        )));
    }
    let resp = ctx.client.call(&Request::Logs {
        id,
        view: LogView::Tail { lines },
    })?;
    let Response::LogTail { lines, running, .. } = resp else {
        return Err(CliError::Failed(format!(
            "unexpected reply to a log read: {resp:?}"
        )));
    };
    for l in &lines {
        writeln!(ctx.out, "{l}")?;
    }
    if running {
        ctx.note(&format!(
            "tasks: job {} is still running; `tasks2 logs {} --follow` streams the rest",
            id.0, id.0
        ));
    }
    Ok(EXIT_OK)
}

fn log_search(ctx: &mut Ctx<'_>, a: &LogSearchArgs) -> Result<i32, CliError> {
    if a.context > MAX_CONTEXT {
        return Err(CliError::Usage(format!(
            "-C {} is too large; at most {MAX_CONTEXT} lines of context",
            a.context
        )));
    }
    let query = LogSearchQuery {
        pattern: a.pattern.clone(),
        regex: !a.fixed,
        owner: a.who.clone(),
        ids: a.ids.iter().copied().map(JobId).collect(),
        phase: a.phase.clone(),
        since: a.since.clone(),
        until: a.until.clone(),
        context: a.context,
        max_matches: a.max_matches,
        max_bytes: a.max_bytes,
    };
    let resp = ctx.client.call(&Request::LogSearch { query })?;
    let Response::LogSearch {
        results,
        matches,
        truncated,
    } = resp
    else {
        return Err(CliError::Failed(format!(
            "unexpected reply to log search: {resp:?}"
        )));
    };
    ctx.out
        .write_all(render::log_search(&results, matches, truncated).as_bytes())?;
    Ok(EXIT_OK)
}

fn status(ctx: &mut Ctx<'_>, json: bool, admission: bool) -> Result<i32, CliError> {
    let resp = ctx.client.call(&Request::Status)?;
    let Response::Status(s) = resp else {
        return Err(CliError::Failed(format!(
            "unexpected reply to status: {resp:?}"
        )));
    };
    let s: StatusSnapshot = *s;
    if json {
        writeln!(ctx.out, "{}", pretty(&s)?)?;
        return Ok(EXIT_OK);
    }
    let text = if admission {
        render::admission(&s, ctx.client_lock_dir.as_deref())
    } else {
        render::status(&s)
    };
    ctx.out.write_all(text.as_bytes())?;
    Ok(EXIT_OK)
}

fn estimate(ctx: &mut Ctx<'_>, command: &[String]) -> Result<i32, CliError> {
    let resp = ctx.client.call(&Request::Estimate {
        argv: command.to_vec(),
        kind: None,
    })?;
    let Response::Estimate(e) = resp else {
        return Err(CliError::Failed(format!(
            "unexpected reply to estimate: {resp:?}"
        )));
    };
    ctx.out.write_all(render::estimate(&e).as_bytes())?;
    Ok(EXIT_OK)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted.get(rank - 1).copied().unwrap_or(0.0)
}

/// Per-command statistics of finished jobs: runs, passes, median and p90 wall time. Cancelled
/// and never-started jobs are left out (R-013).
pub fn stat_rows(jobs: &[JobView]) -> Vec<render::StatRow> {
    let mut groups: BTreeMap<String, (usize, usize, Vec<f64>)> = BTreeMap::new();
    for j in jobs {
        if !matches!(
            j.phase,
            Phase::Passed | Phase::Failed { .. } | Phase::TimedOut { .. }
        ) {
            continue;
        }
        let recipe = RecipeKey::from_argv(&j.argv, &[])
            .first()
            .map(ToString::to_string)
            .or_else(|| j.argv.first().cloned())
            .unwrap_or_default();
        let e = groups.entry(recipe).or_default();
        e.0 += 1;
        if j.phase == Phase::Passed {
            e.1 += 1;
        }
        if let (Some(s), Some(f)) = (j.started_at_ms, j.finished_at_ms) {
            e.2.push((f - s).max(0) as f64 / 1000.0);
        }
    }
    let mut rows: Vec<render::StatRow> = groups
        .into_iter()
        .map(|(recipe, (runs, passed, mut secs))| {
            secs.sort_by(f64::total_cmp);
            render::StatRow {
                recipe,
                machine: None,
                target: None,
                runs,
                passed,
                median_secs: percentile(&secs, 0.5),
                p90_secs: percentile(&secs, 0.9),
            }
        })
        .collect();
    rows.sort_by(|a, b| b.runs.cmp(&a.runs).then_with(|| a.recipe.cmp(&b.recipe)));
    rows
}

/// Asks the daemon for timing history; `recipes` and `machines` narrow it (empty means all).
fn fetch_history(
    ctx: &Ctx<'_>,
    recipes: Vec<String>,
    machines: Vec<String>,
    max_samples: Option<usize>,
) -> Result<HistoryView, CliError> {
    match ctx.client.call(&Request::History {
        recipes,
        machines,
        max_samples,
    })? {
        Response::History(h) => Ok(h),
        other => Err(CliError::Failed(format!(
            "unexpected reply to history: {other:?}"
        ))),
    }
}

/// Rows of `stats` from the daemon's timing history: one per recipe and machine for the whole
/// job, and with `targets` one per test target below it. Cancelled samples are left out
/// (R-013); a series with nothing else is not shown.
pub fn history_rows(
    h: &HistoryView,
    recipe_filter: Option<&str>,
    targets: bool,
) -> Vec<render::StatRow> {
    let mut rows = Vec::new();
    for r in h
        .recipes
        .iter()
        .filter(|r| recipe_filter.is_none_or(|f| r.recipe.contains(f)))
    {
        for m in &r.machines {
            for s in m.series.iter().filter(|s| targets || s.target.is_none()) {
                let mut secs: Vec<f64> = s
                    .samples
                    .iter()
                    .filter(|v| v.outcome != "cancelled")
                    .map(|v| v.secs)
                    .collect();
                if secs.is_empty() {
                    continue;
                }
                let passed = s.samples.iter().filter(|v| v.outcome == "passed").count();
                secs.sort_by(f64::total_cmp);
                rows.push(render::StatRow {
                    recipe: r.recipe.clone(),
                    machine: Some(m.machine.clone()),
                    target: s.target.clone(),
                    runs: secs.len(),
                    passed,
                    median_secs: percentile(&secs, 0.5),
                    p90_secs: percentile(&secs, 0.9),
                });
            }
        }
    }
    rows.sort_by(|a, b| {
        b.runs.cmp(&a.runs).then_with(|| {
            (&a.recipe, &a.machine, &a.target).cmp(&(&b.recipe, &b.machine, &b.target))
        })
    });
    rows
}

fn stats(ctx: &mut Ctx<'_>, a: &StatsArgs) -> Result<i32, CliError> {
    if a.who.is_none() {
        let machines = a.machine.clone().into_iter().collect();
        let per_series = a.limit.clamp(1, tasks_daemon::api::MAX_HISTORY_SAMPLES);
        let h = fetch_history(ctx, Vec::new(), machines, Some(per_series))?;
        if h.truncated {
            ctx.note("tasks: the daemon cut the history at its size limit; narrow it with --machine or --recipe");
        }
        let rows = history_rows(&h, a.recipe.as_deref(), a.targets);
        ctx.out.write_all(render::stats(&rows).as_bytes())?;
        return Ok(EXIT_OK);
    }
    // an owner filter needs the jobs: the history keeps no owners
    let resp = ctx.client.call(&Request::List {
        owner: a.who.clone(),
        active_only: false,
        recent_finished: Some(a.limit.min(tasks_daemon::api::MAX_LIST_ROWS)),
        limit: None,
    })?;
    let (jobs, _) = as_jobs(resp)?;
    let jobs = select(jobs, a.who.as_deref(), false, None);
    ctx.out
        .write_all(render::stats(&stat_rows(&jobs)).as_bytes())?;
    Ok(EXIT_OK)
}

// ---------------------------------------------------------------------------------------------
// control

fn cancel(
    ctx: &mut Ctx<'_>,
    id: i64,
    who: Option<&str>,
    reason: Option<&str>,
) -> Result<i32, CliError> {
    let owner = resolve_owner(ctx, who)?;
    let resp = ctx.client.call(&Request::Cancel {
        id: JobId(id),
        who: owner.as_str().to_string(),
        reason: reason.map(str::to_string),
    });
    let resp = match resp {
        Ok(r) => r,
        Err(ClientError::ReplyLost { detail }) => {
            return Err(CliError::Failed(format!(
                "the cancel request for job {id} was sent but no reply arrived ({detail}); it was not resent. Check with `tasks2 get {id}`"
            )));
        }
        Err(e) => return Err(e.into()),
    };
    let Response::Cancelled { result, .. } = resp else {
        return Err(CliError::Failed(format!(
            "unexpected reply to cancel: {resp:?}"
        )));
    };
    let text = match result.as_str() {
        "cancelled" => format!("job {id} cancelled before it started"),
        "requested" => {
            format!("job {id} is running; cancel requested, its process group is being stopped")
        }
        "already_finished" => format!("job {id} had already finished"),
        other => format!("job {id}: {other}"),
    };
    match reason {
        Some(r) => writeln!(ctx.out, "{text} (reason: {r})")?,
        None => writeln!(ctx.out, "{text}")?,
    }
    Ok(EXIT_OK)
}

fn prioritize(
    ctx: &mut Ctx<'_>,
    id: i64,
    who: Option<&str>,
    priority: &str,
    reason: Option<&str>,
) -> Result<i32, CliError> {
    let args = crate::args::PriorityArgs {
        priority: Some(priority.to_string()),
        reason: reason.map(str::to_string),
    };
    let (class, reason) = args.resolve()?;
    let owner = resolve_owner(ctx, who)?;
    let resp = ctx.client.call(&Request::Prioritize {
        id: JobId(id),
        who: owner.as_str().to_string(),
        priority: class,
        reason: reason.clone(),
    });
    let resp = match resp {
        Ok(r) => r,
        Err(ClientError::ReplyLost { detail }) => {
            return Err(CliError::Failed(format!(
                "the prioritize request for job {id} was sent but no reply arrived ({detail}); it was not resent. Check with `tasks2 get {id}`"
            )));
        }
        Err(e) => return Err(e.into()),
    };
    let Response::Prioritized { from, to, .. } = resp else {
        return Err(CliError::Failed(format!(
            "unexpected reply to prioritize: {resp:?}"
        )));
    };
    match reason {
        Some(r) => writeln!(ctx.out, "job {id}: {from} -> {to} (reason: {r})")?,
        None => writeln!(ctx.out, "job {id}: {from} -> {to}")?,
    }
    Ok(EXIT_OK)
}

fn ack(ctx: &Ctx<'_>, req: &Request) -> Result<(), CliError> {
    match ctx.client.call(req)? {
        Response::Ack => Ok(()),
        other => Err(CliError::Failed(format!("unexpected reply: {other:?}"))),
    }
}

fn drain(ctx: &mut Ctx<'_>, cancel: bool) -> Result<i32, CliError> {
    // who drained is recorded for the queue line of `status` (R-189); a shell with no owner
    // can still drain, the daemon then says "unknown"
    let who = resolve_owner(ctx, None)
        .ok()
        .map(|o| o.as_str().to_string());
    ack(ctx, &Request::Drain { cancel, who })?;
    if cancel {
        writeln!(ctx.out, "resumed: queued jobs start again")?;
    } else {
        writeln!(
            ctx.out,
            "draining: no new job starts until `tasks2 resume` or the next daemon start; running jobs finish"
        )?;
    }
    Ok(EXIT_OK)
}

fn execution(ctx: &mut Ctx<'_>, action: ExecutionCommand) -> Result<i32, CliError> {
    let enabled = matches!(action, ExecutionCommand::Start);
    ack(ctx, &Request::Execution { enabled })?;
    writeln!(
        ctx.out,
        "{}",
        if enabled {
            "execution started: queued jobs run (kept across daemon restarts)"
        } else {
            "execution stopped: queued jobs wait until `tasks2 execution start` (kept across daemon restarts)"
        }
    )?;
    Ok(EXIT_OK)
}

// ---------------------------------------------------------------------------------------------
// plans and suites

fn print_plan(
    ctx: &mut Ctx<'_>,
    title: &str,
    plan_id: i64,
    jobs: &[(String, JobId)],
) -> Result<(), CliError> {
    writeln!(
        ctx.out,
        "tasks: submitted plan {plan_id}{title} with {} step(s)",
        jobs.len()
    )?;
    for (name, id) in jobs {
        writeln!(ctx.out, "  {name} -> job {}", id.0)?;
    }
    Ok(())
}

fn steps(ctx: &mut Ctx<'_>, a: &StepsArgs) -> Result<i32, CliError> {
    let text = (ctx.read_input)(&a.file)
        .map_err(|e| CliError::NotFound(format!("cannot read plan {}: {e}", a.file.display())))?;
    let plan =
        Plan::parse(&text).map_err(|e| CliError::Usage(format!("{}: {e}", a.file.display())))?;
    plan.validate()
        .map_err(|e| CliError::Usage(format!("{}: {e}", a.file.display())))?;
    let placement = a.place.placement(None)?;
    let (priority, priority_reason) = a.class.resolve()?;
    let owner = resolve_owner(ctx, a.who.as_deref())?;
    for s in &plan.steps {
        placement_note(ctx, &s.argv, &placement);
    }
    let resp = ctx.client.call(&Request::SubmitPlan {
        plan,
        owner: owner.as_str().to_string(),
        cwd: ctx.cwd.clone(),
        env: Vec::new(),
        placement,
        broad_test: a.broad_test,
        priority,
        priority_reason,
    })?;
    let Response::PlanSubmitted { plan_id, jobs } = resp else {
        return Err(CliError::Failed(format!(
            "unexpected reply to a plan: {resp:?}"
        )));
    };
    print_plan(ctx, "", plan_id, &jobs)?;
    Ok(EXIT_OK)
}

fn suite_run(ctx: &mut Ctx<'_>, a: &SuiteRunArgs) -> Result<i32, CliError> {
    let placement = a.place.placement(None)?;
    let (priority, priority_reason) = a.class.resolve()?;
    let owner = resolve_owner(ctx, a.who.as_deref())?;
    let resp = ctx.client.call(&Request::SubmitSuite {
        name: a.name.clone(),
        owner: owner.as_str().to_string(),
        cwd: ctx.cwd.clone(),
        env: Vec::new(),
        placement,
        quick: a.quick,
        broad_test: a.broad_test,
        priority,
        priority_reason,
    })?;
    let Response::PlanSubmitted { plan_id, jobs } = resp else {
        return Err(CliError::Failed(format!(
            "unexpected reply to a suite: {resp:?}"
        )));
    };
    print_plan(ctx, &format!(" (suite {})", a.name), plan_id, &jobs)?;
    Ok(EXIT_OK)
}

fn policy_error(e: tasks_policy::PolicyError) -> CliError {
    match e {
        tasks_policy::PolicyError::NotFound(_) => CliError::NotFound(e.to_string()),
        other => CliError::Failed(other.to_string()),
    }
}

fn suite_list(ctx: &mut Ctx<'_>) -> Result<i32, CliError> {
    let loaded = tasks_policy::load(ctx.policy_fs, &ctx.cwd).map_err(policy_error)?;
    for (name, suite) in &loaded.policy.suites {
        let note = suite
            .notes
            .as_deref()
            .and_then(|n| n.lines().next())
            .map(|l| format!(": {l}"))
            .unwrap_or_default();
        writeln!(ctx.out, "{name} ({} step(s)){note}", suite.steps.len())?;
    }
    Ok(EXIT_OK)
}

fn policy_show(ctx: &mut Ctx<'_>) -> Result<i32, CliError> {
    let loaded = tasks_policy::load(ctx.policy_fs, &ctx.cwd).map_err(policy_error)?;
    let text = tasks_policy::show(
        &loaded.policy,
        &loaded.file,
        None,
        &tasks_policy::Candidates::new(),
    )
    .map_err(policy_error)?;
    ctx.out.write_all(text.as_bytes())?;
    Ok(EXIT_OK)
}

/// The proposal text for `loaded` from `history`.
pub fn suggest_report(
    loaded: &tasks_policy::Loaded,
    history: &dyn History,
    machines: &[String],
) -> Result<String, CliError> {
    let timing = tasks_policy::Timing { history, machines };
    let suggestions = tasks_policy::suggest(
        &loaded.policy,
        &timing,
        &tasks_policy::Candidates::new(),
        &tasks_policy::SuggestConfig::default(),
    )
    .map_err(policy_error)?;
    Ok(tasks_policy::render_proposal(
        &loaded.file.display().to_string(),
        &loaded.text,
        &suggestions,
    ))
}

fn policy_suggest(ctx: &mut Ctx<'_>) -> Result<i32, CliError> {
    let loaded = tasks_policy::load(ctx.policy_fs, &ctx.cwd).map_err(policy_error)?;
    let machine = match ctx.client.call(&Request::Hello)? {
        Response::Hello(h) => h.machine,
        other => {
            return Err(CliError::Failed(format!(
                "unexpected reply to hello: {other:?}"
            )));
        }
    };
    let text = match ctx.history {
        Some(history) => suggest_report(&loaded, history, &[machine])?,
        None => {
            // only the recipes the policy's steps run can be suggested for
            let mut recipes: Vec<String> = loaded
                .policy
                .suites
                .values()
                .flat_map(|s| s.steps.iter())
                .filter_map(|d| {
                    RecipeKey::from_argv(&d.command, &[])
                        .first()
                        .map(ToString::to_string)
                })
                .collect();
            recipes.sort();
            recipes.dedup();
            let h = fetch_history(
                ctx,
                recipes,
                vec![machine.clone()],
                Some(tasks_daemon::api::MAX_HISTORY_SAMPLES),
            )?;
            if h.truncated {
                ctx.note("tasks: the daemon cut the history at its size limit; suggestions may miss samples");
            }
            suggest_report(&loaded, &h.to_history(), &[machine])?
        }
    };
    ctx.out.write_all(text.as_bytes())?;
    Ok(EXIT_OK)
}

fn split(ctx: &mut Ctx<'_>, a: &SplitArgs) -> Result<i32, CliError> {
    let text = (ctx.read_input)(&a.exe)
        .map_err(|e| CliError::NotFound(format!("cannot read {}: {e}", a.exe.display())))?;
    let plan = split_plan(&a.command, &text)?;
    writeln!(ctx.out, "{}", pretty(&plan)?)?;
    Ok(EXIT_OK)
}

/// Splits `cargo test ...` using the executables in cargo's JSON output.
pub fn split_plan(command: &[String], cargo_json: &str) -> Result<Plan, CliError> {
    let exes = tasks_timing::parse_test_executables(cargo_json)
        .map_err(|e| CliError::Usage(format!("cargo output: {e}")))?;
    tasks_timing::split_cargo_test(command, &exes).map_err(|e| CliError::Usage(e.to_string()))
}

#[cfg(test)]
mod tests;
