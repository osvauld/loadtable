//! Dependencies between jobs (`tasks steps`, MCP `submit_steps`): a job may wait for other jobs to
//! pass. The edge list lives in the ledger (`job_deps`), is set after submit and before approval,
//! and only the claim query reads it: an approved job whose dependencies have not all passed is
//! not claimable. When a dependency ends without passing, the waiting job is cancelled.
use crate::{Phase, Result, Store};
use rusqlite::params;

/// Most dependencies one job may list.
pub const MAX_AFTER: usize = 64;

/// SQL condition (on `jobs j`) true when every job `j` waits for has passed.
pub const UNBLOCKED: &str = "NOT EXISTS (SELECT 1 FROM job_deps d JOIN jobs p ON p.id=d.after_id \
     WHERE d.job_id=j.id AND p.phase<>'passed')";

impl Store {
    /// Makes queued job `id` wait for `after`. Dependencies must be older jobs (so there are no
    /// cycles) that exist and have not already ended without passing.
    pub fn set_after(&mut self, id: i64, after: &[i64]) -> Result<()> {
        if after.is_empty() || after.len() > MAX_AFTER {
            return Err(format!("a job waits for 1 to {MAX_AFTER} jobs").into());
        }
        let tx = self
            .0
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let phase: String = tx
            .query_row("SELECT phase FROM jobs WHERE id=?1", [id], |r| r.get(0))
            .map_err(|_| format!("job {id} does not exist"))?;
        if phase != "queued" {
            return Err(format!(
                "job {id} is {phase}; only a queued job can be given dependencies"
            )
            .into());
        }
        let mut seen = std::collections::HashSet::new();
        for &dep in after {
            if dep >= id || dep < 1 {
                return Err(format!("job {id} can only wait for an older job, not {dep}").into());
            }
            if !seen.insert(dep) {
                return Err(format!("job {dep} listed twice").into());
            }
            let dep_phase: String = tx
                .query_row("SELECT phase FROM jobs WHERE id=?1", [dep], |r| r.get(0))
                .map_err(|_| format!("job {dep} does not exist"))?;
            if Phase::from_db(&dep_phase)?.terminal() && dep_phase != "passed" {
                return Err(format!("job {dep} already ended as {dep_phase}").into());
            }
            tx.execute(
                "INSERT INTO job_deps(job_id, after_id) VALUES (?1, ?2)",
                params![id, dep],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The jobs `id` waits for that have not passed yet, as `(job, phase)`.
    pub fn waiting_for(&self, id: i64) -> Result<Vec<(i64, String)>> {
        let mut statement = self.0.prepare(
            "SELECT p.id, p.phase FROM job_deps d JOIN jobs p ON p.id=d.after_id \
             WHERE d.job_id=?1 AND p.phase<>'passed' ORDER BY p.id",
        )?;
        let rows = statement.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Cancels every queued or approved job that waits for a job that failed, was cancelled or
    /// was interrupted. Returns the cancelled ids.
    pub fn cancel_blocked(&mut self) -> Result<Vec<i64>> {
        let ids: Vec<i64> = {
            let mut statement = self.0.prepare(
                "SELECT DISTINCT j.id FROM jobs j JOIN job_deps d ON d.job_id=j.id \
                 JOIN jobs p ON p.id=d.after_id \
                 WHERE j.phase IN ('queued','approved') \
                 AND p.phase IN ('failed','cancelled','interrupted') ORDER BY j.id",
            )?;
            let rows = statement.query_map([], |r| r.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for id in &ids {
            self.cancel(*id)?;
        }
        Ok(ids)
    }
}

#[cfg(test)]
#[path = "job_deps_tests.rs"]
mod tests;
