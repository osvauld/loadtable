//! Local task ledger and explicit, allowlisted execution.
#[cfg(target_os = "linux")]
pub mod admission;
#[cfg(target_os = "linux")]
pub mod android;
#[cfg(target_os = "linux")]
pub mod android_build;
#[cfg(target_os = "linux")]
pub mod cache;
pub mod catalog;
#[cfg(target_os = "linux")]
pub mod device;
pub mod execution;
pub mod local_command;
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
use std::time::{SystemTime, UNIX_EPOCH};

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

pub struct Store(pub(crate) Connection);
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }
    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }
    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
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
        let mut statement = self
            .0
            .prepare("SELECT id, spec, phase, submitted_at FROM jobs ORDER BY id ASC")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut jobs = Vec::new();
        for row in rows {
            let (id, spec, phase, submitted_at) = row?;
            let phase = Phase::from_db(&phase)?;
            jobs.push(Job {
                id,
                spec: serde_json::from_str(&spec)?,
                phase,
                submitted_at,
                execution: self.execution(id)?,
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
    Submit(JobSpec),
    Cancel(i64),
    Approve(i64),
    ApproveAndroidTrial(i64),
    ApproveCommand(i64),
    StartWorker,
    Logs(i64),
    Record(RuntimeSample),
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
            Action::Submit(spec) => store.submit(&spec).map(|j| {
                submitted_id = Some(j.id);
            }),
            Action::Cancel(id) => store.cancel(id),
            Action::Approve(id) => store.approve(id),
            Action::ApproveAndroidTrial(id) => store.approve_android_trial(id),
            Action::ApproveCommand(id) => store.approve_command(id),
            Action::StartWorker | Action::Logs(_) => Err("action requires local daemon".into()),
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

/// Recover only when connecting failed, before any request was delivered.
/// Write/read/protocol failures must not replay a possibly applied mutation.
#[cfg(unix)]
pub fn request_with_reconnect(
    path: &Path,
    action: Action,
    start: impl FnOnce() -> Result<()>,
) -> Result<Response> {
    use std::io::ErrorKind;
    use std::os::unix::net::UnixStream;
    let stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            start()?;
            UnixStream::connect(path)?
        }
        Err(error) => return Err(error.into()),
    };
    request_on_stream(stream, action)
}

#[cfg(unix)]
fn request_on_stream(
    mut stream: std::os::unix::net::UnixStream,
    action: Action,
) -> Result<Response> {
    use std::io::{BufRead, BufReader, Read, Write};
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    serde_json::to_writer(
        &mut stream,
        &Request {
            version: PROTOCOL_VERSION,
            action,
        },
    )?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream.take(4 * 1024 * 1024)).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err("incomplete or oversized response".into());
    }
    let response: Response = serde_json::from_str(&line)?;
    if response.version != PROTOCOL_VERSION {
        return Err("unsupported response version; an older daemon may still own this state directory. Stop/restart it only after checking its active work, or choose a new TASK_MANAGER_STATE_DIR".into());
    }
    Ok(response)
}
