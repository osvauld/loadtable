//! Attribution of stalls (design §9): per-job figures, the biggest processes outside every job,
//! pressure trips, and the transfer slot.
//!
//! Whenever an admission gate refuses (CPU, memory or I/O), or machine pressure crosses the alert
//! threshold, one `pressure.trip` audit row records what the machine looked like: its pressure,
//! the figures of every running job, the transfer cgroup, and the ten processes outside any job
//! that moved the most disk bytes and held the most memory over the last sample window. Names
//! only: a process is recorded by its program name, never its arguments or environment.

use crate::api::{
    TransferView, TripDetails, TripJob, TripMachine, TripProc, TripTransfer, TripView,
};
use crate::machine_policy::MachinePolicy;
use crate::scheduler::{Scheduler, lock};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tasks_admission::{Attempt, Lane, WaitReason, Waiter};
use tasks_core::{JobId, SlotKind};
use tasks_ledger::{AuditEvent, AuditQuery, action};
use tasks_peer::sync::{SyncGate, SyncPhase};
use tasks_platform::{
    CgroupName, Level, OutsideSampler, ProcInfo, SlotGuard, TopOutside, tree_pids,
};

/// Newest trips the status lists.
pub const STATUS_TRIPS: usize = 5;

/// Sync work seen through the transfer slot.
#[derive(Debug, Default, Clone)]
pub(crate) struct TransferStats {
    pub active: u32,
    pub waiting: Option<String>,
    pub runs: u64,
    pub read: u64,
    pub write: u64,
}

/// Everything attribution keeps between ticks.
pub(crate) struct AttribState {
    pub sampler: OutsideSampler,
    pub window: TopOutside,
    pub sampled_ms: i64,
    pub transfer: TransferStats,
}

impl AttribState {
    pub(crate) fn new() -> AttribState {
        AttribState {
            sampler: OutsideSampler::new(),
            window: TopOutside::default(),
            sampled_ms: 0,
            transfer: TransferStats::default(),
        }
    }
}

/// Which resource a refused gate is about; `None` for reasons that are not pressure (slots, disk,
/// holds) and for a gate whose reading is missing.
pub(crate) fn gate_resource(r: &WaitReason) -> Option<&'static str> {
    match r {
        WaitReason::Io { .. } => Some("io"),
        WaitReason::Cpu { .. } => Some("cpu"),
        WaitReason::MemoryStalls { .. } | WaitReason::MemoryLow { .. } => Some("memory"),
        _ => None,
    }
}

fn proc_row(p: &tasks_platform::TopProc) -> TripProc {
    TripProc {
        pid: p.pid,
        name: p.name.clone(),
        io_bytes: p.io_bytes,
        rss_bytes: p.rss_bytes,
    }
}

impl Scheduler {
    /// The attribution line for the status.
    pub(crate) fn attribution_line(&self) -> String {
        self.platform.cgroups.status().line()
    }

    /// The pids of every running job's tree, so the outside ranking leaves them out.
    fn job_pids(&self, table: &[ProcInfo]) -> BTreeSet<i32> {
        let mut out = BTreeSet::new();
        for rj in lock(&self.running).values() {
            if let Some(g) = *lock(&rj.group) {
                out.extend(tree_pids(table, g.pgid, g.pgid));
            }
            if let Ok(p) = self.platform.cgroups.pids(CgroupName::Job(rj.id.0)) {
                out.extend(p);
            }
        }
        out
    }

    /// Takes a window sample at `now_ms`.
    fn sample_outside(&self, now_ms: i64) {
        let Ok(table) = self.platform.table.snapshot() else {
            return;
        };
        let in_jobs = self.job_pids(&table);
        let mut st = lock(&self.attrib);
        st.window = st.sampler.sample(now_ms, &table, &in_jobs);
        st.sampled_ms = now_ms;
    }

    /// Takes one outside-process window sample now. The daemon calls this from a thread of its
    /// own every `attribution.sample_secs` ([`Scheduler::run_sampler`]), never from the
    /// scheduling tick: reading every process is the one slow thing attribution does.
    pub fn sample_now(&self) {
        self.sample_outside(self.clock.now_ms());
    }

    /// Samples until shutdown, one window per `attribution.sample_secs`.
    pub fn run_sampler(&self) {
        while !self.is_shutdown() {
            self.sample_now();
            let mut left = Duration::from_secs(self.policy().attrib_sample_secs());
            while !left.is_zero() && !self.is_shutdown() {
                let step = left.min(Duration::from_millis(250));
                std::thread::sleep(step);
                left -= step;
            }
        }
    }

    /// Records a trip when machine pressure is over the alert threshold. Runs after admission,
    /// so a refused gate (which names the waiting job's slot kind) is the first row of a window.
    pub(crate) fn psi_trips(&self, policy: &MachinePolicy) {
        let Ok(p) = self.platform.pressure.snapshot(&policy.psi()) else {
            return;
        };
        if let (Some(limit), Some(io)) = (policy.trip_io_pct(), p.io)
            && io >= limit
        {
            self.record_trip(
                "io",
                "psi",
                &format!("I/O pressure {io:.0}% >= {limit:.0}%"),
            );
        }
        if let (Some(limit), Some(cpu)) = (policy.trip_cpu_pct(), p.cpu)
            && cpu >= limit
        {
            self.record_trip(
                "cpu",
                "psi",
                &format!("CPU pressure {cpu:.0}% >= {limit:.0}%"),
            );
        }
        if p.memory >= Level::Warn {
            self.record_trip(
                "memory",
                "psi",
                &format!("memory pressure level {}", level_word(p.memory)),
            );
        }
    }

    fn trip_machine(&self, policy: &MachinePolicy) -> TripMachine {
        let mut m = TripMachine::default();
        if let Ok(p) = self.platform.pressure.snapshot(&policy.psi()) {
            m.io_pct = p.io;
            m.cpu_pct = p.cpu;
            m.mem_some_pct = p.memory_some;
            m.mem_full_pct = p.memory_full;
            m.mem_level = level_word(p.memory).to_string();
        }
        m.available_bytes = self
            .platform
            .memory
            .snapshot()
            .ok()
            .map(|s| s.available_bytes);
        m
    }

    /// The transfer work as seen now (the daemon's own counters plus the transfer cgroup).
    fn trip_transfer(&self) -> TripTransfer {
        let st = lock(&self.attrib).transfer.clone();
        TripTransfer {
            active: st.active,
            inproc_read_bytes: st.read,
            inproc_write_bytes: st.write,
            figures: self.platform.cgroups.read(CgroupName::Transfer).ok(),
        }
    }

    /// Builds the details of a trip now.
    pub(crate) fn trip_details(
        &self,
        resource: &str,
        source: &str,
        reason: &str,
        suppressed: u64,
    ) -> TripDetails {
        let policy = self.policy();
        // The window comes from the sampler thread. A trip before its first sample has an
        // empty one rather than reading every process on the scheduling tick.
        let window = lock(&self.attrib).window.clone();
        let jobs: Vec<TripJob> = lock(&self.running)
            .values()
            .filter_map(|rj| {
                lock(&rj.figures).clone().map(|figures| TripJob {
                    job: rj.id.0,
                    figures,
                })
            })
            .collect();
        TripDetails {
            resource: resource.to_string(),
            source: source.to_string(),
            reason: reason.to_string(),
            suppressed,
            attribution: self.attribution_line(),
            machine: self.trip_machine(&policy),
            jobs,
            transfer: Some(self.trip_transfer()),
            window_ms: window.window_ms,
            top_io: window.by_io.iter().map(proc_row).collect(),
            top_rss: window.by_rss.iter().map(proc_row).collect(),
            unreadable: window.unreadable,
        }
        .bounded()
    }

    /// Records a `pressure.trip` audit row, at most one per resource per
    /// `attribution.trip_interval_secs`; the next row that is let through says how many were
    /// dropped. Cheap when throttled: nothing is measured.
    pub(crate) fn record_trip(&self, resource: &str, source: &str, reason: &str) {
        let now = self.clock.now_ms();
        let Some(suppressed) = self.trip_throttle.admit(resource, now) else {
            return;
        };
        let d = self.trip_details(resource, source, reason, suppressed);
        let mut ev = AuditEvent::new(action::PRESSURE_TRIP).target(resource);
        if let Ok(serde_json::Value::Object(map)) = serde_json::to_value(&d) {
            for (k, v) in map {
                ev = ev.detail(&k, v);
            }
        }
        self.audit(ev);
    }

    /// The latest trips, newest first, read back from the audit log.
    pub(crate) fn trip_views(&self, n: usize) -> Vec<TripView> {
        let Some(sink) = self.audit_sink() else {
            return Vec::new();
        };
        let q = AuditQuery {
            since_ms: None,
            until_ms: None,
            peer: None,
            job: None,
            action: Some(action::PRESSURE_TRIP.to_string()),
            limit: n,
        };
        let rows = sink.log().query(&q).unwrap_or_default();
        rows.into_iter()
            .filter_map(|r| {
                let d: TripDetails = serde_json::from_value(r.details_json()).ok()?;
                Some(TripView {
                    seq: r.seq,
                    at_ms: r.at_ms,
                    resource: d.resource.clone(),
                    source: d.source.clone(),
                    summary: d.summary(),
                    suppressed: d.suppressed,
                })
            })
            .collect()
    }

    /// The transfer section of the status.
    pub(crate) fn transfer_view(&self) -> Option<TransferView> {
        let st = lock(&self.attrib).transfer.clone();
        let figures = self.platform.cgroups.read(CgroupName::Transfer).ok();
        if st.runs == 0 && st.waiting.is_none() && figures.is_none() {
            return None;
        }
        Some(TransferView {
            active: st.active,
            waiting: st.waiting,
            runs: st.runs,
            inproc_read_bytes: st.read,
            inproc_write_bytes: st.write,
            figures,
        })
    }

    /// Takes a transfer slot for `phase`, waiting while a gate refuses or the slots are busy.
    /// A transfer is local disk work, so unlike the offload slot it obeys the I/O gate. Waiting
    /// is unbounded by design (a sync during a stall should not add to it) but ends when the
    /// daemon shuts down. A refusing gate records a trip like any other.
    pub(crate) fn enter_transfer(self: &Arc<Self>, phase: SyncPhase) -> Box<dyn Send> {
        let mut backoff = Duration::from_millis(100);
        loop {
            if self.is_shutdown() {
                return Box::new(());
            }
            let policy = self.policy();
            let admitter = self.admitter(&policy);
            let count = policy.slot_count(SlotKind::Transfer).max(1);
            let waiter = Waiter::new(JobId(0), SlotKind::Transfer, 0);
            let mut why: Option<WaitReason> = None;
            for index in 0..count {
                match admitter.try_admit(&waiter, (SlotKind::Transfer, index), Lane::Normal) {
                    Ok(Attempt::Admitted(slot)) => {
                        return Box::new(TransferGuard::new(self.clone(), slot));
                    }
                    Ok(Attempt::Wait(r @ WaitReason::SlotTaken(..))) => why = Some(r),
                    Ok(Attempt::Wait(r)) => {
                        // a gate refused: every slot would be refused the same way
                        why = Some(r);
                        break;
                    }
                    Err(e) => {
                        lock(&self.attrib).transfer.waiting = Some(format!(
                            "{}: admission cannot measure the machine: {e}",
                            phase.word()
                        ));
                        break;
                    }
                }
            }
            if let Some(r) = &why {
                lock(&self.attrib).transfer.waiting = Some(format!("{}: {r}", phase.word()));
                if let Some(res) = gate_resource(r) {
                    self.record_trip(res, "transfer", &r.to_string());
                }
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(1));
        }
    }
}

fn level_word(l: Level) -> &'static str {
    match l {
        Level::Normal => "normal",
        Level::Warn => "warn",
        Level::Critical => "critical",
    }
}

/// A held transfer slot. Counts the daemon process's own disk bytes while it is held, because
/// snapshotting and applying run in the daemon (the `git` children are in the transfer cgroup).
struct TransferGuard {
    _slot: SlotGuard,
    sch: Arc<Scheduler>,
    io0: Option<(u64, u64)>,
}

impl TransferGuard {
    fn new(sch: Arc<Scheduler>, slot: SlotGuard) -> TransferGuard {
        let io0 = sch.own_io();
        {
            let mut st = lock(&sch.attrib);
            st.transfer.active += 1;
            st.transfer.runs += 1;
            st.transfer.waiting = None;
        }
        TransferGuard {
            _slot: slot,
            sch,
            io0,
        }
    }
}

impl Drop for TransferGuard {
    fn drop(&mut self) {
        let io1 = self.sch.own_io();
        let mut st = lock(&self.sch.attrib);
        st.transfer.active = st.transfer.active.saturating_sub(1);
        if let (Some(a), Some(b)) = (self.io0, io1) {
            st.transfer.read = st.transfer.read.saturating_add(b.0.saturating_sub(a.0));
            st.transfer.write = st.transfer.write.saturating_add(b.1.saturating_sub(a.1));
        }
    }
}

impl Scheduler {
    fn own_io(&self) -> Option<(u64, u64)> {
        let pid = i32::try_from(std::process::id()).ok()?;
        self.platform.table.io_of(pid)
    }
}

/// The [`SyncGate`] the peer runtime hands to the sync code. It is created before the scheduler
/// (the peer runtime starts first) and bound to it afterwards; unbound, it admits at once.
#[derive(Default)]
pub struct TransferGate {
    sch: Mutex<Weak<Scheduler>>,
}

impl TransferGate {
    /// An unbound gate.
    pub fn new() -> Arc<TransferGate> {
        Arc::new(TransferGate::default())
    }

    /// Binds the gate to the scheduler whose admission it uses.
    pub fn bind(&self, sch: &Arc<Scheduler>) {
        *lock(&self.sch) = Arc::downgrade(sch);
    }
}

impl SyncGate for TransferGate {
    fn enter(&self, phase: SyncPhase) -> Box<dyn Send> {
        let sch = lock(&self.sch).upgrade();
        match sch {
            Some(s) => s.enter_transfer(phase),
            None => Box::new(()),
        }
    }
}
