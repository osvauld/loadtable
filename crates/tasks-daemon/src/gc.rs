//! `loadtable gc`: what each checkout keeps on the build caches of this machine and its peers,
//! and freeing it (R-839).
//!
//! A checkout is a Git working tree that ran jobs here (the ledger) or is a worktree of a
//! repository that did (`git worktree list`). Its build output is every directory under the
//! cache root it owns: `loadtable/checkouts/<key>` (and the older `*/tasks/checkouts/<key>`,
//! the same key), plus wherever its `target` and Android links point. Ownership comes from those
//! links, never from a folder's name, so an older layout a live worktree still builds in is kept
//! with that worktree.
//!
//! Only build output and peers' synced copies are deleted, never a working tree: the worst a
//! deletion costs is a cold build. A checkout with a job queued or running is never touched.

use crate::api::{ApiError, ErrorCode, GcHeld, GcMachine, GcRow, GcShared, GcState, GcView};
use crate::build_cache::outputs::LINK_NAMES;
use crate::scheduler::Scheduler;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use tasks_core::MachineName;
use tasks_ledger::{JobRecord, ListFilter};
use tasks_peer::CheckoutId;
use tasks_peer::proto::HeldCheckout;
use tasks_peer::sync::dir_size;

/// A checkout untouched this long is idle, unless the request says otherwise.
pub const DEFAULT_IDLE_DAYS: u32 = 14;
/// The branches a finished branch is merged into, first found wins.
const BASES: &[&str] = &["dev", "main", "master"];
const DAY_MS: i64 = 86_400_000;

/// What the ledger says about one checkout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Seen {
    /// The newest submit or finish of its jobs, ms.
    pub last_ms: Option<i64>,
    /// A job of it is queued, waiting or running.
    pub busy: bool,
}

/// The checkouts of `records` by root, leaving out the jobs peers sent here (their copies are
/// the peer store's).
pub(crate) fn from_ledger(records: &[JobRecord], peer_store: &Path) -> BTreeMap<PathBuf, Seen> {
    let store = std::fs::canonicalize(peer_store).unwrap_or_else(|_| peer_store.to_path_buf());
    let mut out: BTreeMap<PathBuf, Seen> = BTreeMap::new();
    for r in records {
        if r.origin.is_some()
            || r.spec.cwd.starts_with(&store)
            || r.spec.cwd.starts_with(peer_store)
        {
            continue;
        }
        let root = PathBuf::from(crate::place::checkout_key(&r.spec.cwd));
        let seen = out.entry(root).or_default();
        let t = r.finished_at_ms.unwrap_or(r.submitted_at_ms);
        seen.last_ms = seen.last_ms.max(Some(t));
        seen.busy |= !r.phase.is_terminal();
    }
    out
}

/// Runs `git -C dir args`, its trimmed output when it succeeds.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The worktrees of the repository holding `dir`, `dir` included.
pub(crate) fn worktrees(dir: &Path) -> Vec<PathBuf> {
    git(dir, &["worktree", "list", "--porcelain"])
        .map(|text| {
            text.lines()
                .filter_map(|l| l.strip_prefix("worktree "))
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// A working tree's branch, and the base it is fully merged into: its commit is in the base,
/// it is not the base itself and no tracked file is changed.
pub(crate) fn branch_of(root: &Path) -> (Option<String>, Option<String>) {
    let branch = git(root, &["symbolic-ref", "--short", "-q", "HEAD"]).filter(|b| !b.is_empty());
    let Some(head) = git(root, &["rev-parse", "-q", "--verify", "HEAD"]) else {
        return (branch, None);
    };
    let clean =
        git(root, &["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| s.is_empty());
    let merged = BASES
        .iter()
        .find(|b| {
            git(
                root,
                &["rev-parse", "-q", "--verify", &format!("refs/heads/{b}")],
            )
            .is_some()
        })
        .filter(|b| branch.as_deref() != Some(**b))
        .filter(|b| {
            clean
                && Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(["merge-base", "--is-ancestor", &head, b])
                    .output()
                    .is_ok_and(|o| o.status.success())
        })
        .map(|b| (*b).to_string());
    (branch, merged)
}

/// 64 lowercase hex characters: the name of a checkout's output directory.
fn is_key(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// The key of a local checkout's output directory, as the placement computes it.
fn key_of(root: &Path) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    Sha256::digest(root.as_os_str().as_encoded_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The directories under the cache root that hold checkouts' outputs, canonical: the current
/// `loadtable/checkouts` and every older `<name>/tasks/checkouts`.
pub(crate) fn output_parents(cache_root: &Path) -> Vec<PathBuf> {
    let mut out = BTreeSet::new();
    if let Ok(c) = std::fs::canonicalize(cache_root.join(crate::build_cache::CHECKOUTS_SUBDIR)) {
        out.insert(c);
    }
    if let Ok(rd) = std::fs::read_dir(cache_root) {
        for e in rd.flatten() {
            if let Ok(c) = std::fs::canonicalize(e.path().join("tasks/checkouts"))
                && c.is_dir()
            {
                out.insert(c);
            }
        }
    }
    out.into_iter().collect()
}

/// The output directories `root` owns under `cache` (canonical): its key under each of
/// `parents`, and the directory each of its links points into.
pub(crate) fn owned_dirs(root: &Path, cache: &Path, parents: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = BTreeSet::new();
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for key in [key_of(&canon_root), key_of(root)] {
        for p in parents {
            let d = p.join(&key);
            if d.is_dir() {
                out.insert(d);
            }
        }
    }
    for rel in LINK_NAMES {
        let link = root.join(rel);
        if !std::fs::symlink_metadata(&link).is_ok_and(|m| m.is_symlink()) {
            continue;
        }
        let Ok(target) = std::fs::canonicalize(&link) else {
            continue;
        };
        // `<key>/target` owns `<key>`; a link elsewhere in the cache owns what it points to
        let owner = strip_rel(&target, rel).unwrap_or(target);
        if owner.starts_with(cache) && owner != cache {
            out.insert(owner);
        }
    }
    out.into_iter().collect()
}

/// The output key directory `path` is `<key>/<rel>` of, if it is.
fn strip_rel(path: &Path, rel: &str) -> Option<PathBuf> {
    let mut p = path;
    for _ in Path::new(rel).components() {
        p = p.parent()?;
    }
    (p.join(rel) == path && p.file_name().is_some_and(|n| is_key(&n.to_string_lossy())))
        .then(|| p.to_path_buf())
}

/// The state of a checkout, most important first: a busy one is in use whatever else holds.
pub(crate) fn state_of(
    busy: bool,
    exists: bool,
    merged: bool,
    last_ms: Option<i64>,
    now_ms: i64,
    idle_days: u32,
) -> GcState {
    if busy {
        GcState::InUse
    } else if !exists {
        GcState::Gone
    } else if merged {
        GcState::Merged
    } else if last_ms.is_some_and(|t| now_ms.saturating_sub(t) > i64::from(idle_days) * DAY_MS) {
        GcState::Idle
    } else {
        GcState::Active
    }
}

/// One row while it is assembled.
struct Draft {
    row: GcRow,
    dirs: Vec<PathBuf>,
    /// The id peers keep it under.
    id: Option<CheckoutId>,
}

/// Sizes of `dirs`, measured in parallel.
fn sizes(dirs: &[PathBuf]) -> BTreeMap<PathBuf, u64> {
    std::thread::scope(|s| {
        let handles: Vec<_> = dirs
            .iter()
            .map(|d| (d.clone(), s.spawn(move || dir_size(d))))
            .collect();
        handles
            .into_iter()
            .map(|(d, h)| (d, h.join().unwrap_or(0)))
            .collect()
    })
}

/// The name a row shows for a checkout with no working tree found.
fn unknown(dir: &Path) -> String {
    let short: String = dir
        .file_name()
        .map(|n| n.to_string_lossy().chars().take(12).collect())
        .unwrap_or_default();
    format!("(unknown checkout {short})")
}

/// Home-relative text of `p`, as the CLI shows paths.
fn shown(p: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| p.strip_prefix(h).ok()) {
        Some(rel) => format!("~/{}", rel.display()),
        None => p.display().to_string(),
    }
}

impl Scheduler {
    /// The `gc` report, deleting first the rows whose state is in `delete` and the rows named
    /// in `checkouts` (R-839, R-840).
    pub(crate) fn gc(
        &self,
        delete: &[GcState],
        checkouts: &[String],
        idle_days: Option<u32>,
    ) -> Result<GcView, ApiError> {
        let idle_days = idle_days.unwrap_or(DEFAULT_IDLE_DAYS);
        let now = self.clock.now_ms();
        let cache = self.build_cache();
        let cache_root =
            std::fs::canonicalize(cache.root()).unwrap_or_else(|_| cache.root().to_path_buf());
        let records = self.ledger.list(&ListFilter::default()).map_err(|e| {
            ApiError::new(ErrorCode::Internal, format!("cannot read the ledger: {e}"))
        })?;
        let mut seen = from_ledger(&records, cache.peer_store());
        let repos: Vec<PathBuf> = seen.keys().filter(|r| r.is_dir()).cloned().collect();
        for repo in repos {
            for wt in worktrees(&repo) {
                seen.entry(wt).or_default();
            }
        }
        let home = self
            .parent_env
            .iter()
            .find(|(k, _)| k == "HOME")
            .map(|(_, v)| PathBuf::from(v));
        let parents = output_parents(&cache_root);
        let local = self.cfg.machine.clone();

        let mut drafts: Vec<Draft> = Vec::new();
        let mut claimed: BTreeSet<PathBuf> = BTreeSet::new();
        for (root, s) in &seen {
            let exists = root.is_dir();
            let (branch, merged_into) = if exists {
                branch_of(root)
            } else {
                (None, None)
            };
            let dirs: Vec<PathBuf> = owned_dirs(root, &cache_root, &parents)
                .into_iter()
                .filter(|d| claimed.insert(d.clone()))
                .collect();
            let last_ms = s.last_ms.or_else(|| newest_mtime(&dirs));
            let state = state_of(
                s.busy,
                exists,
                merged_into.is_some(),
                last_ms,
                now,
                idle_days,
            );
            drafts.push(Draft {
                row: GcRow {
                    checkout: shown(root, home.as_deref()),
                    branch,
                    state,
                    merged_into,
                    last_used_ms: last_ms,
                    held: Vec::new(),
                    freed_bytes: 0,
                    kept: None,
                },
                dirs,
                id: self.forwarder.checkout_id(root),
            });
        }
        // output directories no checkout claims: a deleted worktree's, or an older layout's
        for parent in &parents {
            let old = !parent.ends_with(crate::build_cache::CHECKOUTS_SUBDIR);
            let Ok(rd) = std::fs::read_dir(parent) else {
                continue;
            };
            for e in rd.flatten() {
                let d = e.path();
                if !is_key(&e.file_name().to_string_lossy())
                    || !d.is_dir()
                    || !claimed.insert(d.clone())
                {
                    continue;
                }
                let last_ms = newest_mtime(std::slice::from_ref(&d));
                drafts.push(Draft {
                    row: GcRow {
                        checkout: unknown(&d),
                        branch: None,
                        state: if old { GcState::Old } else { GcState::Gone },
                        merged_into: None,
                        last_used_ms: last_ms,
                        held: Vec::new(),
                        freed_bytes: 0,
                        kept: None,
                    },
                    dirs: vec![d],
                    id: None,
                });
            }
        }

        let all_dirs: Vec<PathBuf> = drafts.iter().flat_map(|d| d.dirs.clone()).collect();
        let (local_sizes, peer_reports) = std::thread::scope(|s| {
            let sized = s.spawn(|| sizes(&all_dirs));
            let asked: Vec<_> = self
                .forwarder
                .peers()
                .into_iter()
                .map(|p| {
                    let name = p.machine.name;
                    let fw = &self.forwarder;
                    s.spawn(move || {
                        let r = fw.peer_cache(&name);
                        (name, r)
                    })
                })
                .collect();
            let reports: Vec<(MachineName, Result<Vec<HeldCheckout>, String>)> = asked
                .into_iter()
                .filter_map(|h| h.join().ok())
                .map(|(n, r)| (n, r.map_err(|e| e.to_string())))
                .collect();
            (sized.join().unwrap_or_default(), reports)
        });

        for d in &mut drafts {
            let bytes: u64 = d.dirs.iter().filter_map(|x| local_sizes.get(x)).sum();
            if bytes > 0 || !d.dirs.is_empty() {
                d.row.held.push(GcHeld {
                    machine: local.clone(),
                    bytes,
                });
            }
        }
        let mut machines = vec![GcMachine {
            name: local.clone(),
            held_bytes: local_sizes.values().sum(),
            reclaimable_bytes: 0,
            error: None,
        }];
        for (peer, report) in peer_reports {
            let mut m = GcMachine {
                name: peer.0.clone(),
                held_bytes: 0,
                reclaimable_bytes: 0,
                error: None,
            };
            match report {
                Err(e) => m.error = Some(e),
                Ok(held) => {
                    for h in held {
                        let bytes = h.copy_bytes.saturating_add(h.output_bytes);
                        m.held_bytes = m.held_bytes.saturating_add(bytes);
                        let at = drafts
                            .iter()
                            .position(|d| d.id.as_ref() == Some(&h.checkout));
                        let i = match at {
                            Some(i) => i,
                            None => {
                                drafts.push(Draft {
                                    row: GcRow {
                                        checkout: format!(
                                            "(unknown checkout {})",
                                            h.checkout
                                                .as_str()
                                                .chars()
                                                .take(12)
                                                .collect::<String>()
                                        ),
                                        branch: None,
                                        state: GcState::Gone,
                                        merged_into: None,
                                        last_used_ms: h.last_used_ms,
                                        held: Vec::new(),
                                        freed_bytes: 0,
                                        kept: None,
                                    },
                                    dirs: Vec::new(),
                                    id: Some(h.checkout.clone()),
                                });
                                drafts.len() - 1
                            }
                        };
                        if let Some(d) = drafts.get_mut(i) {
                            if h.busy {
                                d.row.state = GcState::InUse;
                            }
                            d.row.last_used_ms = d.row.last_used_ms.max(h.last_used_ms);
                            d.row.held.push(GcHeld {
                                machine: peer.0.clone(),
                                bytes,
                            });
                        }
                    }
                }
            }
            machines.push(m);
        }
        // rows holding nothing anywhere are not worth a line
        drafts.retain(|d| d.row.held.iter().any(|h| h.bytes > 0) || d.row.state == GcState::InUse);

        let deleted = !delete.is_empty() || !checkouts.is_empty();
        let mut notes = Vec::new();
        // a full path names the same row as its `~/…` form
        let checkouts: Vec<String> = checkouts
            .iter()
            .map(|c| match c.starts_with('/') {
                true => shown(Path::new(c.trim_end_matches('/')), home.as_deref()),
                false => c.clone(),
            })
            .collect();
        let checkouts = checkouts.as_slice();
        if deleted {
            let freed = self.gc_delete(&mut drafts, delete, checkouts, &cache_root);
            // the totals are what each machine keeps now, not what it kept before
            for m in &mut machines {
                m.held_bytes = m
                    .held_bytes
                    .saturating_sub(freed.get(&m.name).copied().unwrap_or(0));
            }
            for c in checkouts {
                match drafts.iter_mut().find(|d| d.row.checkout == *c) {
                    Some(d) if d.row.state == GcState::InUse => {
                        d.row.kept = Some("a job uses it; free it once the job ends".into());
                    }
                    Some(_) => {}
                    None => notes.push(format!(
                        "{c} holds no build output here; run loadtable gc to see what does"
                    )),
                }
            }
        }
        for m in &mut machines {
            m.reclaimable_bytes = drafts
                .iter()
                .filter(|d| d.row.state.reclaimable() && d.row.freed_bytes == 0)
                .flat_map(|d| &d.row.held)
                .filter(|h| h.machine == m.name)
                .map(|h| h.bytes)
                .sum();
        }
        let mut rows: Vec<GcRow> = drafts.into_iter().map(|d| d.row).collect();
        rows.sort_by(|a, b| {
            a.state
                .order()
                .cmp(&b.state.order())
                .then_with(|| total(b).cmp(&total(a)))
        });
        Ok(GcView {
            idle_days,
            machines,
            rows,
            shared: self.gc_shared(),
            notes,
            deleted,
        })
    }

    /// Deletes the output and peer copies of every row in `delete` or named in `checkouts`
    /// that is not in use, and returns the bytes freed on each machine.
    fn gc_delete(
        &self,
        drafts: &mut [Draft],
        delete: &[GcState],
        checkouts: &[String],
        cache_root: &Path,
    ) -> BTreeMap<String, u64> {
        let mut freed: BTreeMap<String, u64> = BTreeMap::new();
        let mut per_peer: BTreeMap<String, Vec<CheckoutId>> = BTreeMap::new();
        for d in drafts.iter_mut() {
            let chosen = delete.contains(&d.row.state) || checkouts.contains(&d.row.checkout);
            if d.row.state == GcState::InUse || !chosen {
                continue;
            }
            let local = d
                .row
                .held
                .iter()
                .find(|h| h.machine == self.cfg.machine)
                .map_or(0, |h| h.bytes);
            let mut failed = Vec::new();
            for dir in &d.dirs {
                // a last look: only a directory inside the cache, never the cache itself
                if !dir.starts_with(cache_root) || dir == cache_root {
                    continue;
                }
                if let Err(e) = std::fs::remove_dir_all(dir) {
                    failed.push(format!("{}: {e}", dir.display()));
                }
            }
            if failed.is_empty() {
                d.row.freed_bytes = d.row.freed_bytes.saturating_add(local);
                *freed.entry(self.cfg.machine.clone()).or_default() += local;
            } else {
                d.row.kept = Some(format!("not all deleted: {}", failed.join("; ")));
            }
            if let Some(id) = &d.id {
                for h in d.row.held.iter().filter(|h| h.machine != self.cfg.machine) {
                    per_peer
                        .entry(h.machine.clone())
                        .or_default()
                        .push(id.clone());
                }
            }
        }
        for (peer, ids) in per_peer {
            match self
                .forwarder
                .peer_cache_drop(&MachineName(peer.clone()), ids.clone())
            {
                Ok(dropped) => {
                    for x in dropped {
                        *freed.entry(peer.clone()).or_default() += x.freed_bytes;
                        if let Some(d) = drafts
                            .iter_mut()
                            .find(|d| d.id.as_ref() == Some(&x.checkout))
                        {
                            d.row.freed_bytes = d.row.freed_bytes.saturating_add(x.freed_bytes);
                            if let Some(k) = x.kept {
                                d.row.kept = Some(format!("{peer}: {k}"));
                            }
                        }
                    }
                }
                Err(e) => {
                    for d in drafts
                        .iter_mut()
                        .filter(|d| d.id.as_ref().is_some_and(|i| ids.contains(i)))
                    {
                        d.row.kept = Some(format!("{peer}: {e}"));
                    }
                }
            }
        }
        freed
    }

    /// Caches every checkout shares, reported and never deleted by `gc`.
    fn gc_shared(&self) -> Vec<GcShared> {
        let cache = self.build_cache();
        let policy = self.policy();
        let mut out = Vec::new();
        let sccache = cache.sccache_dir();
        if sccache.is_dir() {
            out.push(GcShared {
                name: "compiler cache (sccache)".into(),
                path: sccache.display().to_string(),
                bytes: dir_size(&sccache),
                note: format!("sccache keeps itself under {}", policy.sccache_size()),
            });
        }
        if let Some(g) = cache.gradle_home() {
            out.push(GcShared {
                name: "Gradle home".into(),
                path: g.display().to_string(),
                bytes: dir_size(&g),
                note: "shared by every Gradle build; not cleaned by gc".into(),
            });
        }
        let store = cache.peer_store();
        if store.is_dir() {
            out.push(GcShared {
                name: "copies held for peers".into(),
                path: store.display().to_string(),
                bytes: dir_size(store),
                note: format!(
                    "removed after {} idle days, with their build output",
                    policy.sync_retention().idle.as_secs() / 86_400
                ),
            });
        }
        out
    }
}

/// All bytes a row holds.
fn total(r: &GcRow) -> u64 {
    r.held.iter().map(|h| h.bytes).sum()
}

/// The newest modification time among `dirs`, ms.
fn newest_mtime(dirs: &[PathBuf]) -> Option<i64> {
    dirs.iter()
        .filter_map(|d| std::fs::metadata(d).and_then(|m| m.modified()).ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .filter_map(|d| i64::try_from(d.as_millis()).ok())
        .max()
}

#[cfg(test)]
#[path = "gc_tests.rs"]
mod tests;
