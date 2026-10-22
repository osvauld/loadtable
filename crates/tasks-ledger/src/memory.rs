//! In-memory store, used by tests and as a reference implementation.

use crate::logic::{
    apply_cancel, apply_finish, apply_log_path, apply_start, apply_wait, check_deps, new_record,
    validate_sample,
};
use crate::record::dep_failed;
use crate::request::{PruneRow, select_prune};
use crate::{
    CancelOutcome, ChangeToken, Clock, Finish, Forward, JobRecord, LedgerError, ListFilter, NewJob,
    PlanContext, PlanSubmitted, PruneReport, Result, RetentionPolicy, StartInfo, Store,
    SystemClock,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tasks_core::plan::Plan;
use tasks_core::{JobId, MachineName, Phase, WaitReason};
use tasks_timing::{History, Sample, SampleKey, TimingError};

#[derive(Debug, Clone, Default)]
struct Inner {
    jobs: BTreeMap<i64, JobRecord>,
    last_id: i64,
    last_plan: i64,
    samples: Vec<(SampleKey, Sample)>,
    changes: u64,
}

/// A `Store` backed by a mutex-protected map.
pub struct MemoryStore {
    inner: Mutex<Inner>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore").finish_non_exhaustive()
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore::new()
    }
}

impl MemoryStore {
    /// Creates an empty store on the system clock.
    pub fn new() -> MemoryStore {
        MemoryStore::with_clock(Arc::new(SystemClock))
    }

    /// Creates an empty store on `clock`.
    pub fn with_clock(clock: Arc<dyn Clock>) -> MemoryStore {
        MemoryStore {
            inner: Mutex::new(Inner::default()),
            clock,
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        self.inner.lock().map_err(|_| LedgerError::Poisoned)
    }
}

impl Inner {
    fn job(&mut self, id: JobId) -> Result<&mut JobRecord> {
        self.jobs.get_mut(&id.0).ok_or(LedgerError::NotFound(id))
    }

    fn phase_of(&self, id: JobId) -> Option<Phase> {
        self.jobs.get(&id.0).map(|j| j.phase.clone())
    }

    fn insert(&mut self, job: &NewJob, now: i64) -> JobId {
        self.last_id += 1;
        let id = JobId(self.last_id);
        self.jobs.insert(self.last_id, new_record(id, job, now));
        id
    }

    /// Every dependency has passed (a missing one has not).
    fn deps_passed(&self, r: &JobRecord) -> bool {
        r.spec.after.iter().all(|d| {
            self.jobs
                .get(&d.0)
                .is_some_and(|j| j.phase == Phase::Passed)
        })
    }

    fn deps_failed(&self, r: &JobRecord) -> bool {
        r.spec
            .after
            .iter()
            .any(|d| self.jobs.get(&d.0).is_some_and(|j| dep_failed(&j.phase)))
    }

    fn unstarted(&self) -> impl Iterator<Item = &JobRecord> {
        self.jobs
            .values()
            .filter(|r| matches!(r.phase, Phase::Queued | Phase::Waiting(_)))
    }
}

impl Store for MemoryStore {
    fn submit_job(&self, job: &NewJob) -> Result<JobId> {
        job.spec.validate()?;
        let mut s = self.lock()?;
        check_deps(&job.spec.after, |d| s.phase_of(d))?;
        if let Some(o) = &job.origin
            && s.jobs.values().any(|j| j.origin.as_ref() == Some(o))
        {
            return Err(LedgerError::DuplicateOrigin {
                peer: o.peer.clone(),
                key: o.key.clone(),
            });
        }
        let id = s.insert(job, self.clock.now_ms());
        s.changes += 1;
        Ok(id)
    }

    fn submit_plan(&self, ctx: &PlanContext, plan: &Plan) -> Result<PlanSubmitted> {
        plan.validate()?;
        let now = self.clock.now_ms();
        let mut s = self.lock()?;
        // Work on a copy so a refused step leaves nothing behind.
        let mut draft = s.clone();
        draft.last_plan += 1;
        let plan_id = draft.last_plan;
        let ids = crate::logic::plan_jobs(ctx, plan, plan_id, |job| {
            job.spec.validate()?;
            Ok(draft.insert(job, now))
        })?;
        draft.changes += 1;
        *s = draft;
        Ok(PlanSubmitted { plan_id, jobs: ids })
    }

    fn get(&self, id: JobId) -> Result<Option<JobRecord>> {
        Ok(self.lock()?.jobs.get(&id.0).cloned())
    }

    fn set_phase(&self, id: JobId, next: Phase) -> Result<()> {
        let now = self.clock.now_ms();
        let mut s = self.lock()?;
        let mut r = s.job(id)?.clone();
        match &next {
            Phase::Waiting(w) => apply_wait(&mut r, w.clone(), None)?,
            Phase::Running => apply_start(&mut r, None, now)?,
            p => apply_finish(&mut r, &Finish::from_phase(p.clone()), now)?,
        }
        s.jobs.insert(id.0, r);
        s.changes += 1;
        Ok(())
    }

    fn list(&self, filter: &ListFilter) -> Result<Vec<JobRecord>> {
        let s = self.lock()?;
        let all: Vec<JobRecord> = s.jobs.values().cloned().collect();
        Ok(filter.select(&all))
    }

    fn ready(&self) -> Result<Vec<JobId>> {
        let s = self.lock()?;
        Ok(s.unstarted()
            .filter(|r| s.deps_passed(r))
            .map(|r| r.id)
            .collect())
    }

    fn blocked_by_failure(&self) -> Result<Vec<JobId>> {
        let s = self.lock()?;
        Ok(s.unstarted()
            .filter(|r| s.deps_failed(r))
            .map(|r| r.id)
            .collect())
    }

    fn set_wait(&self, id: JobId, reason: WaitReason, text: &str) -> Result<()> {
        let mut s = self.lock()?;
        let mut r = s.job(id)?.clone();
        apply_wait(&mut r, reason, Some(text.to_string()))?;
        s.jobs.insert(id.0, r);
        s.changes += 1;
        Ok(())
    }

    fn start(&self, id: JobId, info: &StartInfo) -> Result<()> {
        let now = self.clock.now_ms();
        let mut s = self.lock()?;
        let mut r = s.job(id)?.clone();
        apply_start(&mut r, Some(info), now)?;
        s.jobs.insert(id.0, r);
        s.changes += 1;
        Ok(())
    }

    fn finish_recording(
        &self,
        id: JobId,
        outcome: &Finish,
        sample: Option<(&SampleKey, &Sample)>,
    ) -> Result<()> {
        let now = self.clock.now_ms();
        if let Some((_, smp)) = sample {
            validate_sample(smp)?;
        }
        let mut s = self.lock()?;
        let mut r = s.job(id)?.clone();
        apply_finish(&mut r, outcome, now)?;
        s.jobs.insert(id.0, r);
        if let Some((k, smp)) = sample {
            s.samples.push((k.clone(), *smp));
        }
        s.changes += 1;
        Ok(())
    }

    fn request_cancel(&self, id: JobId, by: &str) -> Result<CancelOutcome> {
        let now = self.clock.now_ms();
        let mut s = self.lock()?;
        let out = apply_cancel(s.job(id)?, by, now);
        if !matches!(out, CancelOutcome::AlreadyFinished(_)) {
            s.changes += 1;
        }
        Ok(out)
    }

    fn set_forward(
        &self,
        id: JobId,
        peer: &MachineName,
        remote_job: Option<i64>,
        remote_key: Option<&str>,
    ) -> Result<()> {
        let mut s = self.lock()?;
        let job = s.job(id)?;
        let key = remote_key
            .map(str::to_string)
            .or_else(|| job.forward.as_ref().and_then(|f| f.remote_key.clone()));
        job.forward = Some(Forward {
            peer: peer.clone(),
            remote_job,
            remote_key: key,
        });
        s.changes += 1;
        Ok(())
    }

    fn clear_forward(&self, id: JobId) -> Result<()> {
        let mut s = self.lock()?;
        s.job(id)?.forward = None;
        s.changes += 1;
        Ok(())
    }

    fn find_origin(&self, peer: &str, key: &str) -> Result<Option<JobId>> {
        let s = self.lock()?;
        Ok(s.jobs
            .values()
            .find(|j| {
                j.origin
                    .as_ref()
                    .is_some_and(|o| o.peer == peer && o.key == key)
            })
            .map(|j| j.id))
    }

    fn set_remote_phase(&self, id: JobId, phase: &Phase) -> Result<()> {
        let mut s = self.lock()?;
        s.job(id)?.remote_phase = Some(phase.clone());
        s.changes += 1;
        Ok(())
    }

    fn set_log_path(&self, id: JobId, path: &Path) -> Result<()> {
        let mut s = self.lock()?;
        apply_log_path(s.job(id)?, path.to_path_buf());
        s.changes += 1;
        Ok(())
    }

    fn find_recent_spec(
        &self,
        owner: &str,
        cwd: &Path,
        argv: &[String],
        within: Duration,
    ) -> Result<Vec<JobRecord>> {
        let since = self
            .clock
            .now_ms()
            .saturating_sub(i64::try_from(within.as_millis()).unwrap_or(i64::MAX));
        let s = self.lock()?;
        Ok(s.jobs
            .values()
            .filter(|r| {
                r.submitted_at_ms >= since
                    && r.spec.owner.as_str() == owner
                    && r.spec.cwd == cwd
                    && r.spec.argv == argv
            })
            .cloned()
            .collect())
    }

    fn record_sample(&self, key: &SampleKey, sample: &Sample) -> Result<()> {
        validate_sample(sample)?;
        let mut s = self.lock()?;
        s.samples.push((key.clone(), *sample));
        s.changes += 1;
        Ok(())
    }

    fn prune(&self, policy: &RetentionPolicy) -> Result<PruneReport> {
        let now = self.clock.now_ms();
        let mut s = self.lock()?;
        let finished: Vec<PruneRow> = s
            .jobs
            .values()
            .filter(|r| r.phase.is_terminal())
            .map(|r| PruneRow {
                id: r.id.0,
                finished_at_ms: r.finished_at_ms.unwrap_or(0),
            })
            .collect();
        let protected: BTreeSet<i64> = s
            .unstarted()
            .chain(s.jobs.values().filter(|r| r.phase == Phase::Running))
            .flat_map(|r| r.spec.after.iter().map(|d| d.0))
            .collect();
        let victims = select_prune(&finished, &protected, s.last_id, policy, now);
        for id in &victims {
            s.jobs.remove(id);
        }
        let before = s.samples.len();
        prune_samples(&mut s.samples, policy, now);
        let report = PruneReport {
            jobs: victims.len(),
            samples: before - s.samples.len(),
        };
        if report != PruneReport::default() {
            s.changes += 1;
        }
        Ok(report)
    }

    fn change_token(&self) -> Result<ChangeToken> {
        Ok(ChangeToken(self.lock()?.changes, 0))
    }
}

fn prune_samples(samples: &mut Vec<(SampleKey, Sample)>, policy: &RetentionPolicy, now: i64) {
    if let Some(age) = policy.max_sample_age {
        let cutoff = now.saturating_sub(i64::try_from(age.as_millis()).unwrap_or(i64::MAX));
        samples.retain(|(_, s)| i64::try_from(s.at_ms).is_ok_and(|t| t > cutoff));
    }
    if let Some(n) = policy.max_samples_per_key {
        let mut by_key: BTreeMap<SampleKey, Vec<usize>> = BTreeMap::new();
        for (i, (k, _)) in samples.iter().enumerate() {
            by_key.entry(k.clone()).or_default().push(i);
        }
        let mut drop = BTreeSet::new();
        for mut idx in by_key.into_values() {
            // Newest first; later insertion wins a tie, like the SQL store's id order.
            idx.sort_by_key(|i| {
                std::cmp::Reverse((samples.get(*i).map_or(0, |(_, s)| s.at_ms), *i))
            });
            drop.extend(idx.into_iter().skip(n));
        }
        let mut i = 0;
        samples.retain(|_| {
            i += 1;
            !drop.contains(&(i - 1))
        });
    }
}

impl History for MemoryStore {
    fn samples(&self, key: &SampleKey) -> std::result::Result<Vec<Sample>, TimingError> {
        let s = self
            .lock()
            .map_err(|e| TimingError::History(e.to_string()))?;
        Ok(s.samples
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| *v)
            .collect())
    }

    fn targets(&self, recipe: &str) -> std::result::Result<Vec<String>, TimingError> {
        let s = self
            .lock()
            .map_err(|e| TimingError::History(e.to_string()))?;
        let set: BTreeSet<&String> = s
            .samples
            .iter()
            .filter(|(k, _)| k.recipe == recipe)
            .filter_map(|(k, _)| k.target.as_ref())
            .collect();
        Ok(set.into_iter().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_suite() {
        crate::suite::run_all(&|| {
            let clock = Arc::new(crate::ManualClock::new(1_000_000));
            crate::suite::Env {
                store: Box::new(MemoryStore::with_clock(clock.clone())),
                clock,
                _guard: Box::new(()),
            }
        });
    }
}
