//! The daemon's health monitor: two background threads, never the socket threads.
//!
//! * check thread: every `interval` reads the ledger (own SQLite connection), the sampler's
//!   machine snapshot, statvfs of the watched roots, /proc/locks (+ the lock files' inodes),
//!   PSI/memory, the peer runtime's latest peer views, and the logs of newly failed jobs
//!   (bounded tails); runs the checks; updates the alert book; notifies; publishes
//!   [`NodeHealth`] for the `health` snapshot field.
//! * scan thread: every `disk_scan_interval`, one bounded read-only `tmp_usage` scan of the
//!   watched roots at the lowest CPU/idle I/O priority; the check thread picks up its result.
//!
//! State files (small, in `<state>/health/`): `alerts.json` (the book, so first-seen times
//! survive a restart), `capabilities.json` (marks), `disk-history.json` (free-space points).
use super::alert::AlertBook;
use super::capability::{self, CapabilityMark, Marks};
use super::checks::{self, Since, SlotInputs};
use super::classify;
use super::config::{HealthConfig, NotifyMode};
use super::disk::{self, DiskRoot, History, ScanResult};
use super::notify::{self, FileNotifier, Notifier, NotifySend, RateLimiter, Silent};
use super::procs;
use super::{NodeHealth, SNAPSHOT_RECENT};
use crate::metrics::sampler::Sampler;
use crate::metrics::{Role, Snapshot};
use crate::peer_link::PeersReport;
use crate::{Job, Phase, Store};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ALERTS_FILE: &str = "health/alerts.json";
/// Failed jobs older than this are not classified on a first start (no history replay).
const FIRST_START_WINDOW_S: f64 = 24.0 * 3600.0;
/// Failed logs classified per pass at most (the rest wait for the next pass).
const CLASSIFY_PER_PASS: usize = 20;
/// Persist the alert book at least this often while it only changes timestamps.
const SAVE_EVERY: Duration = Duration::from_secs(300);

/// The daemon's handle: the latest published health.
#[derive(Clone, Default)]
pub struct Health {
    shared: Arc<Mutex<NodeHealth>>,
}

impl Health {
    pub fn summary(&self) -> NodeHealth {
        self.shared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// What the scan thread needs from the check thread.
#[derive(Clone, Default)]
struct ScanInputs {
    roots: Vec<DiskRoot>,
    jobs: Vec<crate::tmp_usage::JobRef>,
    /// (root path, filesystem key) of the existing roots.
    keys: Vec<(PathBuf, String)>,
}

/// Starts the monitor threads. `peers` is the peer runtime's report (empty when disabled).
pub fn start(state: PathBuf, sampler: Sampler, peers: Arc<Mutex<PeersReport>>) -> Health {
    let health = Health::default();
    let cfg = HealthConfig::from_env();
    for w in &cfg.warnings {
        eprintln!("health: {w}");
    }
    let scan_inputs: Arc<Mutex<Option<ScanInputs>>> = Arc::default();
    let scan_result: Arc<Mutex<Option<ScanResult>>> = Arc::default();
    {
        let (inputs, result, cfg) = (scan_inputs.clone(), scan_result.clone(), cfg.clone());
        let spawned = std::thread::Builder::new()
            .name("health-scan".into())
            .spawn(move || scan_loop(inputs, result, cfg));
        if let Err(e) = spawned {
            eprintln!("health: disk scan thread not started: {e}");
        }
    }
    let shared = health.shared.clone();
    let spawned = std::thread::Builder::new()
        .name("health".into())
        .spawn(move || {
            let mut monitor =
                match Monitor::new(state, sampler, peers, cfg, scan_inputs, scan_result, shared) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("health: monitor not started: {e}");
                        return;
                    }
                };
            // Let the sampler take its first samples (CPU needs two).
            std::thread::sleep(Duration::from_secs(2).min(monitor.cfg.interval));
            loop {
                let started = Instant::now();
                if let Err(e) = monitor.tick(now()) {
                    eprintln!("health: check pass failed: {e}");
                }
                std::thread::sleep(monitor.cfg.interval.saturating_sub(started.elapsed()));
            }
        });
    if let Err(e) = spawned {
        eprintln!("health: monitor thread not started: {e}");
    }
    health
}

fn scan_loop(
    inputs: Arc<Mutex<Option<ScanInputs>>>,
    result: Arc<Mutex<Option<ScanResult>>>,
    cfg: HealthConfig,
) {
    disk::lower_thread_priority();
    let limits = crate::tmp_usage::Limits {
        max_entries: cfg.disk_scan_max_entries,
        max_time: cfg.disk_scan_max_time,
    };
    let mut runs = 0;
    // First scan shortly after start, once the check thread published its roots.
    std::thread::sleep(Duration::from_secs(5).min(cfg.disk_scan_interval));
    loop {
        let current = inputs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let Some(current) = current else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        let keys = current.keys.clone();
        let fs_key = move |root: &Path| {
            keys.iter()
                .find(|(path, _)| path == root)
                .map(|(_, key)| key.clone())
        };
        let mut scan = disk::scan(&current.roots, &current.jobs, limits, fs_key, now());
        runs += 1;
        scan.summary.runs = runs;
        *result.lock().unwrap_or_else(|p| p.into_inner()) = Some(scan);
        std::thread::sleep(cfg.disk_scan_interval);
    }
}

struct Monitor {
    state: PathBuf,
    sampler: Sampler,
    peers: Arc<Mutex<PeersReport>>,
    cfg: HealthConfig,
    store: Store,
    roots: Vec<DiskRoot>,
    lock_dir: Option<PathBuf>,
    book: AlertBook,
    marks: Marks,
    history: History,
    since: Since,
    classified: HashSet<i64>,
    first_pass: bool,
    notifier: Option<Box<dyn Notifier>>,
    limiter: RateLimiter,
    peer_seen: HashSet<String>,
    saved_at: Instant,
    saved_ids: Vec<String>,
    scan_inputs: Arc<Mutex<Option<ScanInputs>>>,
    scan_result: Arc<Mutex<Option<ScanResult>>>,
    shared: Arc<Mutex<NodeHealth>>,
    probes: Vec<super::node::ProbeResult>,
}

impl Monitor {
    fn new(
        state: PathBuf,
        sampler: Sampler,
        peers: Arc<Mutex<PeersReport>>,
        cfg: HealthConfig,
        scan_inputs: Arc<Mutex<Option<ScanInputs>>>,
        scan_result: Arc<Mutex<Option<ScanResult>>>,
        shared: Arc<Mutex<NodeHealth>>,
    ) -> crate::Result<Self> {
        let store = Store::open(&state.join("tasks.sqlite"))?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let lock_dir = crate::admission::root().ok();
        let peer_checkouts = task_manager_peer::PeerConfig::default_path()
            .filter(|p| p.exists())
            .and_then(|p| task_manager_peer::PeerConfig::load(&p).ok())
            .map(|c| c.checkouts);
        let roots = disk::default_roots(
            home.as_deref(),
            &state,
            lock_dir.as_deref(),
            peer_checkouts.as_deref(),
        );
        let book: AlertBook =
            crate::peer::records::read_json(&state.join(ALERTS_FILE)).unwrap_or_default();
        let limiter = RateLimiter::new(
            cfg.notify_per_alert_s,
            cfg.notify_burst,
            cfg.notify_window_s,
        );
        Ok(Self {
            marks: Marks::load(&state),
            history: History::load(&state),
            state,
            sampler,
            peers,
            store,
            roots,
            lock_dir,
            book,
            since: Since::default(),
            classified: HashSet::new(),
            first_pass: true,
            notifier: None,
            limiter,
            peer_seen: HashSet::new(),
            saved_at: Instant::now(),
            saved_ids: vec![],
            scan_inputs,
            scan_result,
            shared,
            cfg,
            probes: vec![],
        })
    }

    /// The notifier, chosen once the machine role is known (first snapshot).
    fn notifier(&mut self, role: Role) -> &mut dyn Notifier {
        if self.notifier.is_none() {
            let mode = self
                .cfg
                .notify_mode(role == Role::Host, |name| std::env::var_os(name));
            let chosen: Box<dyn Notifier> = match &mode {
                NotifyMode::Desktop => match NotifySend::find() {
                    Some(n) => Box::new(n),
                    None => {
                        eprintln!("health: notify-send not found; desktop notifications off");
                        Box::new(Silent)
                    }
                },
                NotifyMode::File(path) => Box::new(FileNotifier(path.clone())),
                NotifyMode::Off => Box::new(Silent),
            };
            eprintln!("health: notifier {mode:?}");
            self.notifier = Some(chosen);
        }
        self.notifier.as_deref_mut().expect("set above")
    }

    fn tick(&mut self, now: f64) -> crate::Result<()> {
        let jobs = self.store.jobs()?;
        let snapshot = self.sampler.snapshot(&jobs);
        let node = snapshot.machine.id.clone();
        if self.book.origin != node {
            self.book.origin = node.clone();
        }
        let thresholds = crate::admission::thresholds().unwrap_or_default();
        let mut raised = vec![];

        // Disks: statvfs every pass; consumers from the last background scan.
        let measured = disk::measure(&self.roots, disk::fs_stat);
        let mut recorded = false;
        for (key, _, _, free, _) in &measured.filesystems {
            recorded |= self.history.record(key, now, *free);
        }
        if recorded && let Err(e) = self.history.save(&self.state) {
            eprintln!("health: disk history not saved: {e}");
        }
        *self.scan_inputs.lock().unwrap_or_else(|p| p.into_inner()) = Some(ScanInputs {
            roots: self.roots.clone(),
            jobs: jobs.iter().rev().take(500).map(job_ref).collect(),
            keys: measured.keys.clone(),
        });
        let scan = self
            .scan_result
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let disks = disk::statuses(&measured, &thresholds, &self.history, scan.as_ref(), now);
        let waiting_disk: Vec<String> = snapshot
            .jobs
            .iter()
            .filter_map(|j| {
                let why = j.queue.as_ref()?.waiting_for.as_ref()?;
                why.to_lowercase()
                    .contains("disk")
                    .then(|| format!("#{} {}: {why}", j.id, j.owner))
            })
            .collect();
        let mut obs = checks::disks(&node, &disks, &waiting_disk, &self.cfg);
        if let Some(scan) = &scan {
            obs.extend(checks::targets(&node, &scan.targets, &self.cfg));
        }
        raised.extend(self.book.observe("disk", obs, now));

        // Slots held with no live job.
        let locks = std::fs::read_to_string("/proc/locks")
            .map(|t| procs::parse_locks(&t))
            .unwrap_or_default();
        let files: Vec<(String, (u32, u32, u64))> = snapshot
            .machine
            .slots
            .iter()
            .filter_map(|s| {
                let path = self.lock_dir.as_ref()?.join(format!("{}.lock", s.name));
                Some((s.name.clone(), procs::file_id(&path)?))
            })
            .collect();
        let proc_root = Path::new("/proc");
        let cmdline = |pid: u32| procs::cmdline(proc_root, pid);
        // fix/lease-leak's holder scan (fdinfo FLOCK), shared with admission and the snapshot.
        let lock_dir = self.lock_dir.clone();
        let holders = move |name: &str| {
            lock_dir
                .as_ref()
                .map(|d| crate::admission::holders::holders(&d.join(format!("{name}.lock"))))
                .unwrap_or_default()
        };
        let inputs = SlotInputs {
            locks: &locks,
            files: &files,
            cmdline: &cmdline,
            holders: &holders,
        };
        let obs = checks::slots(&node, &snapshot, &inputs, &mut self.since, now, &self.cfg);
        raised.extend(self.book.observe("slots", obs, now));

        // Node setup (node-setup-policy): declared capabilities, the compiler-cache server.
        self.probes = super::node::live_probes(&|name| std::env::var_os(name));
        let mut obs = super::node::capability_alerts(&node, &self.probes);
        let wrapper = std::env::var("RUSTC_WRAPPER").ok();
        let servers: Vec<super::node::CacheServer> =
            if super::node::uses_sccache(wrapper.as_deref()) {
                procs::find_by_comm(proc_root, "sccache")
                    .into_iter()
                    .take(16)
                    .filter_map(|pid| {
                        let cmd = procs::cmdline(proc_root, pid)?;
                        // The server runs with no arguments; `sccache rustc ...` are clients.
                        (cmd.split_whitespace().count() == 1).then(|| super::node::CacheServer {
                            pid,
                            cmd,
                            job: procs::env_var(proc_root, pid, "TASKS_JOB_ID"),
                            lock_fds: self
                                .lock_dir
                                .as_deref()
                                .map(|d| procs::lock_fds(proc_root, pid, d))
                                .unwrap_or_default(),
                        })
                    })
                    .collect()
            } else {
                vec![]
            };
        obs.extend(super::node::compiler_cache(
            &node,
            wrapper.as_deref(),
            &servers,
        ));
        raised.extend(self.book.observe("node", obs, now));

        // Memory pressure.
        let top = || procs::top_memory(proc_root, 5);
        let obs = checks::memory(
            &node,
            &snapshot.machine,
            thresholds.min_available_memory_bytes,
            &top,
            &mut self.since,
            now,
            &self.cfg,
        );
        raised.extend(self.book.observe("memory", obs, now));

        // Jobs: long waits and high CPU.
        let mut obs = checks::waiting(&node, &snapshot.jobs, now, &self.cfg);
        obs.extend(checks::high_cpu(&node, &snapshot.jobs, now, &self.cfg));
        raised.extend(self.book.observe("jobs", obs, now));

        // Failed-log classification -> capability marks -> alerts.
        self.classify(&jobs, &node, now);
        let obs = checks::capabilities(&self.marks.marks);
        raised.extend(self.book.observe("capability", obs, now));

        // Peers: stale/offline streams and idle peers while work queues here.
        let mut report = self.peers.lock().unwrap_or_else(|p| p.into_inner()).clone();
        report.refresh(now);
        let obs = checks::peers(&report.snapshots, &mut self.since, now, &self.cfg);
        raised.extend(self.book.observe("peers", obs, now));
        let placement_peers: Vec<crate::placement::Peer> = report
            .snapshots
            .iter()
            .filter(|s| matches!(s.role.as_str(), "worker" | "both"))
            .map(|s| {
                let mut p = crate::placement::Peer::from_report(s);
                p.missing = crate::placement::marks_for(&p, &self.marks.marks);
                p
            })
            .collect();
        let obs = checks::idle_peer(&node, &snapshot, &placement_peers, now, &self.cfg);
        raised.extend(self.book.observe("placement", obs, now));

        // Notify: this machine's new alerts and, on a controller, peers' new ones.
        let mut to_notify = raised;
        let mut seen_now = HashSet::new();
        for s in &report.snapshots {
            let Some(health) = s
                .snapshot
                .as_ref()
                .and_then(|v| v.get("health"))
                .and_then(|h| serde_json::from_value::<NodeHealth>(h.clone()).ok())
            else {
                continue;
            };
            for a in health.alerts {
                let key = format!("{}|{}", s.name, a.id);
                if !self.peer_seen.contains(&key) {
                    to_notify.push(a);
                }
                seen_now.insert(key);
            }
        }
        self.peer_seen = seen_now;
        let role = snapshot.machine.role;
        let mut limiter = std::mem::replace(&mut self.limiter, RateLimiter::new(0.0, 0, 0.0));
        notify::deliver(self.notifier(role), &mut limiter, &to_notify, now);
        self.limiter = limiter;

        self.publish(&snapshot, disks, scan, now);
        let ids: Vec<String> = self.book.active.keys().cloned().collect();
        if ids != self.saved_ids || self.saved_at.elapsed() >= SAVE_EVERY {
            if let Err(e) =
                crate::peer::records::write_json(&self.state.join(ALERTS_FILE), &self.book)
            {
                eprintln!("health: alerts not saved: {e}");
            }
            self.saved_at = Instant::now();
            self.saved_ids = ids;
        }
        Ok(())
    }

    fn publish(
        &self,
        snapshot: &Snapshot,
        disks: Vec<disk::DiskStatus>,
        scan: Option<ScanResult>,
        now: f64,
    ) {
        let recent: Vec<_> = self
            .book
            .recent
            .iter()
            .rev()
            .take(SNAPSHOT_RECENT)
            .rev()
            .cloned()
            .collect();
        let health = NodeHealth {
            node: snapshot.machine.id.clone(),
            checked_at: now,
            interval_s: self.cfg.interval.as_secs_f64(),
            alerts: self.book.sorted_active(),
            recent,
            disks,
            scan: scan.map(|s| s.summary),
            capabilities: {
                let mut marks = self.marks.marks.clone();
                marks.extend(super::node::probe_marks(
                    &snapshot.machine.id,
                    &self.probes,
                    now,
                    2.0 * self.cfg.interval.as_secs_f64(),
                ));
                marks
            },
            probes: self.probes.clone(),
        };
        *self.shared.lock().unwrap_or_else(|p| p.into_inner()) = health;
    }

    /// Classifies newly failed jobs' logs, clears marks on passes, expiry and requests.
    fn classify(&mut self, jobs: &[Job], node: &str, now: f64) {
        let mut changed = false;
        let mut budget = CLASSIFY_PER_PASS;
        let mut floor = self.marks.classified_through;
        let mut pending_below = None::<i64>;
        let mut sorted: Vec<&Job> = jobs.iter().filter(|j| j.id > floor).collect();
        sorted.sort_by_key(|j| j.id);
        for job in sorted {
            if !job.phase.terminal() {
                pending_below.get_or_insert(job.id);
                continue;
            }
            if self.classified.contains(&job.id) {
                if pending_below.is_none() {
                    floor = job.id;
                }
                continue;
            }
            let finished = job
                .execution
                .as_ref()
                .and_then(|e| e.finished_at)
                .map(|t| t as f64)
                .unwrap_or(job.submitted_at as f64);
            let old = self.first_pass && now - finished > FIRST_START_WINDOW_S;
            let (target, remote_job) = target_of(job, node);
            let kind_key = capability::base_kind(&crate::metrics::history::names(job).1);
            if !old {
                match job.phase {
                    Phase::Failed if budget > 0 => {
                        budget -= 1;
                        let log = self
                            .state
                            .join("jobs")
                            .join(job.id.to_string())
                            .join("job.log");
                        let text = classify::tail(&log, classify::TAIL_BYTES).unwrap_or_default();
                        for f in classify::classify(&text) {
                            let source = if remote_job.is_some() || target != node {
                                "forwarded job log"
                            } else {
                                "job log"
                            };
                            let mark = CapabilityMark {
                                node: target.clone(),
                                capability: f.capability.clone(),
                                family: f.family.into(),
                                job: job.id,
                                remote_job,
                                kind_key: kind_key.clone(),
                                line: f.line.clone(),
                                marked_at: now,
                                expires_at: now + self.cfg.capability_ttl_s,
                                source: source.into(),
                            };
                            eprintln!(
                                "health: job {} on {}: {} ({})",
                                job.id,
                                target,
                                mark.lacks(),
                                f.line
                            );
                            self.marks.mark(mark);
                            changed = true;
                        }
                    }
                    Phase::Failed => {
                        // Over budget: leave it for the next pass.
                        pending_below.get_or_insert(job.id);
                        continue;
                    }
                    Phase::Passed => {
                        let cleared = self.marks.passed(&target, job.id, &kind_key);
                        for m in &cleared {
                            eprintln!(
                                "health: {} capability {} cleared: job {} ({kind_key}) passed",
                                m.node, m.capability, job.id
                            );
                        }
                        changed |= !cleared.is_empty();
                    }
                    _ => {}
                }
            }
            self.classified.insert(job.id);
            if pending_below.is_none() {
                floor = job.id;
            }
        }
        self.first_pass = false;
        if floor != self.marks.classified_through {
            self.marks.classified_through = floor;
            self.classified.retain(|id| *id > floor);
            changed = true;
        }
        for request in capability::take_clear_requests(&self.state) {
            let cleared = self
                .marks
                .clear(&request.node, request.capability.as_deref());
            eprintln!(
                "health: cleared {} capability mark(s) of {} by request",
                cleared.len(),
                request.node
            );
            changed |= !cleared.is_empty();
        }
        changed |= !self.marks.expire(now).is_empty();
        if changed && let Err(e) = self.marks.save(&self.state) {
            eprintln!("health: capability marks not saved: {e}");
        }
    }
}

/// The node a job ran on: the peer for a forwarded job (with the remote job id when the
/// link knows it), else this machine.
fn target_of(job: &Job, node: &str) -> (String, Option<i64>) {
    let argv = crate::metrics::history::argv(job);
    match crate::peer_link::ForwardArgs::from_recipe_argv(&argv) {
        Some(Ok(args)) => {
            let remote = job
                .execution
                .as_ref()
                .and_then(|e| e.peer.as_ref())
                .and_then(|p| p.peer_job_id);
            (args.peer, remote)
        }
        _ => (node.to_string(), None),
    }
}

fn job_ref(job: &Job) -> crate::tmp_usage::JobRef {
    crate::tmp_usage::JobRef {
        id: job.id,
        owner: job.spec.owner.clone(),
        phase: job.phase.as_db().unwrap_or_default(),
        source: job.spec.source.clone(),
    }
}
