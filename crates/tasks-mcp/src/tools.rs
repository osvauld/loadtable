//! The tools: argument checks, daemon requests, and short text plus structured results.

use crate::cancel::CancelToken;
use crate::daemon::{Daemon, DaemonError};
use crate::text::{
    MAX_TEXT_BYTES, bound_head, bound_tail, dur, elapsed_secs, grep_text, job_detail, job_json,
    job_line, phase_tag, phase_text,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tasks_core::plan::{Plan, Step};
use tasks_core::{DeviceId, JobId, JobSpec, Kind, MachineName, Owner, Placement};
use tasks_daemon::api::{
    ErrorCode, JobView, LogView, MAX_CONTEXT, MAX_LOG_CHUNK, MAX_TAIL_LINES, Request, Response,
    StatusSnapshot,
};
use tasks_timing::RecipeKey;

/// Finished jobs `list` shows when no limit is given.
const DEFAULT_LIST_RECENT: usize = 20;
/// Rows `audit` shows when no limit is given.
const DEFAULT_AUDIT_ROWS: usize = 50;
/// Default tail length of `logs`.
const DEFAULT_TAIL: usize = 100;
/// Default byte window of `logs` with an offset.
const DEFAULT_CHUNK: usize = 32 * 1024;
/// Log lines `wait` appends once the job is over.
const DEFAULT_WAIT_TAIL: usize = 20;
/// Finished jobs a submit reconciliation looks at.
const RECONCILE_RECENT: usize = 30;
/// A reconciled job must have been submitted no earlier than this long before the attempt.
const RECONCILE_SLACK_MS: i64 = 5_000;

/// Settings of the tool set.
#[derive(Clone)]
pub struct Config {
    /// Default checkout for submissions: the server's working directory.
    pub cwd: PathBuf,
    /// Default job owner: `$TASKS_WHO`, then `$USER`.
    pub default_who: Option<String>,
    /// Pause between polls of a waited job.
    pub poll: Duration,
    /// Wait budget when the call names none.
    pub default_wait: Duration,
    /// Longest wait one call may ask for.
    pub max_wait: Duration,
    /// Milliseconds since the epoch.
    pub now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl Config {
    /// Settings for a server started in `cwd` with this environment.
    pub fn from_env(cwd: PathBuf, env: &dyn Fn(&str) -> Option<String>) -> Config {
        let non_empty = |k: &str| env(k).filter(|v| !v.is_empty());
        Config {
            cwd,
            default_who: non_empty("TASKS_WHO").or_else(|| non_empty("USER")),
            poll: Duration::from_millis(500),
            default_wait: Duration::from_secs(120),
            max_wait: Duration::from_secs(3600),
            now_ms: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            }),
        }
    }
}

/// What a tool call produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    /// Short text for the model.
    pub text: String,
    /// The same facts as data.
    pub structured: Value,
    /// True for a tool-level failure (the call itself worked, the work did not).
    pub is_error: bool,
}

impl ToolResult {
    /// The `tools/call` result object.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "content": [{"type": "text", "text": self.text}],
            "structuredContent": self.structured,
        });
        if let (true, Some(o)) = (self.is_error, v.as_object_mut()) {
            o.insert("isError".into(), Value::Bool(true));
        }
        v
    }
}

/// How a tool call ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A result to send.
    Done(ToolResult),
    /// The client cancelled the request: send nothing.
    Cancelled,
}

enum ToolError {
    Args(String),
    Refused(String),
    Daemon(DaemonError),
    Cancelled,
}

type ToolOk = Result<ToolResult, ToolError>;

impl From<DaemonError> for ToolError {
    fn from(e: DaemonError) -> ToolError {
        ToolError::Daemon(e)
    }
}

fn code_name(c: ErrorCode) -> &'static str {
    match c {
        ErrorCode::UnsupportedVersion => "unsupported_version",
        ErrorCode::BadRequest => "bad_request",
        ErrorCode::TooLarge => "too_large",
        ErrorCode::ResponseTooLarge => "response_too_large",
        ErrorCode::NotFound => "not_found",
        ErrorCode::NotOwner => "not_owner",
        ErrorCode::Busy => "busy",
        ErrorCode::ConnectionLimit => "connection_limit",
        ErrorCode::Refused => "refused",
        ErrorCode::LedgerUnavailable => "ledger_unavailable",
        ErrorCode::Internal => "internal",
    }
}

fn error_result(kind: &str, message: String) -> ToolResult {
    ToolResult {
        structured: json!({"error": {"kind": kind, "message": message}}),
        text: message,
        is_error: true,
    }
}

impl ToolError {
    fn into_outcome(self) -> Outcome {
        Outcome::Done(match self {
            ToolError::Cancelled => return Outcome::Cancelled,
            ToolError::Args(m) => error_result("invalid_arguments", m),
            ToolError::Refused(m) => error_result("refused", m),
            ToolError::Daemon(DaemonError::Refused(e)) if e.code == ErrorCode::ConnectionLimit => {
                error_result(
                    "connection_limit",
                    format!(
                        "the daemon has too many connections ({}). The request was not processed; it is safe to try again in a moment",
                        e.message
                    ),
                )
            }
            ToolError::Daemon(DaemonError::Refused(e)) => {
                error_result(code_name(e.code), e.message)
            }
            ToolError::Daemon(DaemonError::Unreachable(m)) => error_result(
                "daemon_unreachable",
                format!(
                    "{m}. Nothing was sent; the tasks daemon is not available, try again shortly or run `tasks2 status` in a shell"
                ),
            ),
            ToolError::Daemon(DaemonError::ReplyLost(d)) => error_result(
                "reply_lost",
                format!(
                    "no reply from the daemon ({d}). The request may have been delivered and was not resent; check with list or get before trying again"
                ),
            ),
            ToolError::Daemon(DaemonError::Failed(m)) => error_result("daemon_error", m),
        })
    }
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    let args = if args.is_null() { json!({}) } else { args };
    serde_json::from_value(args).map_err(|e| ToolError::Args(format!("invalid arguments: {e}")))
}

/// The queue class and reason of a call: `blocking` needs a reason, the others take none.
fn class(
    priority: Option<&str>,
    reason: Option<&str>,
) -> Result<(tasks_core::Priority, Option<String>), ToolError> {
    let class = match priority {
        None => tasks_core::Priority::Normal,
        Some(p) => p
            .parse::<tasks_core::Priority>()
            .map_err(|e| ToolError::Args(e.to_string()))?,
    };
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    match (class, reason) {
        (tasks_core::Priority::Blocking, None) => Err(ToolError::Args(
            "priority blocking needs a reason: say what is waiting on this job".into(),
        )),
        (tasks_core::Priority::Blocking, Some(r)) => Ok((class, Some(r.to_string()))),
        (_, Some(_)) => Err(ToolError::Args(
            "reason goes with priority blocking only".into(),
        )),
        (_, None) => Ok((class, None)),
    }
}

fn unexpected(what: &str, r: &Response) -> ToolError {
    ToolError::Daemon(DaemonError::Failed(format!(
        "unexpected reply to {what}: {r:?}"
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitArgs {
    argv: Vec<String>,
    cwd: Option<String>,
    who: Option<String>,
    placement: Option<String>,
    peer: Option<String>,
    device: Option<String>,
    timeout_s: Option<u64>,
    broad_test: Option<bool>,
    after: Option<Vec<i64>>,
    priority: Option<String>,
    reason: Option<String>,
    fetch: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepArg {
    name: String,
    argv: Vec<String>,
    #[serde(default)]
    after: Vec<String>,
    kind: Option<String>,
    #[serde(default)]
    fetch: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepsArgs {
    steps: Vec<StepArg>,
    cwd: Option<String>,
    who: Option<String>,
    placement: Option<String>,
    peer: Option<String>,
    broad_test: Option<bool>,
    priority: Option<String>,
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteArgs {
    name: String,
    cwd: Option<String>,
    who: Option<String>,
    placement: Option<String>,
    peer: Option<String>,
    quick: Option<bool>,
    broad_test: Option<bool>,
    priority: Option<String>,
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    who: Option<String>,
    active: Option<bool>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdArgs {
    id: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogsArgs {
    id: i64,
    tail: Option<usize>,
    grep: Option<String>,
    fixed: Option<bool>,
    context: Option<usize>,
    max_matches: Option<usize>,
    offset: Option<u64>,
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    id: i64,
    timeout_s: Option<u64>,
    tail_lines: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelArgs {
    id: i64,
    who: String,
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrioritizeArgs {
    id: i64,
    who: String,
    priority: String,
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EstimateArgs {
    argv: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditArgs {
    since: Option<String>,
    peer: Option<String>,
    job: Option<i64>,
    action: Option<String>,
    limit: Option<usize>,
}

fn job_id(id: i64) -> Result<JobId, ToolError> {
    if id < 1 {
        return Err(ToolError::Args(format!(
            "id {id} is not a job id; ids start at 1"
        )));
    }
    Ok(JobId(id))
}

/// The placement, or the precise conflict and its fix (R-190's wording for MCP arguments).
fn placement(
    placement: Option<&str>,
    peer: Option<&str>,
    device: Option<&str>,
) -> Result<Placement, ToolError> {
    let bad = |m: String| Err(ToolError::Args(m));
    if let Some(p) = peer {
        if p.trim().is_empty() {
            return bad("peer needs a machine name, for example \"nixos\"".into());
        }
        if let Some(pl) = placement.filter(|pl| *pl != "here") {
            return bad(format!(
                "placement {pl:?} and peer {p:?} conflict: peer sends the job to that machine; pass only one of them"
            ));
        }
        if let Some(d) = device {
            return bad(format!(
                "device {d} and peer {p:?} conflict: a device job runs on the machine the device is attached to, which is this one; drop peer to run here, or drop device to run elsewhere"
            ));
        }
        return Ok(Placement::Peer(MachineName(p.to_string())));
    }
    match placement {
        None | Some("here") => Ok(Placement::Here),
        Some("pin") => Ok(Placement::Pin),
        Some("anywhere") => match device {
            Some(d) => bad(format!(
                "device {d} and placement \"anywhere\" conflict: a device job runs on the machine the device is attached to, which is this one; drop placement to run here, or drop device to run elsewhere"
            )),
            None => Ok(Placement::Anywhere),
        },
        Some(other) => bad(format!(
            "placement {other:?} is not one of here, pin, anywhere; use peer to name a machine"
        )),
    }
}

fn parse_kind(s: &str) -> Result<Kind, ToolError> {
    Ok(match s {
        "compile" => Kind::Compile,
        "test" => Kind::Test,
        "run" => Kind::Run,
        "package" => Kind::Package,
        "device" => Kind::Device,
        "probe" => Kind::Probe,
        other => {
            return Err(ToolError::Args(format!(
                "step kind {other:?} is not one of compile, test, run, package, device, probe"
            )));
        }
    })
}

fn placement_name(p: &Placement) -> String {
    match p {
        Placement::Here => "here".into(),
        Placement::Pin => "pin".into(),
        Placement::Anywhere => "anywhere".into(),
        Placement::Peer(m) => format!("peer {}", m.0),
    }
}

/// The tool set over one daemon.
pub struct Tools {
    daemon: Arc<dyn Daemon>,
    cfg: Config,
}

impl Tools {
    /// Tools that talk to `daemon`.
    pub fn new(daemon: Arc<dyn Daemon>, cfg: Config) -> Tools {
        Tools { daemon, cfg }
    }

    /// Runs the tool `name`. `None` when there is no such tool. Argument and daemon failures
    /// are results with `is_error`, never panics or protocol errors.
    pub fn call(&self, name: &str, args: Value, tok: &CancelToken) -> Option<Outcome> {
        let r = match name {
            "submit" => parse(args).and_then(|a| self.submit(a)),
            "submit_steps" => parse(args).and_then(|a| self.submit_steps(a)),
            "run_suite" => parse(args).and_then(|a| self.run_suite(a)),
            "list" => parse(args).and_then(|a| self.list(a)),
            "get" => parse(args).and_then(|a| self.get(a)),
            "logs" => parse(args).and_then(|a| self.logs(a)),
            "wait" => parse(args).and_then(|a| self.wait(a, tok)),
            "cancel" => parse(args).and_then(|a| self.cancel(a)),
            "prioritize" => parse(args).and_then(|a| self.prioritize(a)),
            "status" => parse::<EmptyArgs>(args).and_then(|_| self.status()),
            "estimate" => parse(args).and_then(|a| self.estimate(a)),
            "audit" => parse(args).and_then(|a| self.audit(a)),
            _ => return None,
        };
        Some(match r {
            Ok(res) => Outcome::Done(res),
            Err(e) => e.into_outcome(),
        })
    }

    fn now(&self) -> i64 {
        (self.cfg.now_ms)()
    }

    fn owner(&self, who: Option<&str>) -> Result<Owner, ToolError> {
        let name = who
            .map(str::to_string)
            .or_else(|| self.cfg.default_who.clone())
            .ok_or_else(|| {
                ToolError::Args(
                    "no job owner: pass who (your agent name) or set TASKS_WHO for the MCP server"
                        .into(),
                )
            })?;
        Owner::new(&name).map_err(|e| {
            ToolError::Args(format!(
                "owner {name:?} is not valid ({e}); use letters, digits, '.', '_' or '-'"
            ))
        })
    }

    fn cwd(&self, cwd: Option<&str>) -> Result<PathBuf, ToolError> {
        match cwd {
            None => Ok(self.cfg.cwd.clone()),
            Some(c) => {
                let p = PathBuf::from(c);
                if p.is_absolute() {
                    Ok(p)
                } else {
                    Err(ToolError::Args(format!(
                        "cwd {c:?} is not an absolute path; pass the full path of your checkout"
                    )))
                }
            }
        }
    }

    fn call_daemon(&self, req: &Request) -> Result<Response, ToolError> {
        Ok(self.daemon.call(req)?)
    }

    fn get_job(&self, id: JobId) -> Result<JobView, ToolError> {
        match self.call_daemon(&Request::Get { id })? {
            Response::Job { job } => Ok(*job),
            other => Err(unexpected("get", &other)),
        }
    }

    // -- submitting ---------------------------------------------------------------------

    fn submit(&self, a: SubmitArgs) -> ToolOk {
        // every check runs before the first daemon request (R-193)
        let placement = placement(
            a.placement.as_deref(),
            a.peer.as_deref(),
            a.device.as_deref(),
        )?;
        let owner = self.owner(a.who.as_deref())?;
        let cwd = self.cwd(a.cwd.as_deref())?;
        if a.argv.is_empty() {
            return Err(ToolError::Args("argv must name a program".into()));
        }
        if a.timeout_s == Some(0) {
            return Err(ToolError::Args("timeout_s must be at least 1".into()));
        }
        // The submit rules (R-191 broad test, R-210 denied devices, R-091 -j cap) are applied by
        // the daemon; its refusal reaches the agent as a tool error.
        let kind = if a.device.is_some() {
            Kind::Device
        } else {
            RecipeKey::from_argv(&a.argv, &[])
                .first()
                .map_or(Kind::Run, RecipeKey::kind)
        };
        let (priority, priority_reason) = class(a.priority.as_deref(), a.reason.as_deref())?;
        let mut spec = JobSpec {
            owner,
            argv: a.argv,
            cwd,
            env: Vec::new(),
            kind,
            requirements: tasks_core::Requirements::default(),
            placement,
            timeout_secs: a.timeout_s,
            broad_test_escape: a.broad_test.unwrap_or(false),
            priority,
            priority_reason,
            after: a
                .after
                .unwrap_or_default()
                .into_iter()
                .map(job_id)
                .collect::<Result<_, _>>()?,
            fetch: a.fetch.unwrap_or_default(),
        };
        spec.requirements.device = a.device.map(DeviceId);
        spec.validate()
            .map_err(|e| ToolError::Refused(format!("invalid job: {e}")))?;
        let mut notes: Vec<String> = Vec::new();
        if let Ok(Response::Jobs { jobs, .. }) = self.daemon.call(&Request::List {
            owner: None,
            active_only: true,
            recent_finished: None,
            limit: None,
        }) && let Some(w) = shared_cwd_warning(&jobs, spec.owner.as_str(), &spec.cwd)
        {
            notes.push(w);
        }
        let id = self.submit_reconciled(&spec, &mut notes)?;
        let mut text = format!(
            "submitted job {} ({}, {}): {}\nnext: wait with id {}, or logs",
            id.0,
            spec.owner.as_str(),
            placement_name(&spec.placement),
            crate::text::short_cmd(&spec.argv, 120),
            id.0
        );
        for n in &notes {
            text.push_str(&format!("\nnote: {n}"));
        }
        Ok(ToolResult {
            text,
            structured: json!({
                "id": id.0,
                "owner": spec.owner.as_str(),
                "placement": placement_name(&spec.placement),
                "kind": format!("{:?}", spec.kind),
                "notes": notes,
            }),
            is_error: false,
        })
    }

    /// Submits `spec`. When the reply is lost, finds the job in the ledger by exact spec match
    /// instead of resending (R-099, R-100).
    fn submit_reconciled(
        &self,
        spec: &JobSpec,
        notes: &mut Vec<String>,
    ) -> Result<JobId, ToolError> {
        let before = self.now();
        match self.daemon.call(&Request::Submit { spec: spec.clone() }) {
            Ok(Response::Submitted { id, warnings }) => {
                notes.extend(warnings.iter().map(|w| format!("warning: {w}")));
                Ok(id)
            }
            Ok(other) => Err(ToolError::Daemon(DaemonError::Failed(format!(
                "unexpected reply to submit: {other:?}; do not resubmit, look in list with who {}",
                spec.owner.as_str()
            )))),
            Err(DaemonError::ReplyLost(lost)) => {
                self.reconcile_lost_submit(spec, before, &lost, notes)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn reconcile_lost_submit(
        &self,
        spec: &JobSpec,
        before_ms: i64,
        lost: &str,
        notes: &mut Vec<String>,
    ) -> Result<JobId, ToolError> {
        let owner = spec.owner.as_str();
        let hint = format!("list with who {owner}");
        let listed = self.daemon.call(&Request::List {
            owner: Some(owner.to_string()),
            active_only: false,
            recent_finished: Some(RECONCILE_RECENT),
            limit: None,
        });
        let fail = |m: String| ToolError::Daemon(DaemonError::Failed(m));
        let Ok(Response::Jobs { jobs, .. }) = listed else {
            return Err(fail(format!(
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
                notes.push(format!(
                    "the daemon's reply to submit was lost ({lost}); found the submitted job {} in the ledger by exact spec match; not resubmitted",
                    id.0
                ));
                Ok(*id)
            }
            [] => Err(fail(format!(
                "submit was sent but no reply arrived ({lost}) and no matching job is listed yet; it may still be recorded. Do NOT resubmit: look in {hint}"
            ))),
            many => {
                let ids: Vec<String> = many.iter().map(|i| i.0.to_string()).collect();
                Err(fail(format!(
                    "submit reply lost ({lost}) and several identical jobs match: {}. One is yours; inspect them with {hint} / logs. Do NOT resubmit",
                    ids.join(", ")
                )))
            }
        }
    }

    fn submit_steps(&self, a: StepsArgs) -> ToolOk {
        let placement = placement(a.placement.as_deref(), a.peer.as_deref(), None)?;
        let owner = self.owner(a.who.as_deref())?;
        let cwd = self.cwd(a.cwd.as_deref())?;
        let mut steps = Vec::new();
        for s in a.steps {
            let kind = match s.kind.as_deref() {
                Some(k) => parse_kind(k)?,
                None => Kind::Compile,
            };
            steps.push(Step {
                name: s.name,
                argv: s.argv,
                after: s.after,
                kind,
                requirements: tasks_core::Requirements::default(),
                hints: tasks_core::StepHints {
                    fetch: s.fetch,
                    ..tasks_core::StepHints::default()
                },
            });
        }
        let plan = Plan { steps };
        plan.validate()
            .map_err(|e| ToolError::Args(format!("invalid plan: {e}")))?;
        let (priority, priority_reason) = class(a.priority.as_deref(), a.reason.as_deref())?;
        let notes: Vec<String> = Vec::new();
        let resp = self.call_daemon(&Request::SubmitPlan {
            plan,
            owner: owner.as_str().to_string(),
            cwd,
            env: Vec::new(),
            placement,
            broad_test: a.broad_test.unwrap_or(false),
            priority,
            priority_reason,
        })?;
        plan_result(resp, "plan", "submit_steps", notes)
    }

    fn run_suite(&self, a: SuiteArgs) -> ToolOk {
        let placement = placement(a.placement.as_deref(), a.peer.as_deref(), None)?;
        let owner = self.owner(a.who.as_deref())?;
        let cwd = self.cwd(a.cwd.as_deref())?;
        let (priority, priority_reason) = class(a.priority.as_deref(), a.reason.as_deref())?;
        let resp = self.call_daemon(&Request::SubmitSuite {
            name: a.name.clone(),
            owner: owner.as_str().to_string(),
            cwd,
            env: Vec::new(),
            placement,
            quick: a.quick.unwrap_or(false),
            broad_test: a.broad_test.unwrap_or(false),
            priority,
            priority_reason,
        })?;
        plan_result(resp, &format!("suite {}", a.name), "run_suite", Vec::new())
    }

    // -- reading ------------------------------------------------------------------------

    fn list(&self, a: ListArgs) -> ToolOk {
        let active = a.active.unwrap_or(false);
        let limit = a.limit.unwrap_or(DEFAULT_LIST_RECENT).clamp(1, 200);
        let resp = self.call_daemon(&Request::List {
            owner: a.who.clone(),
            active_only: active,
            recent_finished: (!active).then_some(limit),
            limit: None,
        })?;
        let Response::Jobs { jobs, truncated } = resp else {
            return Err(unexpected("list", &resp));
        };
        let jobs: Vec<JobView> = jobs
            .into_iter()
            .filter(|j| a.who.as_deref().is_none_or(|w| j.owner == w))
            .filter(|j| !active || !j.phase.is_terminal())
            .collect();
        let now = self.now();
        let body: String = jobs.iter().map(|j| job_line(j, now) + "\n").collect();
        let (body, cut) = bound_head(&body, MAX_TEXT_BYTES);
        let mut text = if jobs.is_empty() {
            "no jobs".to_string()
        } else {
            format!("{} job(s), oldest first\n{}", jobs.len(), body.trim_end())
        };
        if cut || truncated {
            text.push_str("\ntruncated: narrow the list with who, active or limit");
        }
        Ok(ToolResult {
            text,
            structured: json!({
                "jobs": jobs.iter().map(|j| job_json(j, now)).collect::<Vec<_>>(),
                "truncated": cut || truncated,
            }),
            is_error: false,
        })
    }

    fn get(&self, a: IdArgs) -> ToolOk {
        let job = self.get_job(job_id(a.id)?)?;
        let now = self.now();
        Ok(ToolResult {
            text: job_detail(&job, now).trim_end().to_string(),
            structured: job_json(&job, now),
            is_error: false,
        })
    }

    fn logs(&self, a: LogsArgs) -> ToolOk {
        let id = job_id(a.id)?;
        let conflict = |m: &str| Err(ToolError::Args(m.to_string()));
        if a.offset.is_some() && (a.tail.is_some() || a.grep.is_some()) {
            return conflict(
                "offset reads forward from a byte offset; do not combine it with tail or grep",
            );
        }
        if a.tail.is_some() && a.grep.is_some() {
            return conflict("tail and grep are different views; pass one");
        }
        if a.grep.is_none() && (a.fixed.is_some() || a.context.is_some() || a.max_matches.is_some())
        {
            return conflict("fixed, context and max_matches only apply with grep");
        }
        if a.offset.is_none() && a.max_bytes.is_some() {
            return conflict("max_bytes only applies with offset");
        }
        let result = if let Some(offset) = a.offset {
            self.logs_from(id, offset, a.max_bytes)
        } else if let Some(pattern) = a.grep {
            let context = a.context.unwrap_or(0);
            if context > MAX_CONTEXT {
                return Err(ToolError::Args(format!(
                    "context {context} is too large; at most {MAX_CONTEXT} lines"
                )));
            }
            self.logs_grep(
                id,
                pattern,
                a.fixed.unwrap_or(false),
                context,
                a.max_matches,
            )
        } else {
            let lines = a.tail.unwrap_or(DEFAULT_TAIL);
            if lines == 0 || lines > MAX_TAIL_LINES {
                return Err(ToolError::Args(format!(
                    "tail {lines} is out of range; use 1 to {MAX_TAIL_LINES}"
                )));
            }
            self.logs_tail(id, lines)
        };
        match result {
            Err(ToolError::Daemon(DaemonError::Refused(e))) if e.code == ErrorCode::NotFound => {
                Err(ToolError::Daemon(DaemonError::Refused(
                    tasks_daemon::api::ApiError::new(
                        ErrorCode::NotFound,
                        format!(
                            "no log for job {} (no such job, or it has not started yet; check with get): {}",
                            id.0, e.message
                        ),
                    ),
                )))
            }
            other => other,
        }
    }

    fn logs_tail(&self, id: JobId, lines: usize) -> ToolOk {
        let resp = self.call_daemon(&Request::Logs {
            id,
            view: LogView::Tail { lines },
        })?;
        let Response::LogTail {
            lines: got,
            running,
            ..
        } = resp
        else {
            return Err(unexpected("a log read", &resp));
        };
        let all = got.join("\n");
        let (shown, cut) = bound_tail(&all, MAX_TEXT_BYTES);
        let mut text = String::new();
        if cut {
            text.push_str(&format!(
                "truncated: showing the last {} bytes of {} lines; ask for a smaller tail, grep, or logs with offset 0 to read forward from the start\n",
                shown.len(),
                got.len()
            ));
        }
        text.push_str(shown);
        if got.is_empty() {
            text = "(the log is empty so far)".into();
        }
        if running {
            text.push_str("\n(job still running; the last line may grow)");
        }
        Ok(ToolResult {
            text,
            structured: json!({"mode": "tail", "lines": got.len(), "running": running, "truncated": cut}),
            is_error: false,
        })
    }

    fn logs_grep(
        &self,
        id: JobId,
        pattern: String,
        fixed: bool,
        context: usize,
        max_matches: Option<usize>,
    ) -> ToolOk {
        let resp = self.call_daemon(&Request::Logs {
            id,
            view: LogView::Grep {
                pattern,
                context,
                regex: !fixed,
                max_matches,
            },
        })?;
        let Response::LogGrep {
            groups,
            matches,
            truncated,
        } = resp
        else {
            return Err(unexpected("a log read", &resp));
        };
        let body = grep_text(&groups);
        let (shown, cut) = bound_head(&body, MAX_TEXT_BYTES);
        let mut text = if matches == 0 {
            "no matching lines".to_string()
        } else {
            format!(
                "{matches} matching line(s) (N: match, N- context)\n{}",
                shown.trim_end()
            )
        };
        if cut {
            text.push_str(
                "\ntruncated: too much matched text; narrow the pattern or lower max_matches",
            );
        } else if truncated {
            text.push_str("\ntruncated: the search stopped at its match limit; narrow the pattern");
        }
        Ok(ToolResult {
            text,
            structured: json!({"mode": "grep", "matches": matches, "truncated": cut || truncated}),
            is_error: false,
        })
    }

    fn logs_from(&self, id: JobId, offset: u64, max_bytes: Option<usize>) -> ToolOk {
        let want = max_bytes.unwrap_or(DEFAULT_CHUNK).clamp(1, MAX_LOG_CHUNK);
        let resp = self.call_daemon(&Request::Logs {
            id,
            view: LogView::Follow {
                offset,
                max_bytes: Some(want),
            },
        })?;
        let Response::LogChunk {
            data,
            next_offset,
            done,
        } = resp
        else {
            return Err(unexpected("a log read", &resp));
        };
        let (shown, cut) = bound_head(&data, want.min(MAX_TEXT_BYTES));
        let next = if cut {
            offset + shown.len() as u64
        } else {
            next_offset
        };
        let more = cut || data.len() >= want;
        let mut text = shown.to_string();
        if more {
            text.push_str(&format!(
                "\ntruncated, use logs with offset {next} to continue"
            ));
        } else if done {
            text.push_str(&format!("\n(end of log at offset {next})"));
        } else {
            text.push_str(&format!(
                "\n(job still running; logs with offset {next} reads what comes next)"
            ));
        }
        Ok(ToolResult {
            text,
            structured: json!({"mode": "offset", "offset": offset, "next_offset": next, "done": done && !more, "truncated": more}),
            is_error: false,
        })
    }

    fn wait(&self, a: WaitArgs, tok: &CancelToken) -> ToolOk {
        let id = job_id(a.id)?;
        let timeout = a
            .timeout_s
            .map_or(self.cfg.default_wait, Duration::from_secs)
            .min(self.cfg.max_wait);
        let tail = a.tail_lines.unwrap_or(DEFAULT_WAIT_TAIL).min(500);
        let start = Instant::now();
        loop {
            if tok.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let job = self.get_job(id)?;
            let now = self.now();
            if job.phase.is_terminal() {
                return Ok(self.finished(&job, tail, now));
            }
            let left = timeout.saturating_sub(start.elapsed());
            if left.is_zero() {
                let mut text = format!(
                    "job {} is still {} after waiting {}; the job is untouched. Call wait again to keep waiting",
                    job.id.0,
                    phase_text(&job),
                    dur(start.elapsed().as_secs_f64())
                );
                if let Some(w) = job.remote_wait.as_ref().or(job.wait_text.as_ref()) {
                    text.push_str(&format!("\nwaiting: {w}"));
                }
                return Ok(ToolResult {
                    text,
                    structured: json!({"finished": false, "timed_out": true, "job": job_json(&job, now)}),
                    is_error: false,
                });
            }
            if tok.sleep(self.cfg.poll.min(left)) {
                return Err(ToolError::Cancelled);
            }
        }
    }

    fn finished(&self, job: &JobView, tail: usize, now: i64) -> ToolResult {
        let mut text = format!(
            "job {} {} after {}",
            job.id.0,
            phase_text(job),
            dur(elapsed_secs(job, now))
        );
        if let Some(t) = job.summary.as_ref().filter(|t| !t.is_empty()) {
            text.push_str(&format!(": {t}"));
        }
        for r in &job.fetch {
            text.push_str(&format!("\n{}", r.line()));
        }
        let mut log_lines = None;
        if tail > 0 {
            match self.daemon.call(&Request::Logs {
                id: job.id,
                view: LogView::Tail { lines: tail },
            }) {
                Ok(Response::LogTail { lines, .. }) if !lines.is_empty() => {
                    let all = lines.join("\n");
                    let (shown, cut) = bound_tail(&all, MAX_TEXT_BYTES);
                    text.push_str(&format!(
                        "\n--- last {} line(s) of the log ---\n",
                        lines.len()
                    ));
                    if cut {
                        text.push_str("(truncated; use logs for more)\n");
                    }
                    text.push_str(shown);
                    log_lines = Some(lines.len());
                }
                Ok(_) => {}
                Err(e) => text.push_str(&format!("\n(the log could not be read: {e}; try logs)")),
            }
        }
        ToolResult {
            text,
            structured: json!({"finished": true, "passed": job.phase == tasks_core::Phase::Passed, "job": job_json(job, now), "log_lines": log_lines}),
            is_error: false,
        }
    }

    // -- control and machine ------------------------------------------------------------

    fn cancel(&self, a: CancelArgs) -> ToolOk {
        let id = job_id(a.id)?;
        let owner = self.owner(Some(&a.who))?;
        let resp = match self.daemon.call(&Request::Cancel {
            id,
            who: owner.as_str().to_string(),
            reason: a.reason.clone(),
        }) {
            Err(DaemonError::ReplyLost(d)) => {
                return Err(ToolError::Daemon(DaemonError::Failed(format!(
                    "the cancel request for job {} was sent but no reply arrived ({d}); it was not resent. Check with get",
                    id.0
                ))));
            }
            other => other?,
        };
        let Response::Cancelled { result, .. } = resp else {
            return Err(unexpected("cancel", &resp));
        };
        let mut text = match result.as_str() {
            "cancelled" => format!("job {} cancelled before it started", id.0),
            "requested" => format!(
                "job {} is running; cancel requested, its process group is being stopped",
                id.0
            ),
            "already_finished" => format!("job {} had already finished", id.0),
            other => format!("job {}: {other}", id.0),
        };
        if let Some(r) = &a.reason {
            text.push_str(&format!(" (reason: {r})"));
        }
        Ok(ToolResult {
            text,
            structured: json!({"id": id.0, "result": result, "reason": a.reason}),
            is_error: false,
        })
    }

    fn prioritize(&self, a: PrioritizeArgs) -> ToolOk {
        let id = job_id(a.id)?;
        let owner = self.owner(Some(&a.who))?;
        let (priority, reason) = class(Some(&a.priority), a.reason.as_deref())?;
        let resp = match self.daemon.call(&Request::Prioritize {
            id,
            who: owner.as_str().to_string(),
            priority,
            reason: reason.clone(),
        }) {
            Err(DaemonError::ReplyLost(d)) => {
                return Err(ToolError::Daemon(DaemonError::Failed(format!(
                    "the prioritize request for job {} was sent but no reply arrived ({d}); it was not resent. Check with get",
                    id.0
                ))));
            }
            other => other?,
        };
        let Response::Prioritized { from, to, .. } = resp else {
            return Err(unexpected("prioritize", &resp));
        };
        let mut text = format!("job {}: {from} -> {to}", id.0);
        if let Some(r) = &reason {
            text.push_str(&format!(" (reason: {r})"));
        }
        Ok(ToolResult {
            text,
            structured: json!({"id": id.0, "from": from.as_str(), "to": to.as_str(), "reason": reason}),
            is_error: false,
        })
    }

    fn status(&self) -> ToolOk {
        let resp = self.call_daemon(&Request::Status)?;
        let Response::Status(s) = resp else {
            return Err(unexpected("status", &resp));
        };
        let s: StatusSnapshot = *s;
        let text = status_text(&s);
        let mut structured = serde_json::to_value(&s).unwrap_or(Value::Null);
        if let Some(o) = structured.as_object_mut() {
            o.remove("settings");
        }
        Ok(ToolResult {
            text,
            structured,
            is_error: false,
        })
    }

    fn audit(&self, a: AuditArgs) -> ToolOk {
        let limit = a.limit.unwrap_or(DEFAULT_AUDIT_ROWS).clamp(1, 200);
        let since_ms = match a.since.as_deref() {
            Some(t) => Some(
                tasks_daemon::logs::parse_time(t, self.now())
                    .map_err(|e| ToolError::Args(format!("since: {e}")))?,
            ),
            None => None,
        };
        let resp = self.call_daemon(&Request::Audit {
            since_ms,
            until_ms: None,
            peer: a.peer,
            job: a.job,
            action: a.action,
            limit: Some(limit),
        })?;
        let Response::Audit { rows, truncated } = resp else {
            return Err(unexpected("audit", &resp));
        };
        let body: String = rows
            .iter()
            .map(|r| {
                let who = r.actor_name.as_deref().unwrap_or(r.actor.as_str());
                let job = r.job.map_or_else(String::new, |j| format!(" job {j}"));
                let peer = r
                    .peer_name
                    .as_deref()
                    .or(r.peer.as_deref())
                    .map_or_else(String::new, |p| format!(" peer {p}"));
                let owner = if r.owner.is_empty() {
                    String::new()
                } else {
                    format!(" owner {}", r.owner)
                };
                let details = if r.details.as_object().is_some_and(|o| !o.is_empty()) {
                    format!(" {}", r.details)
                } else {
                    String::new()
                };
                format!(
                    "#{} t={} {} by {who}{owner}{job}{peer}{details}\n",
                    r.seq,
                    r.at_ms / 1000,
                    r.action
                )
            })
            .collect();
        let (body, cut) = bound_head(&body, MAX_TEXT_BYTES);
        let mut text = if rows.is_empty() {
            "no audit rows match".to_string()
        } else {
            format!(
                "{} audit row(s), newest first\n{}",
                rows.len(),
                body.trim_end()
            )
        };
        if cut || truncated {
            text.push_str("\ntruncated: narrow with since, peer, job or action");
        }
        Ok(ToolResult {
            text,
            structured: json!({
                "rows": serde_json::to_value(&rows).unwrap_or(Value::Null),
                "truncated": cut || truncated,
            }),
            is_error: false,
        })
    }

    fn estimate(&self, a: EstimateArgs) -> ToolOk {
        if a.argv.is_empty() {
            return Err(ToolError::Args("argv must name a program".into()));
        }
        let resp = self.call_daemon(&Request::Estimate {
            argv: a.argv,
            kind: None,
        })?;
        let Response::Estimate(e) = resp else {
            return Err(unexpected("estimate", &resp));
        };
        Ok(ToolResult {
            text: format!(
                "{} (median {}, p90 {}, {} sample(s))",
                e.text,
                dur(e.median_secs),
                dur(e.p90_secs),
                e.samples
            ),
            structured: serde_json::to_value(&e).unwrap_or(Value::Null),
            is_error: false,
        })
    }
}

fn plan_result(resp: Response, title: &str, what: &str, notes: Vec<String>) -> ToolOk {
    let Response::PlanSubmitted { plan_id, jobs } = resp else {
        return Err(unexpected(what, &resp));
    };
    let mut text = format!(
        "submitted {title} as plan {plan_id} with {} step(s)",
        jobs.len()
    );
    for (name, id) in &jobs {
        text.push_str(&format!("\n  {name} -> job {}", id.0));
    }
    for n in &notes {
        text.push_str(&format!("\nnote: {n}"));
    }
    Ok(ToolResult {
        text,
        structured: json!({
            "plan_id": plan_id,
            "jobs": jobs.iter().map(|(n, i)| json!({"step": n, "id": i.0})).collect::<Vec<_>>(),
            "notes": notes,
        }),
        is_error: false,
    })
}

/// Unfinished jobs of other owners in the same cwd (R-187): a warning, never a block.
fn shared_cwd_warning(jobs: &[JobView], owner: &str, cwd: &std::path::Path) -> Option<String> {
    let others: Vec<String> = jobs
        .iter()
        .filter(|j| !j.phase.is_terminal() && j.cwd == cwd && j.owner != owner)
        .map(|j| format!("{} ({}, {})", j.id.0, j.owner, phase_tag(&j.phase)))
        .collect();
    if others.is_empty() {
        return None;
    }
    Some(format!(
        "other owners have unfinished jobs in this same cwd {}: {}. They build and edit the same checkout; coordinate before relying on results (not blocking)",
        cwd.display(),
        others.join(", ")
    ))
}

fn status_text(s: &StatusSnapshot) -> String {
    let mut t = format!(
        "{} (tasks {}, pid {}){}{}\n",
        s.daemon.machine,
        s.daemon.version,
        s.daemon.pid,
        if s.daemon.draining { ", DRAINING" } else { "" },
        if s.daemon.execution_enabled {
            ""
        } else {
            ", EXECUTION STOPPED"
        },
    );
    for g in &s.slots {
        t.push_str(&format!("slots {}: {}/{} free", g.kind, g.free, g.total));
        let held: Vec<String> = g
            .busy
            .iter()
            .map(|b| match (&b.job, &b.holder) {
                (Some(j), _) => format!("job {}", j.0),
                (None, Some(h)) => h.clone(),
                (None, None) => "external".into(),
            })
            .collect();
        if !held.is_empty() {
            t.push_str(&format!(" (held by {})", held.join(", ")));
        }
        t.push('\n');
    }
    for r in &s.running {
        t.push_str(&format!(
            "running #{} {} {} for {}{}\n",
            r.id.0,
            r.owner,
            r.command,
            dur(r.elapsed_secs),
            r.eta_secs
                .map_or(String::new(), |e| format!(", about {} left", dur(e)))
        ));
    }
    if s.waiting.is_empty() {
        t.push_str("waiting: none\n");
    } else {
        t.push_str(&format!(
            "waiting: {} job(s)\n",
            s.waiting.len() + s.waiting_more
        ));
        for w in s.waiting.iter().take(10) {
            let class = if w.class.is_empty() || w.class == "normal" {
                String::new()
            } else {
                format!(" [{}]", w.class)
            };
            let place = w
                .on_peer
                .as_ref()
                .map_or(String::new(), |p| format!(" on {p}"));
            t.push_str(&format!(
                "  #{} {}{class}{place} {}: {}\n",
                w.id.0, w.owner, w.command, w.wait_text
            ));
            if !w.class_note.is_empty() {
                t.push_str(&format!("      {}\n", w.class_note));
            }
        }
    }
    let p = &s.pressure;
    let pct = |v: Option<f32>| v.map_or("?".to_string(), |v| format!("{v:.0}%"));
    t.push_str(&format!(
        "pressure: memory {} (some {}, full {}), cpu {}, io {}\n",
        p.memory,
        pct(p.memory_some),
        pct(p.memory_full),
        pct(p.cpu),
        pct(p.io)
    ));
    let gib = |v: Option<u64>| {
        v.map_or("?".to_string(), |b| {
            format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64)
        })
    };
    t.push_str(&format!(
        "memory available {}, cache disk free {}, state disk free {}\n",
        gib(p.available_memory),
        gib(p.cache_free),
        gib(p.state_free)
    ));
    if let Some(c) = p.cpu_busy_pct {
        t.push_str(&format!("cpu busy {c:.0}%\n"));
    }
    match p.swap_total {
        Some(0) => t.push_str("no swap\n"),
        Some(total) => t.push_str(&format!(
            "swap {} of {} used (in {}/s, out {}/s)\n",
            gib(p.swap_used),
            gib(Some(total)),
            gib(p.swap_in_per_s),
            gib(p.swap_out_per_s)
        )),
        None => {}
    }
    match &p.compile_blocker {
        Some(b) => t.push_str(&format!("a new compile job would wait: {b}\n")),
        None => t.push_str("nothing blocks a new compile job\n"),
    }
    if let Some(w) = &p.warning {
        t.push_str(&format!("warning: {w}\n"));
    }
    for a in &s.alerts {
        t.push_str(&format!("alert [{}] {}\n", a.level, a.message));
    }
    if let Some(pw) = &s.power {
        t.push_str(&format!("{}\n{}\n", pw.source_line(), pw.awake_line()));
    }
    if !s.attribution.is_empty() {
        t.push_str(&format!("{}\n", s.attribution));
    }
    for r in s.running.iter().filter(|r| r.figures.is_some()) {
        if let Some(f) = &r.figures {
            t.push_str(&format!("cost #{}: {}\n", r.id.0, f.summary()));
        }
    }
    for trip in s.trips.iter().take(3) {
        t.push_str(&format!("pressure trip: {}\n", trip.summary));
    }
    for p in &s.peers {
        let ms = |v: Option<u64>| v.map_or("-".to_string(), |r| format!("{r} ms"));
        t.push_str(&format!(
            "peer {}: {} ({}; path RTT {}, reply time {}){}\n",
            p.name,
            p.state,
            if p.path.is_empty() {
                "unknown"
            } else {
                &p.path
            },
            ms(p.rtt_ms),
            ms(p.reply_ms),
            if p.detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", p.detail)
            }
        ));
        if let Some(m) = &p.machine {
            let slots = m
                .slots
                .iter()
                .map(|x| format!("{} {}/{}", x.kind, x.used, x.total))
                .collect::<Vec<_>>()
                .join(", ");
            let swap = match m.swap_total {
                Some(0) => ", no swap".to_string(),
                Some(total) => format!(", swap {} of {} used", gib(m.swap_used), gib(Some(total))),
                None => String::new(),
            };
            let busy = m
                .cpu_busy_pct
                .map_or(String::new(), |c| format!(", cpu busy {c}%"));
            let power = m.power.as_ref().map_or(String::new(), |p| format!(", {p}"));
            t.push_str(&format!(
                "  {} cores, slots {slots}, {} waiting, {} running{busy}{swap}{power}\n",
                m.cores, m.waiting, m.running
            ));
        }
    }
    t.trim_end().to_string()
}
