//! Disk monitoring: free/total of every filesystem the task manager writes to (cheap statvfs
//! on every health tick), a small history for a growth rate and time-to-full, and the top
//! consumers from a bounded periodic scan.
//!
//! The scan reuses `tmp_usage::inventory` (read-only, never follows symlinks or crosses a
//! mount, bounded by an entry and a time budget) on its own low-priority thread, never on a
//! socket thread. It covers the build cache (per-checkout outputs annotated with their
//! checkout from the ledger, sccache, fetched offload artifacts), Gradle/Android caches, the
//! admission lock/scratch directory (`/tmp/claude-$UID`), the task state directory and the
//! offload/peer checkouts a worker hosts.
use crate::tmp_usage::{self, Context, JobRef, Limits};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};

/// Top consumers kept per filesystem.
pub const TOP: usize = 8;
/// Checkout outputs kept from a scan (for the size check).
const TARGETS: usize = 32;
/// History: one point per this many seconds at most, this many points, this regression window.
const SPACING_S: f64 = 300.0;
const POINTS: usize = 48;
const WINDOW_S: f64 = 2.0 * 3600.0;
/// A growth estimate needs at least this many points spanning at least this long.
const MIN_POINTS: usize = 3;
const MIN_SPAN_S: f64 = 900.0;
pub const HISTORY_FILE: &str = "health/disk-history.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReserveClass {
    /// The state reserve (`TASKS_MIN_STATE_FREE_MIB`, default 256 MiB).
    State,
    /// The cache/target reserve (`TASKS_MIN_CACHE_FREE_GIB`, default 20 GiB). Ordered after
    /// `State`: a filesystem holding both kinds of roots gets the larger, cache reserve.
    Cache,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiskRoot {
    pub label: String,
    pub path: PathBuf,
    pub class: ReserveClass,
    /// Directories whose children are listed as separate consumers.
    pub expand: Vec<PathBuf>,
}

/// The roots the monitor watches. Missing ones are skipped at measurement time.
pub fn default_roots(
    home: Option<&Path>,
    state: &Path,
    lock_dir: Option<&Path>,
    peer_checkouts: Option<&Path>,
) -> Vec<DiskRoot> {
    let mut roots = vec![];
    let root = |label: &str, path: PathBuf, class, expand: Vec<PathBuf>| DiskRoot {
        label: label.into(),
        path,
        class,
        expand,
    };
    if let Some(home) = home {
        let cache = home.join("build-cache");
        let tasks = cache.join("p2p_chat/tasks");
        roots.push(root(
            "build-cache",
            cache.clone(),
            ReserveClass::Cache,
            vec![
                cache.clone(),
                cache.join("p2p_chat"),
                tasks.clone(),
                tasks.join("checkouts"),
                tasks.join("targets"),
                cache.join("offload"),
            ],
        ));
        roots.push(root(
            "gradle",
            home.join(".gradle"),
            ReserveClass::Cache,
            vec![home.join(".gradle")],
        ));
        roots.push(root(
            "android",
            home.join(".android"),
            ReserveClass::Cache,
            vec![],
        ));
        let offload = home.join("offload/checkouts");
        roots.push(root(
            "offload-checkouts",
            offload.clone(),
            ReserveClass::Cache,
            vec![offload],
        ));
    }
    if let Some(p) = peer_checkouts
        && !roots.iter().any(|r| r.path == p)
    {
        roots.push(root(
            "peer-checkouts",
            p.to_path_buf(),
            ReserveClass::Cache,
            vec![p.to_path_buf()],
        ));
    }
    if let Some(lock) = lock_dir {
        roots.push(root(
            "lock-dir",
            lock.to_path_buf(),
            ReserveClass::State,
            vec![lock.to_path_buf()],
        ));
    }
    roots.push(root(
        "state",
        state.to_path_buf(),
        ReserveClass::State,
        vec![state.to_path_buf(), state.join("jobs")],
    ));
    roots
}

/// One consumer (a top-level group of a root) from the last scan.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Consumer {
    pub path: PathBuf,
    pub bytes: u64,
    /// The checkout this output serves, when derivable (cache key, offload identity).
    #[serde(default)]
    pub checkout: Option<PathBuf>,
    /// Live/recent ledger jobs whose cwd is inside.
    #[serde(default)]
    pub jobs: Vec<i64>,
    /// The scan budget ran out inside it (a lower bound).
    #[serde(default)]
    pub partial: bool,
}

/// One filesystem as the health monitor sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiskStatus {
    /// Stable key: the first watched root on this filesystem.
    pub filesystem: String,
    /// `label=path` of every watched root on it.
    pub roots: Vec<String>,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// The admission reserve that applies (cache reserve if any cache root is on it).
    pub reserve_bytes: u64,
    pub reserve_class: ReserveClass,
    /// Bytes consumed per hour over the recent window (negative: freeing). None: not enough history.
    #[serde(default)]
    pub growth_bytes_per_h: Option<f64>,
    /// Seconds until free space reaches zero at that rate. None: not filling or unknown.
    #[serde(default)]
    pub time_to_full_s: Option<f64>,
    #[serde(default)]
    pub top: Vec<Consumer>,
    #[serde(default)]
    pub scanned_at: Option<f64>,
    #[serde(default)]
    pub scan_truncated: bool,
}

impl DiskStatus {
    pub fn line(&self) -> String {
        let mut out = format!(
            "{}: {} free of {} (reserve {})",
            self.filesystem,
            super::procs::gib(self.free_bytes),
            super::procs::gib(self.total_bytes),
            super::procs::gib(self.reserve_bytes)
        );
        if let Some(rate) = self.growth_bytes_per_h {
            out.push_str(&format!(
                ", {:+.1} GiB/h",
                rate / (1024.0 * 1024.0 * 1024.0)
            ));
        }
        if let Some(ttf) = self.time_to_full_s {
            out.push_str(&format!(", full in ~{}", crate::placement::duration(ttf)));
        }
        out
    }
}

/// The last bounded scan and what it cost.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScanSummary {
    pub at: f64,
    pub elapsed_s: f64,
    /// CPU time of the scanning thread (user + system).
    pub cpu_s: f64,
    pub entries: u64,
    pub truncated: bool,
    pub max_entries: u64,
    pub max_s: f64,
    pub runs: u64,
}

/// Result of one scan: consumers per filesystem key and the largest checkout outputs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScanResult {
    pub top: BTreeMap<String, Vec<Consumer>>,
    pub truncated: BTreeMap<String, bool>,
    pub targets: Vec<Consumer>,
    pub summary: ScanSummary,
}

/// statvfs of the nearest existing ancestor: (device, total, available).
pub fn fs_stat(path: &Path) -> Option<(u64, u64, u64)> {
    let mut existing = path;
    while !existing.try_exists().ok()? {
        existing = existing.parent()?;
    }
    let dev = std::fs::metadata(existing).ok()?.dev();
    let name = CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(name.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    let frsize = stat.f_frsize as u128;
    let clamp = |v: u128| v.min(u64::MAX as u128) as u64;
    Some((
        dev,
        clamp(stat.f_blocks as u128 * frsize),
        clamp(stat.f_bavail as u128 * frsize),
    ))
}

/// The watched filesystems of one measurement.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Measured {
    /// (key, `label=path` roots, total, free, reserve class) per filesystem.
    pub filesystems: Vec<(String, Vec<String>, u64, u64, ReserveClass)>,
    /// (root path, filesystem key) of every existing root.
    pub keys: Vec<(PathBuf, String)>,
}

/// Groups the existing roots by filesystem (device of the nearest existing ancestor).
pub fn measure(roots: &[DiskRoot], stat: impl Fn(&Path) -> Option<(u64, u64, u64)>) -> Measured {
    let mut out: Vec<(u64, String, Vec<String>, u64, u64, ReserveClass)> = vec![];
    let mut keys = vec![];
    for r in roots {
        if !r.path.exists() {
            continue;
        }
        let Some((dev, total, free)) = stat(&r.path) else {
            continue;
        };
        let label = format!("{}={}", r.label, r.path.display());
        let key = match out.iter_mut().find(|e| e.0 == dev) {
            Some(e) => {
                e.2.push(label);
                e.5 = e.5.max(r.class);
                e.1.clone()
            }
            None => {
                let key = r.path.display().to_string();
                out.push((dev, key.clone(), vec![label], total, free, r.class));
                key
            }
        };
        keys.push((r.path.clone(), key));
    }
    Measured {
        filesystems: out
            .into_iter()
            .map(|(_, key, roots, total, free, class)| (key, roots, total, free, class))
            .collect(),
        keys,
    }
}

/// Bytes consumed per second over the window (negative: freeing), by least squares; None
/// with too little history.
pub fn growth(points: &[(f64, u64)], now: f64) -> Option<f64> {
    let recent: Vec<(f64, f64)> = points
        .iter()
        .filter(|(t, _)| now - t <= WINDOW_S && *t <= now)
        .map(|(t, free)| (*t, *free as f64))
        .collect();
    if recent.len() < MIN_POINTS {
        return None;
    }
    let span = recent.last()?.0 - recent.first()?.0;
    if span < MIN_SPAN_S {
        return None;
    }
    let n = recent.len() as f64;
    let mean_t = recent.iter().map(|p| p.0).sum::<f64>() / n;
    let mean_f = recent.iter().map(|p| p.1).sum::<f64>() / n;
    let (mut num, mut den) = (0.0, 0.0);
    for (t, f) in &recent {
        num += (t - mean_t) * (f - mean_f);
        den += (t - mean_t) * (t - mean_t);
    }
    (den > 0.0).then(|| -num / den)
}

/// Seconds until `free` reaches zero at `rate` bytes/s consumed; None unless filling.
pub fn time_to_full(free: u64, rate: Option<f64>) -> Option<f64> {
    rate.filter(|r| *r > 0.0).map(|r| free as f64 / r)
}

/// Free-space history per filesystem key (small; persisted in the state directory).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct History {
    pub points: BTreeMap<String, Vec<(f64, u64)>>,
}

impl History {
    pub fn load(state: &Path) -> Self {
        crate::peer::records::read_json(&state.join(HISTORY_FILE)).unwrap_or_default()
    }
    pub fn save(&self, state: &Path) -> std::io::Result<()> {
        crate::peer::records::write_json(&state.join(HISTORY_FILE), self)
    }
    /// Records a point unless the last one is newer than the spacing. True when recorded.
    pub fn record(&mut self, key: &str, now: f64, free: u64) -> bool {
        let points = self.points.entry(key.to_string()).or_default();
        if points
            .last()
            .is_some_and(|(t, _)| now - t < SPACING_S && *t <= now)
        {
            return false;
        }
        points.push((now, free));
        if points.len() > POINTS {
            let drop = points.len() - POINTS;
            points.drain(..drop);
        }
        true
    }
    /// Growth rate including the current, not yet recorded, measurement.
    pub fn rate(&self, key: &str, now: f64, free: u64) -> Option<f64> {
        let mut points = self.points.get(key).cloned().unwrap_or_default();
        if points.last().is_none_or(|(t, _)| *t < now) {
            points.push((now, free));
        }
        growth(&points, now)
    }
}

/// Builds the per-filesystem status from a measurement, the history and the last scan.
pub fn statuses(
    measured: &Measured,
    thresholds: &crate::admission::Thresholds,
    history: &History,
    scan: Option<&ScanResult>,
    now: f64,
) -> Vec<DiskStatus> {
    measured
        .filesystems
        .iter()
        .cloned()
        .map(|(key, roots, total, free, class)| {
            let rate = history.rate(&key, now, free);
            DiskStatus {
                reserve_bytes: match class {
                    ReserveClass::Cache => thresholds.min_cache_free_bytes,
                    ReserveClass::State => thresholds.min_state_free_bytes,
                },
                growth_bytes_per_h: rate.map(|r| r * 3600.0),
                time_to_full_s: time_to_full(free, rate),
                top: scan
                    .and_then(|s| s.top.get(&key))
                    .cloned()
                    .unwrap_or_default(),
                scanned_at: scan.map(|s| s.summary.at),
                scan_truncated: scan
                    .and_then(|s| s.truncated.get(&key))
                    .copied()
                    .unwrap_or(false),
                filesystem: key,
                roots,
                total_bytes: total,
                free_bytes: free,
                reserve_class: class,
            }
        })
        .collect()
}

fn thread_cpu_s() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut usage) } != 0 {
        return 0.0;
    }
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    secs(usage.ru_utime) + secs(usage.ru_stime)
}

/// Lowest CPU and idle I/O priority for the calling thread (best effort).
pub fn lower_thread_priority() {
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 19);
        // IOPRIO_WHO_PROCESS = 1, IOPRIO_CLASS_IDLE = 3 << 13.
        libc::syscall(libc::SYS_ioprio_set, 1, tid, 3 << 13);
    }
}

fn is_output_group(path: &Path) -> bool {
    path.parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "checkouts" || n == "targets")
}

/// One bounded, read-only scan of the existing roots. `fs_key` maps a root to its
/// filesystem key (from [`measure`]).
pub fn scan(
    roots: &[DiskRoot],
    jobs: &[JobRef],
    limits: Limits,
    fs_key: impl Fn(&Path) -> Option<String>,
    now: f64,
) -> ScanResult {
    let existing: Vec<&DiskRoot> = roots.iter().filter(|r| r.path.is_dir()).collect();
    let paths: Vec<PathBuf> = existing.iter().map(|r| r.path.clone()).collect();
    let ctx = Context {
        jobs: jobs.to_vec(),
        processes: vec![],
        expand: existing.iter().flat_map(|r| r.expand.clone()).collect(),
        lock_dir: None,
    };
    let cpu_before = thread_cpu_s();
    let report = tmp_usage::inventory(&paths, &ctx, limits);
    let mut result = ScanResult {
        summary: ScanSummary {
            at: now,
            elapsed_s: report.elapsed_seconds,
            cpu_s: (thread_cpu_s() - cpu_before).max(0.0),
            entries: report.entries_scanned,
            truncated: report.truncated,
            max_entries: report.max_entries,
            max_s: report.max_seconds,
            runs: 0,
        },
        ..Default::default()
    };
    let mut targets = vec![];
    for root in &report.roots {
        let Some(key) = fs_key(&root.path) else {
            continue;
        };
        *result.truncated.entry(key.clone()).or_default() |= root.truncated;
        let top = result.top.entry(key).or_default();
        for g in &root.groups {
            let checkout = g.checkout.clone().or_else(|| {
                let (checkout, _) =
                    crate::metrics::snapshot::checkout_of(&g.path.to_string_lossy());
                (Path::new(&checkout) != g.path).then(|| PathBuf::from(checkout))
            });
            let consumer = Consumer {
                path: g.path.clone(),
                bytes: g.bytes,
                checkout,
                jobs: g.jobs.iter().rev().take(4).map(|j| j.id).collect(),
                partial: g.truncated || g.errors > 0,
            };
            if is_output_group(&g.path) {
                targets.push(consumer.clone());
            }
            top.push(consumer);
        }
    }
    for top in result.top.values_mut() {
        top.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));
        top.truncate(TOP);
    }
    targets.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));
    targets.truncate(TARGETS);
    result.targets = targets;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn growth_and_time_to_full_need_enough_history() {
        let now = 10_000.0;
        assert_eq!(
            growth(&[(now - 600.0, 10 * GIB), (now, 9 * GIB)], now),
            None,
            "2 points"
        );
        let short: Vec<_> = (0..5).map(|i| (now - 100.0 * i as f64, 10 * GIB)).collect();
        assert_eq!(growth(&short, now), None, "span under 15 min");
        // 1 GiB consumed per 10 minutes, steadily, over an hour.
        let filling: Vec<(f64, u64)> = (0..7)
            .map(|i| (now - 3600.0 + 600.0 * i as f64, (16 - i) * GIB))
            .collect();
        let rate = growth(&filling, now).unwrap();
        assert!((rate - GIB as f64 / 600.0).abs() < 1.0, "{rate}");
        let ttf = time_to_full(10 * GIB, Some(rate)).unwrap();
        assert!((ttf - 6000.0).abs() < 1.0, "{ttf}");
        // Freeing space: negative rate, no time-to-full.
        let freeing: Vec<(f64, u64)> = filling
            .iter()
            .rev()
            .enumerate()
            .map(|(i, (_, f))| (now - 3600.0 + 600.0 * i as f64, *f))
            .collect();
        assert!(growth(&freeing, now).unwrap() < 0.0);
        assert_eq!(time_to_full(10 * GIB, growth(&freeing, now)), None);
        // Points outside the 2 h window are ignored.
        let old: Vec<_> = (0..5)
            .map(|i| (now - 20_000.0 + 600.0 * i as f64, i * GIB))
            .collect();
        assert_eq!(growth(&old, now), None);
    }

    #[test]
    fn history_is_spaced_and_bounded() {
        let mut h = History::default();
        assert!(h.record("/fs", 0.0, 100));
        assert!(!h.record("/fs", 100.0, 99), "closer than the spacing");
        for i in 1..100 {
            h.record("/fs", i as f64 * SPACING_S, 100 - i);
        }
        assert_eq!(h.points["/fs"].len(), POINTS);
        let rate = h.rate("/fs", 99.0 * SPACING_S + 10.0, 1).unwrap();
        assert!(rate > 0.0);
    }

    #[test]
    fn measure_groups_roots_by_filesystem_and_statuses_apply_the_reserve() {
        let roots = default_roots(
            Some(Path::new("/")),
            Path::new("/"),
            Some(Path::new("/")),
            None,
        );
        // Only "/" exists among these paths (build-cache etc. do not): a fake stat puts
        // everything on one device.
        let stat = |_: &Path| Some((7, 100 * GIB, 30 * GIB));
        let m = measure(&roots, stat);
        assert_eq!(m.filesystems.len(), 1, "{m:?}");
        assert_eq!(m.filesystems[0].0, "/");
        assert!(
            m.filesystems[0]
                .1
                .iter()
                .any(|r| r.starts_with("lock-dir="))
        );
        assert_eq!(m.keys.len(), 2, "lock dir and state exist");
        assert!(m.keys.iter().all(|(_, k)| k == "/"));
        let t = crate::admission::Thresholds::default();
        let s = statuses(&m, &t, &History::default(), None, 0.0);
        assert_eq!(s[0].reserve_class, ReserveClass::State);
        assert_eq!(s[0].reserve_bytes, t.min_state_free_bytes);
        assert!(
            s[0].line().contains("30.0 GiB free of 100.0 GiB"),
            "{}",
            s[0].line()
        );
    }

    #[test]
    fn scan_reports_top_consumers_and_checkout_outputs() {
        let base = std::env::temp_dir().join(format!("tasks-health-disk-{}", std::process::id()));
        let home = base.join("home");
        let key = "ab".repeat(32);
        let target = home
            .join("build-cache/p2p_chat/tasks/checkouts")
            .join(&key)
            .join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("big.rlib"), vec![1u8; 300_000]).unwrap();
        std::fs::create_dir_all(home.join("build-cache/sccache")).unwrap();
        std::fs::write(home.join("build-cache/sccache/x"), vec![1u8; 10_000]).unwrap();
        let state = base.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let roots = default_roots(Some(&home), &state, None, None);
        let r = scan(
            &roots,
            &[],
            Limits::default(),
            |_| Some("fs".to_string()),
            42.0,
        );
        let top = &r.top["fs"];
        assert!(top[0].path.ends_with(&key), "{top:?}");
        assert!(top[0].bytes >= 300_000);
        assert!(top.iter().any(|c| c.path.ends_with("sccache")));
        assert_eq!(r.targets.len(), 1);
        assert!(r.targets[0].path.ends_with(&key));
        assert_eq!(r.summary.at, 42.0);
        assert!(r.summary.entries > 0 && !r.summary.truncated);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
