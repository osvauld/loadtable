//! Placement glue: builds the placement crate's input from the ledger, the policy and the
//! forwarder, and acts on its decision. Only the local machine runs jobs for now; a peer
//! decision is handed to the [`Forwarder`](crate::forwarder::Forwarder).

use crate::machine_policy::MachinePolicy;
use crate::queue_estimate::{Queued, starts_in_queue_order};
use crate::scheduler::{RunningJob, Scheduler, lock};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use tasks_admission::{Lane, Readings, Thresholds, Waiter, check_gates};
use tasks_core::{DeviceId, JobId, Kind, Machine, MachineName, Placement, SlotKind, Tool};
use tasks_ledger::JobRecord;
use tasks_placement::{
    Checkout, Decision, Input, Link, MachineView, Policy as PlacePolicy, Pressure, Request,
    RunEstimate,
};
use tasks_timing::estimate::{Estimate, MachineInfo, NoEstimate, Planning, Shown, planning, shown};
use tasks_timing::{Cache, Estimates, History, RecipeKey, SampleKey, progress, wanted_cache};

/// Planning figure for a run nothing is known about, seconds.
pub(crate) const UNKNOWN_RUN_SECS: f64 = 60.0;

/// The directories a job will look for tools in: the `PATH` the job carries, else the daemon's,
/// then the fallback directories every job's `PATH` gets (R-543). The daemon runs under
/// systemd or launchd, whose `PATH` lacks `~/.cargo/bin`, so its own `PATH` alone would call
/// an installed cargo missing (R-732).
pub(crate) fn job_tool_dirs(
    spec_env: &[(String, String)],
    parent_env: &[(String, String)],
) -> Vec<String> {
    let path = |env: &[(String, String)]| {
        env.iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
    };
    let mut dirs: Vec<String> = path(spec_env)
        .or_else(|| path(parent_env))
        .map(|p| {
            p.split(':')
                .filter(|d| !d.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    for d in crate::build_cache::fallback_tool_dirs(parent_env) {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

/// True when `tool` is a file in one of `dirs`; `is_file` is the file system, injected.
pub(crate) fn tool_in_dirs(tool: &str, dirs: &[String], is_file: impl Fn(&Path) -> bool) -> bool {
    dirs.iter().any(|d| is_file(&Path::new(d).join(tool)))
}

/// A peer's machine as the origin can know it: peers do not report the tools they have, so
/// every tool the job wants is assumed present and the peer checks it against its own `PATH`
/// when the job arrives, refusing by name if it lacks one (R-734).
pub(crate) fn assume_peer_tools(machine: &mut Machine, wanted: &BTreeSet<String>) {
    machine.tools.extend(wanted.iter().cloned().map(Tool));
}

/// What the daemon knows about a job's cost, computed once per job.
#[derive(Debug)]
pub(crate) struct JobFacts {
    /// The recipe history is keyed by.
    pub recipe: String,
    /// The number the queue and placement plan with; a guess when nothing was measured. Never
    /// shown as an ETA.
    pub plan: Planning,
    /// What may be shown as this job's duration: passing runs only, or the reason there is none.
    pub shown: Shown,
    /// The build state the run starts from (see [`Scheduler::wanted_cache`]).
    pub want: Cache,
    /// Predicted duration for the short lane: the worst of the latest runs, from history only.
    /// `None` is unknown, which is never short.
    pub short_secs: Option<f64>,
    /// Predicted peak memory for the short lane, bytes, from the latest runs' figures.
    pub peak_memory: Option<u64>,
    /// The checkout a build job works in; at most one build runs per checkout.
    pub checkout: Option<String>,
}

/// The queue of one slot kind as a pass sees it, for the wait a job would have in it.
pub(crate) struct KindQueue {
    /// Slots of the kind on this machine.
    pub slots: usize,
    /// Seconds each running job of the kind still needs.
    pub remaining: Vec<f64>,
    /// The jobs waiting for the kind, this pass's candidates included.
    pub queued: Vec<Queued>,
}

/// Facts about the machine for one pass.
pub(crate) struct PlaceCtx {
    pub readings: Option<Readings>,
    pub free: BTreeMap<SlotKind, u32>,
    pub queues: BTreeMap<SlotKind, KindQueue>,
    /// The clock the queue order and aging use, ms.
    pub now_ms: u64,
    /// When each owner last started a job, ms.
    pub last_started: BTreeMap<String, u64>,
    pub thresholds: Thresholds,
}

impl PlaceCtx {
    /// Where `id` stands in its kind's queue: how many jobs are ahead of it in the order
    /// admission will take them, and the seconds until it starts. A job not in the queue (a
    /// probe, which needs no slot) has neither.
    pub(crate) fn standing(&self, kind: SlotKind, id: JobId) -> Option<(u32, Option<f64>)> {
        let q = self.queues.get(&kind)?;
        let order = starts_in_queue_order(
            q.slots,
            &q.remaining,
            &q.queued,
            self.now_ms,
            self.thresholds.aging_ms,
            &self.last_started,
        );
        let at = order.iter().position(|(j, _)| *j == id)?;
        let start = order.get(at).and_then(|(_, s)| *s);
        Some((u32::try_from(at).unwrap_or(u32::MAX), start))
    }
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

    /// The number to plan with for `recipe` on `machine`. Not for display.
    pub(crate) fn planning_for(
        &self,
        recipe: &str,
        machine: &MachineInfo,
        others: &[MachineInfo],
    ) -> Option<Planning> {
        let key = SampleKey {
            recipe: recipe.to_string(),
            target: None,
            machine: machine.name.clone(),
        };
        planning(self.history(), &key, machine, others).ok()
    }

    /// What may be shown as the duration of `recipe` on `machine` starting from `want`.
    pub(crate) fn shown_for(
        &self,
        recipe: &str,
        machine: &MachineInfo,
        others: &[MachineInfo],
        want: Cache,
    ) -> Shown {
        let key = SampleKey {
            recipe: recipe.to_string(),
            target: None,
            machine: machine.name.clone(),
        };
        shown(self.history(), &key, machine, others, want)
            .unwrap_or(Shown::Absent(NoEstimate::HistoryUnreadable))
    }

    /// What may be shown as the duration of a job forwarded to `peer`: the runs this machine
    /// recorded of the same command on that peer, with the peer's warmth (R-790). Other
    /// machines' runs are not offered: the job runs at the peer's speed.
    pub(crate) fn shown_on_peer(&self, rec: &JobRecord, peer: &str) -> Shown {
        let recipe = RecipeKey::history_key(&rec.spec.argv, &rec.spec.env)
            .unwrap_or_else(|| rec.spec.argv.first().cloned().unwrap_or_default());
        let key = SampleKey {
            recipe,
            target: None,
            machine: peer.to_string(),
        };
        let want = self
            .history()
            .samples(&key)
            .map_or(Cache::Cold, |s| wanted_cache(&s));
        let machine = MachineInfo {
            name: peer.to_string(),
            cores: 1,
        };
        shown(self.history(), &key, &machine, &[], want)
            .unwrap_or(Shown::Absent(NoEstimate::HistoryUnreadable))
    }

    /// The build state a run of `recipe` starts from on this machine: warm once this machine
    /// has a passing run of the command, cold before. The same rule stamps the run's sample
    /// when it finishes, so later estimates can tell a first build from a repeat. An unreadable
    /// history reads as cold, the cautious side (it hides an ETA rather than inventing one).
    pub(crate) fn wanted_cache(&self, recipe: &str) -> Cache {
        let key = SampleKey {
            recipe: recipe.to_string(),
            target: None,
            machine: self.cfg.machine.clone(),
        };
        self.history()
            .samples(&key)
            .map_or(Cache::Cold, |s| wanted_cache(&s))
    }

    /// Cost facts of a job, cached until the job leaves the queue.
    pub(crate) fn facts(&self, rec: &JobRecord) -> Arc<JobFacts> {
        if let Some(f) = lock(&self.state).facts.get(&rec.id) {
            return f.clone();
        }
        let recipe = RecipeKey::history_key(&rec.spec.argv, &rec.spec.env)
            .unwrap_or_else(|| rec.spec.argv.first().cloned().unwrap_or_default());
        let info = self.machine_info();
        let peers: Vec<MachineInfo> = self
            .forwarder
            .peers()
            .iter()
            .map(|p| MachineInfo {
                name: p.machine.name.0.clone(),
                cores: p.machine.cores,
            })
            .collect();
        let plan = self.planning_for(&recipe, &info, &[]).unwrap_or(Planning {
            median_secs: UNKNOWN_RUN_SECS,
            p90_secs: UNKNOWN_RUN_SECS,
            samples: 0,
            basis: tasks_timing::Basis::BuiltIn,
        });
        let want = self.wanted_cache(&recipe);
        let shown = self.shown_for(&recipe, &info, &peers, want);
        // A peer's test step that only reads the synced copy (R-700) does not queue behind the
        // other jobs of the copy: the origin's checkout lock already orders them against any
        // job that syncs or builds, and which of the readers run together is the slots' call.
        let reads_only = rec.origin.is_some() && rec.spec.requirements.shared_checkout;
        let checkout = (matches!(rec.spec.kind, Kind::Compile | Kind::Test | Kind::Package)
            && !reads_only)
            .then(|| checkout_key(&rec.spec.cwd));
        let (short_secs, peak_memory) = self.short_facts(&recipe);
        let facts = Arc::new(JobFacts {
            recipe,
            plan,
            shown,
            want,
            short_secs,
            peak_memory,
            checkout,
        });
        lock(&self.state).facts.insert(rec.id, facts.clone());
        facts
    }

    /// What history says about how long and how big this recipe is on this machine, for the
    /// short lane. Nothing the submitter says is consulted; a history that cannot be read is
    /// unknown.
    fn short_facts(&self, recipe: &str) -> (Option<f64>, Option<u64>) {
        let key = SampleKey {
            recipe: recipe.to_string(),
            target: None,
            machine: self.cfg.machine.clone(),
        };
        let h = self.history();
        let secs = h
            .samples(&key)
            .ok()
            .and_then(|s| tasks_timing::short::predicted_secs(&s));
        let peak = h
            .recent_peaks(&key)
            .ok()
            .and_then(|p| tasks_timing::short::predicted_peak(&p));
        (secs, peak)
    }

    /// The median peak memory of this command's past runs on this machine, when history keeps
    /// peaks for it.
    pub(crate) fn usual_peak_mem(&self, rec: &JobRecord) -> Option<u64> {
        let facts = self.facts(rec);
        let key = SampleKey {
            recipe: facts.recipe.clone(),
            target: None,
            machine: self.cfg.machine.clone(),
        };
        let peaks = self.history().recent_peaks(&key).ok()?;
        tasks_core::metrics::median_peak(&peaks)
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
            // a target's time follows the same rule as the job's: passing runs here only
            let e = shown(h, &key, &info, &[], facts.want)
                .ok()
                .and_then(|s| s.eta().map(Estimate::from));
            targets.insert(label, e);
        }
        Estimates {
            compile: None,
            targets,
        }
    }

    /// This machine as placement sees it. Tools the job needs count when they are on the `PATH`
    /// the job will run with (`spec_env` first, then the daemon's, R-732).
    pub(crate) fn local_machine(
        &self,
        policy: &MachinePolicy,
        wanted: &BTreeSet<String>,
        spec_env: &[(String, String)],
    ) -> Machine {
        let mut tools: BTreeSet<Tool> = policy.tools().into_iter().map(Tool).collect();
        let dirs = job_tool_dirs(spec_env, &self.parent_env);
        for t in wanted {
            if tool_in_dirs(t, &dirs, |p| p.is_file()) {
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

    /// Seconds a running job still needs: its progress ETA, else its estimate minus elapsed.
    pub(crate) fn remaining_secs(&self, rj: &RunningJob) -> f64 {
        let elapsed = rj.started.elapsed().as_secs_f64();
        let tl = lock(&rj.timeline);
        let p = progress(&tl, &rj.estimates);
        drop(tl);
        if let Some(eta) = p.eta_secs {
            return eta.max(0.0);
        }
        rj.est_median
            .map_or(UNKNOWN_RUN_SECS, |m| (m - elapsed).max(0.0))
    }

    /// The jobs waiting for slots, by the kind they wait for, as the queue estimate sees them.
    pub(crate) fn queued_by_kind(&self, recs: &[JobRecord]) -> BTreeMap<SlotKind, Vec<Queued>> {
        let mut out: BTreeMap<SlotKind, Vec<Queued>> = BTreeMap::new();
        for rec in recs {
            // a job already placed on a peer waits for an offload slot here
            let kind = if rec.forward.is_some() {
                Some(SlotKind::Offload)
            } else {
                tasks_placement::slot_for(rec.spec.kind)
            };
            let Some(kind) = kind else { continue };
            let facts = self.facts(rec);
            let mut waiter = Waiter::new(
                rec.id,
                kind,
                u64::try_from(rec.submitted_at_ms).unwrap_or(0),
            );
            waiter.owner = rec.spec.owner.as_str().to_string();
            waiter.priority = rec.spec.priority;
            out.entry(kind).or_default().push(Queued {
                waiter,
                runtime_secs: facts.plan.median_secs,
            });
        }
        out
    }

    pub(crate) fn place_ctx(
        &self,
        policy: &MachinePolicy,
        readings: Option<&Readings>,
        running: &[Arc<RunningJob>],
        mut queued: BTreeMap<SlotKind, Vec<Queued>>,
    ) -> PlaceCtx {
        let mut free = BTreeMap::new();
        let mut queues = BTreeMap::new();
        for kind in [SlotKind::Compile, SlotKind::Run, SlotKind::Offload] {
            let total = policy.slot_count(kind);
            let of_kind: Vec<&Arc<RunningJob>> = running
                .iter()
                .filter(|r| r.slot_kind() == Some(kind))
                .collect();
            let used = u32::try_from(of_kind.len()).unwrap_or(u32::MAX);
            free.insert(kind, total.saturating_sub(used));
            queues.insert(
                kind,
                KindQueue {
                    slots: total as usize,
                    remaining: of_kind.iter().map(|r| self.remaining_secs(r)).collect(),
                    queued: queued.remove(&kind).unwrap_or_default(),
                },
            );
        }
        PlaceCtx {
            readings: readings.copied(),
            free,
            queues,
            now_ms: u64::try_from(self.clock.now_ms()).unwrap_or(0),
            last_started: lock(&self.state).last_started.clone(),
            thresholds: policy.thresholds(),
        }
    }

    /// A job whose peer is asleep or away waits for it, up to the grace period (R-600). The
    /// time the job sat waiting is noted for its log when it is finally placed.
    fn wait_for_peer(
        &self,
        rec: &JobRecord,
        policy: &MachinePolicy,
        report: &tasks_placement::Report,
        line: String,
    ) -> Placed {
        use tasks_placement::{Exclusion, Verdict as V};
        let now_ms = self.clock.now_ms();
        // the peer the request names, else the first one that is only out of reach
        let named = match &report.request {
            tasks_placement::Request::Peer(n) => Some(n.clone()),
            _ => None,
        };
        let Some((peer, exclusions)) = report.machines.iter().find_map(|m| match &m.verdict {
            V::Excluded(ex)
                if !m.is_local
                    && ex.iter().all(Exclusion::is_transient)
                    && named.as_ref().is_none_or(|n| *n == m.name) =>
            {
                Some((m.name.0.clone(), ex))
            }
            _ => None,
        }) else {
            return Placed::Waiting(line);
        };
        let ago_secs = exclusions.iter().find_map(|e| match e {
            Exclusion::Unreachable { last_seen_ago_secs } => *last_seen_ago_secs,
            Exclusion::Stale { age_secs, .. } => Some(*age_secs),
            _ => None,
        });
        // fixed at the first pass, so the text does not change with every rounding of the age
        let last_seen_ms = *lock(&self.state)
            .peer_seen
            .entry(rec.id)
            .or_insert_with(|| {
                ago_secs.map(|a| now_ms.saturating_sub(i64::try_from(a).unwrap_or(0) * 1000))
            });
        let offset = |unix: i64| tasks_platform::local_offset_secs(unix);
        let verdict = crate::peer_wait::judge(
            &crate::peer_wait::Waiting {
                peer: &peer,
                last_seen_ms,
                submitted_ms: rec.submitted_at_ms,
                now_ms,
                grace: std::time::Duration::from_secs(policy.peer_wait_secs()),
                timeout: rec
                    .spec
                    .timeout_secs
                    .or(rec.hints.timeout_secs)
                    .map(std::time::Duration::from_secs),
            },
            &offset,
        );
        match verdict {
            crate::peer_wait::Verdict::Wait(text) => {
                let waited =
                    u64::try_from(now_ms.saturating_sub(rec.submitted_at_ms) / 1000).unwrap_or(0);
                lock(&self.state).peer_waited.insert(
                    rec.id,
                    format!(
                        "waited {} for {peer}, which was asleep or unreachable",
                        crate::peer_wait::dur_text(std::time::Duration::from_secs(waited))
                    ),
                );
                Placed::Waiting(text)
            }
            crate::peer_wait::Verdict::GiveUp(text) => Placed::Refused(text),
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
        let local = self.local_machine(policy, &wanted, &rec.spec.env);
        let avoid = lock(&self.state)
            .no_forward
            .get(&rec.id)
            .cloned()
            .unwrap_or_default();
        let mut peers = self.forwarder.peers();
        // a peer this job already failed to reach is not tried again (it runs here instead); it
        // stays in the report with the reason, so a refusal says why the peer was not used
        for p in &mut peers {
            assume_peer_tools(&mut p.machine, &wanted);
            if let Some(why) = avoid.get(&p.machine.name.0) {
                p.marks.push(why.clone());
            }
        }
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
        // placement scores with measured runs only; anything else is "assumed" in its score
        let run_est = |p: Option<Planning>| {
            p.filter(Planning::is_measured)
                .map(|p| RunEstimate::from(&p))
        };
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
        // where this job would stand here: how many are ahead of it, and when it would start
        let standing = slot.and_then(|k| ctx.standing(k, rec.id));
        let local_view = MachineView {
            machine: local,
            is_local: true,
            link: Link::Up { age_secs: 0 },
            free_slots: ctx.free.clone(),
            queue_depth: standing.map_or(0, |(ahead, _)| ahead),
            expected_wait_secs: standing.and_then(|(_, start)| start),
            pressure: Pressure { blocked, summary },
            estimate: run_est(self.planning_for(&facts.recipe, &info, &peer_infos)),
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
                p.estimate = run_est(self.planning_for(&facts.recipe, &mine, &others));
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
            local_cargo_jobs: tasks_policy::submit_rules::requested_jobs(&rec.spec.argv)
                .filter(|n| *n > policy.cargo_max_jobs())
                .map(|asked| tasks_placement::CargoJobs {
                    asked,
                    cap: policy.cargo_max_jobs(),
                }),
            machines,
        };
        let report = match tasks_placement::decide(&input, &placement_policy()) {
            Ok(r) => r,
            Err(e) => return Placed::Waiting(format!("placement failed: {e}")),
        };
        let mut line = report.one_line();
        if rec.origin.is_some() {
            // a job another machine sent here runs here because it was sent: this machine did
            // not choose it, so no placement estimate is claimed
            line = format!(
                "{} (sent here by a peer, which chose this machine; the job runs where it was sent)",
                self.cfg.machine
            );
        }
        match &report.decision {
            Decision::Run(name) if name.0 == self.cfg.machine => {
                if let Some(note) = lock(&self.state).peer_waited.remove(&rec.id) {
                    line = format!("{line}; {note}");
                }
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
            Decision::Wait => self.wait_for_peer(rec, policy, &report, line),
            Decision::Refuse(_) => Placed::Refused(line),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // R-732: a daemon under systemd has no ~/.cargo/bin on its PATH, yet jobs get it
    #[test]
    fn r732_the_tool_check_looks_in_the_fallback_dirs_every_job_gets() {
        let daemon = env(&[("PATH", "/usr/bin:/bin"), ("HOME", "/home/u")]);
        let dirs = job_tool_dirs(&[], &daemon);
        assert_eq!(dirs, ["/usr/bin", "/bin", "/home/u/.cargo/bin"]);
        let has = |p: &Path| p == Path::new("/home/u/.cargo/bin/cargo");
        assert!(tool_in_dirs("cargo", &dirs, has));
        assert!(!tool_in_dirs("gradle", &dirs, has));
        // without a HOME there is nothing to add
        assert_eq!(
            job_tool_dirs(&[], &env(&[("PATH", "/usr/bin")])),
            ["/usr/bin"]
        );
    }

    // R-733: the PATH the job carries is the one it will look in, not the daemon's
    #[test]
    fn r733_the_jobs_own_path_replaces_the_daemons() {
        let daemon = env(&[("PATH", "/usr/bin"), ("HOME", "/h")]);
        let spec = env(&[("PATH", "/opt/x/bin::/usr/local/bin")]);
        let dirs = job_tool_dirs(&spec, &daemon);
        assert_eq!(dirs, ["/opt/x/bin", "/usr/local/bin", "/h/.cargo/bin"]);
        // a fallback dir already on the path is not repeated
        let spec = env(&[("PATH", "/h/.cargo/bin")]);
        assert_eq!(job_tool_dirs(&spec, &daemon), ["/h/.cargo/bin"]);
        assert!(!tool_in_dirs("cargo", &[], |_| true));
    }

    // R-734
    #[test]
    fn r734_a_peer_is_assumed_to_have_the_tools_a_job_wants() {
        let mut m = Machine {
            name: MachineName("nixos".into()),
            os: tasks_core::Os::Linux,
            arch: tasks_core::Arch::current(),
            cores: 4,
            memory_bytes: 0,
            tools: [Tool("keep".into())].into(),
            devices: Default::default(),
        };
        let wanted: BTreeSet<String> = ["cargo".to_string()].into();
        assume_peer_tools(&mut m, &wanted);
        assert!(m.tools.contains(&Tool("cargo".into())));
        assert!(m.tools.contains(&Tool("keep".into())));
    }
}
