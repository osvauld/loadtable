//! Local task ledger and explicit, allowlisted execution.
#[cfg(target_os = "linux")]
pub mod admission;
#[cfg(target_os = "linux")]
pub mod android;
#[cfg(target_os = "linux")]
pub mod android_build;
#[cfg(target_os = "linux")]
pub mod cache;
#[cfg(target_os = "linux")]
pub mod cache_server;
pub mod catalog;
#[cfg(target_os = "linux")]
pub mod device;
pub mod device_denylist;
pub mod execution;
pub mod ledger;
pub mod local_command;
pub mod log_search;
pub mod metrics;
#[cfg(target_os = "linux")]
pub mod peer;
pub mod peer_link;
pub mod placement;
pub mod policy;
#[cfg(target_os = "linux")]
pub mod retention;
pub mod snapshot;
#[cfg(target_os = "linux")]
pub mod tmp_usage;
#[cfg(target_os = "linux")]
pub mod worker;
#[cfg(target_os = "linux")]
pub mod workspace_snapshot;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Cost {
    Quick,
    CpuSimulation,
    RealTimeIntegration,
    Stress,
    Measurement,
    Device,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Queued,
    Approved,
    Preparing,
    WaitingBuild,
    Building,
    WaitingRun,
    Running,
    Passed,
    Failed,
    Interrupted,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobSpec {
    pub suite: String,
    pub owner: String,
    pub source: String,
    pub revision: String,
    pub configuration: String,
    pub cost: Cost,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: i64,
    pub spec: JobSpec,
    pub phase: Phase,
    pub submitted_at: i64,
    #[serde(default)]
    pub execution: Option<execution::Execution>,
}

/// Recent runs matter more. Variance is an observed-spread indicator, not a confidence interval.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    mean: f64,
    variance: f64,
    samples: u64,
}

impl Estimate {
    pub fn record(&mut self, seconds: f64) -> Result<()> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err("runtime must be finite and nonnegative".into());
        }
        if self.samples == 0 {
            self.mean = seconds;
        } else {
            let delta = seconds - self.mean;
            self.mean += 0.2 * delta;
            self.variance = 0.8 * (self.variance + 0.2 * delta * delta);
        }
        self.samples += 1;
        Ok(())
    }
    pub fn seconds(&self) -> Option<f64> {
        (self.samples > 0).then_some(self.mean)
    }
    pub fn samples(&self) -> u64 {
        self.samples
    }
    pub fn range(&self) -> Option<(f64, f64)> {
        if self.samples < 2 {
            return None;
        }
        let spread = 2.0 * self.variance.sqrt();
        Some(((self.mean - spread).max(0.0), self.mean + spread))
    }
}

/// One runtime history row as reported over the socket.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeEstimate {
    pub suite: String,
    pub configuration: String,
    pub stage: String,
    pub seconds: Option<f64>,
    pub range: Option<(f64, f64)>,
    pub samples: u64,
}

/// A completed runtime observation reported by an agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeSample {
    pub suite: String,
    pub configuration: String,
    pub stage: String,
    pub seconds: f64,
}

fn check_field(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 4096 {
        return Err("missing or oversized job field".into());
    }
    Ok(())
}

/// How long one ledger statement waits for another connection's lock before
/// failing with SQLITE_BUSY. Was 5 s; workers additionally retry busy errors
/// (see `ledger`), and clients wait well beyond this (see `ClientTimeouts`).
pub const STORE_BUSY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Store(pub(crate) Connection);
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_busy_timeout(path, STORE_BUSY_TIMEOUT)
    }
    /// Tests use a short timeout to model a busy ledger quickly.
    pub fn open_with_busy_timeout(path: &Path, busy: Duration) -> Result<Self> {
        Self::init(Connection::open(path)?, busy)
    }
    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, STORE_BUSY_TIMEOUT)
    }
    fn init(conn: Connection, busy: Duration) -> Result<Self> {
        conn.busy_timeout(busy)?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                spec TEXT NOT NULL, phase TEXT NOT NULL, submitted_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runtimes (
                suite TEXT NOT NULL, configuration TEXT NOT NULL, stage TEXT NOT NULL,
                estimate TEXT NOT NULL, PRIMARY KEY(suite, configuration, stage)
            );
            CREATE TABLE IF NOT EXISTS executions (
                job_id INTEGER PRIMARY KEY REFERENCES jobs(id), data TEXT NOT NULL
            );",
        )?;
        Ok(Self(conn))
    }
    pub fn submit(&mut self, spec: &JobSpec) -> Result<Job> {
        for value in [
            &spec.suite,
            &spec.owner,
            &spec.source,
            &spec.revision,
            &spec.configuration,
        ] {
            check_field(value)?;
        }
        let submitted_at = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
        self.0.execute(
            "INSERT INTO jobs(spec, phase, submitted_at) VALUES (?1, 'queued', ?2)",
            params![serde_json::to_string(spec)?, submitted_at],
        )?;
        Ok(Job {
            id: self.0.last_insert_rowid(),
            spec: spec.clone(),
            phase: Phase::Queued,
            submitted_at,
            execution: None,
        })
    }
    pub fn jobs(&self) -> Result<Vec<Job>> {
        self.select_jobs(None)
    }
    /// All jobs, or the one with `only`; executions come from the same query
    /// instead of one lookup per job.
    pub(crate) fn select_jobs(&self, only: Option<i64>) -> Result<Vec<Job>> {
        let mut statement = self.0.prepare(
            "SELECT j.id, j.spec, j.phase, j.submitted_at, e.data FROM jobs j \
             LEFT JOIN executions e ON e.job_id = j.id \
             WHERE ?1 IS NULL OR j.id = ?1 ORDER BY j.id ASC",
        )?;
        let rows = statement.query_map([only], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut jobs = Vec::new();
        for row in rows {
            let (id, spec, phase, submitted_at, execution) = row?;
            let phase = Phase::from_db(&phase)?;
            jobs.push(Job {
                id,
                spec: serde_json::from_str(&spec)?,
                phase,
                submitted_at,
                execution: execution.map(|v| serde_json::from_str(&v)).transpose()?,
            });
        }
        Ok(jobs)
    }
    pub fn cancel(&mut self, id: i64) -> Result<()> {
        self.request_cancel(id)
    }
    pub fn estimate(&self, suite: &str, configuration: &str, stage: &str) -> Result<Estimate> {
        use rusqlite::OptionalExtension;
        let value = self
            .0
            .query_row(
                "SELECT estimate FROM runtimes WHERE suite=?1 AND configuration=?2 AND stage=?3",
                params![suite, configuration, stage],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        match value {
            Some(value) => Ok(serde_json::from_str(&value)?),
            None => Ok(Estimate::default()),
        }
    }
    pub fn estimates(&self) -> Result<Vec<RuntimeEstimate>> {
        let mut statement = self.0.prepare(
            "SELECT suite, configuration, stage, estimate FROM runtimes
            ORDER BY suite, configuration, stage",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut estimates = Vec::new();
        for row in rows {
            let (suite, configuration, stage, estimate) = row?;
            let estimate: Estimate = serde_json::from_str(&estimate)?;
            estimates.push(RuntimeEstimate {
                suite,
                configuration,
                stage,
                seconds: estimate.seconds(),
                range: estimate.range(),
                samples: estimate.samples(),
            });
        }
        Ok(estimates)
    }
    /// Only completed, comparable observations should call this. Until a worker exists,
    /// agents report their own timings through `Action::Record`.
    pub fn record_runtime(
        &mut self,
        suite: &str,
        configuration: &str,
        stage: &str,
        seconds: f64,
    ) -> Result<()> {
        for value in [suite, configuration, stage] {
            check_field(value)?;
        }
        let transaction = self.0.transaction()?;
        use rusqlite::OptionalExtension;
        let old = transaction
            .query_row(
                "SELECT estimate FROM runtimes WHERE suite=?1 AND configuration=?2 AND stage=?3",
                params![suite, configuration, stage],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        let mut estimate: Estimate = match old {
            Some(old) => serde_json::from_str(&old)?,
            None => Estimate::default(),
        };
        estimate.record(seconds)?;
        transaction.execute(
            "INSERT INTO runtimes(suite, configuration, stage, estimate) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(suite, configuration, stage) DO UPDATE SET estimate=excluded.estimate",
            params![
                suite,
                configuration,
                stage,
                serde_json::to_string(&estimate)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub action: Action,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Action {
    List,
    /// One job, without the full listing or estimates: what a waiting `tasks run`
    /// polls. Daemons older than this reject it as an unknown variant; clients then
    /// fall back to `List`.
    Job(i64),
    Submit(JobSpec),
    Cancel(i64),
    Approve(i64),
    ApproveAndroidTrial(i64),
    ApproveCommand(i64),
    StartWorker,
    Logs(i64),
    Record(RuntimeSample),
    /// Local machine snapshot (cluster snapshot v1). Answered with a
    /// `metrics::StatusResponse`, not `Response`; see [`request_status`].
    Status,
    /// The daemon's peer runtime (config, endpoint, forwarded/inbound links). Answered with a
    /// `peer_link::PeersResponse`; daemons older than it answer "unknown variant".
    Peers,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    pub execution_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_id: Option<i64>,
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub estimates: Vec<RuntimeEstimate>,
    pub error: Option<String>,
    #[serde(default)]
    pub log: Option<execution::LogTail>,
}

pub fn handle(store: &mut Store, request: Request) -> Response {
    let mut submitted_id = None;
    let result = if request.version != PROTOCOL_VERSION {
        Err("unsupported protocol version".into())
    } else {
        match request.action {
            Action::List => Ok(()),
            Action::Job(id) => {
                let job = store.job(id);
                return Response {
                    version: PROTOCOL_VERSION,
                    execution_enabled: false,
                    submitted_id: None,
                    error: job.as_ref().err().map(|e| e.to_string()),
                    jobs: job.into_iter().collect(),
                    estimates: Vec::new(),
                    log: None,
                };
            }
            Action::Submit(spec) => store.submit(&spec).map(|j| {
                submitted_id = Some(j.id);
            }),
            Action::Cancel(id) => store.cancel(id),
            Action::Approve(id) => store.approve(id),
            Action::ApproveAndroidTrial(id) => store.approve_android_trial(id),
            Action::ApproveCommand(id) => store.approve_command(id),
            Action::StartWorker | Action::Logs(_) | Action::Status | Action::Peers => {
                Err("action requires local daemon".into())
            }
            Action::Record(sample) => {
                if sample.stage.starts_with("worker.") {
                    Err("worker.* runtime stages are reserved for worker observations".into())
                } else {
                    store.record_runtime(
                        &sample.suite,
                        &sample.configuration,
                        &sample.stage,
                        sample.seconds,
                    )
                }
            }
        }
    };
    let jobs = store.jobs();
    let estimates = store.estimates();
    let error = result
        .err()
        .or_else(|| jobs.as_ref().err().map(|e| e.to_string().into()))
        .or_else(|| estimates.as_ref().err().map(|e| e.to_string().into()));
    Response {
        version: PROTOCOL_VERSION,
        execution_enabled: false,
        submitted_id,
        jobs: jobs.unwrap_or_default(),
        estimates: estimates.unwrap_or_default(),
        error: error.map(|e| e.to_string()),
        log: None,
    }
}

pub fn state_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("TASK_MANAGER_STATE_DIR") {
        return path.into();
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(path).join("tinline-task-manager-native");
    }
    PathBuf::from(std::env::var_os("HOME").expect("HOME or TASK_MANAGER_STATE_DIR required"))
        .join(".local/state/tinline-task-manager-native")
}

#[cfg(unix)]
pub fn request(path: &Path, action: Action) -> Result<Response> {
    use std::os::unix::net::UnixStream;
    request_on_stream(UnixStream::connect(path)?, action)
}

/// How long a client waits for the daemon. One reply can queue behind other
/// requests on the daemon's store mutex, and each ledger statement may wait up
/// to [`STORE_BUSY_TIMEOUT`], so `reply` is several busy timeouts long.
/// `total` bounds all retries of one call (connect retries, read-only replays
/// and lost-reply reconciliation).
#[derive(Clone, Copy, Debug)]
pub struct ClientTimeouts {
    pub reply: Duration,
    pub total: Duration,
}
impl Default for ClientTimeouts {
    fn default() -> Self {
        Self {
            reply: Duration::from_secs(60),
            total: Duration::from_secs(180),
        }
    }
}

/// Recover only when connecting failed, before any request was delivered.
/// A delivered mutation whose reply is lost is never replayed: read-only and
/// idempotent actions are retried, a submit is reconciled against the ledger.
#[cfg(unix)]
pub fn request_with_reconnect(
    path: &Path,
    action: Action,
    start: impl FnOnce() -> Result<()>,
) -> Result<Response> {
    request_with_reconnect_timeouts(path, action, start, ClientTimeouts::default())
}

#[cfg(unix)]
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// SQLITE_BUSY / SQLITE_LOCKED as reported in a daemon reply's error text.
fn busy_error(error: &str) -> bool {
    error.contains("database is locked")
        || error.contains("database is busy")
        || error.contains("database table is locked")
}

/// Connects, starting the daemon at most once when no socket answers, and
/// retrying a busy listener (EAGAIN: backlog full) until `deadline`.
#[cfg(unix)]
fn connect(
    path: &Path,
    mut start: Option<Box<dyn FnOnce() -> Result<()> + '_>>,
    deadline: Instant,
) -> Result<std::os::unix::net::UnixStream> {
    use std::io::ErrorKind;
    let mut backoff = Duration::from_millis(50);
    loop {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                ) =>
            {
                match start.take() {
                    Some(start) => start()?,
                    None => return Err(error.into()),
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(2));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

const MAX_REPLY: u64 = 4 * 1024 * 1024;

/// Outcome of one delivered request.
#[cfg(unix)]
enum Attempt {
    Answered(Response),
    /// The request may have reached the daemon, but no complete reply arrived.
    Lost(String),
}

#[cfg(unix)]
pub fn request_with_reconnect_timeouts(
    path: &Path,
    action: Action,
    start: impl FnOnce() -> Result<()>,
    timeouts: ClientTimeouts,
) -> Result<Response> {
    let began = Instant::now();
    let deadline = began + timeouts.total;
    let since = unix_now();
    let mut stream = connect(path, Some(Box::new(start)), deadline)?;
    let mut backoff = Duration::from_millis(250);
    let lost = loop {
        let mut response = match exchange(stream, action.clone(), timeouts.reply)? {
            Attempt::Answered(response) => response,
            Attempt::Lost(why) => break why,
        };
        let busy = response.error.as_deref().is_some_and(busy_error);
        if busy && let (Action::Submit(_), Some(id)) = (&action, response.submitted_id) {
            // The insert committed; only the daemon's listing afterwards was busy.
            eprintln!(
                "tasks: job {id} was recorded; the daemon's job listing afterwards failed ({}), ignored",
                response.error.take().unwrap_or_default()
            );
            return Ok(response);
        }
        // A busy reply to these was not applied (a failed submit insert commits
        // nothing; the others are read-only or idempotent), so sending again is safe.
        let resend =
            busy && matches!(
                action,
                Action::List
                    | Action::Job(_)
                    | Action::Logs(_)
                    | Action::StartWorker
                    | Action::Submit(_)
            ) && Instant::now() + backoff < deadline;
        if !resend {
            return Ok(response);
        }
        eprintln!(
            "tasks: ledger busy ({}); retrying",
            response.error.unwrap_or_default()
        );
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(5));
        stream = connect(path, None, deadline)?;
    };
    let waited = || began.elapsed().as_secs();
    match &action {
        // Read-only or idempotent: safe to send again.
        Action::List | Action::Job(_) | Action::Logs(_) | Action::StartWorker => {
            let mut last = lost;
            let mut backoff = Duration::from_millis(250);
            while Instant::now() < deadline {
                eprintln!("tasks: daemon slow ({last}); retrying read-only request");
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(5));
                let stream = connect(path, None, deadline)?;
                match exchange(stream, action.clone(), timeouts.reply)? {
                    Attempt::Answered(response) => return Ok(response),
                    Attempt::Lost(why) => last = why,
                }
            }
            Err(format!(
                "daemon gave no reply for {}s ({last}); the ledger may be busy. Retry later; nothing was changed by this request",
                waited()
            )
            .into())
        }
        Action::Submit(spec) => reconcile_submit(path, spec, since, &lost, deadline, timeouts),
        Action::Approve(id) | Action::ApproveAndroidTrial(id) | Action::ApproveCommand(id) => {
            reconcile_approve(path, *id, &lost, deadline, timeouts)
        }
        Action::Cancel(id) => Err(format!(
            "cancel of job {id} was sent but no reply arrived ({lost}); it may have been applied. Check `tasks list` before retrying"
        )
        .into()),
        Action::Record(_) => Err(format!(
            "record was sent but no reply arrived ({lost}); it may have been applied. Do not resend blindly"
        )
        .into()),
        Action::Status => Err(format!("status request got no reply ({lost})").into()),
        Action::Peers => Err(format!("peers request got no reply ({lost})").into()),
    }
}

/// Lists the ledger after a lost reply. Needs several clean listings before
/// concluding "not there", because the lost request may still be queued on the
/// daemon's store mutex ahead of (or behind) this listing.
#[cfg(unix)]
fn reconcile<T>(
    path: &Path,
    deadline: Instant,
    timeouts: ClientTimeouts,
    mut found: impl FnMut(&Response) -> Option<T>,
) -> Result<Option<T>> {
    let mut clean = 0;
    let mut backoff = Duration::from_millis(250);
    while clean < 3 && Instant::now() < deadline {
        let stream = connect(path, None, deadline)?;
        match exchange(stream, Action::List, timeouts.reply)? {
            Attempt::Answered(response) if response.error.is_none() => {
                if let Some(value) = found(&response) {
                    return Ok(Some(value));
                }
                clean += 1;
            }
            _ => (),
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }
    Ok(None)
}

/// Jobs that this exact submit could have created: identical spec, recorded no
/// earlier than the moment the client started sending.
pub fn submit_candidates(jobs: &[Job], spec: &JobSpec, since: i64) -> Vec<i64> {
    let wanted = serde_json::to_value(spec).ok();
    jobs.iter()
        .filter(|j| j.submitted_at >= since && serde_json::to_value(&j.spec).ok() == wanted)
        .map(|j| j.id)
        .collect()
}

#[cfg(unix)]
fn reconcile_submit(
    path: &Path,
    spec: &JobSpec,
    since: i64,
    lost: &str,
    deadline: Instant,
    timeouts: ClientTimeouts,
) -> Result<Response> {
    let mut ambiguous = vec![];
    let found = reconcile(
        path,
        deadline,
        timeouts,
        |response| match submit_candidates(&response.jobs, spec, since).as_slice() {
            [] => None,
            [id] => Some((*id, response.jobs.clone())),
            many => {
                ambiguous = many.to_vec();
                None
            }
        },
    )?;
    let hint = format!(
        "owner={} cwd={} submitted at/after unix time {since}",
        spec.owner, spec.source
    );
    match found {
        Some((id, jobs)) => {
            eprintln!(
                "tasks: the daemon's reply to submit was lost ({lost}); found the submitted job {id} in the ledger by exact spec match ({hint}); not resubmitted"
            );
            Ok(Response {
                version: PROTOCOL_VERSION,
                execution_enabled: false,
                submitted_id: Some(id),
                jobs,
                estimates: vec![],
                error: None,
                log: None,
            })
        }
        None if !ambiguous.is_empty() => Err(format!(
            "submit reply lost ({lost}) and several identical jobs match: {ambiguous:?} ({hint}). One is yours; inspect them with `tasks list` / `tasks logs ID`. Do NOT resubmit"
        )
        .into()),
        None => Err(format!(
            "submit was sent but no reply arrived ({lost}) and no matching job is listed yet; it may still be recorded. Do NOT resubmit: look in `tasks list` for {hint}"
        )
        .into()),
    }
}

#[cfg(unix)]
fn reconcile_approve(
    path: &Path,
    id: i64,
    lost: &str,
    deadline: Instant,
    timeouts: ClientTimeouts,
) -> Result<Response> {
    let found = reconcile(path, deadline, timeouts, |response| {
        response
            .jobs
            .iter()
            .any(|j| j.id == id && j.phase != Phase::Queued && j.execution.is_some())
            .then(|| response.jobs.clone())
    })?;
    match found {
        Some(jobs) => {
            eprintln!(
                "tasks: the daemon's reply to approving job {id} was lost ({lost}); the ledger shows it approved"
            );
            Ok(Response {
                version: PROTOCOL_VERSION,
                execution_enabled: false,
                submitted_id: None,
                jobs,
                estimates: vec![],
                error: None,
                log: None,
            })
        }
        None => Err(format!(
            "approval of job {id} was sent but not confirmed ({lost}). Check `tasks list`; if job {id} is still queued, approving it again is safe (only a queued job can be approved)"
        )
        .into()),
    }
}

/// Sends one request. Connection-level failures after connecting are `Lost`:
/// the daemon may have applied the request, so the caller must not replay a
/// mutation. A malformed or mismatched reply is an ordinary error.
#[cfg(unix)]
fn exchange(
    mut stream: std::os::unix::net::UnixStream,
    action: Action,
    reply: Duration,
) -> Result<Attempt> {
    use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
    stream.set_read_timeout(Some(reply))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let request = serde_json::to_vec(&Request {
        version: PROTOCOL_VERSION,
        action,
    })?;
    if let Err(error) = stream
        .write_all(&request)
        .and_then(|()| stream.write_all(b"\n"))
    {
        return Ok(Attempt::Lost(format!("send failed: {error}")));
    }
    let mut line = String::new();
    if let Err(error) = BufReader::new(stream.take(MAX_REPLY)).read_line(&mut line) {
        return Ok(Attempt::Lost(match error.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => {
                format!("no reply within {}s", reply.as_secs())
            }
            _ => format!("reply failed: {error}"),
        }));
    }
    if line.is_empty() {
        return Ok(Attempt::Lost(
            "daemon closed the connection without a reply (incomplete response)".into(),
        ));
    }
    if !line.ends_with('\n') {
        if line.len() < MAX_REPLY as usize {
            return Ok(Attempt::Lost(
                "truncated reply (incomplete response)".into(),
            ));
        }
        return Err("incomplete or oversized response".into());
    }
    let response: Response = serde_json::from_str(&line)?;
    if response.version != PROTOCOL_VERSION {
        return Err("unsupported response version; an older daemon may still own this state directory. Stop/restart it only after checking its active work, or choose a new TASK_MANAGER_STATE_DIR".into());
    }
    Ok(Attempt::Answered(response))
}

/// `tasks status`: one read-only request for the daemon's machine snapshot.
/// An older daemon does not know the action and answers with an error.
#[cfg(unix)]
pub fn request_status(path: &Path, timeout: Duration) -> Result<metrics::StatusResponse> {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let request = serde_json::to_vec(&Request {
        version: PROTOCOL_VERSION,
        action: Action::Status,
    })?;
    stream.write_all(&request)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REPLY)).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err("incomplete status reply from the daemon".into());
    }
    let reply: metrics::StatusResponse = serde_json::from_str(&line)?;
    if reply.status.is_none() && reply.error.as_deref().is_some_and(|e| e.contains("variant")) {
        return Err(format!(
            "the running daemon does not support `status` (older release): {}",
            reply.error.unwrap_or_default()
        )
        .into());
    }
    Ok(reply)
}

#[cfg(unix)]
fn request_on_stream(stream: std::os::unix::net::UnixStream, action: Action) -> Result<Response> {
    match exchange(stream, action, ClientTimeouts::default().reply)? {
        Attempt::Answered(response) => Ok(response),
        Attempt::Lost(why) => Err(why.into()),
    }
}

/// What a caller must do when it stops waiting for a job it submitted.
pub fn live_job_hint(id: i64) -> String {
    format!(
        "job {id} is still live; query with `tasks logs {id}` / `tasks list`; do not resubmit (`tasks cancel {id}` cancels)"
    )
}

#[cfg(unix)]
static DETACH_NOTICE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

#[cfg(unix)]
extern "C" fn print_detach_notice(signal: i32) {
    // Async-signal-safe: one write(2) of a prebuilt message, then _exit.
    if let Some(text) = DETACH_NOTICE.get() {
        unsafe {
            libc::write(libc::STDERR_FILENO, text.as_ptr().cast(), text.len());
        }
    }
    unsafe { libc::_exit(128 + signal) }
}

/// Once a job ID is known, an interrupt or an outer timeout (SIGINT, SIGTERM,
/// SIGHUP) still detaches without cancelling, but first prints how to find the job.
/// SIGKILL cannot be caught, which is why the same hint is printed at submission.
#[cfg(unix)]
pub fn print_hint_on_detach(id: i64) {
    let _ =
        DETACH_NOTICE.set(format!("\ntasks: client stopped; {}\n", live_job_hint(id)).into_bytes());
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            libc::signal(
                signal,
                print_detach_notice as *const () as libc::sighandler_t,
            );
        }
    }
}

#[cfg(test)]
#[path = "job_poll_tests.rs"]
mod job_poll_tests;
