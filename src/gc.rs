//! `tasks gc`: age-based cleanup of the two caches that fill a disk without anyone asking.
//!
//! * **Offload copies** (`~/offload/checkouts/<id>` plus `<id>.identity`, on a worker): the
//!   whole copy and its identity file go once idle longer than `offload_idle_days`.
//! * **Output caches** (`~/build-cache/p2p_chat/tasks/checkouts/<sha256(root)>`, on a host): the
//!   contents of each output directory go, the directories stay (`plan_links` never replaces
//!   an existing link, so a removed directory would leave a dangling `target`).
//!
//! Dry run by default; `apply` deletes. Idle = nothing in the tree has changed for the limit.
//! Anything a live job or a process of this user is using is skipped, a tree that could not be
//! scanned completely is skipped, and symlinks are never followed. Limits come from the
//! `gc::` section of the policy file.
#[cfg(test)]
#[path = "gc_tests.rs"]
mod tests;

use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const DEFAULT_OFFLOAD_IDLE_DAYS: u64 = 3;
pub const DEFAULT_OUTPUT_IDLE_DAYS: u64 = 7;
const DAY: i64 = 24 * 60 * 60;
/// Entries one scan may visit; a tree beyond it is reported, never deleted.
const MAX_ENTRIES: u64 = 5_000_000;
/// An output cache below this is just its (empty) directory skeleton.
const MIN_OUTPUT_BYTES: u64 = 1 << 20;

/// The `gc::` section of the policy file (days, 1..=365).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offload_idle_days: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_idle_days: Option<u64>,
    /// Free the oldest idle caches by itself when the cache disk drops under its reserve
    /// (default on; `false` leaves cleanup to `tasks gc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_when_full: Option<bool>,
}

impl GcPolicy {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
    pub fn auto_when_full(&self) -> bool {
        self.auto_when_full.unwrap_or(true)
    }
    pub fn limits(&self) -> Result<Limits, String> {
        let day = |name: &str, value: Option<u64>, default: u64| match value {
            None => Ok(default),
            Some(d) if (1..=365).contains(&d) => Ok(d),
            Some(d) => Err(format!("gc::{name}: {d} is outside 1..=365 days")),
        };
        Ok(Limits {
            offload_idle_s: day(
                "offload_idle_days",
                self.offload_idle_days,
                DEFAULT_OFFLOAD_IDLE_DAYS,
            )? as i64
                * DAY,
            output_idle_s: day(
                "output_idle_days",
                self.output_idle_days,
                DEFAULT_OUTPUT_IDLE_DAYS,
            )? as i64
                * DAY,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub offload_idle_s: i64,
    pub output_idle_s: i64,
}

#[derive(Clone, Debug)]
pub struct Roots {
    pub offload: PathBuf,
    pub outputs: PathBuf,
}

impl Roots {
    pub fn of_home(home: &Path) -> Self {
        Self {
            offload: home.join("offload/checkouts"),
            outputs: home.join("build-cache/p2p_chat/tasks/checkouts"),
        }
    }
}

/// What is using the disk right now: job sources (live jobs) and process working directories.
#[derive(Clone, Debug, Default)]
pub struct InUse {
    pub sources: Vec<PathBuf>,
    pub cwds: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    OffloadCopy,
    OutputCache,
}

#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub kind: Kind,
    /// OffloadCopy: the copy (removed whole). OutputCache: the cache directory (emptied).
    pub path: PathBuf,
    /// Removed with it (the `.identity` file).
    pub also: Vec<PathBuf>,
    pub bytes: u64,
    pub idle_s: i64,
    /// Where the copy came from, when its identity file says.
    pub origin: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Plan {
    pub candidates: Vec<Candidate>,
    pub skipped: Vec<Skipped>,
}

impl Plan {
    pub fn bytes(&self) -> u64 {
        self.candidates.iter().map(|c| c.bytes).sum()
    }
}

#[derive(Debug, Default)]
struct Scan {
    bytes: u64,
    newest: i64,
    entries: u64,
    complete: bool,
}

/// Allocated bytes and newest mtime of everything below `path`, without following symlinks or
/// crossing into another filesystem.
fn scan(path: &Path) -> Scan {
    let mut out = Scan {
        complete: true,
        ..Scan::default()
    };
    let Ok(top) = fs::symlink_metadata(path) else {
        out.complete = false;
        return out;
    };
    let device = top.dev();
    let mut stack = vec![path.to_path_buf()];
    out.newest = top.mtime();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            out.complete = false;
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                out.complete = false;
                continue;
            };
            out.entries += 1;
            if out.entries > MAX_ENTRIES {
                out.complete = false;
                return out;
            }
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                out.complete = false;
                continue;
            };
            out.bytes += meta.blocks() * 512;
            out.newest = out.newest.max(meta.mtime());
            if meta.is_dir() && meta.dev() == device {
                stack.push(entry.path());
            }
        }
    }
    out
}

fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

fn in_use(path: &Path, using: &InUse) -> Option<String> {
    if using
        .sources
        .iter()
        .any(|s| s.starts_with(path) || path.starts_with(s))
    {
        return Some("a live job uses it".into());
    }
    if using.cwds.iter().any(|c| c.starts_with(path)) {
        return Some("a process of yours is working in it".into());
    }
    None
}

fn origin(identity: &Path) -> Option<String> {
    let text = fs::read_to_string(identity).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(format!(
        "{}:{}",
        value.get("host")?.as_str()?,
        value.get("toplevel")?.as_str()?
    ))
}

/// The key `plan_outputs` files a checkout's outputs under: sha256 of the root path.
fn output_key(root: &Path) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()))
}

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    out.sort();
    out
}

pub fn plan(roots: &Roots, limits: Limits, using: &InUse, now: i64) -> Plan {
    let mut plan = Plan::default();
    offload_copies(&mut plan, roots, limits, using, now);
    output_caches(&mut plan, roots, limits, using, now);
    plan
}

fn offload_copies(plan: &mut Plan, roots: &Roots, limits: Limits, using: &InUse, now: i64) {
    let dir = &roots.offload;
    for name in names(dir) {
        if name.starts_with('.') || name.ends_with(".identity") {
            continue;
        }
        let path = dir.join(&name);
        let identity = dir.join(format!("{name}.identity"));
        if !real_dir(&path) {
            plan.skipped.push(Skipped {
                path,
                reason: "not a real directory".into(),
            });
            continue;
        }
        if let Some(reason) = in_use(&path, using) {
            plan.skipped.push(Skipped { path, reason });
            continue;
        }
        let tree = scan(&path);
        if !tree.complete {
            plan.skipped.push(Skipped {
                path,
                reason: "could not be scanned completely".into(),
            });
            continue;
        }
        let last =
            fs::symlink_metadata(&identity).map_or(tree.newest, |m| m.mtime().max(tree.newest));
        let idle = now - last;
        if idle < limits.offload_idle_s {
            continue;
        }
        let has_identity = fs::symlink_metadata(&identity).is_ok_and(|m| m.is_file());
        plan.candidates.push(Candidate {
            kind: Kind::OffloadCopy,
            bytes: tree.bytes,
            idle_s: idle,
            origin: origin(&identity),
            also: if has_identity { vec![identity] } else { vec![] },
            path,
        });
    }
    // An identity file whose copy is gone is clutter.
    for name in names(dir) {
        let Some(id) = name.strip_suffix(".identity") else {
            continue;
        };
        if id.starts_with('.') || fs::symlink_metadata(dir.join(id)).is_ok() {
            continue;
        }
        let path = dir.join(&name);
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file() && now - meta.mtime() >= limits.offload_idle_s {
            plan.candidates.push(Candidate {
                kind: Kind::OffloadCopy,
                bytes: meta.blocks() * 512,
                idle_s: now - meta.mtime(),
                origin: origin(&path),
                also: vec![],
                path,
            });
        }
    }
}

fn output_caches(plan: &mut Plan, roots: &Roots, limits: Limits, using: &InUse, now: i64) {
    let live_keys: Vec<String> = using
        .sources
        .iter()
        .chain(&using.cwds)
        .flat_map(|p| p.ancestors().map(output_key).collect::<Vec<_>>())
        .collect();
    for name in names(&roots.outputs) {
        let path = roots.outputs.join(&name);
        if !real_dir(&path) {
            continue;
        }
        if live_keys.contains(&name) {
            plan.skipped.push(Skipped {
                path,
                reason: "a live job or a process of yours is in this checkout".into(),
            });
            continue;
        }
        let tree = scan(&path);
        if !tree.complete {
            plan.skipped.push(Skipped {
                path,
                reason: "could not be scanned completely".into(),
            });
            continue;
        }
        let idle = now - tree.newest;
        // An already emptied cache has nothing to give back.
        if idle < limits.output_idle_s || tree.bytes < MIN_OUTPUT_BYTES {
            continue;
        }
        plan.candidates.push(Candidate {
            kind: Kind::OutputCache,
            bytes: tree.bytes,
            idle_s: idle,
            origin: None,
            also: vec![],
            path,
        });
    }
}

/// Nothing modified in the last 10 minutes is taken under disk pressure: a build is writing it.
const PRESSURE_MIN_IDLE_S: i64 = 600;
/// Pressure cleanup stops this far above the reserve, so it does not run again at once.
pub const PRESSURE_HEADROOM: u64 = 5 << 30;

/// One removal made under disk pressure.
#[derive(Debug, Serialize)]
pub struct Relieved {
    pub path: PathBuf,
    pub kind: Kind,
    pub bytes: u64,
    pub idle_s: i64,
    pub free_before: u64,
    pub free_after: u64,
    pub error: Option<String>,
}

/// Disk-pressure cleanup: while a cache's disk has less than `reserve` free, remove the OLDEST
/// idle entries (no age limit beyond the last 10 minutes) until it has `target` free. `free`
/// measures the disk under a path. Whatever a live job or a process of yours uses is never
/// taken; every removal is returned (and so can be audited).
pub fn relieve(
    roots: &Roots,
    using: &InUse,
    now: i64,
    reserve: u64,
    target: u64,
    free: &dyn Fn(&Path) -> Option<u64>,
) -> Vec<Relieved> {
    let none = Limits {
        offload_idle_s: 0,
        output_idle_s: 0,
    };
    let mut candidates = plan(roots, none, using, now).candidates;
    candidates.retain(|c| c.idle_s >= PRESSURE_MIN_IDLE_S);
    candidates.sort_by_key(|c| std::cmp::Reverse(c.idle_s));
    // Per cache kind: removing starts below `reserve` and runs on up to `target`.
    let (mut copies_active, mut outputs_active) = (false, false);
    let mut out = vec![];
    for c in candidates {
        let Some(before) = free(&c.path) else {
            continue;
        };
        let active = match c.kind {
            Kind::OffloadCopy => &mut copies_active,
            Kind::OutputCache => &mut outputs_active,
        };
        *active |= before < reserve;
        if *active && before >= target {
            *active = false;
        }
        if !*active {
            continue;
        }
        let outcome = apply(
            &Plan {
                candidates: vec![c.clone()],
                skipped: vec![],
            },
            roots,
        )
        .remove(0);
        out.push(Relieved {
            free_after: free(&c.path).unwrap_or(before),
            path: c.path,
            kind: c.kind,
            bytes: c.bytes,
            idle_s: c.idle_s,
            free_before: before,
            error: outcome.error,
        });
    }
    out
}

/// Job sources of unfinished jobs (from the daemon at `socket`; never autostarts one) and the
/// working directories of this user's processes. The flag says whether the daemon answered.
pub fn in_use_via(socket: &Path) -> (InUse, bool) {
    use crate::{Action, Phase};
    let mut using = InUse::default();
    let answered = match crate::request(socket, Action::ListActive) {
        Ok(r) if r.error.is_none() => {
            using.sources = r
                .jobs
                .iter()
                .filter(|j| {
                    !matches!(
                        j.phase,
                        Phase::Passed | Phase::Failed | Phase::Interrupted | Phase::Cancelled
                    )
                })
                .map(|j| PathBuf::from(&j.spec.source))
                .collect();
            true
        }
        _ => false,
    };
    let me = unsafe { libc::geteuid() };
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        if entry.metadata().is_ok_and(|m| m.uid() == me)
            && let Ok(cwd) = fs::read_link(entry.path().join("cwd"))
        {
            using.cwds.push(cwd);
        }
    }
    (using, answered)
}

#[derive(Debug, Serialize)]
pub struct Outcome {
    pub path: PathBuf,
    pub error: Option<String>,
}

/// Removes `path` (the tree, not what a symlink in it points at); a missing path is done.
fn remove_tree(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
        Ok(m) if m.is_dir() => fs::remove_dir_all(path).map_err(|e| e.to_string()),
        Ok(_) => fs::remove_file(path).map_err(|e| e.to_string()),
    }
}

/// Empties each output directory of a cache entry and keeps the directory itself: the
/// checkout's `target` link points at it.
fn empty_outputs(cache: &Path) -> Result<(), String> {
    for name in names(cache) {
        let child = cache.join(&name);
        if real_dir(&child) {
            for inner in names(&child) {
                remove_tree(&child.join(inner))?;
            }
        } else {
            remove_tree(&child)?;
        }
    }
    Ok(())
}

/// The candidate's paths must still sit directly under their root: a plan is only ever
/// applied to what it listed.
fn under(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root) && path.file_name().is_some()
}

pub fn apply(plan: &Plan, roots: &Roots) -> Vec<Outcome> {
    let mut out = vec![];
    for c in &plan.candidates {
        let root = match c.kind {
            Kind::OffloadCopy => &roots.offload,
            Kind::OutputCache => &roots.outputs,
        };
        if !under(root, &c.path) || !c.also.iter().all(|a| under(root, a)) {
            out.push(Outcome {
                path: c.path.clone(),
                error: Some("outside its cache root; refused".into()),
            });
            continue;
        }
        let result = match c.kind {
            Kind::OffloadCopy => {
                remove_tree(&c.path).and_then(|()| c.also.iter().try_for_each(|a| remove_tree(a)))
            }
            Kind::OutputCache => empty_outputs(&c.path),
        };
        out.push(Outcome {
            path: c.path.clone(),
            error: result.err(),
        });
    }
    out
}

pub fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

pub fn age(seconds: i64) -> String {
    match seconds {
        s if s >= DAY => format!("{} d", s / DAY),
        s => format!("{} h", s / 3600),
    }
}
