//! Retention: what the worker and the origin delete, and how it is audited.
//!
//! Every deletion reports a [`SyncEvent::Deleted`] with the bytes freed, so the audit log can
//! say what went away and how much disk it gave back.

use super::origin::{OriginSync, read_latest, write_latest};
use super::paths::dir_size;
use super::worker::SyncStore;
use super::{Deleted, SyncEvent};
use crate::ids::{CheckoutId, GitId, PeerId};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

const DAY: Duration = Duration::from_secs(24 * 3600);

/// How long the worker keeps what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRetention {
    /// A working copy not used for this long is deleted (machine policy `sync.idle_days`).
    pub idle: Duration,
    /// Mirrors are garbage collected this often.
    pub gc_every: Duration,
    /// `git gc --prune=` age: unreferenced objects younger than this stay.
    pub gc_prune_age: String,
}

impl Default for WorkerRetention {
    fn default() -> Self {
        WorkerRetention {
            idle: 7 * DAY,
            gc_every: 7 * DAY,
            gc_prune_age: "1.day.ago".to_string(),
        }
    }
}

impl WorkerRetention {
    /// The default policy with `days` of idleness.
    pub fn idle_days(days: u64) -> WorkerRetention {
        WorkerRetention {
            idle: DAY * u32::try_from(days).unwrap_or(u32::MAX),
            ..WorkerRetention::default()
        }
    }
}

/// How long the origin keeps snapshot objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginRetention {
    /// Objects younger than this are never deleted (a snapshot in flight).
    pub min_age: Duration,
    /// A checkout not synced for this long stops holding objects alive.
    pub idle: Duration,
}

impl Default for OriginRetention {
    fn default() -> Self {
        OriginRetention {
            min_age: Duration::from_secs(3600),
            idle: 7 * DAY,
        }
    }
}

fn names(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// A working copy this machine holds for a peer (R-839).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldCopy {
    /// The checkout.
    pub checkout: CheckoutId,
    /// Bytes of the copy (its output links are not followed).
    pub bytes: u64,
    /// When a job last used it, ms since the epoch; the directory's age for a copy never
    /// applied.
    pub last_used_ms: i64,
}

impl SyncStore {
    /// The working copies held for `peer`.
    pub fn held(&self, peer: &PeerId) -> Vec<HeldCopy> {
        names(&self.peer_dir(peer))
            .into_iter()
            .filter_map(|c| CheckoutId::parse(&c).ok())
            .map(|checkout| {
                let dir = self.checkout_dir(peer, &checkout);
                let last_used_ms = self.read_state(&dir).map_or_else(
                    || {
                        fs::metadata(&dir)
                            .and_then(|m| m.modified())
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
                    },
                    |st| st.last_used_ms,
                );
                HeldCopy {
                    bytes: dir_size(&dir),
                    checkout,
                    last_used_ms,
                }
            })
            .collect()
    }

    /// Deletes the working copy of `checkout` held for `peer`, as its owner asked (R-839), and
    /// reports it to the observer; returns the bytes freed. The caller checks that no job uses
    /// it. `None` when there was no copy or it could not be removed.
    pub fn drop_copy(&self, peer: &PeerId, checkout: &CheckoutId) -> Option<u64> {
        let e = self.remove_copy(peer, checkout)?;
        self.observer.event(&e);
        match e {
            SyncEvent::Deleted { bytes, .. } => Some(bytes),
            _ => Some(0),
        }
    }

    /// Deletes the working copy of `checkout` held for `peer` and its refs in the mirror. The
    /// mirror itself goes at the next sweep once no copy uses it.
    fn remove_copy(&self, peer: &PeerId, checkout: &CheckoutId) -> Option<SyncEvent> {
        let dir = self.checkout_dir(peer, checkout);
        if !dir.is_dir() {
            return None;
        }
        let st = self.read_state(&dir);
        let bytes = dir_size(&dir);
        fs::remove_dir_all(&dir).ok()?;
        if let Some(st) = &st
            && let Ok(repo) = CheckoutId::parse(&st.repo)
        {
            let mirror = self.mirror_dir(peer, &repo);
            if let Ok(refs) = self
                .transfer
                .list_refs(&mirror, &format!("refs/tasks/{checkout}/"))
            {
                let del: Vec<String> = refs.into_iter().map(|(n, _)| n).collect();
                let _ = self.transfer.update_refs(&mirror, &[], &del);
            }
        }
        Some(SyncEvent::Deleted {
            kind: Deleted::WorkingCopy,
            peer: Some(peer.clone()),
            target: checkout.to_string(),
            bytes,
        })
    }

    /// Deletes working copies idle past `policy.idle` (never one `busy` says has a live job),
    /// mirrors nothing refers to, and garbage collects mirrors that are due. Returns what it
    /// did; each item was also reported to the observer.
    pub fn sweep(
        &self,
        policy: &WorkerRetention,
        busy: &dyn Fn(&PeerId, &Path) -> bool,
    ) -> Vec<SyncEvent> {
        let now = (self.clock)();
        let idle_ms = i64::try_from(policy.idle.as_millis()).unwrap_or(i64::MAX);
        let mut events = Vec::new();
        for pname in names(&self.root) {
            let Ok(peer) = PeerId::new(&pname) else {
                continue;
            };
            for cname in names(&self.peer_dir(&peer)) {
                let Ok(checkout) = CheckoutId::parse(&cname) else {
                    continue; // `mirrors`, or something that is not ours
                };
                let dir = self.checkout_dir(&peer, &checkout);
                let st = self.read_state(&dir);
                if busy(&peer, &self.tree_root(&peer, &checkout)) {
                    continue;
                }
                // a copy that was never applied has no use time: its directory's age stands in
                let used = match &st {
                    Some(st) => st.last_used_ms,
                    None => fs::metadata(&dir)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| i64::try_from(d.as_millis()).unwrap_or(0))
                        .unwrap_or(now),
                };
                if now.saturating_sub(used) < idle_ms {
                    continue;
                }
                if let Some(e) = self.remove_copy(&peer, &checkout) {
                    events.push(e);
                }
            }
            // which repos do the remaining copies use?
            let mut live_repos: HashSet<String> = HashSet::new();
            for cname in names(&self.peer_dir(&peer)) {
                if let Ok(c) = CheckoutId::parse(&cname)
                    && let Some(st) = self.read_state(&self.checkout_dir(&peer, &c))
                {
                    live_repos.insert(st.repo);
                }
            }
            for mname in fs::read_dir(self.mirrors_dir(&peer))
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
            {
                let Some(repo) = mname.strip_suffix(".git") else {
                    continue;
                };
                let mirror = self.mirrors_dir(&peer).join(&mname);
                if !live_repos.contains(repo) {
                    let bytes = dir_size(&mirror);
                    if fs::remove_dir_all(&mirror).is_ok() {
                        events.push(SyncEvent::Deleted {
                            kind: Deleted::Mirror,
                            peer: Some(peer.clone()),
                            target: repo.to_string(),
                            bytes,
                        });
                    }
                    continue;
                }
                let stamp = mirror.join("tasks-gc");
                let last = fs::read_to_string(&stamp)
                    .ok()
                    .and_then(|t| t.trim().parse::<i64>().ok())
                    .unwrap_or(0);
                let every = i64::try_from(policy.gc_every.as_millis()).unwrap_or(i64::MAX);
                if now.saturating_sub(last) < every {
                    continue;
                }
                let before = dir_size(&mirror);
                if self.transfer.gc(&mirror, &policy.gc_prune_age).is_ok() {
                    let _ = fs::write(&stamp, now.to_string());
                    let freed = before.saturating_sub(dir_size(&mirror));
                    if freed > 0 {
                        events.push(SyncEvent::Deleted {
                            kind: Deleted::MirrorGc,
                            peer: Some(peer.clone()),
                            target: repo.to_string(),
                            bytes: freed,
                        });
                    }
                }
            }
        }
        for e in &events {
            self.observer.event(e);
        }
        events
    }
}

impl OriginSync {
    /// Prunes the snapshot stores: loose objects not reachable from the newest snapshot of any
    /// checkout (and older than `policy.min_age`) are deleted. A checkout not synced for
    /// `policy.idle` stops counting. Returns what it freed; each item was also reported.
    pub fn prune(&self, policy: &OriginRetention) -> Vec<SyncEvent> {
        let now = (self.clock)();
        let idle_ms = i64::try_from(policy.idle.as_millis()).unwrap_or(i64::MAX);
        let mut events = Vec::new();
        for rname in names(&self.state.join("sync")) {
            let Ok(repo) = CheckoutId::parse(&rname) else {
                continue;
            };
            let mut latest = read_latest(&self.state, &repo);
            let before = latest.len();
            let dir = super::transfer::repo_dir(&self.state, &repo);
            latest.retain(|checkout, l| {
                let keep = now.saturating_sub(l.at_ms) < idle_ms;
                if !keep {
                    // its stat cache goes with it
                    let _ = fs::remove_file(dir.join(format!("{checkout}.index")));
                }
                keep
            });
            if latest.len() != before {
                let _ = write_latest(&self.state, &repo, &latest);
            }
            let keep: Vec<GitId> = latest
                .values()
                .flat_map(|l| {
                    [
                        l.commit.as_deref(),
                        Some(l.tree.as_str()),
                        l.extras.as_deref(),
                    ]
                })
                .flatten()
                .filter_map(|t| GitId::parse(t).ok())
                .collect();
            if let Ok(stats) = self.snapshot.prune(&repo, &keep, policy.min_age)
                && stats.bytes > 0
            {
                events.push(SyncEvent::Deleted {
                    kind: Deleted::OriginObjects,
                    peer: None,
                    target: repo.to_string(),
                    bytes: stats.bytes,
                });
            }
        }
        for e in &events {
            self.observer.event(e);
        }
        events
    }
}
