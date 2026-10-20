//! The health checks. Each is a pure function of what the monitor read (snapshots, disk
//! measurements, /proc text, peer views) and returns the conditions it sees right now as
//! [`Observation`]s; the [`AlertBook`](super::alert::AlertBook) turns them into deduplicated,
//! auto-clearing alerts. Duration conditions ("for more than N seconds") use [`Since`].
use super::alert::{Observation, Severity};
use super::capability::CapabilityMark;
use super::config::HealthConfig;
use super::disk::{Consumer, DiskStatus};
use super::procs::{LockEntry, ProcMemory, gib};
use crate::Phase;
use crate::metrics::{HolderProcess, JobView, Machine, Slot, SlotKind, Snapshot};
use crate::peer_link::PeerSnapshot;
use crate::placement::{self, duration};
use std::collections::HashMap;

/// When each tracked condition started (it must hold continuously).
#[derive(Clone, Debug, Default)]
pub struct Since(HashMap<String, f64>);

impl Since {
    /// Records whether `key` holds now; returns for how long it has held.
    pub fn held(&mut self, key: &str, holds: bool, now: f64) -> Option<f64> {
        if !holds {
            self.0.remove(key);
            return None;
        }
        let start = *self.0.entry(key.to_string()).or_insert(now);
        Some(now - start)
    }
    /// Forgets keys of this prefix that were not seen in the last pass.
    pub fn retain_prefix(&mut self, prefix: &str, seen: &[String]) {
        self.0
            .retain(|k, _| !k.starts_with(prefix) || seen.iter().any(|s| s == k));
    }
}

const DISK_FIX: &str = "free space on that filesystem: the top consumers are listed (`tasks \
    tmp-usage` shows more, read-only). Ask a checkout's owner before removing its build \
    output; never delete admission lock files or another agent's data. Offloaded/forwarded \
    jobs that do not fetch artifacts no longer wait on this disk";

fn consumer_line(c: &Consumer) -> String {
    let mut s = format!("{} {}", gib(c.bytes), c.path.display());
    if let Some(checkout) = &c.checkout {
        s.push_str(&format!(" (checkout {})", checkout.display()));
    }
    if !c.jobs.is_empty() {
        let ids: Vec<String> = c.jobs.iter().map(|j| format!("#{j}")).collect();
        s.push_str(&format!(" jobs {}", ids.join(" ")));
    }
    if c.partial {
        s.push_str(" (partial)");
    }
    s
}

/// Disk below 2x the reserve (warn), below the reserve (critical), filling within the
/// time-to-full threshold (warn; critical within an hour). `waiting` names jobs that wait for
/// disk space.
pub fn disks(
    node: &str,
    disks: &[DiskStatus],
    waiting: &[String],
    cfg: &HealthConfig,
) -> Vec<Observation> {
    let mut out = vec![];
    for d in disks {
        let base = |kind: &str, severity| {
            let mut o = Observation::new(kind, severity, node)
                .subject(d.filesystem.clone())
                .evidence(d.line())
                .fix(DISK_FIX)
                .human();
            for c in d.top.iter().take(3) {
                o = o.evidence(format!("top: {}", consumer_line(c)));
            }
            o
        };
        if d.free_bytes < d.reserve_bytes {
            let mut o = base("disk_reserve", Severity::Critical).summary(format!(
                "{} below its reserve: {} free, reserve {} (jobs needing new output there wait)",
                d.filesystem,
                gib(d.free_bytes),
                gib(d.reserve_bytes)
            ));
            for w in waiting.iter().take(4) {
                o = o.evidence(format!("waiting: {w}"));
            }
            out.push(o);
        } else if d.free_bytes < d.reserve_bytes.saturating_mul(2) {
            out.push(base("disk_low", Severity::Warn).summary(format!(
                "{} low: {} free, under 2x its {} reserve",
                d.filesystem,
                gib(d.free_bytes),
                gib(d.reserve_bytes)
            )));
        }
        if let Some(ttf) = d.time_to_full_s.filter(|t| *t < cfg.time_to_full_warn_s) {
            let severity = if ttf < 3600.0 {
                Severity::Critical
            } else {
                Severity::Warn
            };
            out.push(base("disk_filling", severity).summary(format!(
                "{} filling: full in ~{} at {:.1} GiB/h",
                d.filesystem,
                duration(ttf),
                d.growth_bytes_per_h.unwrap_or(0.0) / (1024.0 * 1024.0 * 1024.0)
            )));
        }
    }
    out
}

/// A checkout's build output (cache target / offload checkout) above the size threshold.
pub fn targets(node: &str, targets: &[Consumer], cfg: &HealthConfig) -> Vec<Observation> {
    targets
        .iter()
        .filter(|t| t.bytes > cfg.target_max_bytes)
        .map(|t| {
            let who = t
                .checkout
                .as_ref()
                .map(|c| c.display().to_string())
                .unwrap_or_else(|| "an unknown checkout".into());
            Observation::new("target_large", Severity::Warn, node)
                .subject(t.path.display().to_string())
                .summary(format!(
                    "build output of {who} is {} (threshold {})",
                    gib(t.bytes),
                    gib(cfg.target_max_bytes)
                ))
                .evidence(consumer_line(t))
                .fix(
                    "ask the checkout's owner to `cargo clean` it (or remove old profiles) when \
                      no job of theirs runs; tasks never deletes build output itself",
                )
        })
        .collect()
}

/// What the slot check reads besides the snapshot.
pub struct SlotInputs<'a> {
    /// /proc/locks holders.
    pub locks: &'a [LockEntry],
    /// (slot name, (major, minor, inode)) of each lock file.
    pub files: &'a [(String, (u32, u32, u64))],
    /// Command line of a pid (None: gone).
    pub cmdline: &'a dyn Fn(u32) -> Option<String>,
    /// The processes holding the slot's flock, by slot name: fix/lease-leak's
    /// `admission::holders` scan (a `FLOCK` line in `/proc/PID/fdinfo`, so waiters that merely
    /// have the file open are not named), the same scan `tasks admission`, `tasks status` and
    /// the snapshot's `slots[].holder_processes` use. Asked only when the snapshot did not
    /// already carry holders for the slot.
    pub holders: &'a dyn Fn(&str) -> Vec<HolderProcess>,
}

/// One holder process as evidence, worded like the snapshot's `waiting_for` (fix/lease-leak):
/// pid, command line, the job it was started under and whether that job has ended.
fn holder_line(h: &HolderProcess, jobs: &[JobView]) -> String {
    let text = crate::admission::holders::describe_text(h);
    match h.tasks_job {
        Some(j) if jobs.iter().any(|x| x.id == j && !x.phase.terminal()) => text,
        Some(_) => format!("{text}, which has ended (the process outlived its job)"),
        None => text,
    }
}

/// Compiler-cache servers (sccache) daemonize from inside the first job that needs them.
fn is_compiler_cache(cmd: &str) -> bool {
    cmd.split_whitespace()
        .next()
        .and_then(|w| std::path::Path::new(w).file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "sccache")
}

fn executing(job: &JobView, kind: SlotKind) -> bool {
    match kind {
        SlotKind::Compile => job.phase == Phase::Building && job.resource == SlotKind::Compile,
        SlotKind::Run => {
            job.phase == Phase::Running && matches!(job.resource, SlotKind::Run | SlotKind::Compile)
        }
        SlotKind::Offload => job.phase == Phase::Running && job.resource == SlotKind::Offload,
        SlotKind::Device => job.phase == Phase::Running && job.resource == SlotKind::Device,
        SlotKind::Service => job.phase == Phase::Running && job.resource == SlotKind::Service,
    }
}

/// Busy slots that no live ledger job explains: a holder note naming a live job, or else an
/// executing job of that slot kind not already matched to another slot.
pub fn unexplained_slots<'a>(slots: &'a [Slot], jobs: &[JobView]) -> Vec<&'a Slot> {
    let mut out = vec![];
    for kind in [
        SlotKind::Compile,
        SlotKind::Run,
        SlotKind::Offload,
        SlotKind::Device,
        SlotKind::Service,
    ] {
        let mut free: Vec<i64> = jobs
            .iter()
            .filter(|j| executing(j, kind))
            .map(|j| j.id)
            .collect();
        let busy: Vec<&Slot> = slots.iter().filter(|s| s.kind == kind && s.busy).collect();
        let mut unnamed = vec![];
        for s in busy {
            match s
                .holder_job
                .and_then(|id| free.iter().position(|j| *j == id))
            {
                Some(i) => {
                    free.remove(i);
                }
                None => unnamed.push(s),
            }
        }
        for s in unnamed {
            if free.is_empty() {
                out.push(s);
            } else {
                free.remove(0);
            }
        }
    }
    out
}

/// A slot whose flock is held while no live job holds it, for more than `slot_orphan_s`.
pub fn slots(
    node: &str,
    snapshot: &Snapshot,
    inputs: &SlotInputs,
    since: &mut Since,
    now: f64,
    cfg: &HealthConfig,
) -> Vec<Observation> {
    let unexplained = unexplained_slots(&snapshot.machine.slots, &snapshot.jobs);
    let mut seen = vec![];
    let mut out = vec![];
    for slot in unexplained {
        let key = format!("slot:{}", slot.name);
        seen.push(key.clone());
        let Some(held) = since.held(&key, true, now) else {
            continue;
        };
        if held < cfg.slot_orphan_s {
            continue;
        }
        // Stable id `slot-holder:NODE:LOCK` (shared with fix/lease-leak).
        let mut o = Observation::new("slot-holder", Severity::Warn, node)
            .subject(format!("{}.lock", slot.name))
            .summary(format!(
                "{} slot {} held for {} with no live job holding it; jobs needing it queue",
                kind_name(slot.kind),
                slot.name,
                duration(held)
            ))
            .fix(
                "find the holder below; if it is a stuck tasks daemon/worker or a leftover \
                  client with no job, its owner (or the operator, once nothing else runs) \
                  stops that process. Never delete or recreate the lock file (stable flock \
                  inode)",
            )
            .human();
        if let Some(who) = &slot.holder_who {
            o = o.evidence(format!("holder note: {who}"));
        }
        let file = inputs.files.iter().find(|(n, _)| *n == slot.name);
        let mut named = false;
        let mut compiler_cache = false;
        if let Some((_, id)) = file {
            for l in inputs
                .locks
                .iter()
                .filter(|l| (l.major, l.minor, l.inode) == *id)
            {
                named = true;
                match (inputs.cmdline)(l.pid) {
                    Some(cmd) => {
                        compiler_cache |= is_compiler_cache(&cmd);
                        o = o.evidence(format!(
                            "/proc/locks: {} held by pid {} ({cmd})",
                            l.class, l.pid
                        ))
                    }
                    None => {
                        o = o.evidence(format!(
                            "/proc/locks: {} taken by pid {} (exited; a child inherited the descriptor)",
                            l.class, l.pid
                        ))
                    }
                }
            }
        }
        let holders = if slot.holder_processes.is_empty() {
            (inputs.holders)(&slot.name)
        } else {
            slot.holder_processes.clone()
        };
        for h in &holders {
            compiler_cache |= is_compiler_cache(&h.cmd);
            o = o.evidence(format!("flock held by {}", holder_line(h, &snapshot.jobs)));
        }
        if compiler_cache {
            o = o.fix(
                "a compiler-cache server (sccache) started inside a job inherited the slot \
                 lease and keeps it until its idle exit; run it as the declared user service \
                 outside jobs (node-setup-policy; nix/module.nix), or stop that sccache \
                 server (`sccache --stop-server`) once no build uses it. Never delete the lock \
                 file (stable flock inode)",
            );
        }
        if !named {
            o = o.evidence(
                "no /proc/locks entry matched the lock file (lock dir/namespace differs?)",
            );
        }
        out.push(o);
    }
    since.retain_prefix("slot:", &seen);
    out
}

fn kind_name(kind: SlotKind) -> &'static str {
    match kind {
        SlotKind::Compile => "compile",
        SlotKind::Run => "run",
        SlotKind::Offload => "offload",
        SlotKind::Device => "device",
        SlotKind::Service => "service",
    }
}

/// Memory above the admission gate (available below the reserve, memory PSI some above 1% or
/// full above 0.5%) for more than `pressure_s`, naming the largest RSS+swap processes.
pub fn memory(
    node: &str,
    machine: &Machine,
    min_available: u64,
    top: &dyn Fn() -> Vec<ProcMemory>,
    since: &mut Since,
    now: f64,
    cfg: &HealthConfig,
) -> Vec<Observation> {
    let report = crate::admission::ThresholdReport::new(
        "health",
        crate::admission::Thresholds::default(),
        None,
    );
    let mut why = vec![];
    if let Some(avail) = machine.mem_available_bytes
        && avail < min_available
    {
        why.push(format!(
            "available memory {} below {}",
            gib(avail),
            gib(min_available)
        ));
    }
    if let Some(p) = &machine.pressure {
        if p.memory_some_avg10 > report.memory_some_avg10_max {
            why.push(format!("memory PSI some {:.1}%", p.memory_some_avg10));
        }
        if p.memory_full_avg10 > report.memory_full_avg10_max {
            why.push(format!("memory PSI full {:.1}%", p.memory_full_avg10));
        }
    }
    let Some(held) = since.held("memory", !why.is_empty(), now) else {
        return vec![];
    };
    if held < cfg.pressure_s {
        return vec![];
    }
    let mut o = Observation::new("memory_pressure", Severity::Warn, node)
        .summary(format!(
            "memory above the admission gate for {}: {}",
            duration(held),
            why.join(", ")
        ))
        .fix(
            "look at the largest processes below; a leaking long-running process (e.g. a \
              `tasks ui` window) can be restarted by its owner. Admission queues work until \
              pressure falls; never lower the gates",
        )
        .human();
    if let Some(swap) = machine.swap_used_bytes {
        o = o.evidence(format!("swap used {}", gib(swap)));
    }
    for p in top().iter().take(5) {
        o = o.evidence(p.line());
    }
    vec![o]
}

/// A peer whose status stream is offline, or whose snapshot is stale, for more than `peer_s`.
pub fn peers(
    peers: &[PeerSnapshot],
    since: &mut Since,
    now: f64,
    cfg: &HealthConfig,
) -> Vec<Observation> {
    let mut out = vec![];
    let mut seen = vec![];
    for p in peers {
        let offline = !p.online();
        let kind = if offline {
            "peer_offline"
        } else {
            "peer_stale"
        };
        let bad = offline || p.stale;
        let key = format!("peer:{}:{kind}", p.name);
        if bad {
            seen.push(key.clone());
        }
        let Some(held) = since.held(&key, bad, now) else {
            continue;
        };
        if held < cfg.peer_s {
            continue;
        }
        let age = p
            .age_s
            .map(|a| format!("last snapshot {} ago", duration(a)))
            .unwrap_or_else(|| "no snapshot received".into());
        let summary = if offline {
            format!("peer {} {} for {} ({age})", p.name, p.state, duration(held))
        } else {
            format!(
                "peer {} snapshot stale ({age}); its card and placement data are old",
                p.name
            )
        };
        let mut o = Observation::new(kind, Severity::Warn, &p.name)
            .summary(summary)
            .fix(
                "check that the peer's tasks daemon runs and is reachable (`tasks peer list`, \
                  `tasks peer status-watch NAME`); placement skips it meanwhile",
            )
            .human();
        if let Some(e) = &p.error {
            o = o.evidence(format!("last error: {e}"));
        }
        out.push(o);
    }
    since.retain_prefix("peer:", &seen);
    out
}

fn waiting_phase(phase: &Phase) -> bool {
    matches!(
        phase,
        Phase::Approved | Phase::Preparing | Phase::WaitingBuild | Phase::WaitingRun
    )
}

fn job_reason(j: &JobView) -> String {
    j.queue
        .as_ref()
        .and_then(|q| q.waiting_for.clone())
        .unwrap_or_else(|| "admission".into())
}

/// Jobs waiting longer than `job_wait_s`, with what they wait for.
pub fn waiting(node: &str, jobs: &[JobView], now: f64, cfg: &HealthConfig) -> Vec<Observation> {
    jobs.iter()
        .filter(|j| waiting_phase(&j.phase))
        .filter_map(|j| {
            let waited = now - j.submitted_at as f64;
            (waited > cfg.job_wait_s).then(|| {
                Observation::new("job_waiting", Severity::Warn, node)
                    .subject(j.id.to_string())
                    .summary(format!(
                        "job #{} ({}) waiting {}: {}",
                        j.id,
                        j.owner,
                        duration(waited),
                        job_reason(j)
                    ))
                    .evidence(format!("command: {}", j.command))
                    .fix(
                        "see the reason: a busy slot clears when its holder finishes; a \
                          pressure or disk blocker needs headroom (see the other alerts). \
                          Never bypass admission; do not resubmit a live job",
                    )
            })
        })
        .collect()
}

/// Compile jobs queued here (past `idle_peer_s`) while placement would run them sooner on an
/// idle peer. Informational: queued jobs are never moved automatically.
pub fn idle_peer(
    node: &str,
    local: &Snapshot,
    peers: &[placement::Peer],
    now: f64,
    cfg: &HealthConfig,
) -> Vec<Observation> {
    if peers.is_empty() {
        return vec![];
    }
    let queued: Vec<&JobView> = local
        .jobs
        .iter()
        .filter(|j| j.resource == SlotKind::Compile)
        .filter(|j| matches!(j.phase, Phase::Preparing | Phase::WaitingBuild))
        .filter(|j| now - j.submitted_at as f64 > cfg.idle_peer_s)
        .collect();
    let mut by_peer: HashMap<String, (Vec<String>, String)> = HashMap::new();
    for job in queued {
        let Ok(d) = placement::decide(
            &job.argv,
            &placement::Class::Compile,
            &placement::Request::Anywhere,
            local,
            crate::admission::DEFAULT_MAX_CARGO_JOBS,
            peers,
        ) else {
            continue;
        };
        let Some(peer) = d.peer() else { continue };
        let idle = peers
            .iter()
            .find(|p| p.name == peer)
            .and_then(|p| p.snapshot.as_ref())
            .is_some_and(|s| {
                s.machine.blocker.is_none()
                    && s.machine
                        .slots
                        .iter()
                        .filter(|sl| sl.kind == SlotKind::Compile)
                        .all(|sl| !sl.busy)
            });
        if !idle {
            continue;
        }
        let entry = by_peer
            .entry(peer.to_string())
            .or_insert_with(|| (vec![], d.line()));
        entry.0.push(format!(
            "#{} {} waiting {}: {}",
            job.id,
            job.owner,
            duration(now - job.submitted_at as f64),
            job_reason(job)
        ));
    }
    let pressured = local.machine.blocker.clone();
    by_peer
        .into_iter()
        .map(|(peer, (jobs, line))| {
            let severity = if pressured.is_some() {
                Severity::Warn
            } else {
                Severity::Info
            };
            let mut o = Observation::new("idle_peer", severity, node)
                .subject(peer.clone())
                .summary(format!(
                    "{} compile job(s) queued on {node}{} while peer {peer} is idle",
                    jobs.len(),
                    pressured
                        .as_deref()
                        .map(|b| format!(" (blocked: {b})"))
                        .unwrap_or_default()
                ))
                .fix(format!(
                    "new work: submit with placement anywhere (MCP submit default) or `tasks run \
                     --anywhere` / `--peer {peer}`. Queued jobs are not moved automatically; an \
                     owner may cancel their own job and resubmit it that way"
                ));
            for j in jobs.iter().take(6) {
                o = o.evidence(j.clone());
            }
            o.evidence(line)
        })
        .collect()
}

/// Finished jobs (within the last 6 hours) above `high_cpu_s` CPU time.
pub fn high_cpu(node: &str, jobs: &[JobView], now: f64, cfg: &HealthConfig) -> Vec<Observation> {
    jobs.iter()
        .filter(|j| j.phase.terminal())
        .filter(|j| {
            j.finished_at
                .is_some_and(|f| now - (f as f64) < 6.0 * 3600.0)
        })
        .filter_map(|j| {
            let r = j.result.as_ref()?;
            let cpu = r.cpu_user_s.unwrap_or(0.0) + r.cpu_sys_s.unwrap_or(0.0);
            (cpu > cfg.high_cpu_s).then(|| {
                let agent = j.owner.split('@').next().unwrap_or(&j.owner).to_string();
                Observation::new("high_cpu", Severity::Info, node)
                    .subject(j.id.to_string())
                    .summary(format!(
                        "job #{} by {agent} used {:.1} CPU-hours ({})",
                        j.id,
                        cpu / 3600.0,
                        j.command
                    ))
                    .evidence(format!(
                        "wall {}, exit {:?}, owner {}",
                        r.wall_s.map(duration).unwrap_or_else(|| "?".into()),
                        r.exit_code,
                        j.owner
                    ))
                    .fix(
                        "narrow the command (a package/test filter instead of the whole \
                          workspace) or run it on the worker with placement anywhere",
                    )
            })
        })
        .collect()
}

/// One alert per missing-capability mark.
pub fn capabilities(marks: &[CapabilityMark]) -> Vec<Observation> {
    marks
        .iter()
        .map(|m| {
            let fix = super::classify::pattern(&m.family)
                .map(|p| p.fix)
                .unwrap_or("install the missing tool on that machine");
            Observation::new("missing_capability", Severity::Warn, &m.node)
                .subject(m.capability.clone())
                .summary(format!("{} {}", m.node, m.lacks()))
                .evidence(m.evidence())
                .evidence(format!(
                    "placement skips {} for commands that need it until a later `{}` passes \
                     there, the mark expires, or `tasks alerts clear {} {}`",
                    m.node, m.kind_key, m.node, m.capability
                ))
                .fix(fix)
                .human()
        })
        .collect()
}

#[cfg(test)]
#[path = "checks_tests.rs"]
mod tests;
