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
use tasks_core::{JobFigures, JobId, JobSpec, Kind, Phase, Placement, Priority};

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
/// Most rows one `audit` request returns.
pub const MAX_AUDIT_ROWS: usize = 1000;
/// Rows `audit` returns when the request sets no limit.
pub const DEFAULT_AUDIT_ROWS: usize = 200;
/// Most recipes one `history` request returns.
pub const MAX_HISTORY_RECIPES: usize = 200;
/// Samples per series a `history` request returns when it sets no limit.
pub const DEFAULT_HISTORY_SAMPLES: usize = 30;
/// Most samples per series a `history` request may ask for.
pub const MAX_HISTORY_SAMPLES: usize = 200;
/// Most samples in one `history` reply, over all series; the reply says when it cut.
pub const MAX_HISTORY_TOTAL_SAMPLES: usize = 20_000;

/// The audit action of a pressure trip (`tasks_ledger::action::PRESSURE_TRIP`), for clients that
/// do not depend on the ledger.
pub const PRESSURE_TRIP_ACTION: &str = "pressure.trip";

/// A request to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Who is this daemon: version, pid, directories.
    Hello,
    /// This machine's identity: DID, node id, name.
    Identity,
    /// Mint a pairing ticket.
    PairInvite {
        /// Roles the redeemer receives (`submit`, `observe`, `admin`).
        roles: Vec<String>,
        /// Whether the redeemer grants the same roles back.
        #[serde(default)]
        both_ways: bool,
        /// Ticket lifetime in seconds (default 15 minutes).
        #[serde(default)]
        ttl_secs: Option<u64>,
    },
    /// Redeem a ticket made by another machine.
    PairAccept {
        /// The `tasks1.…` text.
        ticket: String,
    },
    /// Invalidate every unredeemed ticket.
    PairReset,
    /// Paired peers with DID, roles each way, expiry and link path.
    Peers,
    /// Revoke a paired peer's grants and close its live connections.
    PeerRevoke {
        /// The peer's name.
        name: String,
    },
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
        /// The submitter named a placement (R-730). False lets each step's own placement
        /// hint apply; an older client sends none, and then only its default `here` yields.
        #[serde(default)]
        placement_given: bool,
        /// The submitter passed the broad-test escape for every step (R-191).
        #[serde(default)]
        broad_test: bool,
        /// Queue class of every step; normal leaves each step's own hint.
        #[serde(default)]
        priority: Priority,
        /// Why `blocking`.
        #[serde(default)]
        priority_reason: Option<String>,
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
        /// The submitter named a placement (R-730). False lets each step's own placement
        /// hint apply; an older client sends none, and then only its default `here` yields.
        #[serde(default)]
        placement_given: bool,
        /// Drop steps marked `skip_in_quick`.
        #[serde(default)]
        quick: bool,
        /// The test targets of the suite's `split` steps by step name, read from `cargo
        /// metadata` by the submitter, which has the checkout and the toolchain. A split step
        /// without an entry stays whole (R-704).
        #[serde(default)]
        executables: tasks_policy::Executables,
        /// The submitter passed the broad-test escape for every step (R-191).
        #[serde(default)]
        broad_test: bool,
        /// Queue class of every step; normal leaves each step's own hint from the run policy.
        #[serde(default)]
        priority: Priority,
        /// Why `blocking`.
        #[serde(default)]
        priority_reason: Option<String>,
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
        /// Why, kept with the job and shown by `get` and `list`.
        #[serde(default)]
        reason: Option<String>,
    },
    /// Change the queue class of a job that has not started. Refused unless `who` is the job's
    /// owner; refused for a running or finished job, because priority orders the queue and
    /// never preempts. `blocking` needs a reason; the change is audited.
    Prioritize {
        /// The job.
        id: JobId,
        /// The asking owner.
        who: String,
        /// The new class.
        priority: Priority,
        /// Why (required for `blocking`).
        #[serde(default)]
        reason: Option<String>,
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
        /// Who asks; a drain records it with the time, and `status` shows both (R-189).
        #[serde(default)]
        who: Option<String>,
    },
    /// What each checkout keeps on the build caches here and on the peers; with `delete`,
    /// first frees the rows in those states that no job uses (R-839).
    Gc {
        /// The states to free; empty only reports.
        #[serde(default)]
        delete: Vec<GcState>,
        /// Checkouts to free as well, named as the report shows them (`~/…` or
        /// `(unknown checkout …)`); one a job uses is still kept.
        #[serde(default)]
        checkouts: Vec<String>,
        /// Days without a job after which a checkout is idle (default 14).
        #[serde(default)]
        idle_days: Option<u32>,
    },
    /// Live samples and process groups of one job on this machine.
    JobMetrics {
        /// The job.
        id: JobId,
    },
    /// Timing samples and known targets per recipe and machine, bounded in size.
    History {
        /// Only these recipes (the `RecipeKey` display strings); empty means every recipe the
        /// ledger has samples for, up to [`MAX_HISTORY_RECIPES`].
        #[serde(default)]
        recipes: Vec<String>,
        /// Only these machines; empty means every machine with samples.
        #[serde(default)]
        machines: Vec<String>,
        /// Newest samples kept per series (default [`DEFAULT_HISTORY_SAMPLES`], at most
        /// [`MAX_HISTORY_SAMPLES`]).
        #[serde(default)]
        max_samples: Option<usize>,
    },
    /// Read the audit log, newest first. Every filter narrows; none set returns the latest rows.
    Audit {
        /// At or after this time, ms since the epoch.
        #[serde(default)]
        since_ms: Option<i64>,
        /// At or before this time, ms since the epoch.
        #[serde(default)]
        until_ms: Option<i64>,
        /// Rows naming this peer, as its paired name or its DID.
        #[serde(default)]
        peer: Option<String>,
        /// Rows about this job.
        #[serde(default)]
        job: Option<i64>,
        /// This action, or a family (`job` matches every `job.*`).
        #[serde(default)]
        action: Option<String>,
        /// Row limit (default [`DEFAULT_AUDIT_ROWS`], at most [`MAX_AUDIT_ROWS`]).
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Walk the audit chain and report the first break, checking checkpoint signatures.
    AuditVerify,
    /// Sign a checkpoint of the audit chain now.
    AuditCheckpoint,
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
    /// The daemon refuses because jobs run (a shutdown). The request was understood and
    /// refused, not "not processed": never resend it blindly.
    Busy,
    /// The daemon has too many connections and answered before reading the request: the
    /// request was not processed, so any request may be sent again.
    ConnectionLimit,
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
    /// Answer to `identity`.
    Identity(crate::trust_control::IdentityView),
    /// A minted ticket.
    Invite(crate::trust_control::InviteView),
    /// A completed pairing.
    Paired(crate::trust_control::PairedView),
    /// Tickets reset.
    PairReset {
        /// The new ticket epoch.
        epoch: u64,
    },
    /// Paired peers.
    Peers {
        /// The peers, by name.
        peers: Vec<crate::trust_control::PeerTrustView>,
    },
    /// A peer was revoked.
    Revoked(crate::trust_control::RevokeView),
    /// A job was submitted.
    Submitted {
        /// Its id.
        id: JobId,
        /// What the submit rules allowed with a warning (a broad test in `warn` mode, one
        /// deliberately escaped); empty for an older daemon.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
    },
    /// A plan was submitted.
    PlanSubmitted {
        /// The plan id.
        plan_id: i64,
        /// Step name and job id, in declaration order.
        jobs: Vec<(String, JobId)>,
    },
    /// A job's queue class was changed.
    Prioritized {
        /// The job.
        id: JobId,
        /// The class it had.
        from: Priority,
        /// The class it has now.
        to: Priority,
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
        /// The log offset just after the last byte the lines cover: follow from here and no
        /// byte is read twice or skipped. `None` from an older daemon.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_offset: Option<u64>,
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
    /// Timing history.
    History(HistoryView),
    /// Live samples, process groups and the usual peak of one job.
    JobMetrics(tasks_core::metrics::JobMetricsView),
    /// Answer to `gc`.
    Gc(Box<GcView>),
    /// Audit rows, newest first.
    Audit {
        /// The rows.
        rows: Vec<AuditRowView>,
        /// True when the limit cut the list.
        truncated: bool,
    },
    /// The result of walking the audit chain.
    AuditVerify(AuditVerifyView),
    /// A checkpoint was signed.
    AuditCheckpoint {
        /// Its row, or `None` when the log is empty.
        seq: Option<i64>,
    },
}

/// One audit row as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRowView {
    /// Position in the chain.
    pub seq: i64,
    /// When, ms since the epoch.
    pub at_ms: i64,
    /// The machine that did it (DID).
    pub actor: String,
    /// Its paired name, when it is a paired peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_name: Option<String>,
    /// The agent or person; empty when none applies.
    pub owner: String,
    /// What happened, for example `job.cancelled`.
    pub action: String,
    /// The job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<i64>,
    /// The other machine (DID).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// Its paired name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_name: Option<String>,
    /// A checkout, path or node id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The details object.
    pub details: serde_json::Value,
    /// Hash of the previous row.
    pub prev_hash: String,
    /// Hash of this row.
    pub hash: String,
}

/// Where the audit chain broke.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditProblemView {
    /// The row it was found at.
    pub seq: i64,
    /// `edited`, `broken_link`, `gap`, `reordered`, `missing_history`, `bad_record`,
    /// `bad_signature` or `unexpected_signer`.
    pub kind: String,
    /// A sentence for people.
    pub message: String,
}

/// The result of `audit_verify`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditVerifyView {
    /// True when no break was found.
    pub ok: bool,
    /// Rows checked.
    pub rows: u64,
    /// Oldest and newest row seen.
    pub first_seq: Option<i64>,
    /// Newest row seen.
    pub last_seq: Option<i64>,
    /// Checkpoints whose signature verified.
    pub checkpoints: u64,
    /// The row of the newest verified checkpoint; rows after it are covered by the chain only.
    pub signed_through: Option<i64>,
    /// Rows before this one were removed by retention.
    pub pruned_before: Option<i64>,
    /// The first break.
    pub problem: Option<AuditProblemView>,
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
    /// Why the cancel was asked for, when the requester said.
    #[serde(default)]
    pub cancel_reason: Option<String>,
    /// Wall-clock limit in seconds, when the job has one.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Last known phase on the peer a forwarded job runs on; the local `phase` stays the
    /// source of truth (R-015, R-186).
    #[serde(default)]
    pub remote_phase: Option<Phase>,
    /// Plan and step name for plan steps.
    pub plan: Option<(i64, String)>,
    /// The suite a plan step came from (`core-split`), when the daemon knows it. The ledger
    /// does not store it yet, so this is empty until it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    /// Scheduling flags of a plan step, as words: `light`, `isolate`, `retry`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Log file path.
    pub log_path: Option<PathBuf>,
    /// What the job cost the machine and how exact that is (design §9).
    #[serde(default)]
    pub figures: Option<JobFigures>,
    /// The class the job was submitted in or last given (`prioritize`).
    #[serde(default)]
    pub priority: Priority,
    /// The reason a blocking class was given.
    #[serde(default)]
    pub priority_reason: Option<String>,
    /// Set for a forwarded job that the peer holds back on its own gates or slots: the peer's
    /// reason. Such a job is waiting, not running, though its local row says `Running` (R-015).
    #[serde(default)]
    pub remote_wait: Option<String>,
    /// Output files the job asked to have fetched back from its peer (`--fetch`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetch_paths: Vec<String>,
    /// What became of each, once the job has ended.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetch: Vec<tasks_core::FetchResult>,
    /// What the sync before a forwarded run sent and how long it took, once it has finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncView>,
    /// The checkout going to the peer, or an output coming back, while it moves (R-801). A job
    /// whose command is over on the peer and whose outputs are arriving shows that here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<tasks_core::TransferProgress>,
}

/// The sync of a checkout to a peer before a forwarded job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncView {
    /// Bytes of pack sent (0 when the peer already had the tree).
    pub bytes: u64,
    /// Files in the tree.
    pub files: u64,
    /// How long the sync took, milliseconds.
    pub millis: u64,
    /// True when the peer already held this tree.
    pub already_there: bool,
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
    /// Who drained, when the drain request said (R-189).
    #[serde(default)]
    pub drained_by: Option<String>,
    /// When the drain was requested, ms since epoch (R-189).
    #[serde(default)]
    pub drained_at_ms: Option<i64>,
    /// Execution intent (R-017).
    pub execution_enabled: bool,
    /// Seconds since start.
    pub uptime_secs: u64,
    /// The daemon's clock when it built this snapshot, ms since epoch; lets a client say how
    /// long ago something happened without trusting its own clock.
    #[serde(default)]
    pub now_ms: i64,
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
    /// The class the queue orders it by now (`blocking`, `normal` or `background`): its own,
    /// or the one aging lifted it to.
    #[serde(default)]
    pub class: String,
    /// Why it is in that class, when not simply normal: the reason for `blocking`, or how long
    /// it waited to rise out of `background`.
    #[serde(default)]
    pub class_note: String,
    /// The peer a forwarded job waits on (its gates or slots), when it is not the queue here.
    #[serde(default)]
    pub on_peer: Option<String>,
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
    /// Estimated seconds left, to the slowest recent run when the estimate is a range; only
    /// when passing runs back it (R-660..R-665, R-837).
    pub eta_secs: Option<f64>,
    /// Seconds left to the fastest recent run, when that differs from `eta_secs`: the time
    /// left is `eta_low_secs` to `eta_secs` (R-837).
    #[serde(default)]
    pub eta_low_secs: Option<f64>,
    /// The fastest and slowest recent runs in seconds, when the run has gone past them all.
    #[serde(default)]
    pub recent_secs: Option<(f64, f64)>,
    /// Seconds past the slowest recent run: shown as "longer than recent runs", never as
    /// "0s left" (R-675).
    #[serde(default)]
    pub over_by_secs: Option<f64>,
    /// The machine whose runs the estimate rests on, when it is not this one (a rough ETA).
    #[serde(default)]
    pub eta_rough_from: Option<String>,
    /// Why there is no estimate: `first run`, `no passing run yet`, `first build here`.
    #[serde(default)]
    pub no_eta_why: Option<String>,
    /// Log size in bytes.
    pub log_bytes: u64,
    /// What it has cost the machine so far (sampled every few seconds).
    #[serde(default)]
    pub figures: Option<JobFigures>,
    /// CPU in cores over the last sample interval, when two samples exist.
    #[serde(default)]
    pub cores: Option<f64>,
    /// Memory in use now, bytes, when sampled.
    #[serde(default)]
    pub mem_bytes: Option<u64>,
    /// The forwarded job's checkout going out or its outputs coming back, while they move
    /// (R-801).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<tasks_core::TransferProgress>,
}

impl RunningView {
    /// `~3s–54s left`, `up to ~26s left` once past the fastest recent run, or `~54s left`;
    /// none without time left. `d` formats seconds the way the surface does (R-837).
    pub fn left_text(&self, d: impl Fn(f64) -> String) -> Option<String> {
        let hi = self.eta_secs?;
        Some(match self.eta_low_secs {
            Some(lo) if lo < 0.5 => format!("up to ~{} left", d(hi)),
            Some(lo) if d(lo) != d(hi) => format!("~{}–{} left", d(lo), d(hi)),
            _ => format!("~{} left", d(hi)),
        })
    }

    /// The fastest to the slowest recent run, `5s–56s`, once the run is past them all.
    pub fn recent_text(&self, d: impl Fn(f64) -> String) -> Option<String> {
        let (lo, hi) = self.recent_secs?;
        Some(if d(lo) == d(hi) {
            d(hi)
        } else {
            format!("{}–{}", d(lo), d(hi))
        })
    }

    /// `longer than recent runs (5s–56s) by 30s`: past every recent run, never `0s left`
    /// (R-675, R-837).
    pub fn over_text(&self, d: impl Fn(f64) -> String) -> Option<String> {
        let by = self.over_by_secs?;
        let runs = self.recent_text(&d)?;
        Some(format!("longer than recent runs ({runs}) by {}", d(by)))
    }
}
/// The words for a memory level, the same on every surface: `normal` is `ok`, `warn` is
/// `tight`, `critical` stays `critical`. The stall percentage is detail, not the headline.
pub fn memory_word(level: &str) -> &'static str {
    match level {
        "normal" => "ok",
        "warn" => "tight",
        "critical" => "critical",
        _ => "unknown",
    }
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
    /// The memory-stall percentage at which the level turns `warn` and a new compile waits
    /// (the machine policy's `memory.some_warn`); `None` where the machine has no PSI.
    #[serde(default)]
    pub memory_stall_limit: Option<f32>,
    /// CPU pressure.
    pub cpu: Option<f32>,
    /// I/O pressure.
    pub io: Option<f32>,
    /// Available memory in bytes.
    pub available_memory: Option<u64>,
    /// Total memory in bytes, when the machine reported it.
    #[serde(default)]
    pub memory_total: Option<u64>,
    /// CPU busy percent over the last interval (not CPU pressure), when two readings exist.
    #[serde(default)]
    pub cpu_busy_pct: Option<f32>,
    /// The whole machine's CPU in use, in cores (`cpu_busy_pct` of its logical cores). Minus
    /// the jobs' `cores` it is the work outside jobs.
    #[serde(default)]
    pub cores_in_use: Option<f64>,
    /// Logical cores of this machine, the scale `cores_in_use` and the jobs' `cores` are read on.
    #[serde(default)]
    pub cpu_cores: Option<u32>,
    /// Swap size in bytes (0 without swap), when the platform reports it.
    #[serde(default)]
    pub swap_total: Option<u64>,
    /// Swap in use in bytes.
    #[serde(default)]
    pub swap_used: Option<u64>,
    /// Bytes per second read back from swap over the last interval.
    #[serde(default)]
    pub swap_in_per_s: Option<u64>,
    /// Bytes per second written out to swap over the last interval.
    #[serde(default)]
    pub swap_out_per_s: Option<u64>,
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

/// A configured peer and the state of the link to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerView {
    /// The name placement and `--peer` use.
    pub name: String,
    /// The peer's transport identity.
    pub node_id: String,
    /// `unknown`, `up`, `stale` or `offline`.
    pub state: String,
    /// Seconds since it last answered, if ever.
    pub last_seen_ago_secs: Option<u64>,
    /// The last probe's error, or empty.
    pub detail: String,
    /// The peer's DID (empty when not paired through a ticket).
    #[serde(default)]
    pub did: String,
    /// Roles this machine holds there.
    #[serde(default)]
    pub roles_out: Vec<String>,
    /// Roles the peer holds here.
    #[serde(default)]
    pub roles_in: Vec<String>,
    /// The earliest grant expiry either way, unix seconds.
    #[serde(default)]
    pub grant_expires_at: Option<u64>,
    /// `direct`, `relay` or `unknown`.
    #[serde(default)]
    pub path: String,
    /// The transport's round-trip time of the selected path ("path RTT"), milliseconds.
    #[serde(default)]
    pub rtt_ms: Option<u64>,
    /// How long the last status request took to be answered ("reply time"), milliseconds. It
    /// includes the peer's handling, so it is not a measure of the network alone.
    #[serde(default)]
    pub reply_ms: Option<u64>,
    /// What the peer last said about itself, when it ever answered. It can be old: read it
    /// with `state` and `last_seen_ago_secs`.
    #[serde(default)]
    pub machine: Option<PeerMachineView>,
}

impl PeerView {
    /// When the figures in `machine` were taken (ms since the epoch), if they are not current:
    /// the peer is offline or stale and did answer once. `now_ms` is the daemon's clock.
    pub fn figures_as_of_ms(&self, now_ms: i64) -> Option<i64> {
        if self.machine.is_none() || !matches!(self.state.as_str(), "offline" | "stale") {
            return None;
        }
        let ago = i64::try_from(self.last_seen_ago_secs?).ok()?;
        Some(now_ms.saturating_sub(ago.saturating_mul(1000)))
    }
}

/// What a peer last reported about itself: facts, slots, queue depth and pressure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerMachineView {
    /// Logical cores.
    pub cores: u32,
    /// Total memory in bytes.
    pub memory_bytes: u64,
    /// Slot usage by kind (`compile`, `run`, `offload`, `device`).
    pub slots: Vec<PeerSlotView>,
    /// Jobs waiting on the peer.
    pub waiting: u32,
    /// Jobs running on the peer.
    pub running: u32,
    /// CPU pressure percent, when known.
    pub cpu_pct: Option<u8>,
    /// Memory pressure percent, when known.
    pub memory_pct: Option<u8>,
    /// Memory pressure level, when the peer has one (macOS has no percentage).
    #[serde(default)]
    pub memory_level: Option<String>,
    /// I/O pressure percent, when known.
    pub io_pct: Option<u8>,
    /// What blocks a new compile job there, or empty.
    pub pressure_summary: String,
    /// CPU busy percent (not pressure), when the peer reports it.
    #[serde(default)]
    pub cpu_busy_pct: Option<u8>,
    /// Swap size in bytes, when reported.
    #[serde(default)]
    pub swap_total: Option<u64>,
    /// Swap in use in bytes.
    #[serde(default)]
    pub swap_used: Option<u64>,
    /// Memory available to new work now, bytes, when the peer reports it.
    #[serde(default)]
    pub memory_available_bytes: Option<u64>,
    /// Swap read-back rate, bytes per second.
    #[serde(default)]
    pub swap_in_per_s: Option<u64>,
    /// Swap write-out rate, bytes per second.
    #[serde(default)]
    pub swap_out_per_s: Option<u64>,
    /// Where its power comes from (`on battery 80%`), when it reports it.
    #[serde(default)]
    pub power: Option<String>,
    /// Its battery charge, when it reports one.
    #[serde(default)]
    pub battery_pct: Option<u8>,
    /// It holds jobs from peers now: its battery is low (R-834, R-835). False from older peers.
    #[serde(default)]
    pub holds_peer_jobs: bool,
    /// The jobs running there and what each uses now; empty from an older peer. Jobs this
    /// machine forwarded are among them (their remote id is `forwarded_to`).
    #[serde(default)]
    pub jobs: Vec<PeerJobUseView>,
    /// The peer's whole CPU in use, thousandths of a core (minus the jobs' sum it is the work
    /// outside jobs); `None` from an older peer.
    #[serde(default)]
    pub cores_in_use_milli: Option<u32>,
    /// The peer daemon's version and build (`0.1.0 (41ac455 2026-10-27)`); `None` from an
    /// older peer (R-807).
    #[serde(default)]
    pub version: Option<String>,
}

/// One running job on a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerJobUseView {
    /// The job's id on the peer.
    pub id: u64,
    /// Its owner there.
    pub owner: String,
    /// CPU in thousandths of a core, when measured.
    #[serde(default)]
    pub cores_milli: Option<u32>,
    /// Memory in use now, bytes, when measured.
    #[serde(default)]
    pub mem_bytes: Option<u64>,
}

/// One peer slot kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSlotView {
    /// Slot kind name.
    pub kind: String,
    /// Slots of this kind.
    pub total: u32,
    /// Slots held.
    pub used: u32,
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
    /// Link state per configured peer (empty without peers).
    #[serde(default)]
    pub peers: Vec<PeerView>,
    /// One line: whether job costs are exact (cgroups) or summed over process trees, and why.
    #[serde(default)]
    pub attribution: String,
    /// Sync and fetch work (the transfer slot and cgroup).
    #[serde(default)]
    pub transfer: Option<TransferView>,
    /// The latest pressure trips, newest first.
    #[serde(default)]
    pub trips: Vec<TripView>,
    /// Power source and whether the daemon holds the machine awake; absent from older daemons.
    #[serde(default)]
    pub power: Option<PowerView>,
    /// This machine's relay connection and addresses, or that it has no peer transport; absent
    /// from older daemons.
    #[serde(default)]
    pub endpoint: Option<crate::trust_control::EndpointView>,
}

/// This machine's power source and its stay-awake hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerView {
    /// Running on a battery.
    pub on_battery: bool,
    /// Battery charge, when there is a battery.
    pub battery_pct: Option<u8>,
    /// Jobs that keep the machine awake now (running or queued, not waiting on battery).
    pub awake_jobs: u32,
    /// The hold is taken now.
    pub holding: bool,
    /// Seconds until a lingering hold (no jobs left) is released.
    pub linger_secs: Option<u64>,
    /// Why the machine cannot be held awake, when it cannot.
    pub guard_error: Option<String>,
    /// Jobs from peers held back because this machine is on battery.
    pub battery_held: u32,
    /// Whether peer jobs may run on battery (`power.peer_jobs_on_battery`).
    pub peer_jobs_on_battery: bool,
    /// Jobs from peers wait now: on battery and at or below the minimum charge, or not allowed
    /// on battery at all (R-834). Absent from older daemons.
    #[serde(default)]
    pub holds_peer_jobs: bool,
    /// The charge at or below which jobs from peers wait (`power.peer_jobs_min_battery_pct`).
    #[serde(default)]
    pub peer_jobs_min_battery_pct: u8,
}

impl PowerView {
    /// `keeping awake: 2 jobs`, `keeping awake: no jobs, releases in 1m20s` or `sleep allowed`.
    pub fn awake_line(&self) -> String {
        if let Some(e) = &self.guard_error {
            return format!("sleep guard not available: {e}");
        }
        match (self.holding, self.linger_secs) {
            (true, Some(s)) => format!("keeping awake: no jobs, releases in {}", fmt_dur(s)),
            (true, None) => format!(
                "keeping awake: {} job{}",
                self.awake_jobs,
                if self.awake_jobs == 1 { "" } else { "s" }
            ),
            (false, _) => "sleep allowed".to_string(),
        }
    }

    /// `power: on battery 80%`, with the peer jobs held back by it.
    pub fn source_line(&self) -> String {
        let mut t = format!(
            "power: {}",
            if self.on_battery {
                "on battery"
            } else {
                "on AC"
            }
        );
        if let Some(p) = self.battery_pct {
            t.push_str(&format!(" {p}%"));
        }
        let until = if self.peer_jobs_on_battery {
            format!("AC or a charge above {}%", self.peer_jobs_min_battery_pct)
        } else {
            "AC (power.peer_jobs_on_battery is off)".to_string()
        };
        if self.battery_held > 0 {
            t.push_str(&format!(
                "; {} job{} from peers wait for {until}",
                self.battery_held,
                if self.battery_held == 1 { "" } else { "s" }
            ));
        } else if self.holds_peer_jobs {
            // R-835: low battery is said even before a job waits on it
            t.push_str(&format!("; battery low: jobs from peers wait for {until}"));
        }
        t
    }
}

fn fmt_dur(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// Transfer (checkout sync) activity as the status shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferView {
    /// Phases holding a transfer slot now.
    pub active: u32,
    /// Why a phase waits for the slot or the I/O gate, when one does.
    pub waiting: Option<String>,
    /// Phases run since the daemon started.
    pub runs: u64,
    /// Storage bytes read by the daemon process itself while holding a transfer slot
    /// (approximate: includes anything else the daemon did meanwhile).
    pub inproc_read_bytes: u64,
    /// Storage bytes written likewise.
    pub inproc_write_bytes: u64,
    /// What the `git` children in the transfer cgroup cost (exact, cgroup only).
    pub figures: Option<JobFigures>,
}

/// One pressure trip as the status and the UI list it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TripView {
    /// Audit row.
    pub seq: i64,
    /// When, ms since epoch.
    pub at_ms: i64,
    /// `io`, `cpu` or `memory`.
    pub resource: String,
    /// `gate`, `psi` or `transfer`.
    pub source: String,
    /// One line: what tripped, and the biggest process outside any job.
    pub summary: String,
    /// Trips of the same resource dropped by the rate limit since the previous one.
    pub suppressed: u64,
}

/// Machine pressure at a trip.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TripMachine {
    /// I/O pressure, `some` avg10 percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_pct: Option<f32>,
    /// CPU pressure, avg10 percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f32>,
    /// Memory `some` avg10 percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_some_pct: Option<f32>,
    /// Memory `full` avg10 percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_full_pct: Option<f32>,
    /// Memory level: `normal`, `warn` or `critical`.
    #[serde(default)]
    pub mem_level: String,
    /// Available memory in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_bytes: Option<u64>,
}

/// A running job's figures at a trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TripJob {
    /// The job.
    pub job: i64,
    /// Its figures, with their quality.
    pub figures: JobFigures,
}

/// Transfer work at a trip.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TripTransfer {
    /// Phases holding a slot.
    pub active: u32,
    /// Daemon-process bytes read while holding a slot.
    pub inproc_read_bytes: u64,
    /// Daemon-process bytes written while holding a slot.
    pub inproc_write_bytes: u64,
    /// The transfer cgroup's figures, when delegated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub figures: Option<JobFigures>,
}

/// A process outside any job: its program name only, never arguments or environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TripProc {
    /// Process id.
    pub pid: i32,
    /// Program base name.
    pub name: String,
    /// Disk bytes in the sample window.
    pub io_bytes: u64,
    /// Resident memory.
    pub rss_bytes: u64,
}

/// The details of a `pressure.trip` audit row (design §9): what was stalled, what every job and
/// the transfer cgroup had cost, and who outside any job was busiest in the last window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TripDetails {
    /// `io`, `cpu` or `memory`.
    pub resource: String,
    /// `gate` (admission refused), `psi` (machine pressure over the alert threshold) or `transfer`.
    pub source: String,
    /// The gate's wait text or the threshold crossed.
    pub reason: String,
    /// Trips of this resource dropped by the rate limit since the previous one.
    #[serde(default)]
    pub suppressed: u64,
    /// The attribution line (exact cgroup or process tree).
    #[serde(default)]
    pub attribution: String,
    /// Machine-wide pressure.
    #[serde(default)]
    pub machine: TripMachine,
    /// Running jobs' figures.
    #[serde(default)]
    pub jobs: Vec<TripJob>,
    /// Transfer work.
    #[serde(default)]
    pub transfer: Option<TripTransfer>,
    /// Length of the sample window the processes below cover, ms (0: first sample).
    #[serde(default)]
    pub window_ms: u64,
    /// Outside processes by disk bytes in the window, biggest first.
    #[serde(default)]
    pub top_io: Vec<TripProc>,
    /// Outside processes by resident memory, biggest first.
    #[serde(default)]
    pub top_rss: Vec<TripProc>,
    /// Processes whose I/O could not be read (other users').
    #[serde(default)]
    pub unreadable: usize,
}

/// Largest serialized trip details; the audit row bound is 4096 bytes.
const TRIP_DETAILS_BUDGET: usize = 3600;

impl TripDetails {
    /// Drops the least useful entries until the details fit the audit row bound: process lists
    /// shrink first (the head of each ranking is what matters), then the jobs.
    pub fn bounded(mut self) -> TripDetails {
        let size = |d: &TripDetails| serde_json::to_string(d).map_or(usize::MAX, |s| s.len());
        while size(&self) > TRIP_DETAILS_BUDGET {
            if self.top_rss.len() > 3 {
                self.top_rss.pop();
            } else if self.top_io.len() > 3 {
                self.top_io.pop();
            } else if self.jobs.len() > 2 {
                self.jobs.pop();
            } else if !self.top_rss.is_empty() || !self.top_io.is_empty() {
                self.top_rss.pop();
                self.top_io.pop();
            } else {
                self.jobs.clear();
                self.reason.truncate(120);
                break;
            }
        }
        self
    }

    /// One line for lists: the resource, the source, the reason and the busiest outside process.
    pub fn summary(&self) -> String {
        let who = match (self.top_io.first(), self.top_rss.first()) {
            (Some(p), _) if p.io_bytes > 0 => format!(
                "; busiest outside jobs: {} (pid {}) moved {}",
                p.name,
                p.pid,
                tasks_core::human_bytes(p.io_bytes)
            ),
            (None, Some(p)) | (Some(_), Some(p)) => format!(
                "; largest outside jobs: {} (pid {}) {}",
                p.name,
                p.pid,
                tasks_core::human_bytes(p.rss_bytes)
            ),
            _ => String::new(),
        };
        format!("{} {}: {}{who}", self.resource, self.source, self.reason)
    }
}

/// What a checkout's build output is to `gc`, most protected first (R-839).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcState {
    /// A job of it is queued, waiting or running: never deleted.
    InUse,
    /// Used within the idle days.
    Active,
    /// No job for longer than the idle days.
    Idle,
    /// Its branch is fully merged into dev, main or master and nothing tracked is changed.
    Merged,
    /// Its working tree no longer exists.
    Gone,
    /// An output directory of the older layout no working tree links to.
    Old,
}

impl GcState {
    /// Every state, in order.
    pub const ALL: [GcState; 6] = [
        GcState::InUse,
        GcState::Active,
        GcState::Idle,
        GcState::Merged,
        GcState::Gone,
        GcState::Old,
    ];

    /// The word the CLI shows and takes.
    pub fn name(self) -> &'static str {
        match self {
            GcState::InUse => "in_use",
            GcState::Active => "active",
            GcState::Idle => "idle",
            GcState::Merged => "merged",
            GcState::Gone => "gone",
            GcState::Old => "old",
        }
    }

    /// The state named `s` (`in-use` also).
    pub fn parse(s: &str) -> Option<GcState> {
        let s = s.trim().replace('-', "_");
        GcState::ALL.into_iter().find(|g| g.name() == s)
    }

    /// True for the states `gc` suggests freeing: nothing there is still worked on.
    pub fn reclaimable(self) -> bool {
        matches!(self, GcState::Merged | GcState::Gone | GcState::Old)
    }

    /// Sort order of the report: what can go first.
    pub fn order(self) -> u8 {
        match self {
            GcState::Gone => 0,
            GcState::Old => 1,
            GcState::Merged => 2,
            GcState::Idle => 3,
            GcState::Active => 4,
            GcState::InUse => 5,
        }
    }
}

/// The `gc` report (R-839).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GcView {
    /// Days without a job after which a checkout counted as idle.
    pub idle_days: u32,
    /// This machine first, then each peer.
    pub machines: Vec<GcMachine>,
    /// One row per checkout holding anything, what can go first.
    pub rows: Vec<GcRow>,
    /// Caches every checkout shares: reported, never deleted.
    #[serde(default)]
    pub shared: Vec<GcShared>,
    /// Anything else worth a line.
    #[serde(default)]
    pub notes: Vec<String>,
    /// True when the request deleted.
    #[serde(default)]
    pub deleted: bool,
}

/// One machine's totals in a `gc` report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcMachine {
    /// Machine name.
    pub name: String,
    /// Bytes of checkouts' build output (and, on a peer, synced copies) it keeps for this machine.
    pub held_bytes: u64,
    /// Of those, the bytes in merged, gone and old rows still there.
    pub reclaimable_bytes: u64,
    /// Why the machine could not report.
    #[serde(default)]
    pub error: Option<String>,
}

/// One checkout in a `gc` report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcRow {
    /// The working tree (home-relative), or `(unknown checkout …)`.
    pub checkout: String,
    /// Its branch; none when detached or gone.
    #[serde(default)]
    pub branch: Option<String>,
    /// What it is to `gc`.
    pub state: GcState,
    /// The base it is merged into.
    #[serde(default)]
    pub merged_into: Option<String>,
    /// Its last job, ms.
    #[serde(default)]
    pub last_used_ms: Option<i64>,
    /// What each machine keeps of it.
    #[serde(default)]
    pub held: Vec<GcHeld>,
    /// Bytes this request freed.
    #[serde(default)]
    pub freed_bytes: u64,
    /// Why something of it was kept.
    #[serde(default)]
    pub kept: Option<String>,
}

/// What one machine keeps of a checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcHeld {
    /// Machine name.
    pub machine: String,
    /// Bytes.
    pub bytes: u64,
}

/// A cache every checkout shares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcShared {
    /// What it is.
    pub name: String,
    /// Where.
    pub path: String,
    /// Bytes.
    pub bytes: u64,
    /// How it is kept in bounds.
    pub note: String,
}

/// A duration estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EstimateView {
    /// The recipe the history is keyed by.
    pub recipe: String,
    /// Median seconds; `None` when there is no estimate to show (see `why_not`).
    pub median_secs: Option<f64>,
    /// 90th percentile seconds.
    pub p90_secs: Option<f64>,
    /// Passing runs behind it.
    pub samples: usize,
    /// One line with its provenance, or the reason there is no estimate.
    pub text: String,
    /// Why there is no estimate (`first run`, `no passing run yet`, `first build here`).
    #[serde(default)]
    pub why_not: Option<String>,
    /// True when it rests on another machine's runs.
    #[serde(default)]
    pub rough: bool,
}

/// One measured duration as the history API shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleView {
    /// Duration in seconds.
    pub secs: f64,
    /// `warm`, `cold` or `unknown`.
    pub cache: String,
    /// `passed`, `failed` or `cancelled`.
    pub outcome: String,
    /// When the run finished, ms since epoch.
    pub at_ms: u64,
}

/// The newest samples of one target (or of the whole job) of a recipe on a machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesView {
    /// A target label such as `test:lifecycle`; `None` for the whole job.
    pub target: Option<String>,
    /// Samples, oldest first.
    pub samples: Vec<SampleView>,
}

/// What one machine knows about a recipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineHistory {
    /// The machine.
    pub machine: String,
    /// Every target label with samples for this recipe on this machine, sorted. Complete even
    /// when `series` was cut.
    pub targets: Vec<String>,
    /// Samples per target.
    pub series: Vec<SeriesView>,
}

/// History of one recipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipeHistory {
    /// The `RecipeKey` display string.
    pub recipe: String,
    /// Per machine, sorted by name.
    pub machines: Vec<MachineHistory>,
}

/// The answer to a history request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryView {
    /// Per recipe, sorted by name.
    pub recipes: Vec<RecipeHistory>,
    /// True when a bound ([`MAX_HISTORY_RECIPES`], [`MAX_HISTORY_TOTAL_SAMPLES`]) cut the
    /// answer; narrow the request.
    pub truncated: bool,
}

fn cache_name(c: tasks_timing::Cache) -> &'static str {
    match c {
        tasks_timing::Cache::Warm => "warm",
        tasks_timing::Cache::Cold => "cold",
        tasks_timing::Cache::Unknown => "unknown",
    }
}

fn outcome_name(o: tasks_timing::Outcome) -> &'static str {
    match o {
        tasks_timing::Outcome::Passed => "passed",
        tasks_timing::Outcome::Failed => "failed",
        tasks_timing::Outcome::Cancelled => "cancelled",
    }
}

impl SampleView {
    /// The view of a stored sample.
    pub fn of(s: &tasks_timing::Sample) -> SampleView {
        SampleView {
            secs: s.secs,
            cache: cache_name(s.cache).to_string(),
            outcome: outcome_name(s.outcome).to_string(),
            at_ms: s.at_ms,
        }
    }

    /// The sample back; unknown words read as `unknown` warmth and a failed outcome, never as
    /// a pass.
    pub fn to_sample(&self) -> tasks_timing::Sample {
        tasks_timing::Sample {
            secs: self.secs,
            cache: match self.cache.as_str() {
                "warm" => tasks_timing::Cache::Warm,
                "cold" => tasks_timing::Cache::Cold,
                _ => tasks_timing::Cache::Unknown,
            },
            outcome: match self.outcome.as_str() {
                "passed" => tasks_timing::Outcome::Passed,
                "cancelled" => tasks_timing::Outcome::Cancelled,
                _ => tasks_timing::Outcome::Failed,
            },
            at_ms: self.at_ms,
        }
    }
}

impl HistoryView {
    /// The reply as an in-memory [`tasks_timing::History`], for the code that estimates and
    /// suggests from history (`tasks_policy::suggest`, `tasks_timing::estimate`).
    pub fn to_history(&self) -> tasks_timing::MemHistory {
        let mut h = tasks_timing::MemHistory::new();
        for r in &self.recipes {
            for m in &r.machines {
                for s in &m.series {
                    for v in &s.samples {
                        h.add(
                            tasks_timing::SampleKey {
                                recipe: r.recipe.clone(),
                                target: s.target.clone(),
                                machine: m.machine.clone(),
                            },
                            v.to_sample(),
                        );
                    }
                }
            }
        }
        h
    }
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
            reason: Some("wrong branch".into()),
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
    fn r108_older_requests_without_the_new_fields_still_parse() {
        let cancel = parse_request(br#"{"v":1,"request":{"op":"cancel","id":3,"who":"me"}}"#);
        assert_eq!(
            cancel.unwrap(),
            Request::Cancel {
                id: JobId(3),
                who: "me".into(),
                reason: None
            }
        );
        let drain = parse_request(br#"{"v":1,"request":{"op":"drain"}}"#).unwrap();
        assert_eq!(
            drain,
            Request::Drain {
                cancel: false,
                who: None
            }
        );
        let h = parse_request(br#"{"v":1,"request":{"op":"history"}}"#).unwrap();
        assert_eq!(
            h,
            Request::History {
                recipes: vec![],
                machines: vec![],
                max_samples: None
            }
        );
    }

    #[test]
    fn the_connection_limit_has_its_own_wire_name() {
        let line = render_reply(Reply::Err(ApiError::new(
            ErrorCode::ConnectionLimit,
            "full",
        )));
        assert!(line.contains(r#""code":"connection_limit""#), "{line}");
        assert_ne!(ErrorCode::ConnectionLimit, ErrorCode::Busy);
        assert_ne!(ErrorCode::ConnectionLimit, ErrorCode::LedgerUnavailable);
    }

    #[test]
    fn history_views_round_trip_into_a_history() {
        use tasks_timing::{Cache, History, Outcome, Sample, SampleKey};
        let s = Sample {
            secs: 2.5,
            cache: Cache::Cold,
            outcome: Outcome::Cancelled,
            at_ms: 9,
        };
        assert_eq!(SampleView::of(&s).to_sample(), s);
        // an unknown outcome is never read as a pass
        let odd = SampleView {
            secs: 1.0,
            cache: "?".into(),
            outcome: "?".into(),
            at_ms: 1,
        };
        assert_eq!(odd.to_sample().outcome, Outcome::Failed);
        let view = HistoryView {
            recipes: vec![RecipeHistory {
                recipe: "r".into(),
                machines: vec![MachineHistory {
                    machine: "m".into(),
                    targets: vec!["test:a".into()],
                    series: vec![SeriesView {
                        target: Some("test:a".into()),
                        samples: vec![SampleView::of(&s)],
                    }],
                }],
            }],
            truncated: false,
        };
        let h = view.to_history();
        let key = SampleKey {
            recipe: "r".into(),
            target: Some("test:a".into()),
            machine: "m".into(),
        };
        assert_eq!(h.samples(&key).unwrap(), vec![s]);
        assert_eq!(h.targets("r").unwrap(), ["test:a"]);
    }

    #[test]
    fn older_status_json_without_the_ui_fields_still_parses() {
        let peer: PeerView = serde_json::from_str(
            r#"{"name":"nixos","node_id":"n","state":"up","last_seen_ago_secs":1,"detail":""}"#,
        )
        .unwrap();
        assert_eq!(peer.machine, None);
        // R-627: a peer machine written before per-job use existed
        let m: PeerMachineView = serde_json::from_str(
            r#"{"cores":4,"memory_bytes":1,"slots":[],"waiting":0,"running":0,
                "cpu_pct":null,"memory_pct":null,"io_pct":null,"pressure_summary":""}"#,
        )
        .unwrap();
        assert!(m.jobs.is_empty() && m.cores_in_use_milli.is_none());
        let v: tasks_core::metrics::JobMetricsView =
            serde_json::from_str(r#"{"quality":null,"samples":[],"procs":[]}"#).unwrap();
        assert_eq!(v.source, None);
        let p: PressureView = serde_json::from_str(
            r#"{"memory":"normal","memory_some":null,"memory_full":null,"cpu":null,"io":null,
                "available_memory":null,"cache_free":null,"state_free":null,
                "compile_blocker":null,"warning":null}"#,
        )
        .unwrap();
        assert_eq!(p.memory_total, None);
        assert_eq!(p.memory_stall_limit, None);
    }

    #[test]
    fn r641_a_memory_level_reads_ok_tight_or_critical_everywhere() {
        assert_eq!(memory_word("normal"), "ok");
        assert_eq!(memory_word("warn"), "tight");
        assert_eq!(memory_word("critical"), "critical");
        assert_eq!(memory_word("unknown"), "unknown");
        assert_eq!(memory_word(""), "unknown");
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
