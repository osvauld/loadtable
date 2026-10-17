//! `tasks tmp-usage`: a read-only, bounded inventory of /tmp and large cache
//! directories, grouped by top-level entry and annotated with the queue jobs,
//! checkouts and live processes that point at each group, to guide an operator
//! freeing headroom (e.g. RAM-backed /tmp starving the memory gate).
//!
//! It NEVER deletes, moves, truncates, chmods or writes anything, never follows
//! symlinks, never crosses into another filesystem and stops at a file-count and
//! time budget (reported as truncated). Annotations are hints, not ownership
//! proof: no deletion decision may be inferred from them.
use crate::{Action, Job, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_ENTRIES: u64 = 500_000;
pub const DEFAULT_MAX_SECONDS: u64 = 20;
/// Groups listed per root in JSON; the rest are summed into `other`.
const MAX_GROUPS: usize = 400;
const MAX_PIDS: usize = 8192;
const MAX_PIDS_PER_GROUP: usize = 16;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_entries: u64,
    pub max_time: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_time: Duration::from_secs(DEFAULT_MAX_SECONDS),
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct JobRef {
    pub id: i64,
    pub owner: String,
    pub phase: String,
    pub source: String,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Group {
    pub path: PathBuf,
    /// "dir", "file", "symlink" or "other". Symlinks are never followed.
    pub kind: String,
    pub uid: u32,
    pub owned_by_me: bool,
    /// Allocated bytes (st_blocks); on tmpfs this is RAM/swap actually used.
    pub bytes: u64,
    pub apparent_bytes: u64,
    pub files: u64,
    pub dirs: u64,
    /// Unix seconds of the newest mtime seen (a staleness hint only).
    pub newest_mtime: i64,
    /// Unreadable directories/entries (e.g. other users' private dirs).
    pub errors: u64,
    /// Mount points below this group that were not entered.
    pub mounts_skipped: u64,
    pub truncated: bool,
    /// Queue jobs whose live cwd is inside this group.
    pub jobs: Vec<JobRef>,
    /// Checkout this group serves, when derivable (cache key or scratch name).
    pub checkout: Option<PathBuf>,
    /// Own-user processes whose cwd is inside this group (cwd only, bounded).
    pub in_use_by_pids: Vec<u32>,
    pub notes: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct RootReport {
    pub path: PathBuf,
    pub exists: bool,
    pub fs_type: String,
    pub ram_backed: bool,
    pub fs_total_bytes: u64,
    pub fs_available_bytes: u64,
    pub scanned_bytes: u64,
    pub groups: Vec<Group>,
    /// Groups beyond the listed ones, summed.
    pub other_groups: u64,
    pub other_bytes: u64,
    pub truncated: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub read_only: bool,
    pub roots: Vec<RootReport>,
    pub entries_scanned: u64,
    pub max_entries: u64,
    pub max_seconds: f64,
    pub elapsed_seconds: f64,
    pub truncated: bool,
    /// "daemon" when job annotations came from a running daemon, else why not.
    pub jobs_source: String,
    pub notes: Vec<String>,
}

/// Context used to annotate groups. Collected read-only before scanning.
#[derive(Default)]
pub struct Context {
    pub jobs: Vec<JobRef>,
    /// (pid, cwd) of this user's processes.
    pub processes: Vec<(u32, PathBuf)>,
    /// Directories whose children are reported as separate groups.
    pub expand: Vec<PathBuf>,
    pub lock_dir: Option<PathBuf>,
}

fn euid() -> u32 {
    unsafe { libc::geteuid() }
}

pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/tmp")];
    let temp = std::env::temp_dir();
    if temp != Path::new("/tmp") && temp.is_absolute() {
        roots.push(temp);
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join("build-cache"));
        roots.push(
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(".cache")),
        );
    }
    roots.push(crate::state_dir());
    roots
}

fn default_expand() -> Vec<PathBuf> {
    let mut expand = vec![];
    if let Ok(lock) = crate::admission::root() {
        expand.push(lock);
    }
    expand.push(crate::admission::default_root());
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let tasks = home.join("build-cache/p2p_chat/tasks");
        expand.push(home.join("build-cache/p2p_chat"));
        expand.push(tasks.clone());
        expand.push(tasks.join("checkouts"));
        expand.push(tasks.join("targets"));
    }
    expand.push(crate::state_dir().join("jobs"));
    expand
}

/// Jobs from an already running daemon only: never autostarts one.
fn daemon_jobs() -> (Vec<JobRef>, String) {
    let socket = crate::state_dir().join("taskd.sock");
    match crate::request(&socket, Action::List) {
        Ok(r) if r.error.is_none() => (r.jobs.iter().map(job_ref).collect(), "daemon".into()),
        Ok(r) => (
            vec![],
            format!("unavailable: {}", r.error.unwrap_or_default()),
        ),
        Err(e) => (
            vec![],
            format!("unavailable (daemon not running or unreachable): {e}"),
        ),
    }
}
fn job_ref(j: &Job) -> JobRef {
    JobRef {
        id: j.id,
        owner: j.spec.owner.clone(),
        phase: serde_json::to_value(&j.phase)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        source: j.spec.source.clone(),
    }
}
/// cwd of this user's processes, read-only via /proc, bounded.
fn own_processes() -> Vec<(u32, PathBuf)> {
    let mut out = vec![];
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten().take(MAX_PIDS) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if entry.metadata().is_ok_and(|m| m.uid() == euid())
            && let Ok(cwd) = fs::read_link(entry.path().join("cwd"))
        {
            out.push((pid, cwd));
        }
    }
    out
}

struct Fs {
    kind: String,
    ram: bool,
    total: u64,
    available: u64,
}
fn filesystem(path: &Path) -> Option<Fs> {
    let name = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(name.as_ptr(), s.as_mut_ptr()) } != 0 {
        return None;
    }
    let s = unsafe { s.assume_init() };
    // f_type is i64 on x86_64/aarch64 glibc but differs on other targets.
    #[allow(clippy::unnecessary_cast)]
    let magic = s.f_type as i64;
    let kind = match magic {
        0x0102_1994 => "tmpfs",
        0x8584_58f6 => "ramfs",
        0xEF53 => "ext2/3/4",
        0x9123_683E => "btrfs",
        0x5846_5342 => "xfs",
        0x2fc1_2fc1 => "zfs",
        0x794c_7630 => "overlay",
        0x6969 => "nfs",
        0xF2F5_2010 => "f2fs",
        _ => "",
    };
    let block = s.f_bsize as u128;
    Some(Fs {
        kind: if kind.is_empty() {
            format!("0x{magic:x}")
        } else {
            kind.into()
        },
        ram: matches!(magic, 0x0102_1994 | 0x8584_58f6),
        total: (s.f_blocks as u128 * block).min(u64::MAX as u128) as u64,
        available: (s.f_bavail as u128 * block).min(u64::MAX as u128) as u64,
    })
}

/// Global entry/time budget, shared fairly: each root gets an equal share of
/// what is left, so one huge root cannot starve the others.
struct Budget {
    start: Instant,
    limits: Limits,
    entries: u64,
    root_cap: u64,
    root_deadline: Instant,
    exhausted: bool,
    any_exhausted: bool,
}
impl Budget {
    fn start_root(&mut self, roots_left: usize) {
        let roots_left = roots_left.max(1);
        let time_left = self.limits.max_time.saturating_sub(self.start.elapsed());
        self.root_deadline = Instant::now() + time_left / roots_left as u32;
        self.root_cap =
            self.entries + self.limits.max_entries.saturating_sub(self.entries) / roots_left as u64;
        self.exhausted = false;
    }
    fn spend(&mut self) -> bool {
        if self.exhausted {
            return false;
        }
        if self.entries >= self.root_cap || Instant::now() >= self.root_deadline {
            self.exhausted = true;
            self.any_exhausted = true;
            return false;
        }
        self.entries += 1;
        true
    }
}

/// Sizes one group without following symlinks or leaving its filesystem.
fn measure(group: &mut Group, budget: &mut Budget, seen: &mut HashSet<(u64, u64)>) {
    let Ok(top) = fs::symlink_metadata(&group.path) else {
        group.errors += 1;
        return;
    };
    let mut stack = vec![(group.path.clone(), top)];
    while let Some((path, m)) = stack.pop() {
        if !budget.spend() {
            group.truncated = true;
            return;
        }
        group.newest_mtime = group.newest_mtime.max(m.mtime());
        let counted = m.nlink() <= 1 || m.is_dir() || seen.insert((m.dev(), m.ino()));
        if counted {
            group.bytes += m.blocks() * 512;
            group.apparent_bytes += m.len();
        }
        if !m.is_dir() {
            group.files += 1;
            continue;
        }
        group.dirs += 1;
        let Ok(entries) = fs::read_dir(&path) else {
            group.errors += 1;
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                group.errors += 1;
                continue;
            };
            let child = entry.path();
            match fs::symlink_metadata(&child) {
                Ok(cm) if cm.is_dir() && cm.dev() != m.dev() => group.mounts_skipped += 1,
                Ok(cm) => stack.push((child, cm)),
                Err(_) => group.errors += 1,
            }
        }
    }
}

fn kind_of(m: &fs::Metadata) -> &'static str {
    let t = m.file_type();
    if t.is_symlink() {
        "symlink"
    } else if t.is_dir() {
        "dir"
    } else if t.is_file() {
        "file"
    } else {
        "other"
    }
}

/// Top-level groups of a root, expanding configured bucket directories.
fn group_paths(root: &Path, ctx: &Context, budget: &mut Budget, errors: &mut u64) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            *errors += 1;
            continue;
        };
        for entry in entries.flatten() {
            if !budget.spend() {
                return out;
            }
            let path = entry.path();
            let real_dir = fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
            if real_dir && ctx.expand.contains(&path) {
                pending.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// Agent scratch directories (e.g. `/tmp/claude-$UID/-home-u-my_repo`) encode
/// their cwd with every character other than ASCII letters/digits/`-` as `-`.
fn encoded(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}
fn ancestors(source: &str) -> impl Iterator<Item = &Path> {
    Path::new(source)
        .ancestors()
        .take(64)
        .filter(|p| p.components().count() > 1)
}

/// Checkout candidates of every job source (deepest ancestor first), indexed by
/// output-cache key (sha256 of the path) and by scratch-directory name.
struct CheckoutIndex {
    by_key: HashMap<String, PathBuf>,
    by_name: HashMap<String, PathBuf>,
}
fn checkout_index(jobs: &[JobRef]) -> CheckoutIndex {
    let mut index = CheckoutIndex {
        by_key: HashMap::new(),
        by_name: HashMap::new(),
    };
    let mut sources = HashSet::new();
    for job in jobs {
        if !sources.insert(job.source.as_str()) {
            continue;
        }
        for candidate in ancestors(&job.source) {
            let key = format!(
                "{:x}",
                Sha256::digest(candidate.as_os_str().as_encoded_bytes())
            );
            index
                .by_key
                .entry(key)
                .or_insert_with(|| candidate.to_path_buf());
            index
                .by_name
                .entry(encoded(candidate))
                .or_insert_with(|| candidate.to_path_buf());
        }
    }
    index
}

fn annotate(group: &mut Group, ctx: &Context, index: &CheckoutIndex) {
    for job in &ctx.jobs {
        if Path::new(&job.source).starts_with(&group.path) {
            group.jobs.push(job.clone());
        }
    }
    group.in_use_by_pids = ctx
        .processes
        .iter()
        .filter(|(_, cwd)| cwd.starts_with(&group.path))
        .map(|(pid, _)| *pid)
        .take(MAX_PIDS_PER_GROUP)
        .collect();
    let name = group
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Output cache key = sha256(canonical checkout or nested workspace root).
    let parent_is_checkouts = group
        .path
        .parent()
        .is_some_and(|p| p.file_name().is_some_and(|n| n == "checkouts"));
    group.checkout = parent_is_checkouts
        .then(|| index.by_key.get(&name))
        .flatten()
        .or_else(|| index.by_name.get(&name))
        .cloned();
    if parent_is_checkouts && group.checkout.is_none() && name.len() == 64 {
        group
            .notes
            .push("output cache for a checkout not in the job list".into());
    }
    if let Some(lock) = &ctx.lock_dir
        && group.path.parent() == Some(lock.as_path())
        && (name.ends_with(".lock") || name.ends_with(".holder") || name.starts_with("p2p-wait-"))
    {
        group
            .notes
            .push("admission lock/diagnostic file: never delete (stable flock inode)".into());
    }
    if !group.owned_by_me {
        group.notes.push(format!("owned by uid {}", group.uid));
    }
    if !group.in_use_by_pids.is_empty() {
        group
            .notes
            .push("a live process has its cwd inside; in use".into());
    }
    if group.jobs.iter().any(|j| {
        !matches!(
            j.phase.as_str(),
            "passed" | "failed" | "cancelled" | "interrupted"
        )
    }) {
        group.notes.push("an active queue job runs here".into());
    }
}

pub fn inventory(roots: &[PathBuf], ctx: &Context, limits: Limits) -> Report {
    let now = Instant::now();
    let mut budget = Budget {
        start: now,
        limits,
        entries: 0,
        root_cap: 0,
        root_deadline: now,
        exhausted: false,
        any_exhausted: false,
    };
    let index = checkout_index(&ctx.jobs);
    let mut seen = HashSet::new();
    let mut reports = vec![];
    let mut visited: Vec<PathBuf> = vec![];
    for (i, root) in roots.iter().enumerate() {
        let mut report = RootReport {
            path: root.clone(),
            ..Default::default()
        };
        // A root inside an earlier root (e.g. TMPDIR below /tmp) is reported once.
        if visited.iter().any(|v| root.starts_with(v)) {
            report.error = Some("inside an earlier root; already counted there".into());
            report.exists = root.exists();
            reports.push(report);
            continue;
        }
        let meta = match fs::symlink_metadata(root) {
            Ok(m) if m.is_dir() => m,
            Ok(_) => {
                report.exists = true;
                report.error = Some("not a real directory (symlinks are not followed)".into());
                reports.push(report);
                continue;
            }
            Err(e) => {
                report.error = Some(e.to_string());
                reports.push(report);
                continue;
            }
        };
        visited.push(root.clone());
        report.exists = true;
        if let Some(f) = filesystem(root) {
            report.fs_type = f.kind;
            report.ram_backed = f.ram;
            report.fs_total_bytes = f.total;
            report.fs_available_bytes = f.available;
        }
        budget.start_root(roots.len() - i);
        let mut errors = 0;
        let paths = group_paths(root, ctx, &mut budget, &mut errors);
        let mut groups = vec![];
        for path in paths {
            let Ok(m) = fs::symlink_metadata(&path) else {
                continue;
            };
            let mut group = Group {
                kind: kind_of(&m).into(),
                uid: m.uid(),
                owned_by_me: m.uid() == euid(),
                path,
                ..Default::default()
            };
            if m.is_dir() && m.dev() != meta.dev() {
                group.mounts_skipped = 1;
                group.notes.push("separate mount; not entered".into());
            } else {
                measure(&mut group, &mut budget, &mut seen);
            }
            annotate(&mut group, ctx, &index);
            report.truncated |= group.truncated;
            groups.push(group);
        }
        if errors > 0 {
            report.error = Some(format!("{errors} directories could not be listed"));
        }
        if budget.exhausted {
            report.truncated = true;
        }
        groups.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));
        report.scanned_bytes = groups.iter().map(|g| g.bytes).sum();
        if groups.len() > MAX_GROUPS {
            let rest = groups.split_off(MAX_GROUPS);
            report.other_groups = rest.len() as u64;
            report.other_bytes = rest.iter().map(|g| g.bytes).sum();
        }
        report.groups = groups;
        reports.push(report);
    }
    let mut notes = vec![
        "Read-only: nothing was deleted, moved or modified. Sizes are allocated blocks; hard links count once.".into(),
        "Annotations are hints, not proof of ownership. Confirm with the owner before removing anything; never remove admission lock files or another agent's data.".into(),
    ];
    if budget.any_exhausted {
        notes.push(format!(
            "Scan stopped at its budget ({} entries / {:.0} s); totals are lower bounds.",
            limits.max_entries,
            limits.max_time.as_secs_f64()
        ));
    }
    Report {
        read_only: true,
        roots: reports,
        entries_scanned: budget.entries,
        max_entries: limits.max_entries,
        max_seconds: limits.max_time.as_secs_f64(),
        elapsed_seconds: budget.start.elapsed().as_secs_f64(),
        truncated: budget.any_exhausted,
        jobs_source: String::new(),
        notes,
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
fn age(now: i64, mtime: i64) -> String {
    let s = (now - mtime).max(0);
    match s {
        0..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

pub fn render(report: &Report, top: usize) -> String {
    use std::fmt::Write;
    let now = crate::execution::now();
    let mut out = String::new();
    for root in &report.roots {
        let _ = write!(out, "{}", root.path.display());
        if let Some(e) = &root.error
            && root.groups.is_empty()
        {
            let _ = writeln!(out, ": {e}\n");
            continue;
        }
        let _ = writeln!(
            out,
            " [{}{}]: {} scanned; filesystem {} total, {} available{}",
            root.fs_type,
            if root.ram_backed { ", RAM-backed" } else { "" },
            human(root.scanned_bytes),
            human(root.fs_total_bytes),
            human(root.fs_available_bytes),
            if root.truncated { " (TRUNCATED)" } else { "" }
        );
        if let Some(e) = &root.error {
            let _ = writeln!(out, "  note: {e}");
        }
        let _ = writeln!(
            out,
            "  {:>10} {:>9} {:>6} {:<8} PATH",
            "SIZE", "FILES", "AGE", "OWNER"
        );
        for g in root.groups.iter().take(top) {
            let owner = if g.owned_by_me {
                "me".to_string()
            } else {
                format!("uid {}", g.uid)
            };
            let _ = writeln!(
                out,
                "  {:>10} {:>9} {:>6} {:<8} {}{}",
                human(g.bytes),
                g.files,
                age(now, g.newest_mtime),
                owner,
                g.path.display(),
                if g.truncated || g.errors > 0 {
                    " (partial)"
                } else {
                    ""
                }
            );
            let mut hints: Vec<String> = vec![];
            if let Some(c) = &g.checkout {
                hints.push(format!("checkout {}", c.display()));
            }
            for j in g.jobs.iter().rev().take(3) {
                hints.push(format!("job {} {} {}", j.id, j.owner, j.phase));
            }
            if !g.in_use_by_pids.is_empty() {
                hints.push(format!("cwd of pid {:?}", g.in_use_by_pids));
            }
            hints.extend(
                g.notes
                    .iter()
                    .filter(|n| !n.starts_with("owned by uid"))
                    .cloned(),
            );
            if !hints.is_empty() {
                let _ = writeln!(out, "  {:>36}-> {}", "", hints.join("; "));
            }
        }
        let hidden = root.groups.len().saturating_sub(top) as u64 + root.other_groups;
        if hidden > 0 {
            let rest: u64 =
                root.groups.iter().skip(top).map(|g| g.bytes).sum::<u64>() + root.other_bytes;
            let _ = writeln!(
                out,
                "  ... {hidden} smaller entries, {} (see --json)",
                human(rest)
            );
        }
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "{} entries in {:.1} s; job annotations: {}",
        report.entries_scanned, report.elapsed_seconds, report.jobs_source
    );
    for n in &report.notes {
        let _ = writeln!(out, "{n}");
    }
    out
}

const USAGE: &str =
    "usage: tasks tmp-usage [--json] [--top N] [--max-files N] [--max-seconds N] [PATH...]
  Read-only, bounded inventory of /tmp, ~/build-cache, ~/.cache and the task state
  (or the given absolute PATHs). Never deletes; never follows symlinks.";

fn number(value: Option<&String>, min: u64, max: u64) -> Result<u64> {
    value
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| (min..=max).contains(v))
        .ok_or_else(|| USAGE.into())
}

/// `tasks tmp-usage ...` entry point (arguments after the subcommand).
pub fn cli(args: &[String]) -> Result<()> {
    let mut json = false;
    let mut top = 25usize;
    let mut limits = Limits::default();
    let mut roots = vec![];
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--top" => top = number(it.next(), 1, 10_000)? as usize,
            "--max-files" => limits.max_entries = number(it.next(), 1, 50_000_000)?,
            "--max-seconds" => limits.max_time = Duration::from_secs(number(it.next(), 1, 3600)?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            path if path.starts_with('/') => roots.push(PathBuf::from(path)),
            _ => return Err(USAGE.into()),
        }
    }
    if roots.is_empty() {
        roots = default_roots();
    }
    let (jobs, jobs_source) = daemon_jobs();
    let ctx = Context {
        jobs,
        processes: own_processes(),
        expand: default_expand(),
        lock_dir: crate::admission::root().ok(),
    };
    let mut report = inventory(&roots, &ctx, limits);
    report.jobs_source = jobs_source;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render(&report, top));
    }
    Ok(())
}

#[cfg(test)]
#[path = "tmp_usage_tests.rs"]
mod tests;
