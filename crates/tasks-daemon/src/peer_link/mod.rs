//! The peer adapter: [`PeerLink`] implements [`Forwarder`] over the peer crate.
//!
//! One [`tasks_peer::Forward`] state machine per forwarded job, driven on a thread of the
//! adapter's own, over any [`Transport`] (the in-memory fake in tests, iroh behind the daemon's
//! `iroh` feature). The scheduler talks to the adapter only through cheap, non-blocking calls:
//!
//! - [`Forwarder::forward`] registers the job in the per-checkout queue (no network);
//! - [`Forwarder::ready_to_start`] asks the `Forward` whether its checkout lock is held or would
//!   be granted, with `try_lock`, so it never waits for a sync in progress;
//! - [`Forwarder::start`] spawns the job's driver thread; [`Forwarder::poll`] reads what the
//!   driver last published.
//!
//! Peer status is probed by a second thread (or by [`PeerLink::probe_all`] in tests), so
//! [`Forwarder::peers`] and [`Forwarder::links`] read a cache.
//!
//! The receiving half (a daemon hosting `tasks_peer::Receiver`) is in [`serve`].

mod drive;
pub mod fetch;
pub mod metrics;
pub mod serve;
pub mod sync_note;

#[cfg(feature = "iroh")]
pub mod iroh_host;

use crate::forwarder::{
    ForwardEnd, ForwardError, ForwardPoll, ForwardProgress, ForwardTicket, Forwarder, LinkQuality,
    LinkReport, LinkState,
};
use crate::peers_config::PeerSpec;
use crate::scheduler::lock;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};
use tasks_core::{Arch, JobId, Machine, MachineName, Os, SlotKind};
use tasks_ledger::JobRecord;
use tasks_peer::proto::{Body, PeerStatus};
use tasks_peer::sync::{SyncDriver, SyncGate};
use tasks_peer::transport::{LinkInfo as PathInfo, PeerError, Transport};
use tasks_peer::{
    CheckoutId, CheckoutLocks, Forward, ForwardJob, ForwardPolicy, LockOwner, PeerId, Refusal,
    RemoteJobId,
};
use tasks_placement::{Checkout, Link, MachineView, Pressure};

/// Where the adapter learns its peers: the paired machines. Asked on every probe round and when
/// a name is not known yet, so a machine paired (or revoked) while the daemon runs is picked up
/// without a restart.
pub trait PeerSource: Send + Sync {
    /// The machines this daemon may forward to.
    fn peers(&self) -> Vec<PeerSpec>;
}

/// A change of a peer link worth keeping a record of. Probes that find the link as it was are
/// not changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkChange {
    /// The peer answered after not answering (or for the first time).
    Up {
        /// `direct`, `relay` or `unknown`.
        path: String,
    },
    /// The peer stopped answering.
    Down {
        /// The probe's error.
        reason: String,
    },
    /// The link moved between a direct path and the relay.
    Path {
        /// The path before.
        from: String,
        /// The path now.
        to: String,
    },
}

/// Told when a peer link changes state; the audit log plugs in here.
pub trait LinkObserver: Send + Sync {
    /// `peer`'s link changed.
    fn changed(&self, peer: &PeerSpec, change: LinkChange);
}

/// Reads the files that must travel with a checkout beyond its git working tree from the repo's
/// run policy (`[sync] extra` in `.tasks/run-policy.toml`). No policy file means none.
pub struct PolicyExtras;

impl tasks_peer::sync::ExtrasProvider for PolicyExtras {
    fn extras(&self, root: &Path) -> Result<Vec<String>, String> {
        match tasks_policy::load(&tasks_policy::RealFs, root) {
            Ok(loaded) => Ok(loaded.policy.sync.extra),
            Err(tasks_policy::PolicyError::NotFound(_)) => Ok(Vec::new()),
            Err(e) => Err(format!("the repo run policy cannot be used: {e}")),
        }
    }
}

/// How the adapter behaves. [`PeerLinkOptions::new`] gives the production values.
#[derive(Clone)]
pub struct PeerLinkOptions {
    /// This machine's name; part of every checkout id and duplicate-submit key.
    pub local_name: String,
    /// Peers known at start (tests); [`PeerLinkOptions::source`] adds the paired ones.
    pub peers: Vec<PeerSpec>,
    /// The paired machines, kept current while the daemon runs.
    pub source: Option<Arc<dyn PeerSource>>,
    /// Forward timeouts and retry knobs.
    pub policy: ForwardPolicy,
    /// Snapshots a checkout and syncs it to a worker (git objects over the link).
    pub sync: Arc<dyn SyncDriver>,
    /// How often peer status is probed.
    pub status_every: Duration,
    /// A peer that answered but not this recently is `Stale`.
    pub stale_after: Duration,
    /// How long a status probe may take to connect.
    pub probe_timeout: Duration,
    /// How often a driver thread ticks its forward.
    pub tick_every: Duration,
    /// Run the status prober on a thread. Tests turn it off and call [`PeerLink::probe_all`].
    pub background_status: bool,
    /// Told when a link comes up, goes down or changes path.
    pub observer: Option<Arc<dyn LinkObserver>>,
    /// Held while fetched outputs are written into a checkout: the transfer slot and I/O gate
    /// (design: sync and fetch both take a transfer slot).
    pub gate: Arc<dyn SyncGate>,
    /// Paths from a checkout root that are coordinator-placed output links (`target`,
    /// `android/app/build`): a fetch writes through these and through no other symlink.
    pub fetch_links: Vec<String>,
    /// Largest output file fetched back.
    pub fetch_max_bytes: u64,
    /// The longest a request for a forwarded job's metrics may take.
    pub metrics_timeout: Duration,
}

impl PeerLinkOptions {
    /// Production values: probe every 5 s, stale after 20 s, a driver tick every 50 ms.
    pub fn new(
        local_name: &str,
        peers: Vec<PeerSpec>,
        sync: Arc<dyn SyncDriver>,
    ) -> PeerLinkOptions {
        PeerLinkOptions {
            local_name: local_name.to_string(),
            peers,
            source: None,
            policy: ForwardPolicy::default(),
            sync,
            status_every: Duration::from_secs(5),
            stale_after: Duration::from_secs(20),
            probe_timeout: Duration::from_secs(5),
            tick_every: Duration::from_millis(50),
            background_status: true,
            observer: None,
            gate: Arc::new(tasks_peer::sync::OpenGate),
            fetch_links: Vec::new(),
            fetch_max_bytes: tasks_peer::outputs::FetchPolicy::default().max_file_bytes,
            metrics_timeout: Duration::from_secs(2),
        }
    }
}

/// What a driver thread publishes for the scheduler.
#[derive(Default)]
pub(crate) struct Published {
    pub started: bool,
    pub progress: Option<ForwardProgress>,
    pub end: Option<ForwardEnd>,
    pub cancel: Option<String>,
}

/// One registered job.
pub(crate) struct Job {
    pub id: JobId,
    pub checkout: CheckoutId,
    /// The checkout root on this machine, where fetched outputs land.
    pub root: PathBuf,
    /// The peer running the job.
    pub peer: PeerId,
    /// Output files to bring back when the job passes (`--fetch`).
    pub fetch: Vec<String>,
    /// The checkout lock a resumed job holds while it is followed, so later forwards of the
    /// same checkout wait for it instead of syncing over its files.
    pub resumed_lock: Option<(CheckoutId, LockOwner)>,
    pub ticket: ForwardTicket,
    /// `None` for a job resumed after a restart (no checkout lock, no submit).
    pub fwd: Mutex<Option<Forward>>,
    pub published: Mutex<Published>,
}

struct LinkInfo {
    spec: PeerSpec,
    /// False once the machine is no longer paired: it is not probed and shows as offline.
    active: bool,
    path: PathInfo,
    /// Since the adapter's epoch.
    last_ok: Option<Duration>,
    probed: bool,
    ok: bool,
    error: String,
    status: Option<PeerStatus>,
    /// How long the last status request took to be answered.
    reply: Option<Duration>,
}

pub(crate) struct Inner<T: Transport> {
    pub transport: T,
    pub opts: PeerLinkOptions,
    pub locks: Arc<CheckoutLocks>,
    pub jobs: Mutex<BTreeMap<JobId, Arc<Job>>>,
    links: Mutex<BTreeMap<String, LinkInfo>>,
    metrics_cache: Mutex<BTreeMap<(String, i64), metrics::Cached>>,
    epoch: Instant,
    skew: Mutex<Duration>,
    pub stop: AtomicBool,
}

impl<T: Transport> Inner<T> {
    /// Time since the adapter started, plus any test skew.
    pub fn now(&self) -> Duration {
        self.epoch.elapsed() + *lock(&self.skew)
    }
}

/// The [`Forwarder`] over a [`Transport`].
pub struct PeerLink<T: Transport + 'static> {
    inner: Arc<Inner<T>>,
}

impl<T: Transport + 'static> PeerLink<T> {
    /// Builds the adapter. With `background_status` a thread probes the peers until the adapter
    /// is dropped.
    pub fn new(transport: T, opts: PeerLinkOptions) -> PeerLink<T> {
        let links = opts
            .peers
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    LinkInfo {
                        spec: p.clone(),
                        active: true,
                        path: PathInfo::UNKNOWN,
                        last_ok: None,
                        probed: false,
                        ok: false,
                        error: String::new(),
                        status: None,
                        reply: None,
                    },
                )
            })
            .collect();
        let inner = Arc::new(Inner {
            transport,
            opts,
            locks: Arc::new(CheckoutLocks::new()),
            jobs: Mutex::new(BTreeMap::new()),
            links: Mutex::new(links),
            metrics_cache: Mutex::new(BTreeMap::new()),
            epoch: Instant::now(),
            skew: Mutex::new(Duration::ZERO),
            stop: AtomicBool::new(false),
        });
        if inner.opts.background_status {
            let i = inner.clone();
            // a failed spawn leaves every peer `Unknown`, which placement treats as offline
            let _ = std::thread::Builder::new()
                .name("peer-status".into())
                .spawn(move || {
                    while !i.stop.load(Ordering::SeqCst) {
                        probe_all(&i);
                        let mut waited = Duration::ZERO;
                        while waited < i.opts.status_every && !i.stop.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(50));
                            waited += Duration::from_millis(50);
                        }
                    }
                });
        }
        PeerLink { inner }
    }

    /// Probes every peer's status now (blocking). The background thread calls this on its
    /// period.
    pub fn probe_all(&self) {
        probe_all(&self.inner);
    }

    /// Registers and starts a job in one go, as the scheduler would. For tests.
    #[cfg(test)]
    pub fn forward_and_start(&self, rec: &JobRecord, log: &Path) {
        if let Some(fw) = rec.forward.as_ref() {
            let _ = self.forward(rec, &fw.peer);
            let _ = self.start(rec.id, log);
        }
    }

    /// Stops every thread of the adapter without ending any job: the process "dies". For tests
    /// of a daemon restart.
    pub fn shutdown(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
    }

    /// Moves the adapter's clock forward: staleness and every forward timeout see it. For tests.
    pub fn advance_clock(&self, by: Duration) {
        *lock(&self.inner.skew) += by;
    }

    fn job(&self, id: JobId) -> Option<Arc<Job>> {
        lock(&self.inner.jobs).get(&id).cloned()
    }

    /// True when the last probe of `name` failed (an unprobed link is not "down" yet).
    fn link_is_down(&self, name: &str) -> bool {
        lock(&self.inner.links)
            .get(name)
            .is_some_and(|l| l.probed && !l.ok)
    }

    fn spec_of(&self, name: &str) -> Option<PeerSpec> {
        let find = || {
            lock(&self.inner.links)
                .get(name)
                .filter(|l| l.active)
                .map(|l| l.spec.clone())
        };
        find().or_else(|| {
            sync_peers(&self.inner);
            find()
        })
    }
}

impl<T: Transport + 'static> Drop for PeerLink<T> {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
    }
}

/// Brings the link table in line with the paired machines: new ones appear, ones that are no
/// longer paired go inactive (kept, so placement sees them offline with the reason).
fn sync_peers<T: Transport>(inner: &Inner<T>) {
    let Some(src) = &inner.opts.source else {
        return;
    };
    let wanted = src.peers();
    let mut links = lock(&inner.links);
    for spec in &wanted {
        match links.get_mut(&spec.name) {
            Some(l) => {
                l.active = true;
                l.spec = spec.clone();
            }
            None => {
                links.insert(
                    spec.name.clone(),
                    LinkInfo {
                        spec: spec.clone(),
                        active: true,
                        path: PathInfo::UNKNOWN,
                        last_ok: None,
                        probed: false,
                        ok: false,
                        error: String::new(),
                        status: None,
                        reply: None,
                    },
                );
            }
        }
    }
    for l in links.values_mut() {
        if l.active && !wanted.iter().any(|w| w.name == l.spec.name) && inner.opts.source.is_some()
        {
            l.active = false;
            l.ok = false;
            l.probed = true;
            l.error = "no longer paired (grant revoked or expired)".to_string();
        }
    }
}

fn probe_all<T: Transport>(inner: &Inner<T>) {
    sync_peers(inner);
    let specs: Vec<PeerSpec> = lock(&inner.links)
        .values()
        .filter(|l| l.active)
        .map(|l| l.spec.clone())
        .collect();
    for spec in specs {
        let result = probe(inner, &spec.node_id);
        let now = inner.now();
        let mut change = None;
        if let Some(l) = lock(&inner.links).get_mut(&spec.name) {
            let was_up = l.probed.then_some(l.ok);
            let old_path = l.path.path.name();
            l.probed = true;
            match result {
                Ok((status, path, reply)) => {
                    let new_path = path.path.name();
                    change = if was_up != Some(true) {
                        Some(LinkChange::Up {
                            path: new_path.to_string(),
                        })
                    } else if old_path != new_path && old_path != "unknown" && new_path != "unknown"
                    {
                        Some(LinkChange::Path {
                            from: old_path.to_string(),
                            to: new_path.to_string(),
                        })
                    } else {
                        None
                    };
                    l.ok = true;
                    l.last_ok = Some(now);
                    l.error.clear();
                    l.status = Some(status);
                    l.path = path;
                    l.reply = Some(reply);
                }
                Err(e) => {
                    if was_up != Some(false) {
                        change = Some(LinkChange::Down {
                            reason: e.to_string(),
                        });
                    }
                    l.ok = false;
                    l.error = e.to_string();
                }
            }
        }
        // told outside the lock: an observer may call back into the adapter
        if let (Some(c), Some(o)) = (change, &inner.opts.observer) {
            o.changed(&spec, c);
        }
    }
}

fn probe<T: Transport>(
    inner: &Inner<T>,
    node: &PeerId,
) -> Result<(PeerStatus, PathInfo, Duration), PeerError> {
    let mut conn = inner.transport.connect(node, inner.opts.probe_timeout)?;
    // wall time on purpose: the adapter's clock may be a test clock, and this measures the wire
    let sent = std::time::Instant::now();
    let body = conn.request(Body::StatusRequest)?;
    let reply = sent.elapsed();
    match body {
        Body::Status(s) => Ok((s, conn.link_info(), reply)),
        other => Err(PeerError::Protocol(format!(
            "answered a status request with {}",
            tasks_peer::fake::kind(&other)
        ))),
    }
}

fn link_state<T: Transport>(inner: &Inner<T>, l: &LinkInfo) -> LinkState {
    if !l.probed {
        LinkState::Unknown
    } else if !l.ok {
        LinkState::Offline
    } else {
        let age = inner.now().saturating_sub(l.last_ok.unwrap_or_default());
        if age > inner.opts.stale_after {
            LinkState::Stale
        } else {
            LinkState::Up
        }
    }
}

/// A machine record for a peer that has not answered yet. Placement never uses it: the link is
/// down.
fn placeholder(name: &str) -> Machine {
    Machine {
        name: MachineName(name.to_string()),
        os: Os::current(),
        arch: Arch::current(),
        cores: 1,
        memory_bytes: 0,
        tools: BTreeSet::new(),
        devices: BTreeSet::new(),
    }
}

impl<T: Transport + 'static> Forwarder for PeerLink<T> {
    fn peers(&self) -> Vec<MachineView> {
        sync_peers(&self.inner);
        let now = self.inner.now();
        let links = lock(&self.inner.links);
        links
            .values()
            .map(|l| {
                let name = l.spec.name.as_str();
                let seen_ago = l.last_ok.map(|t| now.saturating_sub(t).as_secs());
                let link = if l.ok {
                    Link::Up {
                        age_secs: seen_ago.unwrap_or(0),
                    }
                } else {
                    Link::Down {
                        last_seen_ago_secs: seen_ago,
                    }
                };
                let Some(s) = l.status.as_ref() else {
                    return MachineView {
                        machine: placeholder(name),
                        is_local: false,
                        link,
                        free_slots: BTreeMap::new(),
                        queue_depth: 0,
                        expected_wait_secs: None,
                        pressure: Pressure::default(),
                        estimate: None,
                        checkout: Checkout {
                            synced: false,
                            sync_bytes: 0,
                        },
                        marks: Vec::new(),
                    };
                };
                let mut machine = s.machine.clone();
                // placement and the ledger know the peer by the name this policy gave it
                machine.name = MachineName(name.to_string());
                let free_slots: BTreeMap<SlotKind, u32> = s
                    .slots
                    .iter()
                    .map(|x| (x.kind, x.total.saturating_sub(x.used)))
                    .collect();
                MachineView {
                    machine,
                    is_local: false,
                    link,
                    free_slots,
                    queue_depth: s.queue.waiting,
                    expected_wait_secs: None,
                    // open choice: a peer that states a pressure reason is treated as blocked
                    pressure: Pressure {
                        blocked: !s.pressure.summary.is_empty(),
                        summary: s.pressure.summary.clone(),
                    },
                    estimate: None,
                    // open choice: whether the peer already holds this checkout is not known
                    // per job here; the sync cost is left to the sync itself
                    checkout: Checkout {
                        synced: false,
                        sync_bytes: 0,
                    },
                    marks: Vec::new(),
                }
            })
            .collect()
    }

    fn link_quality(&self) -> Vec<LinkQuality> {
        sync_peers(&self.inner);
        lock(&self.inner.links)
            .values()
            .filter(|l| l.active)
            .map(|l| LinkQuality {
                peer: l.spec.name.clone(),
                path: l.path.path.name().to_string(),
                rtt_ms: l
                    .path
                    .rtt
                    .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
                reply_ms: l
                    .reply
                    .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            })
            .collect()
    }

    fn links(&self) -> Vec<LinkReport> {
        sync_peers(&self.inner);
        let now = self.inner.now();
        lock(&self.inner.links)
            .values()
            .map(|l| LinkReport {
                peer: l.spec.name.clone(),
                node_id: l.spec.node_id.to_string(),
                state: link_state(&self.inner, l),
                last_seen_ago_secs: l.last_ok.map(|t| now.saturating_sub(t).as_secs()),
                detail: l.error.clone(),
                status: l.status.clone(),
            })
            .collect()
    }

    fn forward(&self, rec: &JobRecord, peer: &MachineName) -> Result<ForwardTicket, ForwardError> {
        let Some(spec) = self.spec_of(&peer.0) else {
            return Err(ForwardError::Unavailable(format!(
                "{} is not a configured peer",
                peer.0
            )));
        };
        if let Some(j) = self.job(rec.id) {
            return Ok(j.ticket.clone());
        }
        // A key the ledger already holds wins: a restarted daemon resends it, so the peer
        // deduplicates instead of running the job twice.
        let key = match rec.forward.as_ref().and_then(|f| f.remote_key.clone()) {
            Some(k) => tasks_peer::SubmitKey::new(&k)
                .map_err(|e| ForwardError::Unavailable(format!("stored submit key: {e}")))?,
            None => ForwardJob::default_key(&self.inner.opts.local_name, rec.id.0)
                .map_err(|e| ForwardError::Unavailable(format!("submit key: {e}")))?,
        };
        let root = PathBuf::from(crate::place::checkout_key(&rec.spec.cwd));
        let job = ForwardJob {
            local_id: rec.id.0,
            spec: rec.spec.clone(),
            root: root.clone(),
            origin_host: self.inner.opts.local_name.clone(),
            peer: spec.node_id.clone(),
            key: key.clone(),
        };
        let ticket = ForwardTicket {
            remote_job: rec.forward.as_ref().and_then(|f| f.remote_job),
            remote_key: Some(key.as_str().to_string()),
        };
        // Forward::new takes its place in the checkout queue: registration order is the order.
        let fwd = Forward::new(
            job,
            self.inner.opts.policy.clone(),
            self.inner.locks.clone(),
            self.inner.now(),
        );
        let entry = Arc::new(Job {
            id: rec.id,
            checkout: CheckoutId::derive(&self.inner.opts.local_name, &root),
            root: root.clone(),
            peer: spec.node_id.clone(),
            fetch: rec.spec.fetch.clone(),
            resumed_lock: None,
            ticket: ticket.clone(),
            fwd: Mutex::new(Some(fwd)),
            published: Mutex::new(Published::default()),
        });
        lock(&self.inner.jobs).insert(rec.id, entry);
        Ok(ticket)
    }

    fn registered(&self, job: JobId) -> bool {
        self.job(job).is_some()
    }

    fn ready_to_start(&self, job: JobId) -> bool {
        let Some(j) = self.job(job) else {
            return false;
        };
        match j.fwd.try_lock() {
            Ok(g) => g.as_ref().is_some_and(Forward::ready_to_start),
            // a driver is ticking it: it has been started, so it passed this gate
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Poisoned(_)) => false,
        }
    }

    fn wait_reason(&self, job: JobId) -> Option<String> {
        let j = self.job(job)?;
        // name the job that holds the remote copy, as the old checkout gate did (R-161)
        match self.inner.locks.holder(&j.checkout) {
            Some(h) if h.0 != job.0 => Some(format!(
                "waiting for checkout: job #{} is using the remote copy",
                h.0
            )),
            _ => {
                let g = j.fwd.try_lock().ok()?;
                g.as_ref().map(Forward::mirror)
            }
        }
    }

    fn set_class(&self, job: JobId, priority: tasks_core::Priority, reason: Option<String>) {
        if let Some(j) = self.job(job)
            && let Ok(mut g) = j.fwd.try_lock()
            && let Some(f) = g.as_mut()
        {
            f.set_class(priority, reason);
        }
    }

    fn start(&self, job: JobId, log: &Path) -> Result<(), ForwardError> {
        let Some(j) = self.job(job) else {
            return Err(ForwardError::Unavailable(format!(
                "job {} is not registered for forwarding",
                job.0
            )));
        };
        {
            let mut p = lock(&j.published);
            if p.started {
                return Ok(());
            }
            p.started = true;
        }
        drive::spawn_forward(self.inner.clone(), j, log.to_path_buf())
    }

    fn resume(&self, rec: &JobRecord, log: &Path) -> Result<(), ForwardError> {
        let Some(fw) = rec.forward.as_ref() else {
            return Err(ForwardError::Unavailable("job was not forwarded".into()));
        };
        let Some(spec) = self.spec_of(&fw.peer.0) else {
            return Err(ForwardError::Unavailable(format!(
                "{} is no longer a configured peer",
                fw.peer.0
            )));
        };
        let root = PathBuf::from(crate::place::checkout_key(&rec.spec.cwd));
        let checkout = CheckoutId::derive(&self.inner.opts.local_name, &root);
        let owner = LockOwner(rec.id.0);
        self.inner.locks.enqueue(&checkout, owner);
        let _ = self.inner.locks.try_acquire(&checkout, owner);
        let entry = Arc::new(Job {
            id: rec.id,
            checkout: checkout.clone(),
            root: root.clone(),
            peer: spec.node_id.clone(),
            fetch: rec.spec.fetch.clone(),
            resumed_lock: Some((checkout, owner)),
            ticket: ForwardTicket {
                remote_job: fw.remote_job,
                remote_key: fw.remote_key.clone(),
            },
            fwd: Mutex::new(None),
            published: Mutex::new(Published {
                started: true,
                ..Published::default()
            }),
        });
        lock(&self.inner.jobs).insert(rec.id, entry.clone());
        let remote = fw
            .remote_job
            .and_then(|r| u64::try_from(r).ok())
            .map(RemoteJobId);
        drive::spawn_resume(
            self.inner.clone(),
            entry,
            spec.node_id,
            remote,
            log.to_path_buf(),
        )
    }

    fn poll(&self, job: JobId) -> ForwardPoll {
        let Some(j) = self.job(job) else {
            return ForwardPoll::Done(lost_end(&format!(
                "job {} is not known to the peer link",
                job.0
            )));
        };
        let p = lock(&j.published);
        if let Some(end) = p.end.clone() {
            drop(p);
            lock(&self.inner.jobs).remove(&job);
            return ForwardPoll::Done(end);
        }
        ForwardPoll::Active(p.progress.clone().unwrap_or(ForwardProgress {
            mirror: if p.started {
                "starting".to_string()
            } else {
                "waiting for a slot".to_string()
            },
            remote_job: None,
            accepted: false,
            remote_phase: None,
        }))
    }

    fn cancel(&self, job: JobId, reason: &str) {
        if let Some(j) = self.job(job) {
            lock(&j.published).cancel = Some(reason.to_string());
        }
    }

    fn cancel_remote(&self, peer: &MachineName, remote_job: i64) -> Result<(), ForwardError> {
        let Some(spec) = self.spec_of(&peer.0) else {
            return Err(ForwardError::Unavailable(format!(
                "{} is not a configured peer",
                peer.0
            )));
        };
        let remote = u64::try_from(remote_job)
            .map(RemoteJobId)
            .map_err(|_| ForwardError::Refused(format!("bad remote job id {remote_job}")))?;
        let unreachable = |e: PeerError| {
            ForwardError::Unavailable(format!("cannot cancel {remote} on {}: {e}", peer.0))
        };
        let mut conn = self
            .inner
            .transport
            .connect(&spec.node_id, self.inner.opts.probe_timeout)
            .map_err(unreachable)?;
        match conn.request(Body::Cancel { remote }) {
            Ok(Body::CancelAck { .. }) => Ok(()),
            // the peer no longer knows it: the job is gone, which is what the caller needs
            Err(PeerError::Refused(Refusal::UnknownJob { .. })) => Ok(()),
            Ok(_) => Err(ForwardError::Unavailable(format!(
                "{} answered a cancel with the wrong message",
                peer.0
            ))),
            Err(e) => Err(unreachable(e)),
        }
    }

    fn remote_metrics(
        &self,
        peer: &MachineName,
        remote_job: i64,
    ) -> Result<tasks_core::metrics::JobMetricsView, ForwardError> {
        self.remote_metrics_of(peer, remote_job)
    }

    fn forget(&self, job: JobId) {
        lock(&self.inner.jobs).remove(&job);
    }

    fn forget_unlisted(&self, keep: &BTreeSet<JobId>) {
        lock(&self.inner.jobs).retain(|id, j| keep.contains(id) || lock(&j.published).started);
    }
}

pub(crate) fn lost_end(reason: &str) -> ForwardEnd {
    ForwardEnd {
        phase: tasks_core::Phase::Interrupted,
        reason: reason.to_string(),
        exit_code: None,
        signal: None,
        remote_job: None,
        nothing_submitted: false,
        samples: Vec::new(),
        fetch: Vec::new(),
    }
}

/// The checkout id a job's forward uses, for tests and status.
pub fn checkout_of(local_name: &str, cwd: &Path) -> CheckoutId {
    CheckoutId::derive(local_name, &PathBuf::from(crate::place::checkout_key(cwd)))
}

#[cfg(test)]
mod tests;
