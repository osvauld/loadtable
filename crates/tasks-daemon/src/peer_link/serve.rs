//! The receiving half: this daemon hosting a `tasks_peer::Receiver`.
//!
//! A job forwarded to this daemon is an ordinary queue job: it keeps the origin's owner, runs
//! with placement `Here` (the receiver rewrites it), takes a slot of its kind like any other and
//! shows up in `list` and `status`. What marks it is the ledger's `origin` (peer, key), unique
//! per pair, so a repeated submission is the same job also across a restart (R-152). A peer can
//! see and cancel only jobs it sent.

use crate::scheduler::{Scheduler, lock};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tasks_core::metrics::JobMetricsView;
use tasks_core::{JobId, JobSpec, Phase};
use tasks_ledger::{CancelOutcome, JobRecord, LedgerError, NewJob, Origin, TimeoutReason};
use tasks_peer::proto::{
    CancelState, MAX_STATUS_JOBS, PeerJobUse, PeerStatus, PressureSummary, QueueSummary,
    SlotStatus, WireEnd, WireOutcome, WireSample,
};
use tasks_peer::receiver::{
    Allowlist, FinishedJob, JobRunner, JobView, Receiver, StatusSource, SubmitPolicy,
};
use tasks_peer::sync::{GixTransfer, SyncLimits, SyncStore};
use tasks_peer::{PeerId, RemoteJobId, SubmitKey};
use tasks_platform::CgroupName;
use tasks_timing::{Cache, Outcome as TimingOutcome, RecipeKey, Sample, SampleKey};

/// The scheduler and ledger as the receiver's [`JobRunner`].
pub struct DaemonRunner {
    sch: Arc<Scheduler>,
    /// Serialises find-then-insert so two concurrent submissions of one key cannot both pass
    /// the check; the ledger's unique index is the backstop across processes.
    submit_lock: Mutex<()>,
}

impl DaemonRunner {
    /// A runner over `sch`.
    pub fn new(sch: Arc<Scheduler>) -> DaemonRunner {
        DaemonRunner {
            sch,
            submit_lock: Mutex::new(()),
        }
    }

    /// The job, when it exists and came from `peer`.
    fn owned(&self, peer: &PeerId, id: RemoteJobId) -> Option<JobRecord> {
        let rec = self
            .sch
            .ledger
            .get(JobId(i64::try_from(id.0).ok()?))
            .ok()??;
        (rec.origin.as_ref()?.peer == peer.as_str()).then_some(rec)
    }

    fn log_path(&self, rec: &JobRecord) -> std::path::PathBuf {
        rec.log_path
            .clone()
            .unwrap_or_else(|| self.sch.cfg.log_path(rec.id))
    }
}

fn remote_id(id: JobId) -> RemoteJobId {
    RemoteJobId(u64::try_from(id.0).unwrap_or(0))
}

fn wire_end(rec: &JobRecord) -> WireEnd {
    match rec.phase {
        Phase::Cancelled => WireEnd::Cancelled,
        Phase::TimedOut { .. } => match rec.timeout {
            Some(TimeoutReason::Idle) => WireEnd::IdleTimeout,
            _ => WireEnd::WallTimeout,
        },
        Phase::Passed | Phase::Failed { .. } => WireEnd::Exited,
        // Interrupted and NotStarted: the result is unknown to the origin, never a pass
        _ => WireEnd::Lost,
    }
}

/// The end of a finished job as the receiver reports it. One whole-job timing sample is sent
/// back so the origin's history learns what this machine costs.
fn finished(rec: &JobRecord, machine: &str) -> FinishedJob {
    let wall_ms = match (rec.started_at_ms, rec.finished_at_ms) {
        (Some(s), Some(f)) => u64::try_from((f - s).max(0)).unwrap_or(0),
        _ => 0,
    };
    let outcome = WireOutcome {
        end: wire_end(rec),
        exit_code: rec.exit_code,
        signal: rec.signal,
        wall_ms,
        rss_bytes: None,
        cpu_s: None,
        description: rec
            .summary
            .clone()
            .unwrap_or_else(|| rec.phase.word().to_string()),
    };
    let mut samples = Vec::new();
    let keys = RecipeKey::from_argv(&rec.spec.argv, &rec.spec.env);
    let ran =
        rec.started_at_ms.is_some() && matches!(rec.phase, Phase::Passed | Phase::Failed { .. });
    if let (Some(k), true) = (keys.first(), ran) {
        let key = SampleKey {
            recipe: k.to_string(),
            target: None,
            machine: machine.to_string(),
        };
        let sample = Sample {
            secs: wall_ms as f64 / 1000.0,
            cache: Cache::Unknown,
            outcome: if rec.phase == Phase::Passed {
                TimingOutcome::Passed
            } else {
                TimingOutcome::Failed
            },
            at_ms: u64::try_from(rec.finished_at_ms.unwrap_or(0)).unwrap_or(0),
        };
        samples.push(WireSample::from_sample(&key, &sample));
    }
    FinishedJob { outcome, samples }
}

impl JobRunner for DaemonRunner {
    fn submit(
        &self,
        peer: &PeerId,
        key: &SubmitKey,
        spec: JobSpec,
    ) -> Result<(RemoteJobId, bool), String> {
        let _one_at_a_time = lock(&self.submit_lock);
        let ledger = &self.sch.ledger;
        let find = || ledger.find_origin(peer.as_str(), key.as_str());
        if let Some(id) = find().map_err(|e| e.to_string())? {
            return Ok((remote_id(id), true));
        }
        let mut job = NewJob::new(spec);
        job.origin = Some(Origin {
            peer: peer.as_str().to_string(),
            key: key.as_str().to_string(),
        });
        match ledger.submit_job(&job) {
            Ok(id) => {
                self.sch.wake.notify();
                Ok((remote_id(id), false))
            }
            Err(LedgerError::DuplicateOrigin { .. }) => match find().map_err(|e| e.to_string())? {
                Some(id) => Ok((remote_id(id), true)),
                None => Err("duplicate submit key but no such job".to_string()),
            },
            Err(e) => Err(e.to_string()),
        }
    }

    fn view(&self, peer: &PeerId, id: RemoteJobId) -> Option<JobView> {
        let rec = self.owned(peer, id)?;
        let log_len = std::fs::metadata(self.log_path(&rec))
            .map(|m| m.len())
            .unwrap_or(0);
        Some(JobView {
            detail: rec.wait_text.clone().unwrap_or_default(),
            finished: rec
                .phase
                .is_terminal()
                .then(|| finished(&rec, &self.sch.cfg.machine)),
            phase: rec.phase,
            log_len,
        })
    }

    fn metrics(&self, peer: &PeerId, id: RemoteJobId) -> Option<JobMetricsView> {
        let rec = self.owned(peer, id)?;
        Some(self.sch.job_metrics(&rec))
    }

    fn read_log(&self, peer: &PeerId, id: RemoteJobId, offset: u64, max: usize) -> Vec<u8> {
        let Some(rec) = self.owned(peer, id) else {
            return Vec::new();
        };
        read_range(&self.log_path(&rec), offset, max)
    }

    fn cancel(&self, peer: &PeerId, id: RemoteJobId) -> Option<CancelState> {
        let rec = self.owned(peer, id)?;
        let by = format!("peer {peer}");
        match self.sch.ledger.request_cancel(rec.id, &by).ok()? {
            CancelOutcome::Cancelled => {
                self.sch.wake.notify();
                Some(CancelState::Requested)
            }
            CancelOutcome::Requested => {
                self.sch.stop_running(rec.id);
                Some(CancelState::Requested)
            }
            CancelOutcome::AlreadyFinished(_) => Some(CancelState::AlreadyFinished),
        }
    }

    fn checkout_busy(&self, peer: &PeerId, tree: &Path) -> bool {
        let filter = tasks_ledger::ListFilter {
            active_only: true,
            ..tasks_ledger::ListFilter::default()
        };
        self.sch.ledger.list(&filter).is_ok_and(|rows| {
            rows.iter().any(|r| {
                r.origin.as_ref().is_some_and(|o| o.peer == peer.as_str())
                    && r.spec.cwd.starts_with(tree)
            })
        })
    }
}

fn read_range(path: &Path, offset: u64, max: usize) -> Vec<u8> {
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = f
        .take(u64::try_from(max).unwrap_or(u64::MAX))
        .read_to_end(&mut buf);
    buf
}

/// The store of synced copies under `store_dir`: bare mirrors and working copies, with sync
/// and retention events going to the audit log.
pub fn build_store(sch: &Arc<Scheduler>, store_dir: std::path::PathBuf) -> Arc<SyncStore> {
    let procs = sch.platform.cgroups.create(CgroupName::Transfer).ok();
    let mut store = SyncStore::new(
        store_dir,
        SyncLimits::default(),
        Arc::new(GixTransfer::new().with_cgroup_procs(procs)),
    )
    .with_gate(sch.transfer_gate());
    if let Some(sink) = sch.audit_sink() {
        store = store.with_observer(Arc::new(crate::audited::AuditSync::new(sink)));
    }
    Arc::new(store)
}

/// Retention for the synced copies, run hourly: idle working copies and unused mirrors go,
/// mirrors are collected weekly, and a copy with a live job is never touched.
pub fn sweep_hook(
    sch: &Arc<Scheduler>,
    store: Arc<SyncStore>,
) -> crate::trust_runtime::MaintenanceHook {
    let runner = DaemonRunner::new(sch.clone());
    Box::new(move |policy| {
        let busy = |peer: &PeerId, tree: &Path| runner.checkout_busy(peer, tree);
        store.sweep(&policy.sync_retention(), &busy);
    })
}

/// The receiver this daemon hosts: jobs into the ledger, synced copies in `store`. Who may
/// call it is decided in front of it by the trust gate (`tasks_peer::trust::TrustedHandler`), on
/// grants; the receiver itself admits every caller it is handed.
pub fn build_receiver_with(sch: &Arc<Scheduler>, store: Arc<SyncStore>) -> Arc<Receiver> {
    Arc::new(Receiver::new(
        &sch.cfg.machine,
        Allowlist::open(),
        store,
        Arc::new(DaemonRunner::new(sch.clone())),
        Arc::new(DaemonStatus::new(sch.clone())),
        // the receiving peer applies the same rules as the local API (R-153)
        Arc::new(DaemonRules::new(sch.clone())),
        crate::build_cache::BuildCache::fetch_policy(),
    ))
}

/// [`build_receiver_with`] over a fresh store at `store_dir`.
pub fn build_receiver(sch: &Arc<Scheduler>, store_dir: std::path::PathBuf) -> Arc<Receiver> {
    build_receiver_with(sch, build_store(sch, store_dir))
}

/// The submit rules of this daemon as the receiver's policy: a forwarded job gets the answer a
/// local submission would (device denylist, broad-test refusal or warning, R-153). Cargo
/// parallelism above this machine's cap is not refused: the job is lowered to the cap when it
/// starts, as for any job here (R-137).
pub struct DaemonRules {
    sch: Arc<Scheduler>,
}

impl DaemonRules {
    /// The rules of `sch`.
    pub fn new(sch: Arc<Scheduler>) -> DaemonRules {
        DaemonRules { sch }
    }
}

impl SubmitPolicy for DaemonRules {
    fn check(&self, spec: &JobSpec) -> Result<(), String> {
        self.sch
            .check_spec(spec, crate::submit::Via::Peer)
            .map(|_warnings| ())
    }

    /// A refusal is written to the audit log under the sending peer's DID.
    fn check_from(&self, from: &PeerId, spec: &JobSpec) -> Result<(), String> {
        use crate::submit::Via;
        match self.sch.spec_verdict(spec, Via::Peer) {
            Ok(_) => Ok(()),
            Err(refusals) => {
                let actor = self
                    .sch
                    .audit_sink()
                    .and_then(|a| a.did_of_node(from.as_str()));
                self.sch.audit_refusal(spec, &refusals, Via::Peer, actor);
                Err(refusals
                    .iter()
                    .map(|r| r.message.clone())
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
        }
    }
}

/// The scheduler's slots, queue and pressure as the peer-visible status.
pub struct DaemonStatus {
    sch: Arc<Scheduler>,
}

impl DaemonStatus {
    /// A status source over `sch`.
    pub fn new(sch: Arc<Scheduler>) -> DaemonStatus {
        DaemonStatus { sch }
    }
}

impl StatusSource for DaemonStatus {
    fn status(&self) -> PeerStatus {
        let snap = self.sch.status();
        let policy = self.sch.policy();
        let machine = self.sch.local_machine(&policy, &Default::default());
        PeerStatus {
            machine,
            slots: snap
                .slots
                .iter()
                .filter_map(|g| {
                    let kind = match g.kind.as_str() {
                        "compile" => tasks_core::SlotKind::Compile,
                        "run" => tasks_core::SlotKind::Run,
                        "offload" => tasks_core::SlotKind::Offload,
                        _ => return None,
                    };
                    Some(SlotStatus {
                        kind,
                        total: g.total,
                        used: g.total.saturating_sub(g.free),
                    })
                })
                .collect(),
            queue: QueueSummary {
                waiting: u32::try_from(snap.waiting.len() + snap.waiting_more).unwrap_or(u32::MAX),
                running: u32::try_from(snap.running.len()).unwrap_or(u32::MAX),
            },
            pressure: PressureSummary {
                cpu_pct: snap.pressure.cpu.map(pct),
                memory_pct: snap.pressure.memory_some.map(pct),
                memory_level: Some(snap.pressure.memory.clone()).filter(|m| m != "unknown"),
                io_pct: snap.pressure.io.map(pct),
                summary: snap.pressure.compile_blocker.clone().unwrap_or_default(),
                cpu_busy_pct: snap.pressure.cpu_busy_pct.map(pct),
                swap_total_bytes: snap.pressure.swap_total,
                swap_used_bytes: snap.pressure.swap_used,
                swap_in_per_s: snap.pressure.swap_in_per_s,
                swap_out_per_s: snap.pressure.swap_out_per_s,
            },
            jobs: running_use(&snap.running),
            cores_in_use_milli: snap.pressure.cores_in_use.map(milli),
        }
    }
}

/// Thousandths of a core, rounded; negative and absurd readings clamp.
fn milli(cores: f64) -> u32 {
    (cores.max(0.0) * 1000.0).round().min(f64::from(u32::MAX)) as u32
}

/// The running jobs' use as the status request lists it (the busiest [`MAX_STATUS_JOBS`]).
fn running_use(running: &[crate::api::RunningView]) -> Vec<PeerJobUse> {
    let mut rows: Vec<PeerJobUse> = running
        .iter()
        .map(|r| PeerJobUse {
            id: u64::try_from(r.id.0).unwrap_or(0),
            owner: r.owner.clone(),
            cores_milli: r.cores.map(milli),
            mem_bytes: r.mem_bytes,
        })
        .collect();
    rows.sort_by(|a, b| b.cores_milli.cmp(&a.cores_milli).then(a.id.cmp(&b.id)));
    rows.truncate(MAX_STATUS_JOBS);
    rows
}

fn pct(v: f32) -> u8 {
    v.clamp(0.0, 100.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Rig;
    use tasks_core::Kind;
    use tasks_ledger::Store;

    fn peer() -> PeerId {
        PeerId::new("laptop-node").unwrap()
    }

    // R-152
    #[test]
    fn r152_a_repeated_key_is_the_same_job_and_a_peer_sees_only_its_own() {
        let r = Rig::new();
        let run = DaemonRunner::new(r.sch.clone());
        let key = SubmitKey::new("laptop:1").unwrap();
        let spec = r.spec("origin-owner", &["make", "x"], Kind::Compile);
        let (id, dup) = run.submit(&peer(), &key, spec.clone()).unwrap();
        assert!(!dup);
        let (again, dup) = run.submit(&peer(), &key, spec.clone()).unwrap();
        assert_eq!((again, dup), (id, true));
        assert_eq!(r.store.list(&Default::default()).unwrap().len(), 1);
        // the job is an ordinary queue job with the origin's owner
        let rec = r.record(JobId(i64::try_from(id.0).unwrap()));
        assert_eq!(rec.spec.owner.as_str(), "origin-owner");
        assert_eq!(rec.origin.unwrap().peer, "laptop-node");
        // another peer neither sees nor cancels it, and may reuse the key for its own job
        let other = PeerId::new("mac-node").unwrap();
        assert!(run.view(&other, id).is_none());
        assert!(run.cancel(&other, id).is_none());
        let (id2, dup) = run.submit(&other, &key, spec).unwrap();
        assert!(!dup && id2 != id);
    }

    #[test]
    fn a_remote_job_runs_through_the_queue_and_reports_its_end_and_log() {
        let r = Rig::new();
        r.procs.auto_exit(0);
        r.procs.output(b"hello from the worker\n");
        let run = DaemonRunner::new(r.sch.clone());
        let key = SubmitKey::new("laptop:2").unwrap();
        let (id, _) = run
            .submit(&peer(), &key, r.spec("o", &["make", "x"], Kind::Run))
            .unwrap();
        let local = JobId(i64::try_from(id.0).unwrap());
        assert_eq!(r.until_done(local), Phase::Passed);
        let v = run.view(&peer(), id).unwrap();
        let fin = v.finished.unwrap();
        assert_eq!(fin.outcome.phase(), Phase::Passed);
        assert_eq!(fin.samples.len(), 1);
        // the log holds the daemon's own placement note, then the command's output
        let log = String::from_utf8(run.read_log(&peer(), id, 0, 4096)).unwrap();
        assert!(log.ends_with("hello from the worker\n"), "{log}");
        let at = u64::try_from(log.find("hello").unwrap()).unwrap();
        assert_eq!(run.read_log(&peer(), id, at + 6, 4), b"from");
        assert_eq!(run.cancel(&peer(), id), Some(CancelState::AlreadyFinished));
    }

    #[test]
    fn a_live_remote_job_makes_its_checkout_busy_until_it_ends() {
        let r = Rig::new();
        let run = DaemonRunner::new(r.sch.clone());
        let key = SubmitKey::new("laptop:3").unwrap();
        let tree = r.dir.path().to_path_buf();
        let (id, _) = run
            .submit(&peer(), &key, r.spec("o", &["make", "x"], Kind::Run))
            .unwrap();
        assert!(run.checkout_busy(&peer(), &tree));
        assert!(!run.checkout_busy(&PeerId::new("mac-node").unwrap(), &tree));
        assert!(!run.checkout_busy(&peer(), &tree.join("elsewhere")));
        assert_eq!(run.cancel(&peer(), id), Some(CancelState::Requested));
        assert!(!run.checkout_busy(&peer(), &tree));
    }

    // R-626
    #[test]
    fn r626_status_lists_running_jobs_busiest_first_and_capped() {
        use crate::api::RunningView;
        let mk = |id: i64, cores: Option<f64>| {
            let mut v = r626_row(id);
            v.cores = cores;
            v.mem_bytes = Some(1 << 20);
            v
        };
        let mut rows: Vec<RunningView> = (1..=40).map(|i| mk(i, Some(i as f64 / 10.0))).collect();
        rows.push(mk(99, None));
        let out = running_use(&rows);
        assert_eq!(out.len(), MAX_STATUS_JOBS);
        assert_eq!(out[0].id, 40);
        assert_eq!(out[0].cores_milli, Some(4000));
        assert_eq!(out[0].mem_bytes, Some(1 << 20));
        assert_eq!(milli(-1.0), 0);
        assert_eq!(milli(0.0004), 0);
        assert_eq!(milli(1.2345), 1235);
        assert_eq!(milli(f64::MAX), u32::MAX);
        // an unmeasured job sorts after the measured ones
        let few = running_use(&[mk(5, None), mk(6, Some(0.5))]);
        assert_eq!((few[0].id, few[1].id), (6, 5));
    }

    fn r626_row(id: i64) -> crate::api::RunningView {
        crate::api::RunningView {
            id: JobId(id),
            owner: "o".into(),
            command: "make".into(),
            slot: None,
            elapsed_secs: 0.0,
            done_targets: 0,
            total_targets: 0,
            current: None,
            eta_secs: None,
            log_bytes: 0,
            figures: None,
            cores: None,
            mem_bytes: None,
        }
    }

    // R-622
    #[test]
    fn r622_a_peer_gets_metrics_only_for_jobs_it_sent() {
        let r = Rig::new();
        let run = DaemonRunner::new(r.sch.clone());
        let key = SubmitKey::new("laptop:9").unwrap();
        let (id, _) = run
            .submit(&peer(), &key, r.spec("o", &["make", "x"], Kind::Run))
            .unwrap();
        let v = run.metrics(&peer(), id).unwrap();
        assert!(v.samples.is_empty() && v.note.is_some());
        assert!(
            run.metrics(&PeerId::new("mac-node").unwrap(), id).is_none(),
            "another peer must not read it"
        );
        assert!(run.metrics(&peer(), RemoteJobId(999)).is_none());
    }

    #[test]
    fn status_reports_slots_and_queue() {
        let r = Rig::new();
        let s = DaemonStatus::new(r.sch.clone()).status();
        assert!(s.slots.iter().any(|x| x.total > 0));
        assert_eq!(s.queue.waiting, 0);
    }

    // R-523
    #[test]
    fn r523_a_peers_report_carries_its_memory_level() {
        let r = Rig::new();
        let s = DaemonStatus::new(r.sch.clone()).status();
        assert_eq!(s.pressure.memory_level.as_deref(), Some("normal"));
    }
}
