//! Building the API views: the status snapshot and job rows.

use crate::api::{
    DaemonView, JobView, MAX_STATUS_WAITERS, PeerView, PressureView, RunningView, SlotGroupView,
    SlotView, StatusSnapshot, WaiterView,
};
use crate::queue_estimate::start_offsets;
use crate::scheduler::{RunningJob, Scheduler, lock, shorten};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tasks_admission::{Lane, check_gates, kind_name};
use tasks_core::{Phase, SlotKind};
use tasks_ledger::{JobRecord, ListFilter};
use tasks_timing::progress;

/// A job row for the API. The environment is left out on purpose.
pub fn job_view(r: &JobRecord) -> JobView {
    JobView {
        id: r.id,
        owner: r.spec.owner.as_str().to_string(),
        argv: r.spec.argv.clone(),
        cwd: r.spec.cwd.clone(),
        kind: r.spec.kind,
        placement: r.spec.placement.clone(),
        after: r.spec.after.clone(),
        phase: r.phase.clone(),
        wait_text: r.wait_text.clone(),
        exit: r.shown_exit(),
        summary: r.summary.clone(),
        submitted_at_ms: r.submitted_at_ms,
        started_at_ms: r.started_at_ms,
        finished_at_ms: r.finished_at_ms,
        machine: r.machine.as_ref().map(|m| m.0.clone()),
        forwarded_to: r.forward.as_ref().map(|f| (f.peer.0.clone(), f.remote_job)),
        cancel_requested_by: r.cancel.as_ref().map(|c| c.by.clone()),
        cancel_reason: r.cancel.as_ref().and_then(|c| c.reason.clone()),
        timeout_secs: r.spec.timeout_secs.or(r.hints.timeout_secs),
        remote_phase: r.remote_phase.clone(),
        plan: r.plan_id.zip(r.step_name.clone()),
        log_path: r.log_path.clone(),
    }
}

fn level_name(l: tasks_platform::Level) -> &'static str {
    match l {
        tasks_platform::Level::Normal => "normal",
        tasks_platform::Level::Warn => "warn",
        tasks_platform::Level::Critical => "critical",
    }
}

impl Scheduler {
    /// The status snapshot: slots, waiters with their full wait text, running jobs with
    /// progress and ETA, pressure, alerts and every policy value with its source.
    pub fn status(&self) -> StatusSnapshot {
        let policy = self.policy();
        let now = self.clock.now_ms();
        let drained = if self.draining.load(Ordering::SeqCst) {
            lock(&self.drained).clone()
        } else {
            None
        };
        let running: Vec<Arc<RunningJob>> = lock(&self.running).values().cloned().collect();
        let active = self
            .ledger
            .list(&ListFilter {
                active_only: true,
                ..ListFilter::default()
            })
            .unwrap_or_default();
        let (orphans, orphan_text) = {
            let st = lock(&self.state);
            (
                st.orphan_since.keys().copied().collect::<Vec<_>>(),
                st.orphan_text.clone(),
            )
        };

        let mut slots = Vec::new();
        let layout = policy.layout();
        for kind in [SlotKind::Compile, SlotKind::Run, SlotKind::Offload] {
            let total = layout.counts.get(&kind).copied().unwrap_or(0);
            let mut busy = Vec::new();
            for rj in running
                .iter()
                .filter(|r| r.slot.is_some_and(|s| s.0 == kind))
            {
                if let Some((_, index)) = rj.slot {
                    busy.push(SlotView {
                        index,
                        job: Some(rj.id),
                        external: false,
                        holder: None,
                    });
                }
            }
            // R-050: a busy slot with no ledger job is external work, never "no live jobs"
            for (k, index) in orphans.iter().filter(|(k, _)| *k == kind) {
                busy.push(SlotView {
                    index: *index,
                    job: None,
                    external: true,
                    holder: orphan_text.get(&(*k, *index)).cloned(),
                });
            }
            busy.sort_by_key(|b| b.index);
            let normal_busy = busy
                .iter()
                .filter(|b| b.index < tasks_admission::SHORT_LANE_BASE)
                .count();
            slots.push(SlotGroupView {
                kind: kind_name(kind).to_string(),
                total,
                free: total.saturating_sub(u32::try_from(normal_busy).unwrap_or(u32::MAX)),
                busy,
            });
        }

        // waiters, per slot kind in queue order
        let mut per_kind: BTreeMap<Option<SlotKind>, Vec<&JobRecord>> = BTreeMap::new();
        for r in active
            .iter()
            .filter(|r| matches!(r.phase, Phase::Queued | Phase::Waiting(_)))
        {
            per_kind
                .entry(tasks_placement::slot_for(r.spec.kind))
                .or_default()
                .push(r);
        }
        let mut waiting = Vec::new();
        let mut computed = 0usize;
        for (kind, rows) in &per_kind {
            let (total, remaining) = match kind {
                Some(k) => (
                    policy.slot_count(*k) as usize,
                    running
                        .iter()
                        .filter(|r| r.slot_kind == Some(*k))
                        .map(|r| self.remaining_secs(r))
                        .collect::<Vec<_>>(),
                ),
                None => (0, Vec::new()),
            };
            let mut runtimes = Vec::new();
            let mut rough = Vec::new();
            for r in rows {
                if computed < MAX_STATUS_WAITERS {
                    computed += 1;
                    let f = self.facts(r);
                    rough.push(f.known_median.is_none());
                    runtimes.push(f.est.as_ref().map_or(60.0, |e| e.median_secs));
                } else {
                    rough.push(true);
                    runtimes.push(60.0);
                }
            }
            let starts = start_offsets(total, &remaining, &runtimes);
            for (i, r) in rows.iter().enumerate() {
                waiting.push(WaiterView {
                    id: r.id,
                    owner: r.spec.owner.as_str().to_string(),
                    command: shorten(&r.spec.argv.join(" ")),
                    wait_text: r.wait_text.clone().unwrap_or_else(|| "queued".to_string()),
                    position: i + 1,
                    start_estimate_secs: if kind.is_some() {
                        starts.get(i).copied().flatten()
                    } else {
                        None
                    },
                    rough: rough.get(i).copied().unwrap_or(true),
                });
            }
        }
        waiting.sort_by_key(|w| w.id);
        let waiting_more = waiting.len().saturating_sub(MAX_STATUS_WAITERS);
        waiting.truncate(MAX_STATUS_WAITERS);

        let running_views = running
            .iter()
            .map(|rj| {
                let tl = lock(&rj.timeline);
                let p = progress(&tl, &rj.estimates);
                drop(tl);
                RunningView {
                    id: rj.id,
                    owner: rj.owner.clone(),
                    command: rj.command.clone(),
                    slot: rj.slot.map(|(k, i)| (kind_name(k).to_string(), i)),
                    elapsed_secs: rj.started.elapsed().as_secs_f64(),
                    done_targets: p.done_targets,
                    total_targets: p.total_targets,
                    current: p.current,
                    eta_secs: p.eta_secs.or_else(|| {
                        rj.est_median
                            .map(|m| (m - rj.started.elapsed().as_secs_f64()).max(0.0))
                    }),
                    log_bytes: std::fs::metadata(&rj.log_path)
                        .map(|m| m.len())
                        .unwrap_or(0),
                }
            })
            .collect();

        StatusSnapshot {
            daemon: DaemonView {
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                machine: self.cfg.machine.clone(),
                state_dir: self.cfg.state_dir.clone(),
                lock_dir: self.cfg.lock_dir.clone(),
                policy_file: self.cfg.policy_path(),
                policy_file_loaded: policy.file_loaded,
                policy_problems: policy.problems.clone(),
                draining: self.draining.load(Ordering::SeqCst),
                drained_by: drained.as_ref().map(|d| d.0.clone()),
                drained_at_ms: drained.as_ref().map(|d| d.1),
                execution_enabled: self.execution_enabled.load(Ordering::SeqCst),
                uptime_secs: u64::try_from((now - self.started_ms) / 1000).unwrap_or(0),
                now_ms: now,
            },
            slots,
            waiting,
            waiting_more,
            running: running_views,
            pressure: self.pressure_view(&policy),
            alerts: lock(&self.alerts).views(),
            settings: policy.settings(),
            peers: self.peer_views(),
        }
    }

    /// Every peer: the forwarder's links joined with what trust knows (DID, roles each way, grant
    /// expiry), plus paired peers that have no link yet (they only connect to this machine).
    fn peer_views(&self) -> Vec<PeerView> {
        let trust = self.trust();
        let quality = self.forwarder.link_quality();
        if let Some(t) = &trust {
            t.set_link_quality(quality.clone());
        }
        let known = trust.as_ref().map(|t| t.peers()).unwrap_or_default();
        let roles = |v: &[crate::trust_control::RoleView]| -> Vec<String> {
            v.iter().map(|r| r.role.clone()).collect()
        };
        let mut out: Vec<PeerView> = Vec::new();
        for l in self.forwarder.links() {
            let t = known.iter().find(|p| p.name == l.peer);
            let q = quality.iter().find(|q| q.peer == l.peer);
            out.push(PeerView {
                state: l.state.name().to_string(),
                last_seen_ago_secs: l.last_seen_ago_secs,
                detail: l.detail,
                did: t.map(|p| p.did.clone()).unwrap_or_default(),
                roles_out: t.map(|p| roles(&p.roles_out)).unwrap_or_default(),
                roles_in: t.map(|p| roles(&p.roles_in)).unwrap_or_default(),
                grant_expires_at: t.and_then(|p| p.expires_at),
                path: q.map_or_else(|| "unknown".to_string(), |q| q.path.clone()),
                rtt_ms: q.and_then(|q| q.rtt_ms),
                name: l.peer,
                node_id: l.node_id,
            });
        }
        for p in known {
            if out.iter().any(|v| v.name == p.name) {
                continue;
            }
            out.push(PeerView {
                state: if p.revoked { "revoked" } else { "inbound" }.to_string(),
                last_seen_ago_secs: None,
                detail: String::new(),
                did: p.did,
                roles_out: roles(&p.roles_out),
                roles_in: roles(&p.roles_in),
                grant_expires_at: p.expires_at,
                path: p.path,
                rtt_ms: p.rtt_ms,
                name: p.name,
                node_id: p.node_id,
            });
        }
        out
    }

    fn pressure_view(&self, policy: &crate::machine_policy::MachinePolicy) -> PressureView {
        let admitter = self.admitter(policy);
        match admitter.readings() {
            Ok(r) => PressureView {
                memory: level_name(r.pressure.memory).to_string(),
                memory_some: r.pressure.memory_some,
                memory_full: r.pressure.memory_full,
                cpu: r.pressure.cpu,
                io: r.pressure.io,
                available_memory: Some(r.available_memory),
                cache_free: Some(r.cache_free),
                state_free: Some(r.state_free),
                compile_blocker: check_gates(
                    SlotKind::Compile,
                    Lane::Normal,
                    true,
                    &r,
                    &policy.thresholds(),
                )
                .map(|w| w.to_string()),
                warning: None,
            },
            Err(e) => PressureView {
                memory: "unknown".to_string(),
                memory_some: None,
                memory_full: None,
                cpu: None,
                io: None,
                available_memory: None,
                cache_free: None,
                state_free: None,
                compile_blocker: Some(format!("cannot measure the machine: {e}")),
                warning: Some(e.to_string()),
            },
        }
    }
}
