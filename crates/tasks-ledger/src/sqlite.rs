//! SQLite-backed store.

use crate::logic::{
    apply_cancel, apply_finish, apply_log_path, apply_priority, apply_start, apply_wait,
    check_deps, new_record, plan_jobs, validate_sample,
};
use crate::migrate::{MIGRATIONS, migrate};
use crate::record::{CancelRequest, Forward, LeaseRecord, Origin, TimeoutReason, phase_tag};
use crate::request::{PruneRow, select_prune};
use crate::{
    CancelOutcome, ChangeToken, Clock, Finish, JobRecord, LedgerError, ListFilter, NewJob,
    PlanContext, PlanSubmitted, PruneReport, Result, RetentionPolicy, StartInfo, Store,
    SystemClock,
};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tasks_core::plan::Plan;
use tasks_core::{JobFigures, JobId, MachineName, Phase, Priority, WaitReason};
use tasks_timing::{Cache, History, Outcome, Sample, SampleKey, TimingError};

const ACTIVE: &str = "('queued','waiting','running')";

/// How to open a database.
#[derive(Clone)]
pub struct OpenOptions {
    /// How long SQLite itself waits on a lock before reporting busy.
    pub busy_timeout: Duration,
    /// Time source for stamps.
    pub clock: Arc<dyn Clock>,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            busy_timeout: Duration::from_secs(5),
            clock: Arc::new(SystemClock),
        }
    }
}

impl std::fmt::Debug for OpenOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenOptions")
            .field("busy_timeout", &self.busy_timeout)
            .finish_non_exhaustive()
    }
}

/// A `Store` backed by a SQLite database in WAL mode.
pub struct SqliteStore {
    conn: Mutex<Connection>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore").finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Opens (creating and migrating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<SqliteStore> {
        Self::open_with(path, &OpenOptions::default())
    }

    /// Opens the database at `path` with explicit options. A database written by a newer
    /// version is refused with `LedgerError::SchemaTooNew`.
    pub fn open_with(path: &Path, opts: &OpenOptions) -> Result<SqliteStore> {
        Self::init(Connection::open(path)?, opts)
    }

    /// Opens a private in-memory database.
    pub fn open_in_memory() -> Result<SqliteStore> {
        Self::init(Connection::open_in_memory()?, &OpenOptions::default())
    }

    fn init(mut conn: Connection, opts: &OpenOptions) -> Result<SqliteStore> {
        conn.busy_timeout(opts.busy_timeout)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&mut conn, MIGRATIONS)?;
        Ok(SqliteStore {
            conn: Mutex::new(conn),
            clock: opts.clock.clone(),
        })
    }

    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| LedgerError::Poisoned)
    }

    /// Runs `f` in one short immediate transaction and commits when it returns `Ok`.
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Transaction<'_>, i64) -> Result<T>) -> Result<T> {
        let now = self.clock.now_ms();
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx, now)?;
        tx.commit()?;
        Ok(out)
    }

    /// Read-modify-write of one job inside the write lock, so a stale reader can never
    /// overwrite a concurrent change (R-006, R-007). Writes only when the record changed.
    fn modify<T>(&self, id: JobId, f: impl FnOnce(&mut JobRecord, i64) -> Result<T>) -> Result<T> {
        self.write(|tx, now| {
            let mut r = get_on(tx, id)?.ok_or(LedgerError::NotFound(id))?;
            let before = r.clone();
            let out = f(&mut r, now)?;
            if r != before {
                write_record(tx, &r)?;
            }
            Ok(out)
        })
    }
}

const COLS: &str = "id, spec, hints, plan_id, step_name, phase, wait_text, submitted_at_ms, \
    started_at_ms, finished_at_ms, machine, exit_code, signal, timeout, summary, lease_pgid, \
    lease_start, log_path, fwd_peer, fwd_job, remote_phase, cancel_by, cancel_at_ms, fwd_key, \
    origin_peer, origin_key, cancel_reason, figures";

struct Raw {
    id: i64,
    spec: String,
    hints: String,
    plan_id: Option<i64>,
    step_name: Option<String>,
    phase: String,
    wait_text: Option<String>,
    submitted: i64,
    started: Option<i64>,
    finished: Option<i64>,
    machine: Option<String>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    timeout: Option<String>,
    summary: Option<String>,
    lease_pgid: Option<i32>,
    lease_start: Option<i64>,
    log_path: Option<String>,
    fwd_peer: Option<String>,
    fwd_job: Option<i64>,
    remote_phase: Option<String>,
    cancel_by: Option<String>,
    cancel_at: Option<i64>,
    fwd_key: Option<String>,
    origin_peer: Option<String>,
    origin_key: Option<String>,
    cancel_reason: Option<String>,
    figures: Option<String>,
}

fn raw(r: &Row<'_>) -> rusqlite::Result<Raw> {
    Ok(Raw {
        id: r.get(0)?,
        spec: r.get(1)?,
        hints: r.get(2)?,
        plan_id: r.get(3)?,
        step_name: r.get(4)?,
        phase: r.get(5)?,
        wait_text: r.get(6)?,
        submitted: r.get(7)?,
        started: r.get(8)?,
        finished: r.get(9)?,
        machine: r.get(10)?,
        exit_code: r.get(11)?,
        signal: r.get(12)?,
        timeout: r.get(13)?,
        summary: r.get(14)?,
        lease_pgid: r.get(15)?,
        lease_start: r.get(16)?,
        log_path: r.get(17)?,
        fwd_peer: r.get(18)?,
        fwd_job: r.get(19)?,
        remote_phase: r.get(20)?,
        cancel_by: r.get(21)?,
        cancel_at: r.get(22)?,
        fwd_key: r.get(23)?,
        origin_peer: r.get(24)?,
        origin_key: r.get(25)?,
        cancel_reason: r.get(26)?,
        figures: r.get(27)?,
    })
}

fn timeout_text(t: TimeoutReason) -> &'static str {
    match t {
        TimeoutReason::Wall => "wall",
        TimeoutReason::Idle => "idle",
    }
}

fn parse_timeout(s: &str) -> Result<TimeoutReason> {
    match s {
        "wall" => Ok(TimeoutReason::Wall),
        "idle" => Ok(TimeoutReason::Idle),
        _ => Err(LedgerError::OutOfRange("unknown timeout reason")),
    }
}

fn record(r: Raw) -> Result<JobRecord> {
    let lease = match (r.lease_pgid, r.lease_start) {
        (Some(pgid), Some(start)) => Some(LeaseRecord {
            pgid,
            leader_start: u64::try_from(start)
                .map_err(|_| LedgerError::OutOfRange("lease start time"))?,
        }),
        _ => None,
    };
    Ok(JobRecord {
        id: JobId(r.id),
        spec: serde_json::from_str(&r.spec)?,
        hints: serde_json::from_str(&r.hints)?,
        plan_id: r.plan_id,
        step_name: r.step_name,
        phase: serde_json::from_str(&r.phase)?,
        wait_text: r.wait_text,
        submitted_at_ms: r.submitted,
        started_at_ms: r.started,
        finished_at_ms: r.finished,
        machine: r.machine.map(MachineName),
        exit_code: r.exit_code,
        signal: r.signal,
        timeout: r.timeout.as_deref().map(parse_timeout).transpose()?,
        summary: r.summary,
        lease,
        log_path: r.log_path.map(PathBuf::from),
        forward: r.fwd_peer.map(|p| Forward {
            peer: MachineName(p),
            remote_job: r.fwd_job,
            remote_key: r.fwd_key,
        }),
        origin: match (r.origin_peer, r.origin_key) {
            (Some(peer), Some(key)) => Some(Origin { peer, key }),
            _ => None,
        },
        remote_phase: r
            .remote_phase
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?,
        cancel: r.cancel_by.map(|by| CancelRequest {
            by,
            at_ms: r.cancel_at.unwrap_or(0),
            reason: r.cancel_reason,
        }),
        figures: r.figures.as_deref().map(serde_json::from_str).transpose()?,
    })
}

fn get_on(conn: &Connection, id: JobId) -> Result<Option<JobRecord>> {
    let row = conn
        .query_row(
            &format!("SELECT {COLS} FROM jobs WHERE id = ?1"),
            [id.0],
            raw,
        )
        .optional()?;
    row.map(record).transpose()
}

fn query_records(conn: &Connection, sql: &str, args: Vec<Value>) -> Result<Vec<JobRecord>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args), raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(record).collect()
}

fn query_ids(conn: &Connection, sql: &str) -> Result<Vec<JobId>> {
    let mut stmt = conn.prepare(sql)?;
    let ids = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids.into_iter().map(JobId).collect())
}

/// Writes every mutable column of the record.
fn write_record(tx: &Connection, r: &JobRecord) -> Result<()> {
    let (fwd_peer, fwd_job, fwd_key) = match &r.forward {
        Some(f) => (Some(f.peer.0.clone()), f.remote_job, f.remote_key.clone()),
        None => (None, None, None),
    };
    let lease_start = r
        .lease
        .map(|l| i64::try_from(l.leader_start))
        .transpose()
        .map_err(|_| LedgerError::OutOfRange("lease start time"))?;
    tx.execute(
        "UPDATE jobs SET phase = ?1, phase_tag = ?2, wait_text = ?3, started_at_ms = ?4,
             finished_at_ms = ?5, machine = ?6, exit_code = ?7, signal = ?8, timeout = ?9,
             summary = ?10, lease_pgid = ?11, lease_start = ?12, log_path = ?13, fwd_peer = ?14,
             fwd_job = ?15, remote_phase = ?16, cancel_by = ?17, cancel_at_ms = ?18,
             fwd_key = ?19, cancel_reason = ?20, figures = ?21
         WHERE id = ?22",
        params![
            serde_json::to_string(&r.phase)?,
            phase_tag(&r.phase),
            r.wait_text,
            r.started_at_ms,
            r.finished_at_ms,
            r.machine.as_ref().map(|m| m.0.clone()),
            r.exit_code,
            r.signal,
            r.timeout.map(timeout_text),
            r.summary,
            r.lease.map(|l| l.pgid),
            lease_start,
            r.log_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            fwd_peer,
            fwd_job,
            r.remote_phase
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            r.cancel.as_ref().map(|c| c.by.clone()),
            r.cancel.as_ref().map(|c| c.at_ms),
            fwd_key,
            r.cancel.as_ref().and_then(|c| c.reason.clone()),
            r.figures.as_ref().map(serde_json::to_string).transpose()?,
            r.id.0,
        ],
    )?;
    Ok(())
}

/// Validates deps against the database, then inserts the job and its dependency edges.
fn insert_job(tx: &Connection, job: &NewJob, now: i64) -> Result<JobId> {
    job.spec.validate()?;
    let mut phases = BTreeMap::new();
    for d in &job.spec.after {
        let p: Option<String> = tx
            .query_row("SELECT phase FROM jobs WHERE id = ?1", [d.0], |r| r.get(0))
            .optional()?;
        if let Some(p) = p {
            phases.insert(*d, serde_json::from_str::<Phase>(&p)?);
        }
    }
    check_deps(&job.spec.after, |d| phases.get(&d).cloned())?;
    let rec = new_record(JobId(0), job, now);
    tx.execute(
        "INSERT INTO jobs(spec, phase, phase_tag, submitted_at_ms, owner, cwd, argv, hints,
                          plan_id, step_name, origin_peer, origin_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            serde_json::to_string(&job.spec)?,
            serde_json::to_string(&rec.phase)?,
            phase_tag(&rec.phase),
            now,
            job.spec.owner.as_str(),
            job.spec.cwd.to_string_lossy(),
            serde_json::to_string(&job.spec.argv)?,
            serde_json::to_string(&job.hints)?,
            rec.plan_id,
            rec.step_name,
            rec.origin.as_ref().map(|o| o.peer.clone()),
            rec.origin.as_ref().map(|o| o.key.clone()),
        ],
    )
    .map_err(|e| match (&job.origin, &e) {
        (Some(o), rusqlite::Error::SqliteFailure(f, _))
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            LedgerError::DuplicateOrigin {
                peer: o.peer.clone(),
                key: o.key.clone(),
            }
        }
        _ => LedgerError::from(e),
    })?;
    let id = tx.last_insert_rowid();
    for d in &job.spec.after {
        tx.execute(
            "INSERT OR IGNORE INTO job_deps(job, after) VALUES (?1, ?2)",
            [id, d.0],
        )?;
    }
    Ok(JobId(id))
}

fn cache_text(c: Cache) -> &'static str {
    match c {
        Cache::Warm => "warm",
        Cache::Cold => "cold",
        Cache::Unknown => "unknown",
    }
}

fn outcome_text(o: Outcome) -> &'static str {
    match o {
        Outcome::Passed => "passed",
        Outcome::Failed => "failed",
        Outcome::Cancelled => "cancelled",
    }
}

/// Stores a timing sample; `mem_peak` is the peak memory of the job behind a whole-job sample.
fn insert_sample(
    tx: &Connection,
    key: &SampleKey,
    s: &Sample,
    mem_peak: Option<u64>,
) -> Result<()> {
    validate_sample(s)?;
    let at = i64::try_from(s.at_ms).map_err(|_| LedgerError::InvalidSample("timestamp"))?;
    let mem = mem_peak.and_then(|m| i64::try_from(m).ok());
    tx.execute(
        "INSERT INTO samples(recipe, target, machine, secs, cache, outcome, at_ms, mem_peak)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            key.recipe,
            key.target,
            key.machine,
            s.secs,
            cache_text(s.cache),
            outcome_text(s.outcome),
            at,
            mem
        ],
    )?;
    Ok(())
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

impl Store for SqliteStore {
    fn submit_job(&self, job: &NewJob) -> Result<JobId> {
        job.spec.validate()?;
        self.write(|tx, now| insert_job(tx, job, now))
    }

    fn submit_plan(&self, ctx: &PlanContext, plan: &Plan) -> Result<PlanSubmitted> {
        plan.validate()?;
        self.write(|tx, now| {
            tx.execute(
                "INSERT INTO plans(owner, created_at_ms) VALUES (?1, ?2)",
                params![ctx.owner.as_str(), now],
            )?;
            let plan_id = tx.last_insert_rowid();
            let jobs = plan_jobs(ctx, plan, plan_id, |job| insert_job(tx, job, now))?;
            Ok(PlanSubmitted { plan_id, jobs })
        })
    }

    fn get(&self, id: JobId) -> Result<Option<JobRecord>> {
        let conn = self.lock()?;
        get_on(&conn, id)
    }

    fn set_phase(&self, id: JobId, next: Phase) -> Result<()> {
        self.modify(id, |r, now| match &next {
            Phase::Waiting(w) => apply_wait(r, w.clone(), None),
            Phase::Running => apply_start(r, None, now),
            p => apply_finish(r, &Finish::from_phase(p.clone()), now),
        })
    }

    fn list(&self, filter: &ListFilter) -> Result<Vec<JobRecord>> {
        let mut conds: Vec<String> = vec![];
        let mut args: Vec<Value> = vec![];
        if let Some(o) = &filter.owner {
            conds.push("owner = ?".into());
            args.push(Value::Text(o.clone()));
        }
        if let Some(i) = filter.id {
            conds.push("id = ?".into());
            args.push(Value::Integer(i.0));
        }
        if filter.active_only {
            conds.push(format!("phase_tag IN {ACTIVE}"));
        }
        if let Some(n) = filter.recent_finished {
            conds.push(format!(
                "(phase_tag IN {ACTIVE} OR id IN (SELECT id FROM jobs WHERE phase_tag NOT IN \
                 {ACTIVE} ORDER BY finished_at_ms DESC, id DESC LIMIT ?))"
            ));
            args.push(Value::Integer(i64::try_from(n).unwrap_or(i64::MAX)));
        }
        let wh = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        let limit = filter
            .limit
            .map_or(-1, |n| i64::try_from(n).unwrap_or(i64::MAX));
        args.push(Value::Integer(limit));
        let conn = self.lock()?;
        query_records(
            &conn,
            &format!("SELECT {COLS} FROM jobs {wh} ORDER BY id LIMIT ?"),
            args,
        )
    }

    fn ready(&self) -> Result<Vec<JobId>> {
        let conn = self.lock()?;
        query_ids(
            &conn,
            "SELECT j.id FROM jobs j WHERE j.phase_tag IN ('queued','waiting')
               AND NOT EXISTS (SELECT 1 FROM job_deps d LEFT JOIN jobs p ON p.id = d.after
                               WHERE d.job = j.id AND p.phase_tag IS NOT 'passed')
             ORDER BY j.id",
        )
    }

    fn blocked_by_failure(&self) -> Result<Vec<JobId>> {
        let conn = self.lock()?;
        query_ids(
            &conn,
            "SELECT j.id FROM jobs j WHERE j.phase_tag IN ('queued','waiting')
               AND EXISTS (SELECT 1 FROM job_deps d JOIN jobs p ON p.id = d.after
                           WHERE d.job = j.id
                             -- Same rule as `dep_failed`: terminal and not passed.
                             AND p.phase_tag NOT IN ('queued','waiting','running','passed'))
             ORDER BY j.id",
        )
    }

    fn set_wait(&self, id: JobId, reason: WaitReason, text: &str) -> Result<()> {
        self.modify(id, |r, _| apply_wait(r, reason, Some(text.to_string())))
    }

    fn start(&self, id: JobId, info: &StartInfo) -> Result<()> {
        self.modify(id, |r, now| apply_start(r, Some(info), now))
    }

    fn finish_recording(
        &self,
        id: JobId,
        outcome: &Finish,
        sample: Option<(&SampleKey, &Sample)>,
    ) -> Result<()> {
        if let Some((_, s)) = sample {
            validate_sample(s)?;
        }
        self.write(|tx, now| {
            let mut r = get_on(tx, id)?.ok_or(LedgerError::NotFound(id))?;
            apply_finish(&mut r, outcome, now)?;
            write_record(tx, &r)?;
            if let Some((k, s)) = sample {
                // the job's final figures are already in its row (written before the finish)
                let peak = r.figures.as_ref().and_then(|f| f.mem_peak_bytes);
                insert_sample(tx, k, s, k.target.is_none().then_some(peak).flatten())?;
            }
            Ok(())
        })
    }

    fn request_cancel_with(
        &self,
        id: JobId,
        by: &str,
        reason: Option<&str>,
    ) -> Result<CancelOutcome> {
        self.modify(id, |r, now| Ok(apply_cancel(r, by, reason, now)))
    }

    fn set_forward(
        &self,
        id: JobId,
        peer: &MachineName,
        remote_job: Option<i64>,
        remote_key: Option<&str>,
    ) -> Result<()> {
        self.modify(id, |r, _| {
            let key = remote_key
                .map(str::to_string)
                .or_else(|| r.forward.as_ref().and_then(|f| f.remote_key.clone()));
            r.forward = Some(Forward {
                peer: peer.clone(),
                remote_job,
                remote_key: key,
            });
            Ok(())
        })
    }

    fn clear_forward(&self, id: JobId) -> Result<()> {
        self.modify(id, |r, _| {
            r.forward = None;
            Ok(())
        })
    }

    fn find_origin(&self, peer: &str, key: &str) -> Result<Option<JobId>> {
        let conn = self.lock()?;
        Ok(conn
            .query_row(
                "SELECT id FROM jobs WHERE origin_peer = ?1 AND origin_key = ?2",
                params![peer, key],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(JobId))
    }

    fn set_remote_phase(&self, id: JobId, phase: &Phase) -> Result<()> {
        self.modify(id, |r, _| {
            r.remote_phase = Some(phase.clone());
            Ok(())
        })
    }

    fn set_log_path(&self, id: JobId, path: &Path) -> Result<()> {
        self.modify(id, |r, _| {
            apply_log_path(r, path.to_path_buf());
            Ok(())
        })
    }

    fn set_figures(&self, id: JobId, figures: &JobFigures) -> Result<()> {
        self.modify(id, |r, _| {
            r.figures = Some(figures.clone());
            Ok(())
        })
    }

    fn find_running(&self) -> Result<Vec<JobRecord>> {
        let conn = self.lock()?;
        query_records(
            &conn,
            &format!("SELECT {COLS} FROM jobs WHERE phase_tag = 'running' ORDER BY id"),
            vec![],
        )
    }

    fn set_priority(
        &self,
        id: JobId,
        priority: Priority,
        reason: Option<&str>,
    ) -> Result<Priority> {
        self.write(|tx, _| {
            let mut r = get_on(tx, id)?.ok_or(LedgerError::NotFound(id))?;
            let before = apply_priority(&mut r, priority, reason)?;
            // the class lives in the spec column, which `write_record` leaves alone
            tx.execute(
                "UPDATE jobs SET spec = ?1 WHERE id = ?2",
                params![serde_json::to_string(&r.spec)?, id.0],
            )?;
            Ok(before)
        })
    }

    fn find_recent_spec(
        &self,
        owner: &str,
        cwd: &Path,
        argv: &[String],
        within: Duration,
    ) -> Result<Vec<JobRecord>> {
        let since = self.clock.now_ms().saturating_sub(millis(within));
        let conn = self.lock()?;
        let found = query_records(
            &conn,
            &format!(
                "SELECT {COLS} FROM jobs WHERE owner = ? AND cwd = ? AND submitted_at_ms >= ?
                 ORDER BY id"
            ),
            vec![
                Value::Text(owner.to_string()),
                Value::Text(cwd.to_string_lossy().into_owned()),
                Value::Integer(since),
            ],
        )?;
        Ok(found
            .into_iter()
            .filter(|r| r.spec.argv == argv && r.spec.cwd == cwd)
            .collect())
    }

    fn record_sample(&self, key: &SampleKey, sample: &Sample) -> Result<()> {
        validate_sample(sample)?;
        self.write(|tx, _| insert_sample(tx, key, sample, None))
    }

    fn sample_keys(&self, recipes: &[String], limit: usize) -> Result<Vec<SampleKey>> {
        let conn = self.lock()?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut out: Vec<SampleKey> = Vec::new();
        // NULL sorts first in SQLite, which puts the whole-job series before its targets
        let sql = if recipes.is_empty() {
            "SELECT DISTINCT recipe, machine, target FROM samples
             ORDER BY recipe, machine, target LIMIT ?1"
        } else {
            "SELECT DISTINCT recipe, machine, target FROM samples WHERE recipe = ?2
             ORDER BY recipe, machine, target LIMIT ?1"
        };
        let mut stmt = conn.prepare(sql)?;
        let mut collect = |rows: rusqlite::Rows<'_>| -> Result<()> {
            let mut rows = rows;
            while let Some(r) = rows.next()? {
                out.push(SampleKey {
                    recipe: r.get(0)?,
                    machine: r.get(1)?,
                    target: r.get(2)?,
                });
            }
            Ok(())
        };
        if recipes.is_empty() {
            collect(stmt.query(params![limit])?)?;
        } else {
            for recipe in recipes {
                collect(stmt.query(params![limit, recipe])?)?;
            }
        }
        out.sort_by(|a, b| {
            (&a.recipe, &a.machine, &a.target).cmp(&(&b.recipe, &b.machine, &b.target))
        });
        out.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(out)
    }

    fn prune(&self, policy: &RetentionPolicy) -> Result<PruneReport> {
        self.write(|tx, now| {
            let finished = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT id, COALESCE(finished_at_ms, 0) FROM jobs
                     WHERE phase_tag NOT IN {ACTIVE}"
                ))?;
                stmt.query_map([], |r| {
                    Ok(PruneRow {
                        id: r.get(0)?,
                        finished_at_ms: r.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let protected: BTreeSet<i64> = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT DISTINCT d.after FROM job_deps d JOIN jobs j ON j.id = d.job
                     WHERE j.phase_tag IN {ACTIVE}"
                ))?;
                stmt.query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            let newest: i64 =
                tx.query_row("SELECT COALESCE(MAX(id), 0) FROM jobs", [], |r| r.get(0))?;
            let victims = select_prune(&finished, &protected, newest, policy, now);
            for id in &victims {
                tx.execute("DELETE FROM job_deps WHERE job = ?1 OR after = ?1", [id])?;
                tx.execute("DELETE FROM jobs WHERE id = ?1", [id])?;
            }
            tx.execute(
                "DELETE FROM plans WHERE id NOT IN
                 (SELECT plan_id FROM jobs WHERE plan_id IS NOT NULL)",
                [],
            )?;
            let mut samples = 0;
            if let Some(age) = policy.max_sample_age {
                samples += tx.execute(
                    "DELETE FROM samples WHERE at_ms <= ?1",
                    [now.saturating_sub(millis(age))],
                )?;
            }
            if let Some(n) = policy.max_samples_per_key {
                samples += tx.execute(
                    "DELETE FROM samples WHERE id IN (
                       SELECT id FROM (
                         SELECT id, ROW_NUMBER() OVER (
                           PARTITION BY recipe, target, machine ORDER BY at_ms DESC, id DESC) AS rn
                         FROM samples) WHERE rn > ?1)",
                    [i64::try_from(n).unwrap_or(i64::MAX)],
                )?;
            }
            Ok(PruneReport {
                jobs: victims.len(),
                samples,
            })
        })
    }

    fn change_token(&self) -> Result<ChangeToken> {
        let conn = self.lock()?;
        let dv: i64 = conn.query_row("PRAGMA data_version", [], |r| r.get(0))?;
        Ok(ChangeToken(
            u64::try_from(dv).unwrap_or(0),
            conn.total_changes(),
        ))
    }
}

fn hist_err(e: impl std::fmt::Display) -> TimingError {
    TimingError::History(e.to_string())
}

fn parse_cache(s: &str) -> std::result::Result<Cache, TimingError> {
    match s {
        "warm" => Ok(Cache::Warm),
        "cold" => Ok(Cache::Cold),
        "unknown" => Ok(Cache::Unknown),
        other => Err(hist_err(format!("bad cache value {other:?}"))),
    }
}

fn parse_outcome(s: &str) -> std::result::Result<Outcome, TimingError> {
    match s {
        "passed" => Ok(Outcome::Passed),
        "failed" => Ok(Outcome::Failed),
        "cancelled" => Ok(Outcome::Cancelled),
        other => Err(hist_err(format!("bad outcome value {other:?}"))),
    }
}

impl History for SqliteStore {
    fn samples(&self, key: &SampleKey) -> std::result::Result<Vec<Sample>, TimingError> {
        let conn = self.lock().map_err(hist_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT secs, cache, outcome, at_ms FROM samples
                 WHERE recipe = ?1 AND machine = ?2 AND target IS ?3 ORDER BY id",
            )
            .map_err(hist_err)?;
        let rows = stmt
            .query_map(params![key.recipe, key.machine, key.target], |r| {
                Ok((
                    r.get::<_, f64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(hist_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(hist_err)?;
        rows.into_iter()
            .map(|(secs, cache, outcome, at)| {
                Ok(Sample {
                    secs,
                    cache: parse_cache(&cache)?,
                    outcome: parse_outcome(&outcome)?,
                    at_ms: u64::try_from(at).map_err(hist_err)?,
                })
            })
            .collect()
    }

    fn recent_peaks(&self, key: &SampleKey) -> std::result::Result<Vec<Option<u64>>, TimingError> {
        let conn = self.lock().map_err(hist_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT mem_peak FROM samples
                 WHERE recipe = ?1 AND machine = ?2 AND target IS ?3 AND outcome != 'cancelled'
                 ORDER BY id DESC LIMIT ?4",
            )
            .map_err(hist_err)?;
        let limit = i64::try_from(tasks_timing::history::RECENT_PEAKS).unwrap_or(3);
        let rows = stmt
            .query_map(params![key.recipe, key.machine, key.target, limit], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .map_err(hist_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(hist_err)?;
        Ok(rows
            .into_iter()
            .map(|m| m.and_then(|m| u64::try_from(m).ok()))
            .collect())
    }

    fn targets(&self, recipe: &str) -> std::result::Result<Vec<String>, TimingError> {
        let conn = self.lock().map_err(hist_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT target FROM samples
                 WHERE recipe = ?1 AND target IS NOT NULL ORDER BY target",
            )
            .map_err(hist_err)?;
        stmt.query_map([recipe], |r| r.get::<_, String>(0))
            .map_err(hist_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(hist_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::SCHEMA_VERSION;
    use crate::retry::{RetryPolicy, retry_busy};
    use crate::suite::Env;
    use crate::{LeaseRecord, ManualClock};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tasks_core::{Kind, Owner, Placement, Requirements};

    static N: AtomicU32 = AtomicU32::new(0);

    /// A unique database path whose files are removed on drop.
    struct TempDb(PathBuf);

    impl TempDb {
        fn new() -> TempDb {
            let n = N.fetch_add(1, Ordering::SeqCst);
            TempDb(std::env::temp_dir().join(format!("tasks-ledger-{}-{n}.db", std::process::id())))
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", self.0.display()));
            }
        }
    }

    fn opts(clock: &Arc<ManualClock>, busy_ms: u64) -> OpenOptions {
        OpenOptions {
            busy_timeout: Duration::from_millis(busy_ms),
            clock: clock.clone(),
        }
    }

    fn spec(owner: &str) -> tasks_core::JobSpec {
        tasks_core::JobSpec {
            owner: Owner::new(owner).unwrap(),
            argv: vec!["cargo".into(), "test".into()],
            cwd: PathBuf::from("/work"),
            env: vec![],
            kind: Kind::Test,
            requirements: Requirements::default(),
            placement: Placement::Anywhere,
            after: vec![],
            timeout_secs: None,
            broad_test_escape: false,
            priority: tasks_core::Priority::Normal,
            priority_reason: None,
            fetch: Vec::new(),
            isolate: false,
            light: false,
            retry_alone: false,
        }
    }

    fn start_info() -> StartInfo {
        StartInfo {
            machine: MachineName("m".into()),
            lease: LeaseRecord {
                pgid: 1,
                leader_start: 2,
            },
            log_path: None,
        }
    }

    #[test]
    fn sqlite_store_suite() {
        crate::suite::run_all(&|| {
            let db = TempDb::new();
            let clock = Arc::new(ManualClock::new(1_000_000));
            let store = SqliteStore::open_with(&db.0, &opts(&clock, 5000)).unwrap();
            Env {
                store: Box::new(store),
                clock,
                _guard: Box::new(db),
            }
        });
    }

    #[test]
    fn schema_version_and_wal_are_set() {
        let db = TempDb::new();
        let store = SqliteStore::open(&db.0).unwrap();
        let conn = store.lock().unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let busy: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(busy, 5000);
    }

    #[test]
    fn a_newer_database_is_refused_on_open() {
        let db = TempDb::new();
        drop(SqliteStore::open(&db.0).unwrap());
        Connection::open(&db.0)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        match SqliteStore::open(&db.0) {
            Err(LedgerError::SchemaTooNew { found, supported }) => {
                assert_eq!((found, supported), (SCHEMA_VERSION + 1, SCHEMA_VERSION));
            }
            other => panic!("expected SchemaTooNew, got {other:?}"),
        }
    }

    #[test]
    fn a_version_one_database_migrates_with_its_rows() {
        let db = TempDb::new();
        {
            let c = Connection::open(&db.0).unwrap();
            c.execute_batch(MIGRATIONS[0]).unwrap();
            c.pragma_update(None, "user_version", 1).unwrap();
            let s = serde_json::to_string(&spec("old")).unwrap();
            for (id, phase) in [
                (1, "\"Passed\""),
                (2, "{\"Failed\":{\"exit\":3}}"),
                (3, "\"Queued\""),
            ] {
                c.execute(
                    "INSERT INTO jobs(id, spec, phase, submitted_at_ms) VALUES (?1, ?2, ?3, 500)",
                    params![id, s, phase],
                )
                .unwrap();
            }
        }
        let clock = Arc::new(ManualClock::new(1000));
        let store = SqliteStore::open_with(&db.0, &opts(&clock, 1000)).unwrap();
        let passed = store.get(JobId(1)).unwrap().unwrap();
        assert_eq!((passed.phase, passed.exit_code), (Phase::Passed, Some(0)));
        assert_eq!(passed.spec.owner.as_str(), "old");
        let all = store.list(&ListFilter::default()).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(store.ready().unwrap(), vec![JobId(3)]);
        let active = ListFilter {
            active_only: true,
            ..ListFilter::default()
        };
        assert_eq!(store.list(&active).unwrap().len(), 1);
        let argv = vec!["cargo".to_string(), "test".to_string()];
        let found = store
            .find_recent_spec("old", Path::new("/work"), &argv, Duration::from_secs(60))
            .unwrap();
        assert_eq!(found.len(), 3);
        assert!(store.submit(&spec("new")).unwrap() > JobId(3));
    }

    // R-152, R-173: the (peer, key) pair of a forward and the origin of a received job survive a
    // restart, and a version-2 database migrates with its forwarded rows keeping no key.
    #[test]
    fn r152_forward_key_and_origin_survive_reopening_and_a_v2_database_migrates() {
        let db = TempDb::new();
        let clock = Arc::new(ManualClock::new(10));
        let peer = MachineName("nixos".into());
        let (fwd, recv) = {
            let s = SqliteStore::open_with(&db.0, &opts(&clock, 1000)).unwrap();
            let fwd = s.submit(&spec("a")).unwrap();
            s.set_forward(fwd, &peer, None, Some("laptop:1")).unwrap();
            let mut job = NewJob::new(spec("remote"));
            job.origin = Some(crate::Origin {
                peer: "p".into(),
                key: "k".into(),
            });
            (fwd, s.submit_job(&job).unwrap())
        };
        let s = SqliteStore::open_with(&db.0, &opts(&clock, 1000)).unwrap();
        let f = s.get(fwd).unwrap().unwrap().forward.unwrap();
        assert_eq!((f.peer, f.remote_key.as_deref()), (peer, Some("laptop:1")));
        assert_eq!(s.find_origin("p", "k").unwrap(), Some(recv));
        drop(s);

        let old = TempDb::new();
        {
            let c = Connection::open(&old.0).unwrap();
            c.execute_batch(MIGRATIONS[0]).unwrap();
            c.execute_batch(MIGRATIONS[1]).unwrap();
            c.pragma_update(None, "user_version", 2).unwrap();
            let js = serde_json::to_string(&spec("old")).unwrap();
            c.execute(
                "INSERT INTO jobs(id, spec, phase, submitted_at_ms, fwd_peer, fwd_job)
                 VALUES (1, ?1, '\"Running\"', 5, 'nixos', 7)",
                params![js],
            )
            .unwrap();
        }
        let s = SqliteStore::open_with(&old.0, &opts(&clock, 1000)).unwrap();
        let f = s.get(JobId(1)).unwrap().unwrap().forward.unwrap();
        assert_eq!((f.remote_job, f.remote_key), (Some(7), None));
        s.set_forward(JobId(1), &MachineName("nixos".into()), Some(7), Some("k"))
            .unwrap();
        assert_eq!(
            s.get(JobId(1))
                .unwrap()
                .unwrap()
                .forward
                .unwrap()
                .remote_key,
            Some("k".into())
        );
    }

    // R-012: the queue survives reopening the ledger.
    #[test]
    fn r012_queue_is_fifo_and_survives_reopening() {
        let db = TempDb::new();
        let clock = Arc::new(ManualClock::new(10));
        let made: Vec<JobId> = {
            let s = SqliteStore::open_with(&db.0, &opts(&clock, 1000)).unwrap();
            let ids = (0..4).map(|_| s.submit(&spec("a")).unwrap()).collect();
            s.start(JobId(1), &start_info()).unwrap();
            ids
        };
        let s = SqliteStore::open_with(&db.0, &opts(&clock, 1000)).unwrap();
        let listed: Vec<JobId> = s
            .list(&ListFilter::default())
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(listed, made);
        assert_eq!(s.find_running().unwrap().len(), 1);
        assert_eq!(s.find_running().unwrap()[0].lease.unwrap().pgid, 1);
        assert_eq!(s.submit(&spec("a")).unwrap(), JobId(5));
    }

    // R-001, R-004: a locked ledger is a busy error (unknown), never a cancel, and a retry lands
    // the write once the lock is released.
    #[test]
    fn r001_busy_ledger_is_an_error_not_a_cancellation() {
        let db = TempDb::new();
        let clock = Arc::new(ManualClock::new(10));
        let s = SqliteStore::open_with(&db.0, &opts(&clock, 0)).unwrap();
        let id = s.submit(&spec("a")).unwrap();
        s.start(id, &start_info()).unwrap();

        let blocker = Connection::open(&db.0).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let err = s.request_cancel(id, "u").unwrap_err();
        assert!(err.is_busy(), "{err:?}");
        let err = s
            .finish(id, &Finish::from_phase(Phase::Passed))
            .unwrap_err();
        assert!(err.is_busy());
        // Reads still work in WAL mode and show no cancellation.
        assert!(!s.cancel_requested(id).unwrap());
        assert_eq!(s.get(id).unwrap().unwrap().phase, Phase::Running);

        let tries = std::cell::Cell::new(0);
        let released = std::cell::RefCell::new(Some(blocker));
        let done = retry_busy(
            &RetryPolicy::default(),
            &|_| {
                tries.set(tries.get() + 1);
                if let Some(b) = released.borrow_mut().take() {
                    b.execute_batch("ROLLBACK").unwrap();
                }
            },
            || s.request_cancel(id, "u"),
        )
        .unwrap();
        assert_eq!(done, CancelOutcome::Requested);
        assert_eq!(tries.get(), 1);
        assert!(s.cancel_requested(id).unwrap());
    }

    // R-002: another connection sees a committed cancel on its very next read.
    #[test]
    fn r002_real_cancel_is_seen_on_the_first_poll() {
        let db = TempDb::new();
        let clock = Arc::new(ManualClock::new(10));
        let a = SqliteStore::open_with(&db.0, &opts(&clock, 1000)).unwrap();
        let b = SqliteStore::open_with(&db.0, &opts(&clock, 1000)).unwrap();
        let id = a.submit(&spec("a")).unwrap();
        a.start(id, &start_info()).unwrap();
        assert!(!b.cancel_requested(id).unwrap());
        a.request_cancel(id, "u").unwrap();
        assert!(b.cancel_requested(id).unwrap());
    }

    // R-006: two connections: the stale worker's terminal write cannot override a committed cancel.
    #[test]
    fn r006_a_stale_connection_cannot_overwrite_a_cancel() {
        let db = TempDb::new();
        let clock = Arc::new(ManualClock::new(10));
        let worker = SqliteStore::open_with(&db.0, &opts(&clock, 1000)).unwrap();
        let cli = SqliteStore::open_with(&db.0, &opts(&clock, 1000)).unwrap();
        let id = worker.submit(&spec("a")).unwrap();
        worker.start(id, &start_info()).unwrap();
        let stale = worker.get(id).unwrap().unwrap();
        assert!(!stale.cancel_requested());
        cli.request_cancel(id, "alice").unwrap();
        worker
            .finish(id, &Finish::from_phase(Phase::Passed))
            .unwrap();
        let r = cli.get(id).unwrap().unwrap();
        assert_eq!(r.phase, Phase::Cancelled);
        assert_eq!(r.cancel.unwrap().by, "alice");
    }

    // R-025: the token moves when another connection commits, and only then.
    #[test]
    fn r025_reloads_only_when_this_or_another_connection_commits() {
        let db = TempDb::new();
        let clock = Arc::new(ManualClock::new(10));
        let a = SqliteStore::open_with(&db.0, &opts(&clock, 1000)).unwrap();
        let b = SqliteStore::open_with(&db.0, &opts(&clock, 1000)).unwrap();
        let t0 = a.change_token().unwrap();
        assert_eq!(a.change_token().unwrap(), t0);
        b.submit(&spec("a")).unwrap();
        let t1 = a.change_token().unwrap();
        assert_ne!(t1, t0);
        assert_eq!(a.change_token().unwrap(), t1);
        a.submit(&spec("a")).unwrap();
        assert_ne!(a.change_token().unwrap(), t1);
    }

    #[test]
    fn samples_survive_reopen_and_none_target_is_distinct() {
        let db = TempDb::new();
        let k = SampleKey {
            recipe: "r".into(),
            target: None,
            machine: "m".into(),
        };
        let mut kt = k.clone();
        kt.target = Some("test:a".into());
        let s = Sample {
            secs: 2.5,
            cache: Cache::Cold,
            outcome: Outcome::Failed,
            at_ms: 42,
        };
        drop({
            let st = SqliteStore::open(&db.0).unwrap();
            st.record_sample(&k, &s).unwrap();
            st
        });
        let st = SqliteStore::open(&db.0).unwrap();
        assert_eq!(st.samples(&k).unwrap(), vec![s]);
        assert!(st.samples(&kt).unwrap().is_empty());
        assert_eq!(st.targets("r").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn in_memory_opens() {
        assert!(SqliteStore::open_in_memory().is_ok());
    }
}
