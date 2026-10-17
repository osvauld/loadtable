//! Durable execution transitions. Old queued proposals require per-job approval.
#[cfg(test)]
#[path = "execution_build_tests.rs"]
mod build_tests;
use crate::{Job, Phase, Result, Store, catalog};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_ticks: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StageTiming {
    pub stage: String,
    pub wait_seconds: f64,
    pub seconds: Option<f64>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

/// Durable build-only output reference, never a test/install result. Paths are
/// relative to the job directory; provenance is separately hashed and retained.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildArtifact {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub source_digest: String,
    pub configuration: String,
    pub provenance_sha256: String,
}
impl BuildArtifact {
    fn validate(
        &self,
        spec: &crate::JobSpec,
        snapshot: Option<&crate::snapshot::Snapshot>,
    ) -> Result<()> {
        let hex = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
        if self.path != "apk/app-debug.apk"
            || !hex(&self.sha256)
            || !hex(&self.provenance_sha256)
            || self.bytes == 0
            || self.bytes > 512 * 1024 * 1024
            || self.configuration != spec.configuration
            || !snapshot.is_some_and(|s| {
                s.digest == self.source_digest && hex(&s.digest) && s.revision == spec.revision
            })
        {
            return Err("invalid build artifact/source/configuration receipt".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Execution {
    pub approved_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub cancel_requested: bool,
    pub worker: Option<ProcessIdentity>,
    pub supervisor: Option<ProcessIdentity>,
    pub snapshot: Option<crate::snapshot::Snapshot>,
    pub stages: Vec<StageTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_artifact: Option<BuildArtifact>,
    #[serde(default)]
    pub android_trial: bool,
    #[serde(default)]
    pub local_command: bool,
    pub detail: String,
    /// Finished-job measurements (feature/metrics). Optional and additive:
    /// older releases ignore it, rows without it read as unknown warmth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<crate::metrics::JobMetrics>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogTail {
    pub job_id: i64,
    pub text: String,
    pub truncated: bool,
}

impl Phase {
    pub fn from_db(value: &str) -> Result<Self> {
        Ok(serde_json::from_value(serde_json::Value::String(
            value.into(),
        ))?)
    }
    pub fn as_db(&self) -> Result<String> {
        Ok(serde_json::to_value(self)?
            .as_str()
            .ok_or("invalid phase")?
            .into())
    }
    pub fn active(&self) -> bool {
        matches!(
            self,
            Self::Preparing
                | Self::WaitingBuild
                | Self::Building
                | Self::WaitingRun
                | Self::Running
        )
    }
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Passed | Self::Failed | Self::Interrupted | Self::Cancelled
        )
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Queued => "proposal (approval required)",
            Self::Approved => "approved",
            Self::Preparing => "preparing",
            Self::WaitingBuild => "waiting for compile slot",
            Self::Building => "building",
            Self::WaitingRun => "waiting for run slot",
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted (not retried)",
            Self::Cancelled => "cancelled",
        }
    }
}

impl Store {
    pub fn job(&self, id: i64) -> Result<Job> {
        self.select_jobs(Some(id))?
            .pop()
            .ok_or_else(|| "job not found".into())
    }
    pub fn execution(&self, id: i64) -> Result<Option<Execution>> {
        let value: Option<String> = self
            .0
            .query_row("SELECT data FROM executions WHERE job_id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        value
            .map(|v| serde_json::from_str(&v).map_err(Into::into))
            .transpose()
    }
    pub fn approve(&mut self, id: i64) -> Result<()> {
        self.approve_kind(id, false, false)
    }
    /// Explicit opt-in for build-only validation before normal catalog rollout.
    /// Trusted local source/host config, not a sandbox or permission to install.
    pub fn approve_android_trial(&mut self, id: i64) -> Result<()> {
        self.approve_kind(id, true, false)
    }
    pub fn approve_command(&mut self, id: i64) -> Result<()> {
        self.approve_kind(id, false, true)
    }
    fn approve_kind(&mut self, id: i64, trial: bool, command: bool) -> Result<()> {
        let job = self.job(id)?;
        if command {
            crate::local_command::Recipe::from_spec(&job.spec)?;
        } else if trial {
            catalog::validate_android_trial(&job.spec)?;
        } else {
            catalog::validate(&job.spec)?;
        }
        let execution = Execution {
            approved_at: now(),
            started_at: None,
            finished_at: None,
            cancel_requested: false,
            worker: None,
            supervisor: None,
            snapshot: None,
            stages: Vec::new(),
            build_artifact: None,
            android_trial: trial,
            local_command: command,
            metrics: None,
            detail: if command {
                "Explicitly approved trusted local argv/live cwd; NOT a source snapshot or sandbox; command may write to workspace"
            } else if trial {
                "Explicitly approved EXPERIMENTAL Android build-only; trusts local source/host configuration; no install/launch/publish"
            } else {
                "Explicitly approved trusted local Rust source; snapshot captured on worker pickup"
            }.into(),
        };
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE jobs SET phase='approved' WHERE id=?1 AND phase='queued'",
            [id],
        )?;
        if changed != 1 {
            return Err(
                "only queued proposals may be approved; terminal jobs require a new submission"
                    .into(),
            );
        }
        tx.execute(
            "INSERT INTO executions(job_id,data) VALUES (?1,?2)",
            params![id, serde_json::to_string(&execution)?],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn request_cancel(&mut self, id: i64) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: String =
            tx.query_row("SELECT phase FROM jobs WHERE id=?1", [id], |r| r.get(0))?;
        let current = Phase::from_db(&current)?;
        if current.terminal() {
            return Err("terminal jobs cannot be cancelled".into());
        }
        let data: Option<String> = tx
            .query_row("SELECT data FROM executions WHERE job_id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let mut execution: Option<Execution> =
            data.map(|v| serde_json::from_str(&v)).transpose()?;
        if let Some(e) = &mut execution {
            e.cancel_requested = true;
        }
        let phase = if current.active() {
            current
        } else {
            Phase::Cancelled
        };
        if phase == Phase::Cancelled
            && let Some(e) = &mut execution
        {
            e.finished_at = Some(now());
            e.detail = "Cancelled before execution".into();
        }
        tx.execute(
            "UPDATE jobs SET phase=?2 WHERE id=?1",
            params![id, phase.as_db()?],
        )?;
        if let Some(e) = execution {
            tx.execute(
                "UPDATE executions SET data=?2 WHERE job_id=?1",
                params![id, serde_json::to_string(&e)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Oldest approved job, FIFO.
    pub fn claim(&mut self, worker: ProcessIdentity) -> Result<Option<Job>> {
        self.claim_class(worker, true, true)
    }
    /// Oldest approved job of an accepted class, FIFO within it, so the worker can
    /// keep separate capacity for offload clients (`local_command::is_offload`).
    /// Anything that is not a readable offload recipe belongs to the general class.
    pub fn claim_class(
        &mut self,
        worker: ProcessIdentity,
        general: bool,
        offload: bool,
    ) -> Result<Option<Job>> {
        // Only one worker holds worker.lock, but the transaction also protects against CLI cancellation.
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = {
            let mut statement =
                tx.prepare("SELECT id,spec FROM jobs WHERE phase='approved' ORDER BY id")?;
            let mut rows = statement.query([])?;
            let mut found = None;
            while let Some(row) = rows.next()? {
                let accepted = (general && offload) || {
                    let spec: String = row.get(1)?;
                    let is_offload = serde_json::from_str::<crate::JobSpec>(&spec)
                        .is_ok_and(|s| crate::local_command::is_offload(&s));
                    if is_offload { offload } else { general }
                };
                if accepted {
                    found = Some(row.get::<_, i64>(0)?);
                    break;
                }
            }
            found
        };
        let Some(id) = id else { return Ok(None) };
        let data: String =
            tx.query_row("SELECT data FROM executions WHERE job_id=?1", [id], |r| {
                r.get(0)
            })?;
        let mut e: Execution = serde_json::from_str(&data)?;
        e.started_at = Some(now());
        e.worker = Some(worker);
        e.detail = "Capturing bounded suite source snapshot".into();
        tx.execute(
            "UPDATE jobs SET phase='preparing' WHERE id=?1 AND phase='approved'",
            [id],
        )?;
        tx.execute(
            "UPDATE executions SET data=?2 WHERE job_id=?1",
            params![id, serde_json::to_string(&e)?],
        )?;
        tx.commit()?;
        Ok(Some(self.job(id)?))
    }
    pub fn update_execution(&mut self, id: i64, phase: Phase, mut e: Execution) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: String =
            tx.query_row("SELECT phase FROM jobs WHERE id=?1", [id], |r| r.get(0))?;
        if !Phase::from_db(&current)?.active() {
            return Err("execution update requires an active claimed job".into());
        }
        let current_data: String =
            tx.query_row("SELECT data FROM executions WHERE job_id=?1", [id], |r| {
                r.get(0)
            })?;
        let current_e: Execution = serde_json::from_str(&current_data)?;
        // Never erase a cancellation committed concurrently by the daemon.
        e.cancel_requested |= current_e.cancel_requested;
        // Apply cancellation precedence inside the same write transaction as completion.
        let phase = if e.cancel_requested && phase.terminal() && phase != Phase::Interrupted {
            Phase::Cancelled
        } else {
            phase
        };
        tx.execute(
            "UPDATE jobs SET phase=?2 WHERE id=?1",
            params![id, phase.as_db()?],
        )?;
        tx.execute(
            "UPDATE executions SET data=?2 WHERE job_id=?1",
            params![id, serde_json::to_string(&e)?],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn finish(&mut self, id: i64, phase: Phase, detail: &str) -> Result<()> {
        if !phase.terminal() {
            return Err("finish requires a terminal phase".into());
        }
        let job = self.job(id)?;
        let mut e = job.execution.ok_or("job has no execution")?;
        let phase = if e.cancel_requested && phase != Phase::Interrupted {
            Phase::Cancelled
        } else {
            phase
        };
        e.finished_at = Some(now());
        e.supervisor = None;
        e.detail = detail.into();
        self.update_execution(id, phase, e)
    }
    pub fn complete_success(&mut self, id: i64) -> Result<()> {
        self.complete_result(id, None)
    }
    pub fn complete_build_success(&mut self, id: i64, artifact: BuildArtifact) -> Result<()> {
        self.complete_result(id, Some(artifact))
    }
    fn complete_result(&mut self, id: i64, artifact: Option<BuildArtifact>) -> Result<()> {
        // Completion and learned history commit together, so cancellation cannot
        // turn a job into Cancelled after it already taught "successful" timings.
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (phase, spec): (String, String) =
            tx.query_row("SELECT phase,spec FROM jobs WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        let spec: crate::JobSpec = serde_json::from_str(&spec)?;
        let command = if spec.suite == crate::local_command::SUITE {
            Some(crate::local_command::Recipe::from_spec(&spec)?)
        } else {
            None
        };
        let build_only = artifact.is_some();
        if Phase::from_db(&phase)?
            != if build_only
                || command
                    .as_ref()
                    .is_some_and(|r| r.stages().last() == Some(&"build"))
            {
                Phase::Building
            } else {
                Phase::Running
            }
        {
            return Err("successful completion requires the correct active result phase".into());
        }
        if build_only {
            catalog::android_contract(&spec)?;
        } else if spec.suite != catalog::SUITE && command.is_none() {
            return Err("test completion cannot complete a build-only workload".into());
        }
        let data: String =
            tx.query_row("SELECT data FROM executions WHERE job_id=?1", [id], |r| {
                r.get(0)
            })?;
        let mut e: Execution = serde_json::from_str(&data)?;
        let phase = if e.cancel_requested {
            Phase::Cancelled
        } else {
            Phase::Passed
        };
        if !e.cancel_requested {
            let expected = command.as_ref().map(|r| r.stages()).unwrap_or_else(|| {
                if build_only {
                    vec!["build"]
                } else {
                    vec!["build", "run"]
                }
            });
            if (command.is_some() && !e.local_command)
                || e.stages.len() != expected.len()
                || e.stages
                    .iter()
                    .zip(expected)
                    .any(|(s, name)| s.stage != name)
                || e.stages
                    .iter()
                    .any(|s| s.exit_code != Some(0) || s.signal.is_some() || s.seconds.is_none())
            {
                return Err(
                    "successful completion lacks required successful stage observations".into(),
                );
            }
            if let Some(artifact) = &artifact {
                artifact.validate(&spec, e.snapshot.as_ref())?;
            }
            for stage in &e.stages {
                for (name, seconds) in [
                    (
                        format!("worker.{}", stage.stage),
                        stage.seconds.unwrap_or(0.0),
                    ),
                    (format!("worker.wait_{}", stage.stage), stage.wait_seconds),
                ] {
                    let old: Option<String> = tx.query_row("SELECT estimate FROM runtimes WHERE suite=?1 AND configuration=?2 AND stage=?3", params![spec.suite,spec.configuration,name], |r| r.get(0)).optional()?;
                    let mut estimate: crate::Estimate = match old {
                        Some(data) => serde_json::from_str(&data)?,
                        None => crate::Estimate::default(),
                    };
                    estimate.record(seconds)?;
                    tx.execute("INSERT INTO runtimes(suite,configuration,stage,estimate) VALUES (?1,?2,?3,?4) ON CONFLICT(suite,configuration,stage) DO UPDATE SET estimate=excluded.estimate", params![spec.suite,spec.configuration,name,serde_json::to_string(&estimate)?])?;
                }
            }
        }
        e.finished_at = Some(now());
        e.supervisor = None;
        e.build_artifact = artifact;
        e.detail = if e.cancel_requested { "Cancelled during completion; no runtime history recorded" } else if command.is_some() {
            "Requested local command exited successfully; live workspace, no source snapshot or inferred test/artifact result"
        } else if build_only {
            "Debug APK assembled and preserved; no tests, installation or application launch performed"
        } else {
            "All freshly built test harnesses passed (lib/bins/integration; doctests excluded from this catalog)"
        }.into();
        tx.execute(
            "UPDATE jobs SET phase=?2 WHERE id=?1",
            params![id, phase.as_db()?],
        )?;
        tx.execute(
            "UPDATE executions SET data=?2 WHERE job_id=?1",
            params![id, serde_json::to_string(&e)?],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn reconcile_interrupted(&mut self) -> Result<usize> {
        // Call only while holding the exclusive worker lock. Never blindly replay a job.
        let ids: Vec<_> = self
            .jobs()?
            .into_iter()
            .filter(|j| j.phase.active())
            .map(|j| j.id)
            .collect();
        for id in &ids {
            self.finish(*id, Phase::Interrupted, "Worker stopped before recording completion; not automatically retried. Inspect retained logs/artifacts before submitting again.")?;
        }
        Ok(ids.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cost, JobSpec};
    fn spec() -> JobSpec {
        JobSpec {
            suite: catalog::SUITE.into(),
            configuration: catalog::CONFIGURATION.into(),
            cost: Cost::Quick,
            source: "/fixture".into(),
            revision: "a".repeat(40),
            owner: "fixture-agent".into(),
        }
    }
    #[test]
    fn proposals_never_execute_without_individual_approval() {
        let mut s = Store::memory().unwrap();
        let old = s.submit(&spec()).unwrap();
        assert!(
            s.claim(ProcessIdentity {
                pid: 1,
                start_ticks: 1
            })
            .unwrap()
            .is_none()
        );
        let ready = s.submit(&spec()).unwrap();
        s.approve(ready.id).unwrap();
        let job = s
            .claim(ProcessIdentity {
                pid: 1,
                start_ticks: 1,
            })
            .unwrap()
            .unwrap();
        assert_eq!(job.id, ready.id);
        assert_eq!(s.job(old.id).unwrap().phase, Phase::Queued);
    }
    #[test]
    fn cancellation_wins_over_stale_worker_update() {
        let mut s = Store::memory().unwrap();
        let j = s.submit(&spec()).unwrap();
        s.approve(j.id).unwrap();
        let j = s
            .claim(ProcessIdentity {
                pid: 1,
                start_ticks: 1,
            })
            .unwrap()
            .unwrap();
        let stale = j.execution.unwrap();
        s.cancel(j.id).unwrap();
        s.update_execution(j.id, Phase::Building, stale).unwrap();
        assert!(s.job(j.id).unwrap().execution.unwrap().cancel_requested);
        s.finish(j.id, Phase::Passed, "finished during cancellation race")
            .unwrap();
        assert_eq!(s.job(j.id).unwrap().phase, Phase::Cancelled);
        assert!(s.approve(j.id).is_err());
    }
    #[test]
    fn worker_restart_marks_claimed_jobs_interrupted_not_queued() {
        let mut s = Store::memory().unwrap();
        let j = s.submit(&spec()).unwrap();
        s.approve(j.id).unwrap();
        s.claim(ProcessIdentity {
            pid: 1,
            start_ticks: 1,
        })
        .unwrap()
        .unwrap();
        assert_eq!(s.reconcile_interrupted().unwrap(), 1);
        assert_eq!(s.job(j.id).unwrap().phase, Phase::Interrupted);
        assert!(
            s.claim(ProcessIdentity {
                pid: 2,
                start_ticks: 2
            })
            .unwrap()
            .is_none()
        );
    }
    #[test]
    fn terminal_write_honors_newly_committed_cancel_and_is_immutable() {
        let mut store = Store::memory().unwrap();
        let job = store.submit(&spec()).unwrap();
        store.approve(job.id).unwrap();
        let claimed = store
            .claim(ProcessIdentity {
                pid: 1,
                start_ticks: 1,
            })
            .unwrap()
            .unwrap();
        let stale = claimed.execution.unwrap();
        store.cancel(job.id).unwrap();
        store
            .update_execution(job.id, Phase::Passed, stale)
            .unwrap();
        assert_eq!(store.job(job.id).unwrap().phase, Phase::Cancelled);
        assert!(
            store
                .finish(job.id, Phase::Passed, "late duplicate completion")
                .is_err()
        );
    }
    #[test]
    fn completion_history_is_atomic_and_cancelled_jobs_do_not_teach() {
        let mut s = Store::memory().unwrap();
        for cancel in [false, true] {
            let job = s.submit(&spec()).unwrap();
            s.approve(job.id).unwrap();
            let job = s
                .claim(ProcessIdentity {
                    pid: 1,
                    start_ticks: 1,
                })
                .unwrap()
                .unwrap();
            let mut e = job.execution.unwrap();
            e.stages = vec![
                StageTiming {
                    stage: "build".into(),
                    seconds: Some(2.0),
                    wait_seconds: 1.0,
                    exit_code: Some(0),
                    signal: None,
                },
                StageTiming {
                    stage: "run".into(),
                    seconds: Some(3.0),
                    wait_seconds: 0.5,
                    exit_code: Some(0),
                    signal: None,
                },
            ];
            s.update_execution(job.id, Phase::Running, e).unwrap();
            if cancel {
                s.cancel(job.id).unwrap();
            }
            s.complete_success(job.id).unwrap();
            assert_eq!(
                s.job(job.id).unwrap().phase,
                if cancel {
                    Phase::Cancelled
                } else {
                    Phase::Passed
                }
            );
        }
        assert_eq!(
            s.estimate(catalog::SUITE, catalog::CONFIGURATION, "worker.run")
                .unwrap()
                .samples(),
            1
        );
    }
    #[test]
    fn manual_record_cannot_spoof_worker_timing_provenance() {
        let mut store = Store::memory().unwrap();
        let response = crate::handle(
            &mut store,
            crate::Request {
                version: crate::PROTOCOL_VERSION,
                action: crate::Action::Record(crate::RuntimeSample {
                    suite: catalog::SUITE.into(),
                    configuration: catalog::CONFIGURATION.into(),
                    stage: "worker.run".into(),
                    seconds: 1.0,
                }),
            },
        );
        assert!(response.error.unwrap().contains("reserved"));
        assert!(store.estimates().unwrap().is_empty());
    }
    #[test]
    fn legacy_ledger_migration_does_not_approve_backlog() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE jobs (id INTEGER PRIMARY KEY AUTOINCREMENT, spec TEXT NOT NULL, phase TEXT NOT NULL, submitted_at INTEGER NOT NULL);").unwrap();
        connection
            .execute(
                "INSERT INTO jobs(spec,phase,submitted_at) VALUES (?1,'queued',1)",
                [serde_json::to_string(&spec()).unwrap()],
            )
            .unwrap();
        let mut store = Store::init(connection, crate::STORE_BUSY_TIMEOUT).unwrap();
        assert_eq!(store.jobs().unwrap()[0].phase, Phase::Queued);
        assert!(store.jobs().unwrap()[0].execution.is_none());
        assert!(
            store
                .claim(ProcessIdentity {
                    pid: 1,
                    start_ticks: 1
                })
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn approve_rejects_unknown_or_terminal_jobs() {
        let mut s = Store::memory().unwrap();
        let mut bad = spec();
        bad.suite = "arbitrary".into();
        let j = s.submit(&bad).unwrap();
        assert!(s.approve(j.id).is_err());
        let j = s.submit(&spec()).unwrap();
        s.cancel(j.id).unwrap();
        assert!(s.approve(j.id).is_err());
    }
}
