//! TEMPORARY adapter, to be deleted once feature/metrics provides
//! `tasks status --json`: builds a partial cluster snapshot (v1) for this machine
//! from what already exists, i.e. the daemon's `list` reply plus /proc. It reads
//! only; it never probes admission locks.
//!
//! What it cannot know stays `null` ("unknown" in the window): live per-job CPU
//! and memory, crate progress, warmth. Slots are inferred from the ledger's
//! running jobs, so holders outside the ledger (legacy helpers) are not shown.
//! Estimates come from the existing per-(suite, configuration) runtime history.
use crate::ui_model::{Snapshot, short_command};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use tinline_task_manager::local_command::{self, Recipe, Resource};
use tinline_task_manager::{Action, Job, Phase, Response, RuntimeEstimate};

const HISTORY_POINTS: usize = 150;
const FINISHED_KEPT: usize = 50;
const SLOTS: [(&str, &str, usize); 3] = [
    ("compile", "p2p-build", 1),
    ("run", "p2p-run-", 4),
    ("offload", "p2p-offload-", 4),
];

/// Machine counters between polls (CPU deltas and the 5-minute history).
#[derive(Default)]
pub struct Sampler {
    last_cpu: Option<(u64, u64)>,
    cpu: VecDeque<Option<f64>>,
    mem: VecDeque<Option<f64>>,
}

pub fn poll(socket: &Path, sampler: &mut Sampler) -> Result<Snapshot, String> {
    let response = tinline_task_manager::request_with_reconnect(socket, Action::List, || {
        crate::ensure_daemon(socket)
    })
    .map_err(|e| e.to_string())?;
    if let Some(error) = &response.error
        && response.jobs.is_empty()
    {
        return Err(error.clone());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let machine = sampler.machine(now);
    Snapshot::parse(snapshot(&response, machine, now))
}

impl Sampler {
    fn machine(&mut self, _now: f64) -> MachineFacts {
        let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
        let busy = cpu_times(&stat).and_then(|now| {
            let previous = self.last_cpu.replace(now);
            let (idle0, total0) = previous?;
            let (idle, total) = now;
            let dt = total.checked_sub(total0).filter(|d| *d > 0)?;
            let busy = 1.0 - idle.saturating_sub(idle0) as f64 / dt as f64;
            Some(busy.clamp(0.0, 1.0) * logical_cpus() as f64)
        });
        let mem = meminfo(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default());
        let used = match (mem.get("MemTotal"), mem.get("MemAvailable")) {
            (Some(t), Some(a)) => Some(t.saturating_sub(*a) as f64),
            _ => None,
        };
        push(&mut self.cpu, busy);
        push(&mut self.mem, used);
        let pressure = tinline_task_manager::admission::Pressure::read().ok();
        let blocker = pressure.as_ref().and_then(|p| {
            tinline_task_manager::admission::thresholds()
                .map(|t| p.blocker_with(&t))
                .unwrap_or_else(|e| Some(format!("invalid admission thresholds: {e}")))
        });
        MachineFacts {
            id: std::fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|h| h.trim().to_string())
                .ok()
                .filter(|h| !h.is_empty()),
            logical_cpus: logical_cpus(),
            cpu_busy_cores: busy,
            mem_total: mem.get("MemTotal").copied(),
            mem_available: mem.get("MemAvailable").copied(),
            swap_total: mem.get("SwapTotal").copied(),
            swap_used: match (mem.get("SwapTotal"), mem.get("SwapFree")) {
                (Some(t), Some(f)) => Some(t.saturating_sub(*f)),
                _ => None,
            },
            pressure: pressure.map(|p| {
                json!({
                    "cpu_some_avg10": p.cpu_some_avg10,
                    "memory_some_avg10": p.memory_some_avg10,
                    "memory_full_avg10": p.memory_full_avg10,
                    "io_some_avg10": p.io_some_avg10,
                })
            }),
            blocker,
            cpu_history: self.cpu.iter().copied().collect(),
            mem_history: self.mem.iter().copied().collect(),
        }
    }
}

fn push(ring: &mut VecDeque<Option<f64>>, value: Option<f64>) {
    if ring.len() == HISTORY_POINTS {
        ring.pop_front();
    }
    ring.push_back(value);
}

fn logical_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// (idle incl. iowait, total) jiffies from the aggregate `cpu` line.
pub fn cpu_times(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|v| v.parse().ok())
        .collect::<Option<_>>()?;
    let idle = values.get(3)? + values.get(4).copied().unwrap_or(0);
    // guest/guest_nice are already included in user/nice.
    let total = values.iter().take(8).sum();
    Some((idle, total))
}

/// `/proc/meminfo` in bytes.
pub fn meminfo(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
            Some((name.to_string(), kib * 1024))
        })
        .collect()
}

#[derive(Default)]
pub struct MachineFacts {
    pub id: Option<String>,
    pub logical_cpus: usize,
    pub cpu_busy_cores: Option<f64>,
    pub mem_total: Option<u64>,
    pub mem_available: Option<u64>,
    pub swap_total: Option<u64>,
    pub swap_used: Option<u64>,
    pub pressure: Option<Value>,
    pub blocker: Option<String>,
    pub cpu_history: Vec<Option<f64>>,
    pub mem_history: Vec<Option<f64>>,
}

fn recipe(job: &Job) -> Option<Recipe> {
    (job.spec.suite == local_command::SUITE)
        .then(|| serde_json::from_str(&job.spec.configuration).ok())
        .flatten()
}

/// (slot kind, device serial). Non-command suites use their build/run stages.
fn resource(job: &Job) -> (&'static str, Option<String>) {
    match recipe(job).map(|r| r.resource) {
        Some(Resource::Compile) => ("compile", None),
        Some(Resource::Run) => ("run", None),
        Some(Resource::Offload) => ("offload", None),
        Some(Resource::Service) => ("service", None),
        Some(Resource::Device(serial)) => ("device", Some(serial)),
        None if matches!(job.phase, Phase::Running | Phase::WaitingRun) => ("run", None),
        None => ("compile", None),
    }
}

fn holds_slot(job: &Job) -> bool {
    matches!(job.phase, Phase::Building | Phase::Running)
}

fn waits(job: &Job) -> bool {
    matches!(
        job.phase,
        Phase::Approved | Phase::Preparing | Phase::WaitingBuild | Phase::WaitingRun
    )
}

/// The same history selection as the previous window's "Est. runtime" column.
fn estimate(job: &Job, history: &[&RuntimeEstimate], now: f64) -> Value {
    let worker = history
        .iter()
        .any(|s| s.stage == "worker.build" || s.stage == "worker.run");
    let known: Vec<_> = history
        .iter()
        .filter(|s| {
            s.seconds.is_some()
                && if worker {
                    matches!(s.stage.as_str(), "worker.build" | "worker.run")
                } else {
                    !s.stage.starts_with("worker.")
                }
        })
        .collect();
    if known.is_empty() {
        return Value::Null;
    }
    let total: f64 = known.iter().filter_map(|s| s.seconds).sum();
    let samples = known.iter().map(|s| s.samples).min().unwrap_or(0);
    let remaining = match job.execution.as_ref().and_then(|e| e.started_at) {
        Some(start) if holds_slot(job) => Some(total - (now - start as f64)),
        _ => None,
    };
    json!({
        "total_s": total,
        "remaining_s": remaining,
        "samples": samples,
        "basis": format!(
            "recency-weighted mean of {samples} run(s) of this exact command (any warmth; \
             legacy history until tasks status --json)"
        ),
    })
}

fn exit_code(job: &Job) -> Option<i64> {
    job.phase
        .reported_exit_code(
            job.execution
                .as_ref()
                .map_or(&[][..], |e| e.stages.as_slice()),
        )
        .map(i64::from)
}

fn offload_worker(argv: &[String]) -> Option<String> {
    argv.iter()
        .position(|a| a == "--worker")
        .and_then(|i| argv.get(i + 1))
        .cloned()
}

/// The partial snapshot document. Pure: tested with synthetic replies.
pub fn snapshot(response: &Response, machine: MachineFacts, now: f64) -> Value {
    let mut jobs: Vec<&Job> = response.jobs.iter().collect();
    jobs.sort_by_key(|j| std::cmp::Reverse(j.id));
    let mut finished = 0;
    jobs.retain(|j| {
        !j.phase.terminal() || {
            finished += 1;
            finished <= FINISHED_KEPT
        }
    });
    let mut history: HashMap<(&str, &str), Vec<&RuntimeEstimate>> = HashMap::new();
    for e in &response.estimates {
        history
            .entry((e.suite.as_str(), e.configuration.as_str()))
            .or_default()
            .push(e);
    }

    // Slots from the ledger's running jobs, padded to the README's counts.
    let mut slots = Vec::new();
    let mut holders: HashMap<&str, Vec<&Job>> = HashMap::new();
    let mut ascending = jobs.clone();
    ascending.reverse();
    for job in ascending.iter().filter(|j| holds_slot(j)) {
        let (kind, device) = resource(job);
        if kind == "device" {
            slots.push(json!({
                "kind": "device",
                "name": format!("p2p-device-{}", device.unwrap_or_default()),
                "busy": true, "holder_job": job.id, "holder_who": job.spec.owner,
            }));
        } else {
            holders.entry(kind).or_default().push(job);
        }
    }
    for (kind, prefix, count) in SLOTS {
        let held = holders.get(kind).map(Vec::as_slice).unwrap_or(&[]);
        for i in 0..count.max(held.len()) {
            let name = if count == 1 {
                prefix.to_string()
            } else {
                format!("{prefix}{}", i + 1)
            };
            let holder = held.get(i);
            slots.push(json!({
                "kind": kind, "name": name, "busy": holder.is_some(),
                "holder_job": holder.map(|j| j.id),
                "holder_who": holder.map(|j| j.spec.owner.clone()),
            }));
        }
    }

    let mut positions: HashMap<&str, u64> = HashMap::new();
    let mut out = Vec::new();
    for job in ascending {
        let recipe = recipe(job);
        let argv = recipe.as_ref().map(|r| r.argv.clone()).unwrap_or_default();
        let command = if recipe.is_some() {
            short_command(&argv)
        } else {
            format!("{} {}", job.spec.suite, job.spec.configuration)
        };
        let (kind, device) = resource(job);
        let checkout = recipe.is_some().then(|| job.spec.source.clone());
        let execution = job.execution.as_ref();
        let started = execution.and_then(|e| e.started_at).map(|t| t as f64);
        let finished_at = execution
            .and_then(|e| e.finished_at)
            .filter(|_| job.phase.terminal())
            .map(|t| t as f64);
        let queue = waits(job).then(|| {
            let position = positions.entry(kind).or_insert(0);
            *position += 1;
            let held = holders.get(kind).map(Vec::len).unwrap_or(0);
            let capacity = SLOTS
                .iter()
                .find(|(k, _, _)| *k == kind)
                .map(|(_, _, n)| *n)
                .unwrap_or(1);
            let waiting_for = match (job.phase.clone(), holders.get(kind)) {
                (Phase::Approved, _) => Some("the local worker to start".to_string()),
                (_, Some(h)) if held >= capacity => Some(format!(
                    "{kind} slot held by {}",
                    h.iter()
                        .map(|j| format!("#{}", j.id))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                _ => machine.blocker.clone(),
            };
            json!({"position": *position, "waiting_for": waiting_for})
        });
        let empty = Vec::new();
        let runs = history
            .get(&(job.spec.suite.as_str(), job.spec.configuration.as_str()))
            .unwrap_or(&empty);
        out.push(json!({
            "id": job.id,
            "owner": job.spec.owner,
            "phase": job.phase.as_db().unwrap_or_default(),
            "resource": kind,
            "device": device,
            "checkout": checkout,
            "checkout_name": checkout.as_deref().map(|c| c.trim_end_matches('/').rsplit('/').next().unwrap_or(c).to_string()),
            "command": command,
            "kind_key": command,
            "argv": argv,
            "submitted_at": job.submitted_at as f64,
            "started_at": started,
            "finished_at": finished_at,
            "queue": queue,
            "live": null,
            "progress": null,
            "warmth": "unknown",
            "warmth_reason": "not measured yet (needs tasks status --json from feature/metrics)",
            "estimate": if job.phase.terminal() { Value::Null } else { estimate(job, runs, now) },
            "result": job.phase.terminal().then(|| json!({
                "exit_code": exit_code(job),
                "wall_s": started.zip(finished_at).map(|(s, f)| f - s),
            })),
            "offload": (kind == "offload").then(|| json!({
                "worker": offload_worker(&argv),
                "remote_job_id": null, "transport": null, "sync_s": null, "fetch_s": null,
            })),
        }));
    }
    json!({
        "version": 1,
        "taken_at": now,
        "adapter": "ui_legacy (temporary)",
        "machine": {
            "id": machine.id,
            "role": "host",
            "logical_cpus": machine.logical_cpus,
            "cpu_busy_cores": machine.cpu_busy_cores,
            "mem_total_bytes": machine.mem_total,
            "mem_available_bytes": machine.mem_available,
            "swap_total_bytes": machine.swap_total,
            "swap_used_bytes": machine.swap_used,
            "pressure": machine.pressure,
            "blocker": machine.blocker,
            "slots": slots,
            "history": {"interval_s": 2.0, "cpu_busy_cores": machine.cpu_history, "mem_used_bytes": machine.mem_history},
        },
        "jobs": out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_model::{self, Class, Target};
    use tinline_task_manager::{Cost, JobSpec, execution::Execution};

    fn command_job(id: i64, phase: Phase, argv: &[&str], resource: &str) -> Job {
        let configuration = format!(
            "{{\"argv\":{},\"resource\":{resource}}}",
            serde_json::to_string(argv).unwrap()
        );
        assert!(
            serde_json::from_str::<Recipe>(&configuration).is_ok(),
            "{configuration}"
        );
        Job {
            id,
            spec: JobSpec {
                suite: local_command::SUITE.into(),
                owner: format!("agent-{id}"),
                source: format!("/synthetic/checkouts/c{id}"),
                revision: "live".into(),
                configuration,
                cost: Cost::CpuSimulation,
            },
            phase: phase.clone(),
            submitted_at: 1000 + id,
            execution: (phase != Phase::Queued).then(|| Execution {
                approved_at: 1000 + id,
                started_at: matches!(
                    phase,
                    Phase::Building | Phase::Running | Phase::Passed | Phase::Failed
                )
                .then_some(1100),
                finished_at: phase.terminal().then_some(1172),
                cancel_requested: false,
                worker: None,
                supervisor: None,
                snapshot: None,
                stages: vec![],
                build_artifact: None,
                android_trial: false,
                local_command: true,
                detail: String::new(),
                metrics: None,
                peer: None,
                reroute: None,
                remote_result: None,
                priority: None,
            }),
        }
    }

    #[test]
    fn proc_parsers() {
        assert_eq!(
            cpu_times("cpu  10 0 20 100 5 1 1 0 0 0\ncpu0 1 1 1 1 1 1 1 1 1 1\n"),
            Some((105, 137))
        );
        assert_eq!(cpu_times("nothing"), None);
        let mem = meminfo("MemTotal:       16000 kB\nMemAvailable:    8000 kB\nbogus\n");
        assert_eq!(mem["MemTotal"], 16000 * 1024);
        assert_eq!(mem.len(), 2);
    }

    #[test]
    fn legacy_reply_becomes_a_readable_partial_snapshot() {
        let jobs = vec![
            command_job(
                1,
                Phase::Passed,
                &["cargo", "test", "--locked", "-j", "2"],
                "\"compile\"",
            ),
            command_job(
                2,
                Phase::Building,
                &["env", "A=b", "cargo", "build", "-j", "2"],
                "\"compile\"",
            ),
            command_job(3, Phase::WaitingBuild, &["cargo", "clippy"], "\"compile\""),
            command_job(
                4,
                Phase::Running,
                &["tasks-offload", "--worker", "u@w", "--", "cargo", "test"],
                "\"offload\"",
            ),
            command_job(5, Phase::Queued, &["cargo", "doc"], "\"compile\""),
            command_job(
                6,
                Phase::Running,
                &["adb", "shell", "true"],
                "{\"device\":\"SYNTH1\"}",
            ),
        ];
        let estimates = vec![RuntimeEstimate {
            suite: jobs[1].spec.suite.clone(),
            configuration: jobs[1].spec.configuration.clone(),
            stage: "worker.build".into(),
            seconds: Some(300.0),
            range: None,
            samples: 3,
        }];
        let response = Response {
            version: tinline_task_manager::PROTOCOL_VERSION,
            execution_enabled: true,
            submitted_id: None,
            jobs,
            estimates,
            error: None,
            log: None,
        };
        let facts = MachineFacts {
            id: Some("synthetic-host".into()),
            logical_cpus: 8,
            blocker: Some("memory PSI full avg10 above 0.5%".into()),
            ..Default::default()
        };
        let snap = Snapshot::parse(snapshot(&response, facts, 1200.0)).unwrap();
        assert!(snap.warnings.is_empty(), "{:?}", snap.warnings);
        assert_eq!(snap.machine.id.as_deref(), Some("synthetic-host"));
        let kinds: Vec<_> = snap
            .machine
            .slots
            .iter()
            .map(|s| (s.kind.clone().unwrap(), s.busy.unwrap(), s.holder_job))
            .collect();
        assert!(kinds.contains(&("compile".into(), true, Some(2))));
        assert!(kinds.contains(&("offload".into(), true, Some(4))));
        assert!(kinds.contains(&("device".into(), true, Some(6))));
        assert_eq!(kinds.iter().filter(|k| k.0 == "run").count(), 4);

        let job = |id| snap.jobs.iter().find(|j| j.id == id).unwrap();
        assert_eq!(job(2).command.as_deref(), Some("cargo build"));
        assert_eq!(job(2).checkout_name.as_deref(), Some("c2"));
        // Running for 100 s of a 300 s history.
        let estimate = job(2).estimate.clone().unwrap();
        assert_eq!(
            (estimate.total_s, estimate.remaining_s),
            (Some(300.0), Some(200.0))
        );
        let queue = job(3).queue.clone().unwrap();
        assert_eq!(queue.position, Some(1));
        assert_eq!(
            queue.waiting_for.as_deref(),
            Some("compile slot held by #2")
        );
        assert_eq!(job(1).result.clone().unwrap().wall_s, Some(72.0));
        assert_eq!(job(1).result.clone().unwrap().exit_code, Some(0));
        assert_eq!(
            job(4).offload.clone().unwrap().worker.as_deref(),
            Some("u@w")
        );
        assert_eq!(job(6).device.as_deref(), Some("SYNTH1"));
        assert!(job(2).live.is_none() && job(2).progress.is_none());

        // The whole view builds from it, with local buttons.
        let view = ui_model::build(&[ui_model::MachineState {
            fallback_name: "laptop".into(),
            snapshot: Some(snap.clone()),
            local: true,
            seen_ago_s: Some(0.0),
            error: None,
            configured: true,
        }]);
        assert_eq!(view.machines[0].pressure_text, "memory high");
        let card = view.running.iter().find(|r| r.key.id == 2).unwrap();
        assert_eq!(card.cpu_text, "CPU unknown");
        assert!(matches!(card.cancel, Target::Local { id: 2, .. }));
        assert_eq!(ui_model::class(job(5)), Class::Proposal);
        assert_eq!(view.recent.len(), 1);
    }
}
