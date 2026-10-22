//! In-memory store, used by tests and as a reference implementation.

use crate::record::{now_ms, stamps};
use crate::{JobRecord, LedgerError, ListFilter, Result, Store};
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use tasks_core::{JobId, JobSpec, Phase};

/// A `Store` backed by a mutex-protected map.
#[derive(Debug, Default)]
pub struct MemoryStore {
    jobs: Mutex<BTreeMap<i64, JobRecord>>,
}

impl MemoryStore {
    /// Creates an empty store.
    pub fn new() -> MemoryStore {
        MemoryStore::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<i64, JobRecord>>> {
        self.jobs.lock().map_err(|_| LedgerError::Poisoned)
    }
}

fn deps_state(jobs: &BTreeMap<i64, JobRecord>, r: &JobRecord) -> (bool, bool) {
    let mut all_passed = true;
    let mut failed = false;
    for d in &r.spec.after {
        match jobs.get(&d.0).map(|j| &j.phase) {
            Some(Phase::Passed) => {}
            Some(p) if p.is_terminal() => {
                failed = true;
                all_passed = false;
            }
            _ => all_passed = false,
        }
    }
    (all_passed, failed)
}

impl Store for MemoryStore {
    fn submit(&self, spec: &JobSpec) -> Result<JobId> {
        spec.validate()?;
        let mut jobs = self.lock()?;
        if let Some(d) = spec.after.iter().find(|d| !jobs.contains_key(&d.0)) {
            return Err(LedgerError::UnknownDependency(*d));
        }
        let id = jobs.keys().next_back().map_or(1, |k| k + 1);
        jobs.insert(
            id,
            JobRecord {
                id: JobId(id),
                spec: spec.clone(),
                phase: Phase::Queued,
                submitted_at_ms: now_ms(),
                started_at_ms: None,
                finished_at_ms: None,
                machine: None,
            },
        );
        Ok(JobId(id))
    }

    fn get(&self, id: JobId) -> Result<Option<JobRecord>> {
        Ok(self.lock()?.get(&id.0).cloned())
    }

    fn set_phase(&self, id: JobId, next: Phase) -> Result<()> {
        let mut jobs = self.lock()?;
        let r = jobs.get_mut(&id.0).ok_or(LedgerError::NotFound(id))?;
        if !r.phase.can_move_to(&next) {
            return Err(LedgerError::IllegalTransition {
                from: r.phase.clone(),
                to: next,
            });
        }
        let (start, finish) = stamps(&next);
        if start {
            r.started_at_ms = Some(now_ms());
        }
        if finish {
            r.finished_at_ms = Some(now_ms());
        }
        r.phase = next;
        Ok(())
    }

    fn list(&self, filter: &ListFilter) -> Result<Vec<JobRecord>> {
        let jobs = self.lock()?;
        Ok(jobs
            .values()
            .filter(|r| filter.matches(r))
            .take(filter.limit.unwrap_or(usize::MAX))
            .cloned()
            .collect())
    }

    fn ready(&self) -> Result<Vec<JobId>> {
        let jobs = self.lock()?;
        Ok(jobs
            .values()
            .filter(|r| r.phase == Phase::Queued && deps_state(&jobs, r).0)
            .map(|r| r.id)
            .collect())
    }

    fn blocked_by_failure(&self) -> Result<Vec<JobId>> {
        let jobs = self.lock()?;
        Ok(jobs
            .values()
            .filter(|r| r.phase == Phase::Queued && deps_state(&jobs, r).1)
            .map(|r| r.id)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_suite() {
        crate::suite::run(&MemoryStore::new());
    }
}
