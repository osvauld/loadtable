//! The daemon's local socket API: versioned request and response types.
//!
//! One JSON object per line. A request line is `{"v":1,"request":{"op":"status"}}`; the reply
//! line is `{"v":1,"reply":{"status":"ok","body":{"kind":"ack"}}}` or
//! `{"v":1,"reply":{"status":"err","body":{"code":"not_owner","message":"..."}}}`.
//!
//! This module has no daemon internals: the CLI and the MCP front end share it. Every message
//! has a size limit ([`MAX_REQUEST_BYTES`], [`MAX_RESPONSE_BYTES`]); an unknown version is a
//! named refusal ([`ErrorCode::UnsupportedVersion`]), never a guess.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tasks_core::plan::Plan;
use tasks_core::{JobId, JobSpec, Kind, Phase, Placement};

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;
/// Largest accepted request line, in bytes.
pub const MAX_REQUEST_BYTES: usize = 1 << 20;
/// Largest reply line the daemon sends, in bytes (R-027: hot replies stay small).
pub const MAX_RESPONSE_BYTES: usize = 4 << 20;
/// Most rows one `list` returns.
pub const MAX_LIST_ROWS: usize = 1000;
/// Rows `list` returns when the request sets no limit.
pub const DEFAULT_LIST_ROWS: usize = 200;
/// Largest log chunk one `follow` returns, in bytes.
pub const MAX_LOG_CHUNK: usize = 256 * 1024;
/// Most lines of context around a log match.
pub const MAX_CONTEXT: usize = 100;
/// Most lines one tail returns.
pub const MAX_TAIL_LINES: usize = 5000;
/// Most waiter rows in a status snapshot.
pub const MAX_STATUS_WAITERS: usize = 100;

/// A request to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Who is this daemon: version, pid, directories.
    Hello,
    /// Submit one job.
    Submit {
        /// The job.
        spec: JobSpec,
    },
    /// Submit a plan of dependent steps.
    SubmitPlan {
        /// The plan.
        plan: Plan,
        /// Submitting owner.
        owner: String,
        /// Working directory of every step.
        cwd: PathBuf,
        /// Extra environment of every step.
        #[serde(default)]
        env: Vec<(String, String)>,
        /// Placement of every step.
        #[serde(default = "default_placement")]
        placement: Placement,
    },
    /// Expand a suite of the repo's run policy and submit it as a plan.
    SubmitSuite {
        /// Suite name.
        name: String,
        /// Submitting owner.
        owner: String,
        /// A directory inside the repo.
        cwd: PathBuf,
        /// Extra environment of every step.
        #[serde(default)]
        env: Vec<(String, String)>,
        /// Placement of every step.
        #[serde(default = "default_placement")]
        placement: Placement,
        /// Drop steps marked `skip_in_quick`.
        #[serde(default)]
        quick: bool,
    },
    /// List jobs, oldest first.
    List {
        /// Only this owner.
        #[serde(default)]
        owner: Option<String>,
        /// Only unfinished jobs.
        #[serde(default)]
        active_only: bool,
        /// All unfinished jobs plus only the newest n finished ones.
        #[serde(default)]
        recent_finished: Option<usize>,
        /// Row limit (default [`DEFAULT_LIST_ROWS`], at most [`MAX_LIST_ROWS`]).
        #[serde(default)]
        limit: Option<usize>,
    },
    /// One job.
    Get {
        /// The job.
        id: JobId,
    },
    /// Read a job's log.
    Logs {
        /// The job.
        id: JobId,
        /// What to read.
        view: LogView,
    },
    /// Search the retained logs of many jobs, newest first.
    LogSearch {
        /// The query.
        query: LogSearchQuery,
    },
    /// Cancel a job. Refused unless `who` is the job's owner (R-108).
    Cancel {
        /// The job.
        id: JobId,
        /// The asking owner.
        who: String,
    },
    /// A snapshot of slots, waiters, running jobs, pressure and alerts.
    Status,
    /// Estimate a command's duration on this machine.
    Estimate {
        /// The command.
        argv: Vec<String>,
        /// Kind, when the recipe does not imply it.
        #[serde(default)]
        kind: Option<Kind>,
    },
    /// Stop claiming new jobs until the next daemon start; `cancel` resumes (R-018).
    Drain {
        /// True undoes a drain.
        #[serde(default)]
        cancel: bool,
    },
    /// Persist the execution intent: enabled or stopped (R-017).
    Execution {
        /// True lets queued jobs start.
        enabled: bool,
    },
    /// Stop the daemon. Refused while jobs run.
    Shutdown,
}

fn default_placement() -> Placement {
    Placement::Here
}

/// What to read from a log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum LogView {
    /// The last lines.
    Tail {
        /// How many (at most [`MAX_TAIL_LINES`]).
        lines: usize,
    },
    /// Lines matching a pattern, with context (R-112).
    Grep {
        /// The pattern.
        pattern: String,
        /// Lines of context before and after.
        #[serde(default)]
        context: usize,
        /// True reads the pattern as the documented regex subset; false as a substring.
        #[serde(default)]
        regex: bool,
        /// Most matches returned.
        #[serde(default)]
        max_matches: Option<usize>,
    },
    /// Bytes from an offset on, for following a running job.
    Follow {
        /// Byte offset to read from.
        offset: u64,
        /// Most bytes returned (at most [`MAX_LOG_CHUNK`]).
        #[serde(default)]
        max_bytes: Option<usize>,
    },
}

/// A cross-job log search (R-114).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogSearchQuery {
    /// The pattern.
    pub pattern: String,
    /// Regex subset instead of substring.
    pub regex: bool,
    /// Only this owner.
    pub owner: Option<String>,
    /// Only these jobs.
    pub ids: Vec<JobId>,
    /// Only jobs in this phase tag (`passed`, `failed`, `running`, ...).
    pub phase: Option<String>,
    /// Only jobs submitted at or after this time: unix seconds or a relative duration (`2h`).
    pub since: Option<String>,
    /// Only jobs submitted at or before this time.
    pub until: Option<String>,
    /// Lines of context.
    pub context: usize,
    /// Most matching lines in total.
    pub max_matches: Option<usize>,
    /// Most reply bytes of matched text.
    pub max_bytes: Option<usize>,
}

/// A reply envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplyEnvelope {
    /// Protocol version.
    pub v: u32,
    /// The reply.
    pub reply: Reply,
}

/// A request envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// Protocol version.
    pub v: u32,
    /// The request.
    pub request: Request,
}

/// The daemon's answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "body", rename_all = "snake_case")]
pub enum Reply {
    /// Success.
    Ok(Response),
    /// A named refusal or failure.
    Err(ApiError),
}

/// Machine-readable error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request's `v` is not supported (see `supported`).
    UnsupportedVersion,
    /// The request line is not valid or a field is out of range.
    BadRequest,
    /// The request line exceeds [`MAX_REQUEST_BYTES`].
    TooLarge,
    /// The reply would exceed [`MAX_RESPONSE_BYTES`].
    ResponseTooLarge,
    /// No such job or log.
    NotFound,
    /// The caller is not the job's owner (R-108).
    NotOwner,
    /// The daemon refuses because jobs run, or it has too many connections.
    Busy,
    /// The job or plan failed validation.
    Refused,
    /// The ledger stayed busy; this is not a cancellation (R-003).
    LedgerUnavailable,
    /// Anything else.
    Internal,
}

/// A refusal or failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    /// The code.
    pub code: ErrorCode,
    /// A sentence for people.
    pub message: String,
    /// For [`ErrorCode::UnsupportedVersion`], the version this daemon speaks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported: Option<u32>,
}

impl ApiError {
    /// An error with a code and message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ApiError {
        ApiError {
            code,
            message: message.into(),
            supported: None,
        }
    }

    /// The named refusal of an unknown protocol version.
    pub fn unsupported_version(got: Option<u64>) -> ApiError {
        let got = got.map_or_else(|| "missing".to_string(), |g| g.to_string());
        ApiError {
            code: ErrorCode::UnsupportedVersion,
            message: format!(
                "protocol version {got} is not supported; this daemon speaks version {PROTOCOL_VERSION}"
            ),
            supported: Some(PROTOCOL_VERSION),
        }
    }
}

/// Parses one request line. The version is checked before the body, so a newer client gets the
/// named refusal rather than a parse error.
pub fn parse_request(line: &[u8]) -> Result<Request, ApiError> {
    if line.len() > MAX_REQUEST_BYTES {
        return Err(ApiError::new(
            ErrorCode::TooLarge,
            format!(
                "request is {} bytes; the limit is {MAX_REQUEST_BYTES}",
                line.len()
            ),
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(line)
        .map_err(|e| ApiError::new(ErrorCode::BadRequest, format!("not JSON: {e}")))?;
    let v = value.get("v").and_then(serde_json::Value::as_u64);
    if v != Some(u64::from(PROTOCOL_VERSION)) {
        return Err(ApiError::unsupported_version(v));
    }
    let env: RequestEnvelope = serde_json::from_value(value)
        .map_err(|e| ApiError::new(ErrorCode::BadRequest, format!("bad request: {e}")))?;
    Ok(env.request)
}

/// Renders a reply line (without the newline), replacing an oversize reply by a named error.
pub fn render_reply(reply: Reply) -> String {
    let env = ReplyEnvelope {
        v: PROTOCOL_VERSION,
        reply,
    };
    let text = serde_json::to_string(&env).unwrap_or_default();
    if text.len() <= MAX_RESPONSE_BYTES && !text.is_empty() {
        return text;
    }
    let err = ReplyEnvelope {
        v: PROTOCOL_VERSION,
        reply: Reply::Err(ApiError::new(
            ErrorCode::ResponseTooLarge,
            format!("reply exceeds {MAX_RESPONSE_BYTES} bytes; ask for less"),
        )),
    };
    serde_json::to_string(&err).unwrap_or_else(|_| String::from("{}"))
}

/// A successful reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Nothing to report.
    Ack,
    /// Answer to `hello`.
    Hello(HelloInfo),
    /// A job was submitted.
    Submitted {
        /// Its id.
        id: JobId,
    },
    /// A plan was submitted.
    PlanSubmitted {
        /// The plan id.
        plan_id: i64,
        /// Step name and job id, in declaration order.
        jobs: Vec<(String, JobId)>,
    },
    /// Jobs.
    Jobs {
        /// The rows.
        jobs: Vec<JobView>,
        /// True when the limit cut the list.
        truncated: bool,
    },
    /// One job.
    Job {
        /// The job.
        job: Box<JobView>,
    },
    /// The last lines of a log.
    LogTail {
        /// Lines, oldest first.
        lines: Vec<String>,
        /// True while the job still runs (the last line may grow).
        running: bool,
    },
    /// Grep result.
    LogGrep {
        /// Groups of lines, separated by gaps.
        groups: Vec<GrepGroup>,
        /// Matching lines found.
        matches: usize,
        /// True when `max_matches` cut the search.
        truncated: bool,
    },
    /// A chunk of a log.
    LogChunk {
        /// Text (invalid UTF-8 replaced).
        data: String,
        /// Offset to pass next.
        next_offset: u64,
        /// True when the job is over and nothing more will come.
        done: bool,
    },
    /// Cross-job search result, newest job first.
    LogSearch {
        /// One entry per job with matches.
        results: Vec<JobGrep>,
        /// Matching lines found.
        matches: usize,
        /// True when a bound cut the search.
        truncated: bool,
    },
    /// A cancel request was accepted.
    Cancelled {
        /// The job.
        id: JobId,
        /// `cancelled` (was not started), `requested` (running, being stopped) or
        /// `already_finished`.
        result: String,
    },
    /// Status snapshot.
    Status(Box<StatusSnapshot>),
    /// A duration estimate.
    Estimate(EstimateView),
}

/// Who the daemon is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloInfo {
    /// Protocol version.
    pub protocol: u32,
    /// Daemon crate version.
    pub version: String,
    /// Daemon pid.
    pub pid: u32,
    /// The state directory.
    pub state_dir: PathBuf,
    /// The slot-lock directory every client must use (R-231).
    pub lock_dir: PathBuf,
    /// Machine name.
    pub machine: String,
}

/// A job as the API shows it. The environment is deliberately left out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobView {
    /// Job id.
    pub id: JobId,
    /// Owner.
    pub owner: String,
    /// Command.
    pub argv: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Kind.
    pub kind: Kind,
    /// Placement asked for.
    pub placement: Placement,
    /// Dependencies.
    pub after: Vec<JobId>,
    /// Phase.
    pub phase: Phase,
    /// Full wait text while waiting.
    pub wait_text: Option<String>,
    /// Exit code every surface shows (R-009).
    pub exit: Option<i32>,
    /// One-line outcome.
    pub summary: Option<String>,
    /// Submission time, ms since epoch.
    pub submitted_at_ms: i64,
    /// Start time.
    pub started_at_ms: Option<i64>,
    /// Finish time.
    pub finished_at_ms: Option<i64>,
    /// Machine that ran or runs it.
    pub machine: Option<String>,
    /// Peer it was forwarded to and the remote id.
    pub forwarded_to: Option<(String, Option<i64>)>,
    /// Who asked for a cancel.
    pub cancel_requested_by: Option<String>,
    /// Plan and step name for plan steps.
    pub plan: Option<(i64, String)>,
    /// Log file path.
    pub log_path: Option<PathBuf>,
}

/// One block of grep output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrepGroup {
    /// Consecutive lines; groups are separated by a gap in the log.
    pub lines: Vec<GrepLine>,
}

/// One line of grep output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrepLine {
    /// 1-based line number in the retained log.
    pub n: u64,
    /// The text.
    pub text: String,
    /// True for a match, false for context.
    pub matched: bool,
}

/// Matches in one job's log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobGrep {
    /// The job.
    pub id: JobId,
    /// Its owner.
    pub owner: String,
    /// Phase tag.
    pub phase: String,
    /// The matches.
    pub groups: Vec<GrepGroup>,
}

/// Where a policy value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The documented default.
    Default,
    /// The machine policy file.
    File,
    /// An environment variable of the daemon.
    Env,
}

/// One machine-policy value and its source (R-219).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingView {
    /// Dotted key, e.g. `slots.compile`.
    pub key: String,
    /// The effective value, as text.
    pub value: String,
    /// Where it came from.
    pub source: Source,
}

/// The daemon's own facts in a status snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonView {
    /// Pid.
    pub pid: u32,
    /// Version.
    pub version: String,
    /// Machine name.
    pub machine: String,
    /// State directory.
    pub state_dir: PathBuf,
    /// Slot-lock directory (R-045).
    pub lock_dir: PathBuf,
    /// Machine policy file path and whether it was read.
    pub policy_file: PathBuf,
    /// True when a policy file was found and valid.
    pub policy_file_loaded: bool,
    /// Problems found in the policy file or environment, each naming its field (R-218).
    pub policy_problems: Vec<String>,
    /// True while draining (R-018).
    pub draining: bool,
    /// Execution intent (R-017).
    pub execution_enabled: bool,
    /// Seconds since start.
    pub uptime_secs: u64,
}

/// One slot kind's state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotGroupView {
    /// `compile`, `run`, `offload` or `device`.
    pub kind: String,
    /// Slots of this kind.
    pub total: u32,
    /// Free slots.
    pub free: u32,
    /// Busy slots.
    pub busy: Vec<SlotView>,
}

/// One busy slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotView {
    /// Slot index (1000 and up is the short lane).
    pub index: u32,
    /// The ledger job holding it.
    pub job: Option<JobId>,
    /// True when the holder is not a ledger job (R-050).
    pub external: bool,
    /// Holder description for external work (pid, command, origin job).
    pub holder: Option<String>,
}

/// A waiting job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaiterView {
    /// Job id.
    pub id: JobId,
    /// Owner.
    pub owner: String,
    /// Command, shortened.
    pub command: String,
    /// The full wait text.
    pub wait_text: String,
    /// 1-based position among waiters of its slot kind.
    pub position: usize,
    /// Estimated seconds until it starts (R-129).
    pub start_estimate_secs: Option<f64>,
    /// True when the estimate rests on no history.
    pub rough: bool,
}

/// A running job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunningView {
    /// Job id.
    pub id: JobId,
    /// Owner.
    pub owner: String,
    /// Command, shortened.
    pub command: String,
    /// Slot kind and index.
    pub slot: Option<(String, u32)>,
    /// Seconds since start.
    pub elapsed_secs: f64,
    /// Targets finished.
    pub done_targets: usize,
    /// Targets expected.
    pub total_targets: usize,
    /// Running target.
    pub current: Option<String>,
    /// Estimated seconds left.
    pub eta_secs: Option<f64>,
    /// Log size in bytes.
    pub log_bytes: u64,
}

/// Pressure and disk readings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PressureView {
    /// Memory pressure level.
    pub memory: String,
    /// Memory PSI some avg10.
    pub memory_some: Option<f32>,
    /// Memory PSI full avg10.
    pub memory_full: Option<f32>,
    /// CPU pressure.
    pub cpu: Option<f32>,
    /// I/O pressure.
    pub io: Option<f32>,
    /// Available memory in bytes.
    pub available_memory: Option<u64>,
    /// Free bytes where cache output lands.
    pub cache_free: Option<u64>,
    /// Free bytes where state lives.
    pub state_free: Option<u64>,
    /// What blocks a new compile job right now, if anything (R-057, R-067).
    pub compile_blocker: Option<String>,
    /// Reading error, a warning rather than a failure (R-067).
    pub warning: Option<String>,
}

/// An alert (R-221).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertView {
    /// Stable id.
    pub id: String,
    /// `info`, `warn` or `critical`.
    pub level: String,
    /// What is wrong.
    pub message: String,
    /// When it first appeared, ms since epoch.
    pub since_ms: i64,
}

/// The status snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    /// The daemon.
    pub daemon: DaemonView,
    /// Slots by kind.
    pub slots: Vec<SlotGroupView>,
    /// Waiting jobs, in queue order.
    pub waiting: Vec<WaiterView>,
    /// Waiting jobs not listed because of the row limit.
    pub waiting_more: usize,
    /// Running jobs.
    pub running: Vec<RunningView>,
    /// Pressure.
    pub pressure: PressureView,
    /// Alerts.
    pub alerts: Vec<AlertView>,
    /// Every policy value with its source.
    pub settings: Vec<SettingView>,
}

/// A duration estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EstimateView {
    /// The recipe the history is keyed by.
    pub recipe: String,
    /// Median seconds.
    pub median_secs: f64,
    /// 90th percentile seconds.
    pub p90_secs: f64,
    /// Samples behind it.
    pub samples: usize,
    /// One line with its provenance.
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_version_is_a_named_refusal() {
        let e = parse_request(br#"{"v":2,"request":{"op":"status"}}"#).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedVersion);
        assert_eq!(e.supported, Some(PROTOCOL_VERSION));
        let e = parse_request(br#"{"request":{"op":"status"}}"#).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedVersion);
        // a future request body is not parsed once the version is wrong
        let e = parse_request(br#"{"v":9,"request":{"op":"from_the_future"}}"#).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedVersion);
    }

    #[test]
    fn requests_round_trip() {
        let r = Request::Cancel {
            id: JobId(3),
            who: "me".into(),
        };
        let line = serde_json::to_vec(&RequestEnvelope {
            v: PROTOCOL_VERSION,
            request: r.clone(),
        })
        .unwrap();
        assert_eq!(parse_request(&line).unwrap(), r);
    }

    #[test]
    fn oversize_and_garbage_requests_are_refused() {
        let big = vec![b' '; MAX_REQUEST_BYTES + 1];
        assert_eq!(parse_request(&big).unwrap_err().code, ErrorCode::TooLarge);
        assert_eq!(
            parse_request(b"not json").unwrap_err().code,
            ErrorCode::BadRequest
        );
        assert_eq!(
            parse_request(br#"{"v":1,"request":{"op":"nope"}}"#)
                .unwrap_err()
                .code,
            ErrorCode::BadRequest
        );
    }

    #[test]
    fn oversize_reply_becomes_a_named_error() {
        let huge = Response::LogChunk {
            data: "x".repeat(MAX_RESPONSE_BYTES + 10),
            next_offset: 0,
            done: false,
        };
        let line = render_reply(Reply::Ok(huge));
        let env: ReplyEnvelope = serde_json::from_str(&line).unwrap();
        assert!(matches!(
            env.reply,
            Reply::Err(ApiError {
                code: ErrorCode::ResponseTooLarge,
                ..
            })
        ));
    }
}
