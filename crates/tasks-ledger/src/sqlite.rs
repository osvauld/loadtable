//! SQLite-backed store.

use crate::record::{now_ms, stamps};
use crate::{JobRecord, LedgerError, ListFilter, Result, Store};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use tasks_core::{JobId, JobSpec, MachineName, Phase};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs(
    id INTEGER PRIMARY KEY,
    spec TEXT NOT NULL,
    phase TEXT NOT NULL,
    submitted_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    machine TEXT
);
CREATE TABLE IF NOT EXISTS job_deps(
    job INTEGER NOT NULL,
    after INTEGER NOT NULL,
    PRIMARY KEY(job, after)
);
";

/// A `Store` backed by a SQLite database in WAL mode.
#[derive(Debug)]
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Opens (creating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<SqliteStore> {
        Self::init(Connection::open(path)?)
    }

    /// Opens a private in-memory database.
    pub fn open_in_memory() -> Result<SqliteStore> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<SqliteStore> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", 1)?;
        Ok(SqliteStore {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| LedgerError::Poisoned)
    }
}

struct Raw {
    id: i64,
    spec: String,
    phase: String,
    submitted: i64,
    started: Option<i64>,
    finished: Option<i64>,
    machine: Option<String>,
}

const COLS: &str = "id, spec, phase, submitted_at_ms, started_at_ms, finished_at_ms, machine";

fn raw(r: &Row<'_>) -> rusqlite::Result<Raw> {
    Ok(Raw {
        id: r.get(0)?,
        spec: r.get(1)?,
        phase: r.get(2)?,
        submitted: r.get(3)?,
        started: r.get(4)?,
        finished: r.get(5)?,
        machine: r.get(6)?,
    })
}

fn record(r: Raw) -> Result<JobRecord> {
    Ok(JobRecord {
        id: JobId(r.id),
        spec: serde_json::from_str(&r.spec)?,
        phase: serde_json::from_str(&r.phase)?,
        submitted_at_ms: r.submitted,
        started_at_ms: r.started,
        finished_at_ms: r.finished,
        machine: r.machine.map(MachineName),
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

fn all_on(conn: &Connection) -> Result<Vec<JobRecord>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM jobs ORDER BY id"))?;
    let rows = stmt
        .query_map([], raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(record).collect()
}

/// Queued job ids whose dependency states satisfy `pick(all_passed, any_failed)`.
fn queued_by_deps(conn: &Connection, pick: fn(bool, bool) -> bool) -> Result<Vec<JobId>> {
    let jobs = all_on(conn)?;
    let phase_of = |id: &JobId| jobs.iter().find(|j| j.id == *id).map(|j| j.phase.clone());
    Ok(jobs
        .iter()
        .filter(|r| r.phase == Phase::Queued)
        .filter(|r| {
            let mut passed = true;
            let mut failed = false;
            for d in &r.spec.after {
                match phase_of(d) {
                    Some(Phase::Passed) => {}
                    Some(p) if p.is_terminal() => {
                        failed = true;
                        passed = false;
                    }
                    _ => passed = false,
                }
            }
            pick(passed, failed)
        })
        .map(|r| r.id)
        .collect())
}

impl Store for SqliteStore {
    fn submit(&self, spec: &JobSpec) -> Result<JobId> {
        spec.validate()?;
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        for d in &spec.after {
            let found: Option<i64> = tx
                .query_row("SELECT id FROM jobs WHERE id = ?1", [d.0], |r| r.get(0))
                .optional()?;
            if found.is_none() {
                return Err(LedgerError::UnknownDependency(*d));
            }
        }
        tx.execute(
            "INSERT INTO jobs(spec, phase, submitted_at_ms) VALUES (?1, ?2, ?3)",
            params![
                serde_json::to_string(spec)?,
                serde_json::to_string(&Phase::Queued)?,
                now_ms()
            ],
        )?;
        let id = tx.last_insert_rowid();
        for d in &spec.after {
            tx.execute(
                "INSERT OR IGNORE INTO job_deps(job, after) VALUES (?1, ?2)",
                [id, d.0],
            )?;
        }
        tx.commit()?;
        Ok(JobId(id))
    }

    fn get(&self, id: JobId) -> Result<Option<JobRecord>> {
        let conn = self.lock()?;
        get_on(&conn, id)
    }

    fn set_phase(&self, id: JobId, next: Phase) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let cur = get_on(&tx, id)?.ok_or(LedgerError::NotFound(id))?;
        if !cur.phase.can_move_to(&next) {
            return Err(LedgerError::IllegalTransition {
                from: cur.phase,
                to: next,
            });
        }
        let (start, finish) = stamps(&next);
        let now = now_ms();
        tx.execute(
            "UPDATE jobs SET phase = ?1,
                 started_at_ms = CASE WHEN ?2 THEN ?4 ELSE started_at_ms END,
                 finished_at_ms = CASE WHEN ?3 THEN ?4 ELSE finished_at_ms END
             WHERE id = ?5",
            params![serde_json::to_string(&next)?, start, finish, now, id.0],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn list(&self, filter: &ListFilter) -> Result<Vec<JobRecord>> {
        let conn = self.lock()?;
        Ok(all_on(&conn)?
            .into_iter()
            .filter(|r| filter.matches(r))
            .take(filter.limit.unwrap_or(usize::MAX))
            .collect())
    }

    fn ready(&self) -> Result<Vec<JobId>> {
        let conn = self.lock()?;
        queued_by_deps(&conn, |passed, _| passed)
    }

    fn blocked_by_failure(&self) -> Result<Vec<JobId>> {
        let conn = self.lock()?;
        queued_by_deps(&conn, |_, failed| failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_store_suite() {
        let path = std::env::temp_dir().join(format!(
            "tasks-ledger-{}-{}.db",
            std::process::id(),
            now_ms()
        ));
        let store = SqliteStore::open(&path).unwrap();
        crate::suite::run(&store);
        let v: i64 = store
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
        drop(store);
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }

    #[test]
    fn in_memory_opens() {
        assert!(SqliteStore::open_in_memory().is_ok());
    }
}
