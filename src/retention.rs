//! Read-only retirement proposals and admission budgets, NOT a deletion API.
//! Ownership is cooperative provenance for initially empty targets, not a sandbox.
#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;

use crate::{Result, cache::TargetLease, execution::now};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

pub const TARGET_BUDGET_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const IDLE_SECONDS: u64 = 7 * 24 * 60 * 60;
const LOCK: &str = ".tasks-target.lock";
const OWNER: &str = ".tasks-target.owner.json";
const USAGE: &str = ".tasks-target.usage.json";
const MAX_ENTRIES: usize = 50_000;
const MAX_ROOTS: usize = 256;
const MAX_TARGETS: usize = 16;
const MAX_PLAN_TIME: Duration = Duration::from_secs(5);
const MAX_DEPTH: usize = 64;
const MAX_SCAN_TIME: Duration = Duration::from_secs(2);
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Owner {
    schema: u32,
    manager: String,
    family: String,
    path: PathBuf,
    uid: u32,
    device: u64,
    inode: u64,
    lease_device: u64,
    lease_inode: u64,
}
impl Owner {
    fn expected(target: &Path) -> Result<Self> {
        Self::for_family(target, "self-tests")
    }
    fn for_family(target: &Path, family: &str) -> Result<Self> {
        let meta = fs::symlink_metadata(target)?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
            return Err("target must be a real directory owned by this user".into());
        }
        let lock = fs::symlink_metadata(target.join(LOCK))?;
        if !lock.is_file() || lock.nlink() != 1 || lock.uid() != meta.uid() {
            return Err("target lease must be a private regular file with one link".into());
        }
        Ok(Self {
            schema: 1,
            manager: "tinline-tasks".into(),
            family: family.into(),
            path: fs::canonicalize(target)?,
            uid: meta.uid(),
            device: meta.dev(),
            inode: meta.ino(),
            lease_device: lock.dev(),
            lease_inode: lock.ino(),
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.len() > 4096
        || meta.nlink() != 1
        || meta.uid() != unsafe { libc::geteuid() }
    {
        return Err("invalid target control file type/size/link count/owner".into());
    }
    let mut bytes = Vec::new();
    (&mut file).take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err("target control file is too large".into());
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}
fn create_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.sync_all()?;
    Ok(())
}
fn write_usage(owner: &Owner, usage: &Usage) -> Result<()> {
    if Owner::for_family(&owner.path, &owner.family)? != *owner
        || read_json::<Owner>(&owner.path.join(OWNER))?.as_ref() != Some(owner)
    {
        return Err("target identity/ownership changed".into());
    }
    let temp = owner.path.join(format!(
        ".tasks-target.usage.{}.{}.tmp",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    create_json(&temp, usage)?;
    fs::rename(temp, owner.path.join(USAGE))?;
    File::open(&owner.path)?.sync_all()?;
    Ok(())
}

/// The borrow keeps the lease alive through final usage recording. If the worker
/// is killed, unfinished usage remains protected from retirement proposals.
pub struct TargetUse<'a> {
    _lease: &'a TargetLease,
    owner: Owner,
    usage: Usage,
}
impl Drop for TargetUse<'_> {
    fn drop(&mut self) {
        self.usage.finished_at = Some(now());
        // Failure leaves unfinished usage: the planner fails closed, not eligible.
        let _ = write_usage(&self.owner, &self.usage);
    }
}

/// Must be called while holding this target's lease. Only empty directories
/// (apart from the stable lock) are initialized; populated legacy caches are NOT
/// adopted, even when they happen to have an old task-manager lock file.
pub fn begin_use<'a>(lease: &'a TargetLease, target: &Path) -> Result<Option<TargetUse<'a>>> {
    begin_family(lease, target, "self-tests")
}

/// Distinct provenance; not eligible for self-test retirement. No legacy adoption.
pub(crate) fn begin_android_use<'a>(
    lease: &'a TargetLease,
    target: &Path,
) -> Result<TargetUse<'a>> {
    begin_family(lease, target, "android-debug")?
        .ok_or_else(|| "unowned populated Android lane; refusing adoption".into())
}
fn begin_family<'a>(
    lease: &'a TargetLease,
    target: &Path,
    family: &str,
) -> Result<Option<TargetUse<'a>>> {
    let lock_meta = fs::symlink_metadata(target.join(LOCK))?;
    let held = lease.file().metadata()?;
    if !lock_meta.is_file() || (lock_meta.dev(), lock_meta.ino()) != (held.dev(), held.ino()) {
        return Err("target lease identity mismatch".into());
    }
    let expected = Owner::for_family(target, family)?;
    let owner = match read_json::<Owner>(&target.join(OWNER))? {
        Some(owner) if owner == expected => owner,
        Some(_) => return Err("target ownership marker does not match this directory".into()),
        None => {
            for entry in fs::read_dir(target)? {
                if entry?.file_name() != LOCK {
                    return Ok(None); // Preserve unknown legacy ownership.
                }
            }
            create_json(&target.join(OWNER), &expected)?;
            File::open(target)?.sync_all()?;
            expected
        }
    };
    if let Some(previous) = read_json::<Usage>(&target.join(USAGE))? {
        validate_usage(&previous)?;
    }
    let usage = Usage {
        started_at: now(),
        finished_at: None,
    };
    write_usage(&owner, &usage)?;
    Ok(Some(TargetUse {
        _lease: lease,
        owner,
        usage,
    }))
}
fn validate_usage(usage: &Usage) -> Result<()> {
    if usage.started_at < 0 || usage.finished_at.is_some_and(|at| at < usage.started_at) {
        return Err("invalid target usage timestamps".into());
    }
    Ok(())
}

#[derive(Debug, Default, Serialize)]
pub struct Inventory {
    pub allocated_bytes: u64,
    pub disposable_allocated_bytes: u64,
    pub entries: usize,
    pub latest_modified_at: i64,
    pub complete: bool,
    pub symlinks: usize,
    pub external_hardlinks: usize,
    pub issues: Vec<String>,
}
impl Inventory {
    fn issue(&mut self, reason: &str) {
        self.complete = false;
        if self.issues.len() < 8 && !self.issues.iter().any(|s| s == reason) {
            self.issues.push(reason.into());
        }
    }
}
struct Inode {
    links: u64,
    seen: u64,
}

/// Bounded metadata-only walk. Never intentionally follows links or crosses
/// filesystem boundaries. Callers hold the cooperative target lease. It is not
/// an atomic filesystem snapshot or defense against hostile same-user races.
pub fn inventory(target: &Path) -> Result<Inventory> {
    inventory_with_limits(target, MAX_ENTRIES, MAX_DEPTH, MAX_SCAN_TIME)
}
fn inventory_with_limits(
    target: &Path,
    max_entries: usize,
    max_depth: usize,
    max_time: Duration,
) -> Result<Inventory> {
    let root = fs::symlink_metadata(target)?;
    if !root.is_dir() {
        return Err("inventory root must be a real directory".into());
    }
    let started = Instant::now();
    let mut report = Inventory {
        complete: true,
        ..Default::default()
    };
    let mut files: HashMap<(u64, u64), Inode> = HashMap::new();
    let mut pending = vec![(target.to_path_buf(), 0usize)];
    let mut controls = 0u64;
    while let Some((dir, depth)) = pending.pop() {
        if started.elapsed() >= max_time {
            report.issue("inventory time limit reached");
            break;
        }
        let before = match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() && meta.dev() == root.dev() => meta,
            _ => {
                report.issue("directory changed or crosses a filesystem");
                continue;
            }
        };
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                report.issue("unreadable directory");
                continue;
            }
        };
        for entry in entries {
            if report.entries >= max_entries || started.elapsed() >= max_time {
                report.issue("inventory entry/time limit reached");
                pending.clear();
                break;
            }
            report.entries += 1;
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(_) => {
                    report.issue("unreadable directory entry");
                    continue;
                }
            };
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => {
                    report.issue("entry changed/unreadable");
                    continue;
                }
            };
            if meta.dev() != root.dev() {
                report.issue("cross-filesystem entry");
                continue;
            }
            match meta
                .modified()
                .ok()
                .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            {
                Some(at) => {
                    report.latest_modified_at = report
                        .latest_modified_at
                        .max(at.as_secs().min(i64::MAX as u64) as i64)
                }
                None => report.issue("unreadable modification time"),
            }
            let allocated = meta.blocks().saturating_mul(512);
            if meta.is_dir() {
                report.allocated_bytes = report.allocated_bytes.saturating_add(allocated);
                if depth >= max_depth {
                    report.issue("inventory depth limit reached");
                } else {
                    pending.push((path, depth + 1));
                }
            } else if meta.is_file() || meta.is_symlink() {
                if meta.is_symlink() {
                    report.symlinks += 1;
                }
                let key = (meta.dev(), meta.ino());
                if let Some(inode) = files.get_mut(&key) {
                    inode.seen += 1;
                    if inode.links != meta.nlink() {
                        report.issue("link count changed during inventory");
                    }
                } else {
                    files.insert(
                        key,
                        Inode {
                            links: meta.nlink(),
                            seen: 1,
                        },
                    );
                    report.allocated_bytes = report.allocated_bytes.saturating_add(allocated);
                    if depth == 0
                        && matches!(
                            path.file_name().and_then(|n| n.to_str()),
                            Some(LOCK | OWNER | USAGE)
                        )
                    {
                        controls = controls.saturating_add(allocated);
                        if meta.nlink() != 1 {
                            report.issue("control file has multiple links");
                        }
                    }
                }
            } else {
                report.issue("special file in target");
            }
        }
        if !fs::symlink_metadata(&dir).is_ok_and(|after| {
            after.dev() == before.dev()
                && after.ino() == before.ino()
                && after.mtime() == before.mtime()
                && after.mtime_nsec() == before.mtime_nsec()
        }) {
            report.issue("directory changed during inventory");
        }
    }
    report.external_hardlinks = files
        .values()
        .filter(|inode| inode.links != inode.seen)
        .count();
    report.disposable_allocated_bytes = report.allocated_bytes.saturating_sub(controls);
    Ok(report)
}

/// Admission threshold only, NOT a quota. A build can grow past the threshold;
/// a subsequent job is refused rather than silently deleting its target.
pub fn require_budget(target: &Path) -> Result<Inventory> {
    let inventory = inventory(target)?;
    require_budget_for(&inventory, TARGET_BUDGET_BYTES)?;
    Ok(inventory)
}
fn require_budget_for(inventory: &Inventory, budget: u64) -> Result<()> {
    if !inventory.complete {
        return Err(format!(
            "target budget unverifiable: {:?}; inspect target before resubmitting",
            inventory.issues
        )
        .into());
    }
    if inventory.allocated_bytes >= budget {
        return Err(format!("target budget reached: {} allocated bytes, {} byte limit; run tasks cache-plan and review before resubmitting", inventory.allocated_bytes, budget).into());
    }
    Ok(())
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Missing,
    ProtectedActive,
    ProtectedUnknown,
    ProtectedReview,
    RetainRecent,
    ReviewCandidate,
}
#[derive(Debug, Serialize)]
pub struct TargetPlan {
    pub path: PathBuf,
    pub status: Status,
    pub reason: String,
    pub ownership_verified: bool,
    pub usage: Option<Usage>,
    pub inventory: Option<Inventory>,
    pub target_budget_bytes: u64,
}
impl TargetPlan {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            status: Status::ProtectedReview,
            reason: String::new(),
            ownership_verified: false,
            usage: None,
            inventory: None,
            target_budget_bytes: TARGET_BUDGET_BYTES,
        }
    }
}

pub fn inspect(target: &Path) -> TargetPlan {
    let mut plan = TargetPlan::new(target);
    let result = inspect_inner(target, &mut plan, now(), IDLE_SECONDS);
    if let Err(error) = result {
        plan.status = Status::ProtectedReview;
        plan.reason = error.to_string();
    }
    plan
}
fn inspect_inner(target: &Path, plan: &mut TargetPlan, time: i64, idle: u64) -> Result<()> {
    if !target.is_absolute() {
        return Err("target path must be absolute".into());
    }
    match fs::symlink_metadata(target) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            plan.status = Status::Missing;
            plan.reason = "not created; no action".into();
            return Ok(());
        }
        Err(e) => return Err(e.into()),
        Ok(meta) if !meta.is_dir() => {
            return Err("target root is not a real directory; do not follow aliases".into());
        }
        _ => {}
    }
    let _lease = match TargetLease::try_existing(target) {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            plan.status = Status::ProtectedActive;
            plan.reason = "target lease busy; inventory skipped".into();
            return Ok(());
        }
        Err(_) => {
            plan.status = Status::ProtectedUnknown;
            plan.reason =
                "stable lease missing/unverifiable; never create/adopt it during planning".into();
            return Ok(());
        }
    };
    let expected = Owner::expected(target)?;
    match read_json::<Owner>(&target.join(OWNER))? {
        Some(owner) if owner == expected => plan.ownership_verified = true,
        _ => {
            plan.status = Status::ProtectedUnknown;
            plan.reason =
                "no matching manager provenance; legacy caches require owner review".into();
            return Ok(());
        }
    }
    let usage = read_json::<Usage>(&target.join(USAGE))?.ok_or("target usage record missing")?;
    validate_usage(&usage)?;
    plan.usage = Some(usage.clone());
    let finished = usage.finished_at.ok_or("unfinished usage (crash or metadata-write failure); protected until a clean managed use/review")?;
    let inventory = inventory(target)?;
    let latest = finished.max(inventory.latest_modified_at);
    let clean = inventory.complete && inventory.symlinks == 0 && inventory.external_hardlinks == 0;
    let over_budget = inventory.allocated_bytes >= TARGET_BUDGET_BYTES;
    plan.inventory = Some(inventory);
    if !clean {
        return Err(
            "inventory incomplete or contains links needing review; not a retirement candidate"
                .into(),
        );
    }
    if over_budget || time.saturating_sub(latest) >= idle.min(i64::MAX as u64) as i64 {
        plan.status = Status::ReviewCandidate;
        plan.reason = if over_budget {
            "idle, manager-owned target at/over budget"
        } else {
            "idle, clean managed target unused for retention interval"
        }
        .into();
    } else {
        plan.status = Status::RetainRecent;
        plan.reason = "recently used managed target; retain".into();
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub schema: u32,
    pub generated_at: i64,
    pub dry_run: bool,
    pub deletion_enabled: bool,
    pub preserve_target_control_files: [&'static str; 3],
    pub job_results_in_scope: bool,
    pub compiler_cache_in_scope: bool,
    pub target_budget_bytes: u64,
    pub idle_retention_seconds: u64,
    pub enumeration_complete: bool,
    pub targets: Vec<TargetPlan>,
    pub legacy_roots_owner_unknown: Vec<PathBuf>,
    pub warnings: Vec<String>,
}
/// Read-only: no target/control/lock creation; briefly probes existing leases.
/// Legacy roots are listed, NOT recursively inventoried or declared safe.
pub fn plan() -> Result<Plan> {
    let home = std::env::var_os("HOME").ok_or("HOME is required for cache planning")?;
    let root = PathBuf::from(home).join("build-cache/p2p_chat");
    plan_at(&root, &crate::cache::target_path()?)
}
fn plan_at(root: &Path, selected: &Path) -> Result<Plan> {
    let mut plan = Plan {
        schema: 1,
        generated_at: now(),
        dry_run: true,
        deletion_enabled: false,
        preserve_target_control_files: [LOCK, OWNER, USAGE],
        job_results_in_scope: false,
        compiler_cache_in_scope: false,
        target_budget_bytes: TARGET_BUDGET_BYTES,
        idle_retention_seconds: IDLE_SECONDS,
        enumeration_complete: true,
        targets: Vec::new(),
        legacy_roots_owner_unknown: Vec::new(),
        warnings: Vec::new(),
    };
    let mut targets = BTreeSet::from([
        selected.to_path_buf(),
        root.join("tasks/targets/self-tests"),
    ]);
    for (directory, managed) in [
        (root.to_path_buf(), false),
        (root.join("tasks/targets"), true),
    ] {
        match fs::symlink_metadata(&directory) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(meta) if meta.is_dir() => {}
            _ => {
                plan.enumeration_complete = false;
                plan.warnings.push(format!(
                    "cannot enumerate real directory {}",
                    directory.display()
                ));
                continue;
            }
        }
        match fs::read_dir(&directory) {
            Ok(entries) => {
                for (index, entry) in entries.enumerate() {
                    if index >= MAX_ROOTS {
                        plan.enumeration_complete = false;
                        plan.warnings.push("cache root enumeration capped".into());
                        break;
                    }
                    match entry {
                        Ok(entry) if managed => {
                            targets.insert(entry.path());
                        }
                        Ok(entry) if entry.file_name() != "tasks" => {
                            plan.legacy_roots_owner_unknown.push(entry.path())
                        }
                        Ok(_) => {}
                        Err(_) => {
                            plan.enumeration_complete = false;
                            plan.warnings.push("unreadable cache root entry".into());
                        }
                    }
                }
            }
            Err(e) => {
                plan.enumeration_complete = false;
                plan.warnings.push(format!("{}: {e}", directory.display()));
            }
        }
    }
    let started = Instant::now();
    // Always inspect the explicitly selected target before applying pool caps.
    let ordered = std::iter::once(selected.to_path_buf())
        .chain(targets.into_iter().filter(|target| target != selected));
    for (index, target) in ordered.enumerate() {
        if index >= MAX_TARGETS || started.elapsed() >= MAX_PLAN_TIME {
            plan.enumeration_complete = false;
            plan.warnings
                .push("target count/plan time cap reached; remaining targets not inspected".into());
            break;
        }
        plan.targets.push(inspect(&target));
    }
    plan.legacy_roots_owner_unknown.sort();
    Ok(plan)
}
