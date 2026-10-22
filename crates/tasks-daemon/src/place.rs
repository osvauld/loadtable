//! Placement glue: builds the placement crate's input from the ledger, the policy and the
//! forwarder, and acts on its decision. Only the local machine runs jobs for now; a peer
//! decision is handed to the [`Forwarder`](crate::forwarder::Forwarder).

use crate::machine_policy::MachinePolicy;
use crate::queue_estimate::start_offsets;
use crate::scheduler::{RunningJob, Scheduler, lock};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use tasks_admission::{Lane, Readings, Thresholds, check_gates};
use tasks_core::{DeviceId, JobId, Kind, Machine, MachineName, Placement, SlotKind, Tool};
use tasks_ledger::JobRecord;
use tasks_placement::{
    Checkout, Decision, Input, Link, MachineView, Policy as PlacePolicy, Pressure, Request,
    RunEstimate,
};
use tasks_timing::estimate::{Estimate, MachineInfo, estimate};
use tasks_timing::{Estimates, History, RecipeKey, SampleKey, progress};

/// What the daemon knows about a job's cost, computed once per job.
#[derive(Debug)]
pub(crate) struct JobFacts {
    /// The recipe history is keyed by.
    pub recipe: String,
    /// The estimate on this machine (built-in default when there is no history).
    pub est: Option<Estimate>,
    /// The median, only when history backs it (a job with none is never "short").
    pub known_median: Option<f64>,
    /// The checkout a build job works in; at most one build runs per checkout.
    pub checkout: Option<String>,
}

/// Facts about the machine for one pass.
pub(crate) struct PlaceCtx {
    pub readings: Option<Readings>,
    pub free: BTreeMap<SlotKind, u32>,
    pub queue_depth: u32,
    pub wait_secs: BTreeMap<SlotKind, f64>,
    pub thresholds: Thresholds,
}

/// What placement decided for a job.
pub(crate) enum Placed {
    /// Run here.
    Local,
    /// Handed to a peer.
    Forwarded,
    /// Try again next pass.
    Waiting(String),
    /// Will never run as asked.
    Refused(String),
}

/// Placement tuning. Open design question: these could live in the machine policy.
fn placement_policy() -> PlacePolicy {
    PlacePolicy {
        staleness_limit_secs: 120,
        sync_secs_per_mib: 2.0,
        local_bonus_secs: 30.0,
        unknown_run_secs: 120.0,
        unknown_wait_secs: 300.0,
        pressure_wait_secs: 60.0,
        p90_weight: 0.0,
        reroute_margin_secs: 60.0,
        reroute_cooldown_secs: 30,
        max_reroutes: 2,
    }
}

/// The checkout a directory belongs to: the nearest ancestor with a `.git`, else itself.
pub(crate) fn checkout_key(cwd: &Path) -> String {
    for dir in cwd.ancestors() {
        if dir.join(".git").exists() {
            return dir.display().to_string();
        }
    }
    cwd.display().to_string()
}

impl Scheduler {
    pub(crate) fn machine_info(&self) -> MachineInfo {
        MachineInfo {
            name: self.cfg.machine.clone(),
            cores: self.cores,
        }
    }

    fn history(&self) -> &dyn History {
        &*self.ledger
    }

    pub(crate) fn estimate_for(
        &self,
        recipe: &str,
        machine: &MachineInfo,
        others: &[MachineInfo],
    ) -> Option<Estimate> {
        let key = SampleKey {
            recipe: recipe.to_string(),
            target: None,
            machine: machine.name.clone(),
        };
        estimate(self.history(), &key, machine, others).ok()
    }

    /// Cost facts of a job, cached until the job leaves the queue.
    pub(crate) fn facts(&self, rec: &JobRecord) -> Arc<JobFacts> {
        if let Some(f) = lock(&self.state).facts.get(&rec.id) {
            return f.clone();
        }
        let keys = RecipeKey::from_argv(&rec.spec.argv, &rec.spec.env);
        let recipe = keys.first().map_or_else(
            || rec.spec.argv.first().cloned().unwrap_or_default(),
            ToString::to_string,
        );
        let est = self.estimate_for(&recipe, &self.machine_info(), &[]);
        let known_median = est
            .as_ref()
            .filter(|e| e.samples > 0)
            .map(|e| e.median_secs);
        let checkout = matches!(rec.spec.kind, Kind::Compile | Kind::Test | Kind::Package)
            .then(|| checkout_key(&rec.spec.cwd));
        let facts = Arc::new(JobFacts {
            recipe,
            est,
            known_median,
            checkout,
        });
        lock(&self.state).facts.insert(rec.id, facts.clone());
        facts
    }

    /// Per-target and compile estimates for the progress display of a running job.
    pub(crate) fn build_estimates(&self, facts: &JobFacts) -> Estimates {
        let h = self.history();
        let info = self.machine_info();
        let mut targets = BTreeMap::new();
        for label in h.targets(&facts.recipe).unwrap_or_default() {
            let key = SampleKey {
                recipe: facts.recipe.clone(),
                target: Some(label.clone()),
                machine: info.name.clone(),
            };
            let e = estimate(h, &key, &info, &[]).ok().filter(|e| e.samples > 0);
            targets.insert(label, e);
        }
        Estimates {
            compile: None,
            targets,
        }
    }

    /// This machine as placement sees it. Tools the job needs count when they are on `PATH`.
    pub(crate) fn local_machine(
        &self,
        policy: &MachinePolicy,
        wanted: &BTreeSet<String>,
    ) -> Machine {
        let mut tools: BTreeSet<Tool> = policy.tools().into_iter().map(Tool).collect();
        for t in wanted {
            if self.on_path(t) {
                tools.insert(Tool(t.clone()));
            }
        }
        Machine {
            name: MachineName(self.cfg.machine.clone()),
            os: self.os,
            arch: self.arch,
            cores: self.cores,
            memory_bytes: self
                .platform
                .memory
                .snapshot()
                .map(|m| m.total_bytes)
                .unwrap_or(0),
            tools,
            devices: policy.devices().into_iter().map(DeviceId).collect(),
        }
    }

    fn on_path(&self, tool: &str) -> bool {
        self.parent_env
            .iter()
            .find(|(k, _)| k == "PATH")
            .is_some_and(|(_, p)| p.split(':').any(|d| Path::new(d).join(tool).is_file()))
    }

    /// Seconds a running job still needs: its progress ETA, else its estimate minus elapsed.
    pub(crate) fn remaining_secs(&self, rj: &RunningJob) -> f64 {
        let elapsed = rj.started.elapsed().as_secs_f64();
        let tl = lock(&rj.timeline);
        let p = progress(&tl, &rj.estimates);
        drop(tl);
        if let Some(eta) = p.eta_secs {
            return eta.max(0.0);
        }
        rj.est_median.map_or(60.0, |m| (m - elapsed).max(0.0))
    }

    pub(crate) fn place_ctx(
        &self,
        policy: &MachinePolicy,
        readings: Option<&Readings>,
        running: &[Arc<RunningJob>],
        ready: &[JobId],
    ) -> PlaceCtx {
        let mut free = BTreeMap::new();
        let mut wait_secs = BTreeMap::new();
        let queue_depth = u32::try_from(
            ready
                .iter()
                .filter(|id| !running.iter().any(|r| r.id == **id))
                .count(),
        )
        .unwrap_or(u32::MAX);
        for kind in [SlotKind::Compile, SlotKind::Run, SlotKind::Offload] {
            let total = policy.slot_count(kind);
            let of_kind: Vec<&Arc<RunningJob>> = running
                .iter()
                .filter(|r| r.slot_kind == Some(kind))
                .collect();
            let used = u32::try_from(of_kind.len()).unwrap_or(u32::MAX);
            let f = total.saturating_sub(used);
            free.insert(kind, f);
            if f == 0 {
                let remaining: Vec<f64> = of_kind.iter().map(|r| self.remaining_secs(r)).collect();
                let avg = if remaining.is_empty() {
                    60.0
                } else {
                    remaining.iter().sum::<f64>() / remaining.len() as f64
                };
                let queued = vec![avg; queue_depth as usize];
                let mut with_new = queued;
                with_new.push(0.0);
                if let Some(Some(w)) = start_offsets(total as usize, &remaining, &with_new)
                    .last()
                    .copied()
                {
                    wait_secs.insert(kind, w);
                }
            }
        }
        PlaceCtx {
            readings: readings.copied(),
            free,
            queue_depth,
            wait_secs,
            thresholds: policy.thresholds(),
        }
    }

    /// Decides where a job runs, and acts on a peer decision.
    pub(crate) fn place(&self, rec: &JobRecord, policy: &MachinePolicy, ctx: &PlaceCtx) -> Placed {
        let facts = self.facts(rec);
        let wanted: BTreeSet<String> = rec
            .spec
            .requirements
            .tools
            .iter()
            .map(|t| t.0.clone())
            .chain(rec.hints.tools.iter().cloned())
            .collect();
        let local = self.local_machine(policy, &wanted);
        let avoid = lock(&self.state)
            .no_forward
            .get(&rec.id)
            .cloned()
            .unwrap_or_default();
        let mut peers = self.forwarder.peers();
        // a peer this job already failed to reach is not tried again (it runs here instead)
        peers.retain(|p| !avoid.contains(&p.machine.name.0));
        let info = MachineInfo {
            name: local.name.0.clone(),
            cores: local.cores,
        };
        let peer_infos: Vec<MachineInfo> = peers
            .iter()
            .map(|p| MachineInfo {
                name: p.machine.name.0.clone(),
                cores: p.machine.cores,
            })
            .collect();
        let run_est =
            |e: Option<Estimate>| e.filter(|e| e.samples > 0).map(|e| RunEstimate::from(&e));
        let slot = tasks_placement::slot_for(rec.spec.kind);
        let (blocked, summary) = match (slot, &ctx.readings) {
            (None, _) => (false, String::new()),
            (Some(_), None) => (true, "pressure unreadable".to_string()),
            (Some(k), Some(r)) => {
                let new_output =
                    matches!(rec.spec.kind, Kind::Compile | Kind::Test | Kind::Package);
                match check_gates(k, Lane::Normal, new_output, r, &ctx.thresholds) {
                    Some(w) => (true, w.to_string()),
                    None => (false, String::new()),
                }
            }
        };
        let local_view = MachineView {
            machine: local,
            is_local: true,
            link: Link::Up { age_secs: 0 },
            free_slots: ctx.free.clone(),
            queue_depth: ctx.queue_depth,
            expected_wait_secs: slot.and_then(|k| ctx.wait_secs.get(&k).copied()),
            pressure: Pressure { blocked, summary },
            estimate: run_est(self.estimate_for(&facts.recipe, &info, &peer_infos)),
            checkout: Checkout {
                synced: true,
                sync_bytes: 0,
            },
            marks: Vec::new(),
        };
        let mut machines = vec![local_view];
        for mut p in peers {
            p.is_local = false;
            if p.estimate.is_none() {
                let mine = MachineInfo {
                    name: p.machine.name.0.clone(),
                    cores: p.machine.cores,
                };
                let mut others = peer_infos.clone();
                others.retain(|o| o.name != mine.name);
                others.push(info.clone());
                p.estimate = run_est(self.estimate_for(&facts.recipe, &mine, &others));
            }
            machines.push(p);
        }
        let request = match (&rec.spec.placement, &rec.hints.placement) {
            (Placement::Anywhere, Some(h)) => Request::from_hint(h),
            (p, _) => Request::from_placement(p),
        };
        let input = Input {
            kind: rec.spec.kind,
            requirements: rec.spec.requirements.clone(),
            hints: rec.hints.clone(),
            request,
            local_only_reason: None,
            machines,
        };
        let report = match tasks_placement::decide(&input, &placement_policy()) {
            Ok(r) => r,
            Err(e) => return Placed::Waiting(format!("placement failed: {e}")),
        };
        let line = report.one_line();
        match &report.decision {
            Decision::Run(name) if name.0 == self.cfg.machine => {
                lock(&self.state).placed.insert(rec.id, line.clone());
                Placed::Local
            }
            Decision::Run(name) => match self.forwarder.forward(rec, name) {
                Ok(ticket) => {
                    // the (peer, key) pair is persisted before anything is sent
                    let saved = self.guard(
                        "set_forward",
                        self.ledger.set_forward(
                            rec.id,
                            name,
                            ticket.remote_job,
                            ticket.remote_key.as_deref(),
                        ),
                    );
                    if saved.is_none() {
                        self.forwarder.forget(rec.id);
                        return Placed::Waiting(format!(
                            "cannot record the forward to {} in the ledger",
                            name.0
                        ));
                    }
                    self.wake.notify();
                    Placed::Forwarded
                }
                Err(e) => Placed::Waiting(format!("cannot forward to {}: {e}", name.0)),
            },
            Decision::Wait => Placed::Waiting(line),
            Decision::Refuse(_) => Placed::Refused(line),
        }
    }
}
