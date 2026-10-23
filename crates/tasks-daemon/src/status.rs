//! Building the API views: the status snapshot and job rows.

use crate::api::{
    DaemonView, JobView, MAX_STATUS_WAITERS, PeerView, PressureView, RunningView, SlotGroupView,
    SlotView, StatusSnapshot, WaiterView,
};
use crate::api::{PeerJobUseView, PeerMachineView, PeerSlotView};
use crate::queue_estimate::{Queued, starts_in_queue_order};
use crate::queue_view::{class_note, peer_hold, remote_wait};
use crate::scheduler::{RunningJob, Scheduler, lock, shorten};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tasks_admission::{Lane, Waiter, check_gates, effective, kind_name};
use tasks_core::{JobId, Phase, SlotKind};
use tasks_ledger::{JobRecord, ListFilter};
use tasks_timing::progress;

/// What a peer last reported about itself, as the status snapshot shows it.
fn peer_machine_view(s: &tasks_peer::proto::PeerStatus) -> PeerMachineView {
    PeerMachineView {
        cores: s.machine.cores,
        memory_bytes: s.machine.memory_bytes,
        slots: s
            .slots
            .iter()
            .map(|x| PeerSlotView {
                kind: kind_name(x.kind).to_string(),
                total: x.total,
                used: x.used,
            })
            .collect(),
        waiting: s.queue.waiting,
        running: s.queue.running,
        cpu_pct: s.pressure.cpu_pct,
        memory_pct: s.pressure.memory_pct,
        memory_level: s.pressure.memory_level.clone(),
        io_pct: s.pressure.io_pct,
        pressure_summary: s.pressure.summary.clone(),
        cpu_busy_pct: s.pressure.cpu_busy_pct,
        memory_available_bytes: s.pressure.memory_available_bytes,
        swap_total: s.pressure.swap_total_bytes,
        swap_used: s.pressure.swap_used_bytes,
        swap_in_per_s: s.pressure.swap_in_per_s,
        swap_out_per_s: s.pressure.swap_out_per_s,
        jobs: s
            .jobs
            .iter()
            .map(|j| PeerJobUseView {
                id: j.id,
                owner: j.owner.clone(),
                cores_milli: j.cores_milli,
                mem_bytes: j.mem_bytes,
            })
            .collect(),
        cores_in_use_milli: s.cores_in_use_milli,
        power: s.power.map(|p| power_text(p.on_battery, p.percent)),
    }
}

/// `on battery 80%` / `on AC`.
pub(crate) fn power_text(on_battery: bool, percent: Option<u8>) -> String {
    tasks_platform::PowerReading {
        on_battery,
        percent,
    }
    .to_string()
}

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
        figures: r.figures.clone(),
        priority: r.spec.priority,
        priority_reason: r.spec.priority_reason.clone(),
        remote_wait: remote_wait(r),
        fetch_paths: r.spec.fetch.clone(),
        fetch: fetch_results(r),
        sync: sync_view(r),
    }
}

/// The sync before a forwarded run, when it has happened (R-648).
fn sync_view(r: &JobRecord) -> Option<crate::api::SyncView> {
    match (&r.log_path, &r.forward) {
        (Some(log), Some(_)) => crate::peer_link::sync_note::read(log),
        _ => None,
    }
}

/// The recorded fetch results of a finished job that asked for any.
fn fetch_results(r: &JobRecord) -> Vec<tasks_core::FetchResult> {
    match (&r.log_path, r.spec.fetch.is_empty(), r.phase.is_terminal()) {
        (Some(log), false, true) => crate::peer_link::fetch::read_results(log),
        _ => Vec::new(),
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
        for kind in [
            SlotKind::Compile,
            SlotKind::Run,
            SlotKind::Offload,
            SlotKind::Transfer,
        ] {
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
            if kind == SlotKind::Transfer {
                // transfer slots are held by sync phases, not by ledger jobs
                let active = lock(&self.attrib).transfer.active;
                for index in 0..active.min(total) {
                    busy.push(SlotView {
                        index,
                        job: None,
                        external: false,
                        holder: Some("checkout sync".to_string()),
                    });
                }
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

        // waiters, per slot kind in the order admission takes them
        let aging_ms = policy.thresholds().aging_ms;
        let now_ms = u64::try_from(now).unwrap_or(0);
        let last_started = lock(&self.state).last_started.clone();
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
        let peer_views = self.forwarder.peers();
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
            let mut queued = Vec::new();
            let mut by_id: BTreeMap<JobId, (&JobRecord, Waiter, bool)> = BTreeMap::new();
            for r in rows {
                let (runtime, rough) = if computed < MAX_STATUS_WAITERS {
                    computed += 1;
                    let f = self.facts(r);
                    // queue order plans with the planning figure; "rough" says no passing run
                    // backs it
                    (f.plan.median_secs, !f.plan.is_measured())
                } else {
                    (60.0, true)
                };
                let mut w = Waiter::new(
                    r.id,
                    kind.unwrap_or(SlotKind::Run),
                    u64::try_from(r.submitted_at_ms).unwrap_or(0),
                );
                w.owner = r.spec.owner.as_str().to_string();
                w.priority = r.spec.priority;
                queued.push(Queued {
                    waiter: w.clone(),
                    runtime_secs: runtime,
                });
                by_id.insert(r.id, (r, w, rough));
            }
            let ordered =
                starts_in_queue_order(total, &remaining, &queued, now_ms, aging_ms, &last_started);
            for (i, (id, start)) in ordered.iter().enumerate() {
                let Some((r, w, rough)) = by_id.get(id) else {
                    continue;
                };
                let eff = effective(w, now_ms, aging_ms);
                let waited = now_ms.saturating_sub(w.queued_at_ms);
                // a forwarded job the peer holds back: say what the peer says, not "starting"
                // and a start time of about now (R-670)
                let blocker = r.forward.as_ref().and_then(|fw| {
                    let gated = tasks_placement::slot_for(r.spec.kind) == Some(SlotKind::Compile);
                    peer_views
                        .iter()
                        .find(|p| p.machine.name.0 == fw.peer.0)
                        .filter(|p| gated && p.pressure.blocked)
                        .map(|p| p.pressure.summary.as_str())
                });
                let held = peer_hold(r, blocker);
                waiting.push(WaiterView {
                    id: r.id,
                    owner: r.spec.owner.as_str().to_string(),
                    command: shorten(&r.spec.argv.join(" ")),
                    wait_text: held
                        .clone()
                        .or_else(|| r.wait_text.clone())
                        .unwrap_or_else(|| "queued".to_string()),
                    position: i + 1,
                    start_estimate_secs: if kind.is_some() && held.is_none() {
                        *start
                    } else {
                        None
                    },
                    rough: *rough,
                    class: eff.class.as_str().to_string(),
                    class_note: class_note(
                        r.spec.priority,
                        r.spec.priority_reason.as_deref(),
                        &eff,
                        waited,
                        aging_ms,
                    ),
                    on_peer: held.and(r.forward.as_ref().map(|fw| fw.peer.0.clone())),
                });
            }
        }
        // a forwarded job the peer holds back on its own gates is waiting there, not running
        let records: BTreeMap<JobId, &JobRecord> = active.iter().map(|r| (r.id, r)).collect();
        let mut held_on_peer: std::collections::BTreeSet<JobId> = Default::default();
        for rj in &running {
            let Some(r) = records.get(&rj.id) else {
                continue;
            };
            let (Some(text), Some(fw)) = (remote_wait(r), r.forward.as_ref()) else {
                continue;
            };
            held_on_peer.insert(rj.id);
            waiting.push(WaiterView {
                id: r.id,
                owner: r.spec.owner.as_str().to_string(),
                command: shorten(&r.spec.argv.join(" ")),
                wait_text: text,
                position: 0,
                start_estimate_secs: None,
                rough: true,
                class: r.spec.priority.as_str().to_string(),
                class_note: String::new(),
                on_peer: Some(fw.peer.0.clone()),
            });
        }
        let waiting_more = waiting.len().saturating_sub(MAX_STATUS_WAITERS);
        waiting.truncate(MAX_STATUS_WAITERS);

        let running_views = running
            .iter()
            .filter(|rj| !held_on_peer.contains(&rj.id))
            .map(|rj| {
                let tl = lock(&rj.timeline);
                let p = progress(&tl, &rj.estimates);
                drop(tl);
                let live = lock(&self.metrics).latest(rj.id);
                let elapsed = rj.started.elapsed().as_secs_f64();
                // time left only when passing runs back the job's estimate (R-660..R-665)
                let (eta, over, rough_from, no_why) = match &rj.shown {
                    tasks_timing::Shown::Eta(e) => {
                        let (eta, over) = match e.left(p.eta_secs, elapsed) {
                            tasks_timing::Left::Secs(s) => (Some(s), None),
                            tasks_timing::Left::Over {
                                usual_secs,
                                by_secs,
                            } => (None, Some((usual_secs, by_secs))),
                        };
                        (eta, over, e.rough.then(|| e.machine.clone()), None)
                    }
                    tasks_timing::Shown::Absent(w) => (None, None, None, Some(w.to_string())),
                };
                RunningView {
                    id: rj.id,
                    owner: rj.owner.clone(),
                    command: rj.command.clone(),
                    slot: rj.slot.map(|(k, i)| (kind_name(k).to_string(), i)),
                    elapsed_secs: rj.started.elapsed().as_secs_f64(),
                    done_targets: p.done_targets,
                    total_targets: p.total_targets,
                    current: p.current,
                    eta_secs: eta,
                    usual_secs: over.map(|o| o.0),
                    over_by_secs: over.map(|o| o.1),
                    eta_rough_from: rough_from,
                    no_eta_why: no_why,
                    log_bytes: std::fs::metadata(&rj.log_path)
                        .map(|m| m.len())
                        .unwrap_or(0),
                    figures: lock(&rj.figures).clone(),
                    cores: live.and_then(|l| l.cores),
                    mem_bytes: live.and_then(|l| l.mem_bytes),
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
            attribution: self.attribution_line(),
            transfer: self.transfer_view(),
            trips: self.trip_views(crate::attrib::STATUS_TRIPS),
            power: Some(self.power_view()),
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
                reply_ms: q.and_then(|q| q.reply_ms),
                machine: l.status.map(|s| peer_machine_view(&s)),
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
                reply_ms: p.reply_ms,
                machine: None,
                name: p.name,
                node_id: p.node_id,
            });
        }
        out
    }

    /// CPU and swap rates between the previous status reading and this one. The base reading is
    /// replaced only after two seconds, so a UI refreshing faster still sees a smooth figure.
    fn host_rates(&self, now: &tasks_platform::MemorySnapshot) -> tasks_platform::HostRates {
        let mut st = lock(&self.state);
        let at = std::time::Instant::now();
        match st.host_prev {
            None => {
                st.host_prev = Some((at, *now, Default::default()));
                Default::default()
            }
            Some((then, prev, last)) => {
                let ms = at.duration_since(then).as_millis() as u64;
                if ms < 2000 {
                    return last;
                }
                let rates = tasks_platform::HostRates::between(&prev, now, ms);
                st.host_prev = Some((at, *now, rates));
                rates
            }
        }
    }

    fn pressure_view(&self, policy: &crate::machine_policy::MachinePolicy) -> PressureView {
        let admitter = self.admitter(policy);
        let mem = admitter.memory.snapshot().ok();
        let rates = mem.map(|m| self.host_rates(&m)).unwrap_or_default();
        match admitter.readings() {
            Ok(r) => PressureView {
                memory: level_name(r.pressure.memory).to_string(),
                memory_some: r.pressure.memory_some,
                memory_full: r.pressure.memory_full,
                memory_stall_limit: r
                    .pressure
                    .memory_some
                    .map(|_| policy.psi().memory_some_warn),
                cpu: r.pressure.cpu,
                io: r.pressure.io,
                available_memory: Some(r.available_memory),
                memory_total: mem.map(|m| m.total_bytes),
                cpu_busy_pct: rates.cpu_pct,
                cpu_cores: Some(self.cores),
                cores_in_use: rates
                    .cpu_pct
                    .map(|p| f64::from(p) / 100.0 * f64::from(self.cores)),
                swap_total: mem.map(|m| m.swap_total_bytes),
                swap_used: mem.map(|m| m.swap_used_bytes),
                swap_in_per_s: rates.swap_in_per_s,
                swap_out_per_s: rates.swap_out_per_s,
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
                memory_stall_limit: None,
                cpu: None,
                io: None,
                available_memory: None,
                memory_total: None,
                cpu_busy_pct: None,
                cores_in_use: None,
                cpu_cores: Some(self.cores),
                swap_total: None,
                swap_used: None,
                swap_in_per_s: None,
                swap_out_per_s: None,
                cache_free: None,
                state_free: None,
                compile_blocker: Some(format!("cannot measure the machine: {e}")),
                warning: Some(e.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod peer_view_tests {
    use super::*;
    use tasks_peer::proto::{PeerStatus, PressureSummary, QueueSummary, SlotStatus};

    #[test]
    fn r643_a_peers_last_report_reaches_the_status_view() {
        let machine = tasks_core::Machine {
            name: tasks_core::MachineName("nixos".into()),
            os: tasks_core::Os::Linux,
            arch: tasks_core::Arch::current(),
            cores: 16,
            memory_bytes: 64 << 30,
            tools: Default::default(),
            devices: Default::default(),
        };
        let v = peer_machine_view(&PeerStatus {
            machine,
            slots: vec![SlotStatus {
                kind: SlotKind::Compile,
                total: 4,
                used: 3,
            }],
            queue: QueueSummary {
                waiting: 2,
                running: 3,
            },
            pressure: PressureSummary {
                cpu_pct: Some(40),
                memory_pct: None,
                memory_level: None,
                io_pct: Some(0),
                summary: "memory stalls".into(),
                cpu_busy_pct: Some(61),
                memory_available_bytes: Some(51 << 30),
                swap_total_bytes: Some(4 << 30),
                swap_used_bytes: Some(1 << 30),
                swap_in_per_s: Some(0),
                swap_out_per_s: Some(512),
            },
            jobs: vec![],
            cores_in_use_milli: None,
            power: None,
        });
        assert_eq!(
            (v.cores, v.memory_bytes, v.waiting, v.running),
            (16, 64 << 30, 2, 3)
        );
        assert_eq!(
            (v.slots[0].kind.as_str(), v.slots[0].total, v.slots[0].used),
            ("compile", 4, 3)
        );
        // R-643: what the peer has free reaches the origin's view
        assert_eq!(v.memory_available_bytes, Some(51 << 30));
        // unknown stays unknown; a measured zero stays zero
        assert_eq!(
            (v.cpu_pct, v.memory_pct, v.io_pct),
            (Some(40), None, Some(0))
        );
        assert_eq!(v.pressure_summary, "memory stalls");
        // R-360: what the peer says about CPU and swap reaches the view, unknown stays unknown
        assert_eq!(v.cpu_busy_pct, Some(61));
        assert_eq!(
            (v.swap_total, v.swap_used, v.swap_in_per_s, v.swap_out_per_s),
            (Some(4 << 30), Some(1 << 30), Some(0), Some(512))
        );
        let old = peer_machine_view(&PeerStatus {
            pressure: PressureSummary::default(),
            ..PeerStatus {
                machine: tasks_core::Machine {
                    name: tasks_core::MachineName("old".into()),
                    os: tasks_core::Os::Linux,
                    arch: tasks_core::Arch::current(),
                    cores: 2,
                    memory_bytes: 1 << 30,
                    tools: Default::default(),
                    devices: Default::default(),
                },
                slots: vec![],
                queue: QueueSummary::default(),
                pressure: PressureSummary::default(),
                jobs: vec![],
                cores_in_use_milli: None,
                power: None,
            }
        });
        assert_eq!(
            (old.cpu_busy_pct, old.swap_total, old.swap_used),
            (None, None, None)
        );
    }

    // R-360: a peer of the previous build sends none of the new fields and still parses
    #[test]
    fn r360_a_pressure_report_from_an_older_peer_parses_with_the_new_fields_unknown() {
        let old = r#"{"cpu_pct":3,"memory_pct":null,"io_pct":0,"summary":""}"#;
        let p: PressureSummary = serde_json::from_str(old).unwrap();
        assert_eq!(p.cpu_pct, Some(3));
        assert_eq!((p.cpu_busy_pct, p.swap_total_bytes), (None, None));
        // and the daemon's own view of an older daemon's status parses too
        let v: crate::api::PeerMachineView = serde_json::from_str(
            r#"{"cores":4,"memory_bytes":1,"slots":[],"waiting":0,"running":0,"cpu_pct":null,"memory_pct":null,"io_pct":null,"pressure_summary":""}"#,
        )
        .unwrap();
        assert_eq!((v.cpu_busy_pct, v.swap_total), (None, None));
    }
}
