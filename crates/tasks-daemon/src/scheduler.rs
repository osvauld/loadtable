//! The scheduler: ledger `ready()` -> placement -> admission -> a runner thread per job.
//!
//! [`Scheduler`] is a struct over trait objects (ledger, platform, clock, forwarder). Its
//! [`Scheduler::tick`] does one pass and is driven by tests deterministically; the daemon calls
//! it on a periodic wake-up and whenever a submit, cancel or job end calls [`Wake::notify`].

use crate::alerts::AlertBook;
use crate::cargo_jobs::{self, CargoJobs};
use crate::config::DaemonConfig;
use crate::forwarder::Forwarder;
use crate::machine_policy::MachinePolicy;
use crate::place::Placed;
use crate::platform::{Ledger, Platform};
use crate::runjob::{self, JobMeta};
use crate::stuck::StuckDetector;
use crate::submit::Via;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};
use tasks_admission::{
    Admitter, Attempt, Hold, Layout, Queue, Verdict, WaitReason as AdmitWait, Waiter,
};
use tasks_core::{Arch, JobId, Kind, Os, Phase, SlotKind, WaitReason as LedgerWait};
use tasks_ledger::{Clock, Finish, JobRecord, LedgerError};
use tasks_platform::{GroupId, SlotGuard};
use tasks_runner::{CancelToken, JobStart, LogNotes, Policy as RunPolicy};
use tasks_timing::{Estimates, Timeline};

/// Locks a mutex, carrying on after a panic elsewhere (the data is plain bookkeeping).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wakes the scheduler loop.
#[derive(Debug, Default)]
pub struct Wake {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Wake {
    /// Asks for a tick now.
    pub fn notify(&self) {
        *lock(&self.flag) = true;
        self.cv.notify_all();
    }

    /// Waits until notified or `timeout` passes.
    pub fn wait(&self, timeout: Duration) {
        let g = lock(&self.flag);
        let (mut g, _) = self
            .cv
            .wait_timeout_while(g, timeout, |f| !*f)
            .unwrap_or_else(PoisonError::into_inner);
        *g = false;
    }
}

/// A job holding or about to hold a slot, with everything the daemon watches about it.
pub(crate) struct RunningJob {
    pub id: JobId,
    pub owner: String,
    pub command: String,
    pub slot_kind: Option<SlotKind>,
    pub slot: Option<(SlotKind, u32)>,
    pub checkout: Option<String>,
    pub token: CancelToken,
    pub started: Instant,
    pub log_path: PathBuf,
    pub group: Mutex<Option<GroupId>>,
    pub timeline: Mutex<Timeline>,
    pub estimates: Estimates,
    pub kill_reason: Mutex<Option<String>>,
    pub stuck: Mutex<StuckDetector>,
    pub est_median: Option<f64>,
    /// The latest sample of what the job costs the machine.
    pub figures: Mutex<Option<tasks_core::JobFigures>>,
}

/// Per-tick bookkeeping that survives between ticks.
#[derive(Default)]
pub(crate) struct TickState {
    pub placed: BTreeMap<JobId, String>,
    /// The last host counters and the rates computed from them (status).
    pub host_prev: Option<(
        std::time::Instant,
        tasks_platform::MemorySnapshot,
        tasks_platform::HostRates,
    )>,
    pub facts: BTreeMap<JobId, Arc<crate::place::JobFacts>>,
    pub last_wait: BTreeMap<JobId, (LedgerWait, String)>,
    pub mem_pressure_since: Option<i64>,
    pub orphan_since: BTreeMap<(SlotKind, u32), i64>,
    pub orphan_text: BTreeMap<(SlotKind, u32), String>,
    pub last_audit_ms: i64,
    pub last_prune_ms: i64,
    pub last_error: Option<(i64, String)>,
    pub readings_error: Option<String>,
    /// Peers a job must not be forwarded to again: a forward to them failed with nothing
    /// submitted and the job fell back to local placement. Forgotten on restart.
    pub no_forward: BTreeMap<JobId, BTreeSet<String>>,
    /// Why a job runs here though placement chose a peer; appended to its summary at the end.
    pub fallbacks: BTreeMap<JobId, String>,
    /// Since when each peer has been stale or offline, ms.
    pub peer_bad_since: BTreeMap<String, i64>,
    /// Set once the forwarded jobs left running by a previous daemon were picked up.
    pub reconciled: bool,
    /// When each owner last started a job, ms: the queue gives the next turn to the owner that
    /// started least recently. Forgotten on restart (everyone is then "never").
    pub last_started: BTreeMap<String, u64>,
}

/// What one tick did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Jobs started.
    pub started: Vec<JobId>,
    /// Jobs ended as `NotStarted`.
    pub not_started: Vec<JobId>,
    /// Jobs handed to a peer.
    pub forwarded: Vec<JobId>,
    /// Jobs cancelled because a dependency failed.
    pub cancelled_blocked: Vec<JobId>,
    /// Jobs left waiting.
    pub waiting: usize,
}

/// What the scheduler is built from.
pub struct SchedulerParts {
    /// Fixed directories.
    pub cfg: DaemonConfig,
    /// The machine policy.
    pub policy: MachinePolicy,
    /// The ledger.
    pub ledger: Arc<dyn Ledger>,
    /// The operating system.
    pub platform: Platform,
    /// Wall clock.
    pub clock: Arc<dyn Clock>,
    /// Peers.
    pub forwarder: Arc<dyn Forwarder>,
    /// The daemon's environment, read once at start; jobs get an allowlisted subset.
    pub parent_env: Vec<(String, String)>,
    /// The persisted execution intent.
    pub execution_enabled: bool,
    /// How often a runner polls its job.
    pub runner_poll: Duration,
    /// The gate sync work enters; bound to the scheduler by [`Scheduler::new`]. `None` makes one.
    pub transfer_gate: Option<Arc<crate::attrib::TransferGate>>,
}

/// The scheduler and the state the API reads.
pub struct Scheduler {
    pub(crate) cfg: DaemonConfig,
    pub(crate) policy: RwLock<Arc<MachinePolicy>>,
    pub(crate) ledger: Arc<dyn Ledger>,
    pub(crate) platform: Platform,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) forwarder: Arc<dyn Forwarder>,
    pub(crate) os: Os,
    pub(crate) arch: Arch,
    pub(crate) cores: u32,
    pub(crate) running: Mutex<BTreeMap<JobId, Arc<RunningJob>>>,
    pub(crate) alerts: Mutex<AlertBook>,
    /// The audit log; set by the daemon (design §8).
    pub(crate) audit: Mutex<Option<Arc<crate::audited::AuditSink>>>,
    /// Trust control (identity, pairing, peers); set by the daemon when the trust runtime is up.
    pub(crate) trust: Mutex<Option<Arc<dyn crate::trust_control::TrustControl>>>,
    /// Extra hourly work from the peer runtime (sync retention).
    pub(crate) maintenance_hooks: Mutex<Vec<crate::trust_runtime::MaintenanceHook>>,
    pub(crate) state: Mutex<TickState>,
    pub(crate) wake: Wake,
    /// Attribution between ticks: the outside-process window and the transfer counters.
    pub(crate) attrib: Mutex<crate::attrib::AttribState>,
    /// At most one pressure trip per resource per interval.
    pub(crate) trip_throttle: tasks_ledger::Throttle,
    pub(crate) draining: AtomicBool,
    /// Who drained and when (ms); set with `draining`, cleared by a resume (R-189).
    pub(crate) drained: Mutex<Option<(String, i64)>>,
    pub(crate) execution_enabled: AtomicBool,
    pub(crate) shutdown: AtomicBool,
    pub(crate) started_ms: i64,
    pub(crate) parent_env: Vec<(String, String)>,
    pub(crate) runner_poll: Duration,
    /// This machine's build caches (compiler cache, output links, Gradle home).
    pub(crate) build_cache: Mutex<Arc<crate::build_cache::BuildCache>>,
}

impl Scheduler {
    /// Builds the scheduler.
    pub fn new(parts: SchedulerParts) -> Arc<Scheduler> {
        let started_ms = parts.clock.now_ms();
        let trip_window_ms =
            i64::try_from(parts.policy.trip_interval_secs().saturating_mul(1000)).unwrap_or(60_000);
        let gate = parts.transfer_gate;
        let build_cache = Arc::new(crate::build_cache::BuildCache::discover(
            &parts.cfg.cache_dir,
            &parts.cfg.state_dir.join("peer-store"),
            &parts.parent_env,
        ));
        let sch = Arc::new(Scheduler {
            cfg: parts.cfg,
            policy: RwLock::new(Arc::new(parts.policy)),
            ledger: parts.ledger,
            platform: parts.platform,
            clock: parts.clock,
            forwarder: parts.forwarder,
            os: Os::current(),
            arch: Arch::current(),
            cores: std::thread::available_parallelism()
                .map(|n| u32::try_from(n.get()).unwrap_or(1))
                .unwrap_or(1),
            running: Mutex::new(BTreeMap::new()),
            alerts: Mutex::new(AlertBook::new()),
            audit: Mutex::new(None),
            trust: Mutex::new(None),
            maintenance_hooks: Mutex::new(Vec::new()),
            state: Mutex::new(TickState::default()),
            wake: Wake::default(),
            attrib: Mutex::new(crate::attrib::AttribState::new()),
            trip_throttle: tasks_ledger::Throttle::new(trip_window_ms, 1),
            draining: AtomicBool::new(false),
            drained: Mutex::new(None),
            execution_enabled: AtomicBool::new(parts.execution_enabled),
            shutdown: AtomicBool::new(false),
            started_ms,
            parent_env: parts.parent_env,
            runner_poll: parts.runner_poll,
            build_cache: Mutex::new(build_cache),
        });
        if let Some(g) = gate {
            g.bind(&sch);
        }
        sch
    }

    /// The build caches in force.
    pub fn build_cache(&self) -> Arc<crate::build_cache::BuildCache> {
        lock(&self.build_cache).clone()
    }

    /// Replaces the build caches (an explicit sccache, or none); for embedding and tests.
    pub fn set_build_cache(&self, cache: crate::build_cache::BuildCache) {
        *lock(&self.build_cache) = Arc::new(cache);
    }

    /// The gate that makes sync work wait for a transfer slot and the I/O gate.
    pub fn transfer_gate(self: &Arc<Self>) -> Arc<crate::attrib::TransferGate> {
        let g = crate::attrib::TransferGate::new();
        g.bind(self);
        g
    }

    /// Plugs in trust control: `Identity`, `Pair*` and `Peers` requests, peer trust in the
    /// status and grant-expiry alerts.
    pub fn set_trust(&self, trust: Arc<dyn crate::trust_control::TrustControl>) {
        if let Some(a) = self.audit_sink() {
            a.set_trust(trust.clone());
        }
        *lock(&self.trust) = Some(trust);
    }

    /// Adds work to the hourly maintenance (sync retention).
    pub fn add_maintenance(&self, hook: crate::trust_runtime::MaintenanceHook) {
        lock(&self.maintenance_hooks).push(hook);
    }

    /// Plugs in the audit log: `Audit*` requests, drain and refusal rows, retention.
    pub fn set_audit(&self, sink: Arc<crate::audited::AuditSink>) {
        if let Some(t) = self.trust() {
            sink.set_trust(t);
        }
        *lock(&self.audit) = Some(sink);
    }

    pub(crate) fn audit_sink(&self) -> Option<Arc<crate::audited::AuditSink>> {
        lock(&self.audit).clone()
    }

    /// Records an audit event when the daemon keeps a log.
    pub(crate) fn audit(&self, ev: tasks_ledger::AuditEvent) {
        if let Some(a) = self.audit_sink() {
            a.record(ev);
        }
    }

    pub(crate) fn trust(&self) -> Option<Arc<dyn crate::trust_control::TrustControl>> {
        lock(&self.trust).clone()
    }

    /// The current machine policy.
    pub fn policy(&self) -> Arc<MachinePolicy> {
        self.policy
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The fixed directories.
    pub fn config(&self) -> &DaemonConfig {
        &self.cfg
    }

    /// The wake-up the loop waits on.
    pub fn wake(&self) -> &Wake {
        &self.wake
    }

    /// True once a shutdown was accepted.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Number of jobs holding or starting a slot.
    pub fn running_count(&self) -> usize {
        lock(&self.running).len()
    }

    /// Waits until no job runs, up to `timeout`. For tests and orderly shutdown.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let t0 = Instant::now();
        while self.running_count() > 0 {
            if t0.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    /// Records a ledger error without stopping the pass (R-005). A job that vanished or was
    /// cancelled between two calls is a race, not an error.
    pub(crate) fn guard<T>(&self, what: &str, r: Result<T, LedgerError>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(LedgerError::IllegalTransition { .. } | LedgerError::NotFound(_)) => None,
            Err(e) => {
                self.note_error(&format!("{what}: {e}"));
                None
            }
        }
    }

    pub(crate) fn note_error(&self, text: &str) {
        let now = self.clock.now_ms();
        lock(&self.state).last_error = Some((now, text.to_string()));
    }

    /// One scheduling pass. Never blocks on a job; errors on one job never stop the others.
    pub fn tick(self: &Arc<Self>) -> TickReport {
        let policy = self.policy();
        let now = self.clock.now_ms();
        let mut rep = TickReport::default();
        let first = !std::mem::replace(&mut lock(&self.state).reconciled, true);
        if first {
            self.reconcile_forwards();
        }
        if let Some(ids) = self.guard("cancel_blocked", self.ledger.cancel_blocked()) {
            rep.cancelled_blocked = ids;
        }
        self.sync_cancels();
        self.supervise(&policy, now);
        self.admit(&policy, &mut rep);
        self.psi_trips(&policy);
        self.maintenance(&policy, now);
        self.evaluate_alerts(&policy, now);
        rep
    }

    /// A cancel committed in the ledger stops the running job (also one made by another daemon
    /// connection). A ledger error never counts as a cancel (R-001).
    fn sync_cancels(&self) {
        let jobs: Vec<Arc<RunningJob>> = lock(&self.running).values().cloned().collect();
        for rj in jobs {
            if rj.token.state() == tasks_runner::CancelState::Cancel {
                continue;
            }
            if let Some(true) = self.guard("cancel_requested", self.ledger.cancel_requested(rj.id))
            {
                rj.token.cancel();
            }
        }
    }

    /// An admitter over the live machine with the current policy.
    pub(crate) fn admitter(&self, policy: &MachinePolicy) -> Admitter<'_> {
        Admitter {
            memory: &*self.platform.memory,
            pressure: &*self.platform.pressure,
            disk: &*self.platform.disk,
            locks: &*self.platform.locks,
            cache_path: self.cfg.cache_dir.clone(),
            state_path: self.cfg.state_dir.clone(),
            thresholds: policy.thresholds(),
            psi: policy.psi(),
        }
    }

    pub(crate) fn layout(&self, policy: &MachinePolicy) -> Layout {
        let mut layout = policy.layout();
        for rj in lock(&self.running).values() {
            if let Some(slot) = rj.slot {
                layout.holders.insert(slot, rj.id);
            }
        }
        layout
    }

    fn admit(self: &Arc<Self>, policy: &Arc<MachinePolicy>, rep: &mut TickReport) {
        let Some(ready) = self.guard("ready", self.ledger.ready()) else {
            return;
        };
        {
            // forget cached facts of jobs that are no longer candidates
            let keep: BTreeSet<JobId> = ready.iter().copied().collect();
            let mut st = lock(&self.state);
            st.facts.retain(|id, _| keep.contains(id));
            st.placed.retain(|id, _| keep.contains(id));
            st.last_wait.retain(|id, _| keep.contains(id));
            st.no_forward.retain(|id, _| keep.contains(id));
            drop(st);
            // a forward cancelled or finished while it waited must not hold up its checkout
            self.forwarder.forget_unlisted(&keep);
        }
        let admitter = self.admitter(policy);
        let readings = admitter.readings();
        lock(&self.state).readings_error = readings.as_ref().err().map(ToString::to_string);

        let running_snapshot: Vec<Arc<RunningJob>> =
            lock(&self.running).values().cloned().collect();
        // every job that could start now, read once: the queue the placement estimates is these
        let mut recs: Vec<JobRecord> = Vec::new();
        for id in ready {
            if running_snapshot.iter().any(|r| r.id == id) {
                continue;
            }
            let Some(Some(rec)) = self.guard("get", self.ledger.get(id)) else {
                continue;
            };
            if rec.phase.is_terminal() || rec.cancel_requested() {
                continue;
            }
            recs.push(rec);
        }
        let queued = self.queued_by_kind(&recs);
        let ctx = self.place_ctx(policy, readings.as_ref().ok(), &running_snapshot, queued);
        let blocked_hold = self.global_hold();

        let mut candidates: BTreeMap<JobId, (JobRecord, Waiter)> = BTreeMap::new();
        let mut probes: Vec<JobRecord> = Vec::new();
        for rec in recs {
            let id = rec.id;
            if rec.forward.is_some() {
                // a job already placed on a peer: an offload-client job (R-162, R-230)
                if let Some(w) = self.forward_waiter(&rec, blocked_hold.as_ref()) {
                    candidates.insert(id, (rec, w));
                }
                continue;
            }
            match self.place(&rec, policy, &ctx) {
                Placed::Local => {}
                Placed::Forwarded => {
                    rep.forwarded.push(id);
                    continue;
                }
                Placed::Waiting(text) => {
                    self.set_wait(id, LedgerWait::Placement(text.clone()), &text);
                    rep.waiting += 1;
                    continue;
                }
                Placed::Refused(text) => {
                    let mut fin = Finish::from_phase(Phase::NotStarted {
                        reason: text.clone(),
                    });
                    fin.summary = Some(text);
                    if self.guard("finish", self.ledger.finish(id, &fin)).is_some() {
                        rep.not_started.push(id);
                    }
                    continue;
                }
            }
            let facts = self.facts(&rec);
            let Some(slot_kind) = tasks_placement::slot_for(rec.spec.kind) else {
                probes.push(rec);
                continue;
            };
            let mut w = Waiter::new(
                id,
                slot_kind,
                u64::try_from(rec.submitted_at_ms).unwrap_or(0),
            );
            w.owner = rec.spec.owner.as_str().to_string();
            w.priority = rec.spec.priority;
            w.predicted_secs = facts.short_secs;
            w.predicted_peak_memory = facts.peak_memory;
            w.checkout = facts.checkout.clone();
            w.device = rec.spec.requirements.device.clone();
            // only a job that would write NEW output on the cache disk waits for its reserve;
            // one whose output links all exist proceeds (R-061, R-404)
            w.new_output = matches!(rec.spec.kind, Kind::Compile | Kind::Test | Kind::Package)
                && self.plan_outputs(&rec.spec).needs_new_output();
            if let Some(h) = &blocked_hold {
                w.holds.push(h.clone());
            }
            if let Some(co) = &w.checkout
                && let Some(other) = running_snapshot
                    .iter()
                    .find(|r| r.checkout.as_ref() == Some(co))
            {
                w.holds.push(Hold::SameCheckoutBusy(other.id));
            }
            candidates.insert(id, (rec, w));
        }

        // Probes need no slot and no gate, but still obey drain and the execution intent.
        for rec in probes {
            if blocked_hold.is_some() {
                let text = self.hold_text(blocked_hold.as_ref());
                self.set_wait(rec.id, LedgerWait::Pressure(text.clone()), &text);
                rep.waiting += 1;
            } else if self.launch(policy, rec.clone(), None, None) {
                rep.started.push(rec.id);
            }
        }

        if candidates.is_empty() {
            return;
        }
        let layout = self.layout(policy);
        let waiters: Vec<Waiter> = candidates.values().map(|(_, w)| w.clone()).collect();
        let queue = Queue {
            now_ms: u64::try_from(self.clock.now_ms()).unwrap_or(0),
            last_started: lock(&self.state).last_started.clone(),
        };
        let decisions = match &readings {
            Ok(_) => admitter
                .plan(waiters, &layout, &queue)
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let decisions = match decisions {
            Ok(d) => d,
            Err(e) => {
                // fail closed: nothing starts while the machine cannot be measured
                let text = format!("admission cannot measure the machine: {e}");
                for id in candidates.keys() {
                    self.set_wait(*id, LedgerWait::Pressure(text.clone()), &text);
                }
                rep.waiting += candidates.len();
                return;
            }
        };
        for d in decisions {
            let Some((rec, w)) = candidates.remove(&d.job) else {
                continue;
            };
            match d.verdict {
                Verdict::Admit { slot, lane } => match admitter.try_admit(&w, slot, lane) {
                    Ok(Attempt::Admitted(guard)) => {
                        let ok = if rec.forward.is_some() {
                            self.launch_forwarded(rec, Some(slot), Some(guard))
                        } else {
                            self.launch(policy, rec, Some(slot), Some(guard))
                        };
                        if ok {
                            rep.started.push(d.job);
                            lock(&self.state)
                                .last_started
                                .insert(w.owner.clone(), queue.now_ms);
                        }
                    }
                    Ok(Attempt::Wait(r)) => {
                        self.wait_on(d.job, &r);
                        rep.waiting += 1;
                    }
                    Err(e) => {
                        let text = format!("admission failed: {e}");
                        self.set_wait(d.job, LedgerWait::Pressure(text.clone()), &text);
                        rep.waiting += 1;
                    }
                },
                Verdict::Wait(r) => {
                    self.wait_on(d.job, &r);
                    rep.waiting += 1;
                }
            }
        }
    }

    /// Drain and the execution intent hold every waiter without blocking others' order.
    fn global_hold(&self) -> Option<Hold> {
        if self.draining.load(Ordering::SeqCst) {
            Some(Hold::Drain)
        } else if !self.execution_enabled.load(Ordering::SeqCst) {
            Some(Hold::Other(
                "execution is stopped (start it with `tasks2 execution start`)".to_string(),
            ))
        } else {
            None
        }
    }

    fn hold_text(&self, h: Option<&Hold>) -> String {
        match h {
            Some(Hold::Drain) => "draining: no new jobs start until the daemon restarts".into(),
            Some(Hold::Other(t)) => t.clone(),
            _ => "held".into(),
        }
    }

    fn wait_on(&self, id: JobId, reason: &AdmitWait) {
        // a refused pressure gate is a trip: record what the machine was doing (rate-limited)
        if let Some(resource) = crate::attrib::gate_resource(reason) {
            self.record_trip(resource, "gate", &reason.to_string());
        }
        self.set_wait(id, coarse(reason), &reason.to_string());
    }

    /// Records why a job waits, writing the ledger only when the reason changed.
    pub(crate) fn set_wait(&self, id: JobId, coarse: LedgerWait, text: &str) {
        {
            let st = lock(&self.state);
            if st
                .last_wait
                .get(&id)
                .is_some_and(|(c, t)| *c == coarse && t == text)
            {
                return;
            }
        }
        if self
            .guard("set_wait", self.ledger.set_wait(id, coarse.clone(), text))
            .is_some()
        {
            lock(&self.state)
                .last_wait
                .insert(id, (coarse, text.to_string()));
        }
    }

    /// Starts a job: writes the log's first lines, registers it and spawns its runner thread.
    /// Returns false when the job was not started.
    fn launch(
        self: &Arc<Self>,
        policy: &Arc<MachinePolicy>,
        rec: JobRecord,
        slot: Option<(SlotKind, u32)>,
        guard: Option<SlotGuard>,
    ) -> bool {
        let id = rec.id;
        match self.ledger.cancel_requested(id) {
            Ok(false) => {}
            Ok(true) => return false,
            Err(LedgerError::NotFound(_)) => return false,
            Err(e) => {
                self.note_error(&format!("cancel_requested: {e}"));
                return false;
            }
        }
        let facts = self.facts(&rec);
        let busy = lock(&self.running)
            .values()
            .any(|r| r.slot_kind == Some(SlotKind::Compile));
        let max = policy.cargo_max_jobs();
        let cj = CargoJobs {
            max,
            when_busy: policy.cargo_jobs_when_busy(),
            default: 2.min(max),
        };
        let rewritten = cargo_jobs::rewrite(&rec.spec.argv, &rec.spec.env, &cj, busy);

        let log_path = self.cfg.log_path(id);
        if let Some(dir) = log_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let line = lock(&self.state).placed.get(&id).cloned();
        let mut notes = LogNotes::new();
        let touch = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path);
        if let Err(e) = touch {
            let reason = format!("cannot create the job log {}: {e}", log_path.display());
            self.end_unstarted(id, &reason);
            return false;
        }
        let settings = crate::build_cache::Settings::of(policy);
        let cache = self.build_cache();
        let lookup = |name: &str| {
            rec.spec
                .env
                .iter()
                .chain(self.parent_env.iter())
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        if crate::build_cache::outputs::is_gradle(&rec.spec.argv)
            && lookup("CARGO_TARGET_DIR").is_some()
        {
            self.end_unstarted(
                id,
                &crate::build_cache::CacheError::TargetOverride.to_string(),
            );
            return false;
        }
        let props = crate::build_cache::outputs::ensure_android_local_properties(
            &rec.spec.argv,
            &rec.spec.cwd,
            lookup,
        );
        let props_line = match props {
            Ok(l) => l,
            Err(e) => {
                self.end_unstarted(id, &e.to_string());
                return false;
            }
        };
        let placed = self.plan_outputs(&rec.spec).apply();
        // the cache line comes first in the job's output (R-405)
        let _ = notes.note(&log_path, "cache", &cache.header(&settings, &placed));
        for (i, n) in placed.notes.iter().chain(props_line.iter()).enumerate() {
            let _ = notes.note(&log_path, &format!("cache-note-{i}"), n);
        }
        if let Some(l) = line {
            let _ = notes.note(&log_path, "placement", &format!("placement: {l}"));
        }
        if let Some(n) = &rewritten.note {
            let _ = notes.note(&log_path, "cargo-jobs", n);
        }
        // what the submit rules allowed with a warning (a broad test in `warn` mode, or one
        // deliberately escaped) stays visible with the job (R-191)
        let via = if rec.origin.is_some() {
            Via::Peer
        } else {
            Via::Local
        };
        if let Ok(warnings) = self.check_spec(&rec.spec, via) {
            for (i, w) in warnings.iter().enumerate() {
                let _ = notes.note(
                    &log_path,
                    &format!("submit-rule-{i}"),
                    &format!("warning: {w}"),
                );
            }
        }

        let slot_kind = tasks_placement::slot_for(rec.spec.kind);
        let rj = Arc::new(RunningJob {
            id,
            owner: rec.spec.owner.as_str().to_string(),
            command: shorten(&rec.spec.argv.join(" ")),
            slot_kind,
            slot,
            checkout: facts.checkout.clone(),
            token: CancelToken::new(),
            started: Instant::now(),
            log_path: log_path.clone(),
            group: Mutex::new(None),
            timeline: Mutex::new(Timeline::new()),
            estimates: self.build_estimates(&facts),
            kill_reason: Mutex::new(None),
            stuck: Mutex::new(StuckDetector::new(policy.stuck_window_secs())),
            est_median: facts
                .known_median
                .or(facts.est.as_ref().map(|e| e.median_secs)),
            figures: Mutex::new(None),
        });
        // the spec's own limit, else a plan step's hint (older rows), capped by the machine's
        let wall = crate::machine_policy::effective_wall_timeout(
            rec.spec
                .timeout_secs
                .or(rec.hints.timeout_secs)
                .map(Duration::from_secs),
            policy.wall_timeout(),
        );
        let run_policy = RunPolicy {
            cancel_grace: policy.cancel_grace(),
            wall_timeout: wall,
            idle_timeout: policy.idle_timeout(),
            cancel_on_client_loss: false,
            poll: self.runner_poll,
        };
        let start = JobStart {
            job: id,
            argv: rewritten.argv,
            cwd: rec.spec.cwd.clone(),
            env: rewritten.env,
            log_path,
            timed_path: None,
        };
        let meta = JobMeta {
            kind: rec.spec.kind,
            recipe: facts.recipe.clone(),
            piece: rec.hints.split.clone(),
        };
        lock(&self.running).insert(id, rj.clone());
        lock(&self.state).last_wait.remove(&id);
        let env_policy = cache.env_policy(&settings, Some(cj.default));
        let sch = self.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("job-{}", id.0))
            .spawn(move || runjob::run_job(sch, rj, start, guard, run_policy, env_policy, meta));
        if let Err(e) = spawned {
            lock(&self.running).remove(&id);
            self.end_unstarted(id, &format!("cannot start a runner thread: {e}"));
            return false;
        }
        true
    }

    /// The output links a job would get, planned without creating anything.
    pub(crate) fn plan_outputs(
        &self,
        spec: &tasks_core::JobSpec,
    ) -> crate::build_cache::outputs::OutputPlan {
        let cache = self.build_cache();
        crate::build_cache::outputs::plan(cache.root(), cache.peer_store(), &spec.cwd, &spec.argv)
    }

    pub(crate) fn end_unstarted(&self, id: JobId, reason: &str) {
        let mut fin = Finish::from_phase(Phase::NotStarted {
            reason: reason.to_string(),
        });
        fin.summary = Some(reason.to_string());
        let _ = self.guard("finish", self.ledger.finish(id, &fin));
    }
}

/// The coarse wait reason stored beside the full text.
fn coarse(r: &AdmitWait) -> LedgerWait {
    match r {
        AdmitWait::SlotsBusy { kind, .. } | AdmitWait::SlotTaken(kind, _) => {
            LedgerWait::Slot(*kind)
        }
        AdmitWait::BehindCheckout(j) | AdmitWait::Held(Hold::SameCheckoutBusy(j)) => {
            LedgerWait::Dependency(*j)
        }
        AdmitWait::Held(Hold::Dependency(j)) => LedgerWait::Dependency(*j),
        AdmitWait::Held(_) => LedgerWait::Pressure(r.to_string()),
        AdmitWait::DeviceBusy { device, .. } | AdmitWait::DeviceUnknown(device) => {
            LedgerWait::Device(device.clone())
        }
        AdmitWait::NoDevice => LedgerWait::Device(tasks_core::DeviceId(String::new())),
        AdmitWait::Disk { .. } => LedgerWait::Disk,
        AdmitWait::Cpu { .. }
        | AdmitWait::CpuUnavailable(_)
        | AdmitWait::Io { .. }
        | AdmitWait::IoUnavailable(_)
        | AdmitWait::MemoryStalls { .. }
        | AdmitWait::MemoryLow { .. } => LedgerWait::Pressure(r.to_string()),
    }
}

/// A command shortened for listings.
pub(crate) fn shorten(s: &str) -> String {
    if s.chars().count() > 100 {
        let head: String = s.chars().take(97).collect();
        format!("{head}...")
    } else {
        s.to_string()
    }
}
