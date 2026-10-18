//! MCP tools about compute: who is compiling what (`who_is_compiling`), searching job logs
//! across machines (`search_logs`) and what a job cost (`job_cost`). All read-only.
use std::time::Duration;

use serde_json::{Value, json};
use tinline_task_manager::log_search::{self, Query};
use tinline_task_manager::metrics::{JobView, Snapshot, compile};
use tinline_task_manager::{Action, Phase, Result, placement};

use crate::mcp::{Args, Daemon, Outcome, phase_name};

/// This machine plus each worker/both peer's latest snapshot (if the peer link is enabled).
struct Machine {
    name: String,
    local: bool,
    state: String,
    snapshot: Option<Snapshot>,
    error: Option<String>,
}

fn machines(daemon: &Daemon) -> Result<Vec<Machine>> {
    let local = daemon.status()?;
    let mut out = vec![Machine {
        name: local.machine.id.clone(),
        local: true,
        state: "online".into(),
        snapshot: Some(local),
        error: None,
    }];
    if !placement::peer_config_present() {
        return Ok(out);
    }
    match tinline_task_manager::peer_link::request_peers(&daemon.socket, Duration::from_secs(30)) {
        Ok(report) => {
            for peer in report.snapshots {
                let parsed = peer
                    .snapshot
                    .as_ref()
                    .map(|v| serde_json::from_value::<Snapshot>(v.clone()));
                let (snapshot, error) = match parsed {
                    Some(Ok(s)) => (Some(s), peer.error.clone()),
                    Some(Err(e)) => (None, Some(format!("unreadable snapshot: {e}"))),
                    None => (None, peer.error.clone()),
                };
                out.push(Machine {
                    name: peer.name.clone(),
                    local: false,
                    state: if peer.stale && peer.online() {
                        "online, stale".into()
                    } else {
                        peer.state.clone()
                    },
                    snapshot,
                    error,
                });
            }
        }
        Err(e) => out.push(Machine {
            name: "peers".into(),
            local: false,
            state: "unavailable".into(),
            snapshot: None,
            error: Some(e.to_string()),
        }),
    }
    Ok(out)
}

fn agent(owner: &str) -> &str {
    owner.split_once('@').map_or(owner, |(a, _)| a)
}

fn mib(bytes: u64) -> String {
    format!("{} MiB", bytes / (1 << 20))
}

pub fn who_is_compiling(daemon: &Daemon, args: &Args) -> Result<Outcome> {
    let wanted = args.str("crate")?.unwrap_or("").trim().to_owned();
    let now = tinline_task_manager::execution::now();
    let mut text = String::new();
    let mut out = vec![];
    let mut total = 0;
    for m in machines(daemon)? {
        let mut jobs = vec![];
        if let Some(s) = &m.snapshot {
            for j in &s.jobs {
                if !matches!(j.phase, Phase::Building | Phase::Running) {
                    continue;
                }
                // A forwarded job compiles on its peer; that machine lists it.
                if j.offload.is_some() {
                    continue;
                }
                let compiling: Vec<&compile::Compiling> = j
                    .compiling
                    .iter()
                    .flatten()
                    .filter(|c| compile::concerns(c, &wanted))
                    .collect();
                if compiling.is_empty() && (!wanted.is_empty() || j.phase != Phase::Building) {
                    continue;
                }
                jobs.push(job_entry(j, &compiling, now));
            }
        }
        total += jobs.len();
        text.push_str(&format!(
            "{} ({}{}): {}\n",
            m.name,
            if m.local { "this machine" } else { "peer" },
            if m.local {
                String::new()
            } else {
                format!(", {}", m.state)
            },
            match (&m.snapshot, jobs.len()) {
                (None, _) => format!(
                    "no snapshot{}",
                    m.error
                        .as_deref()
                        .map(|e| format!(" ({e})"))
                        .unwrap_or_default()
                ),
                (_, 0) if wanted.is_empty() => "nothing compiling".into(),
                (_, 0) => format!("nobody compiling {wanted}"),
                (_, n) => format!("{n} job(s)"),
            }
        ));
        for j in &jobs {
            text.push_str(&format!(
                "  #{} {} [{}] {} {}s{}: {}\n",
                j["id"],
                j["owner"].as_str().unwrap_or("?"),
                j["checkout_name"].as_str().unwrap_or("?"),
                j["phase"].as_str().unwrap_or("?"),
                j["elapsed_s"].as_i64().unwrap_or(0),
                j["cpu_cores"]
                    .as_f64()
                    .map(|c| format!(", {c:.1} cores"))
                    .unwrap_or_default(),
                j["command"].as_str().unwrap_or("")
            ));
            for c in j["compiling"].as_array().into_iter().flatten() {
                text.push_str(&format!(
                    "     {} {} ({} via {}) pid {} {:.0}s{} {}\n",
                    c["tool"].as_str().unwrap_or("?"),
                    c["unit"].as_str().unwrap_or("?"),
                    c["kind"].as_str().unwrap_or("?"),
                    c["proc"].as_str().unwrap_or("?"),
                    c["pid"],
                    c["elapsed_s"].as_f64().unwrap_or(0.0),
                    c["cpu"]
                        .as_f64()
                        .map(|v| format!(" {v:.2} cores"))
                        .unwrap_or_default(),
                    mib(c["rss_bytes"].as_u64().unwrap_or(0))
                ));
            }
            if j["compiling"].as_array().is_some_and(Vec::is_empty) {
                text.push_str("     (no compiler process at the last sample: linking, waiting or between crates)\n");
            }
        }
        out.push(json!({
            "machine": m.name, "local": m.local, "state": m.state,
            "error": m.error, "jobs": jobs,
        }));
    }
    if total == 0 && !wanted.is_empty() {
        text.push_str(&format!(
            "nobody is compiling {wanted} right now (sampled every 2 s; very short compiles can be missed)\n"
        ));
    }
    Ok(Outcome {
        text,
        structured: json!({"crate": if wanted.is_empty() { Value::Null } else { json!(wanted) },
                           "machines": out}),
    })
}

fn job_entry(j: &JobView, compiling: &[&compile::Compiling], now: i64) -> Value {
    json!({
        "id": j.id,
        "owner": j.owner,
        "agent": agent(&j.owner),
        "phase": phase_name(&j.phase),
        "checkout": j.checkout,
        "checkout_name": j.checkout_name,
        "command": j.command,
        "elapsed_s": j.started_at.map(|s| (now - s).max(0)),
        "cpu_cores": j.live.as_ref().and_then(|l| l.cpu_cores),
        "rss_bytes": j.live.as_ref().map(|l| l.rss_bytes),
        "compiling": compiling,
        "broad_test_build": j.broad_test_build,
    })
}

// ------------------------------------------------------------------------------------------

fn ids(args: &Args, key: &str) -> Result<Vec<i64>> {
    match args.get(key) {
        None => Ok(vec![]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_i64()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("{key} must be an array of job ids").into())
            })
            .collect(),
        Some(_) => Err(format!("{key} must be an array of job ids").into()),
    }
}

fn strings(args: &Args, key: &str) -> Result<Vec<String>> {
    match args.get(key) {
        None => Ok(vec![]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{key} must be an array of strings").into())
            })
            .collect(),
        Some(_) => Err(format!("{key} must be an array of strings").into()),
    }
}

fn time(args: &Args, key: &str, now: i64) -> Result<Option<i64>> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("{key} must be unix seconds or e.g. \"2h\"").into()),
        Some(Value::String(s)) => log_search::parse_time(s, now)
            .map(Some)
            .map_err(|e| format!("{key}: {e}").into()),
        Some(_) => Err(format!("{key} must be unix seconds or e.g. \"2h\"").into()),
    }
}

/// This machine's id as the sampler names it.
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_owned())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

pub fn search_logs(daemon: &Daemon, args: &Args) -> Result<Outcome> {
    let now = tinline_task_manager::execution::now();
    let phases = strings(args, "phases")?;
    let query = Query {
        pattern: args.str("pattern")?.unwrap_or_default().to_owned(),
        regex: args.bool("regex", false)?,
        ignore_case: args.bool("ignore_case", false)?,
        owner: args.str("owner")?.map(str::to_owned),
        job_ids: ids(args, "job_ids")?,
        phases,
        since: time(args, "since", now)?,
        until: time(args, "until", now)?,
        context: args.int("context", 0, log_search::MAX_CONTEXT as i64, 0)? as usize,
        max_jobs: args.int(
            "max_jobs",
            1,
            log_search::MAX_MAX_JOBS as i64,
            log_search::DEFAULT_MAX_JOBS as i64,
        )? as usize,
        max_matches: args.int(
            "max_matches",
            1,
            log_search::MAX_MAX_MATCHES as i64,
            log_search::DEFAULT_MAX_MATCHES as i64,
        )? as usize,
        max_bytes: args.int(
            "max_bytes",
            1,
            log_search::MAX_MAX_BYTES as i64,
            log_search::DEFAULT_MAX_BYTES as i64,
        )? as u64,
        max_output_bytes: args.int(
            "max_output_bytes",
            1024,
            log_search::MAX_MAX_OUTPUT_BYTES as i64,
            log_search::DEFAULT_MAX_OUTPUT_BYTES as i64,
        )? as usize,
        exclude_job_ids: vec![],
    };
    // Validate the pattern before touching any machine.
    log_search::Pattern::new(&query.pattern, query.regex, query.ignore_case)?;
    let machine = args.str("machine")?.unwrap_or("all").to_owned();
    let host = hostname();
    let state = daemon.state();
    let jobs = daemon.ok(Action::List)?.jobs;
    let machine_of = log_search::local_machine_of(&state, &host);
    // Peers that can be searched: worker/both peers this daemon dials.
    let peers: Vec<String> = if placement::peer_config_present() {
        match tinline_task_manager::peer_link::request_peers(
            &daemon.socket,
            Duration::from_secs(30),
        ) {
            Ok(r) if r.enabled => r
                .peers
                .iter()
                .filter(|p| p.addr.is_some() && matches!(p.role.as_str(), "worker" | "both"))
                .map(|p| p.name.clone())
                .collect(),
            _ => vec![],
        }
    } else {
        vec![]
    };
    let local_names = ["local".to_string(), host.clone()];
    if machine != "all" && !local_names.contains(&machine) && !peers.contains(&machine) {
        return Err(format!(
            "unknown machine {machine:?}: use \"all\", \"local\", {host:?}{}",
            peers.iter().map(|p| format!(", {p:?}")).collect::<String>()
        )
        .into());
    }
    let keep = |m: &str| match machine.as_str() {
        "all" => true,
        "local" => m == host,
        other => m == other,
    };
    let mut reports = vec![];
    let mut text = String::new();
    // This ledger (with forwarded jobs' mirrored logs named by their peer).
    let local = log_search::search(&state, &jobs, &query, &host, now, &machine_of, &keep)?;
    reports.push(json!({"machine": host, "local": true, "report": local}));
    // Peers: jobs that exist only there (the mirrored ones are skipped by id).
    let mirrored = log_search::mirrored_remote_ids(&jobs, &machine_of);
    for peer in peers.iter().filter(|p| machine == "all" || machine == **p) {
        let mut q = query.clone();
        q.exclude_job_ids = mirrored.get(peer).cloned().unwrap_or_default();
        let result = serde_json::to_value(&q)
            .map_err(|e| e.to_string())
            .and_then(|v| {
                tinline_task_manager::peer_link::request_peer_search(
                    &state,
                    peer,
                    &v,
                    Duration::from_secs(90),
                )
                .map_err(|e| e.to_string())
            });
        match result.and_then(|v| {
            serde_json::from_value::<log_search::Report>(v).map_err(|e| e.to_string())
        }) {
            Ok(mut r) => {
                // The peer names its own jobs by its hostname; this side knows it by name.
                for m in &mut r.matches {
                    if m.machine == r.machine {
                        m.machine = peer.clone();
                    }
                }
                reports.push(json!({"machine": peer, "local": false, "report": r}));
            }
            Err(e) => {
                text.push_str(&format!("{peer}: search failed: {e}\n"));
                reports.push(json!({"machine": peer, "local": false, "error": e}));
            }
        }
    }
    let mut matches = vec![];
    let mut truncated = false;
    for r in &reports {
        let Some(report) = r.get("report") else {
            continue;
        };
        let machine = r["machine"].as_str().unwrap_or("?");
        truncated |= report["truncated"] == true;
        text.push_str(&format!(
            "{machine}: {} matches in {} of {} jobs ({} bytes read){}\n",
            report["matches"].as_array().map_or(0, Vec::len),
            report["jobs_scanned"],
            report["jobs_matching_filters"],
            report["bytes_scanned"],
            report["truncated_reason"]
                .as_str()
                .map(|w| format!("; TRUNCATED by {w}: narrow owner/job_ids/since or raise it"))
                .unwrap_or_default()
        ));
        for m in report["matches"].as_array().into_iter().flatten() {
            for line in m["before"].as_array().into_iter().flatten() {
                text.push_str(&format!("    | {}\n", line.as_str().unwrap_or("")));
            }
            text.push_str(&format!(
                "  {} #{}{} {} ({}) line {}: {}\n",
                m["machine"].as_str().unwrap_or("?"),
                m["job_id"],
                m["remote_job_id"]
                    .as_i64()
                    .map(|r| format!(" (remote #{r})"))
                    .unwrap_or_default(),
                m["owner"].as_str().unwrap_or("?"),
                m["phase"].as_str().unwrap_or("?"),
                m["line_number"],
                m["line"].as_str().unwrap_or("")
            ));
            for line in m["after"].as_array().into_iter().flatten() {
                text.push_str(&format!("    | {}\n", line.as_str().unwrap_or("")));
            }
            matches.push(m.clone());
        }
    }
    Ok(Outcome {
        text,
        structured: json!({
            "pattern": query.pattern, "regex": query.regex, "machine": machine,
            "matches": matches, "truncated": truncated, "machines": reports,
        }),
    })
}

// ------------------------------------------------------------------------------------------

pub fn job_cost(daemon: &Daemon, args: &Args) -> Result<Outcome> {
    let id = args.id()?;
    let job = daemon.job(id)?;
    let state = daemon.state();
    let host = hostname();
    let (machine, remote) = log_search::local_machine_of(&state, &host)(&job);
    let forwarded = machine != host;
    let argv = tinline_task_manager::metrics::history::argv(&job);
    let mut source = "ledger".to_string();
    let metrics = job.execution.as_ref().and_then(|e| e.metrics.clone());
    // Just after a job finishes the sampler may not have merged its final metrics yet; the
    // stage timings are recorded with the completion itself.
    let mut wall = metrics
        .as_ref()
        .and_then(|m| m.wall_s)
        .or_else(|| tinline_task_manager::metrics::history::exec_seconds(&job));
    let mut cpu_user = metrics.as_ref().and_then(|m| m.cpu_user_s);
    let mut cpu_sys = metrics.as_ref().and_then(|m| m.cpu_sys_s);
    let mut peak = metrics.as_ref().and_then(|m| m.peak_rss_bytes);
    let mut warmth = metrics.as_ref().map(|m| m.warmth.as_str().to_owned());
    let mut warmth_reason = metrics.as_ref().map(|m| m.warmth_reason.clone());
    let mut crates = metrics.as_ref().and_then(|m| m.crates_compiled);
    let mut tests = metrics.as_ref().and_then(|m| m.test_targets_built);
    let mut live = Value::Null;
    // A live job: the sampler's current values.
    if !job.phase.terminal()
        && let Ok(snap) = daemon.status()
        && let Some(view) = snap.jobs.iter().find(|j| j.id == id)
    {
        source = "live".into();
        live = json!({"cpu_cores": view.live.as_ref().and_then(|l| l.cpu_cores),
                      "rss_bytes": view.live.as_ref().map(|l| l.rss_bytes),
                      "compiling": view.compiling});
        warmth = Some(view.warmth.as_str().into());
        warmth_reason = Some(view.warmth_reason.clone());
        crates = view.progress.as_ref().map(|p| p.crates_compiled).or(crates);
        tests = view
            .progress
            .as_ref()
            .and_then(|p| p.test_targets_built)
            .or(tests);
    }
    // A forwarded job: the peer did the work; prefer the peer's view of its job.
    if forwarded
        && let Some(remote) = remote
        && placement::peer_config_present()
        && let Ok(report) =
            tinline_task_manager::peer_link::request_peers(&daemon.socket, Duration::from_secs(30))
        && let Some(snap) = report
            .snapshots
            .iter()
            .find(|p| p.name == machine)
            .and_then(|p| p.snapshot.clone())
            .and_then(|v| serde_json::from_value::<Snapshot>(v).ok())
        && let Some(view) = snap.jobs.iter().find(|j| j.id == remote)
    {
        source = format!("peer {machine} snapshot (job {remote})");
        warmth = Some(view.warmth.as_str().into());
        warmth_reason = Some(view.warmth_reason.clone());
        if let Some(r) = &view.result {
            wall = r.wall_s.or(wall);
            cpu_user = r.cpu_user_s;
            cpu_sys = r.cpu_sys_s;
            peak = r.peak_rss_bytes;
            crates = r.crates_compiled.or(crates);
            tests = r.test_targets_built.or(tests);
        } else if let Some(p) = &view.progress {
            crates = Some(p.crates_compiled);
            tests = p.test_targets_built.or(tests);
        }
    }
    // Without measured counts (older rows, forwarded jobs whose peer snapshot dropped the
    // job), count the retained (mirrored) log.
    if (crates.is_none() || tests.is_none())
        && let Some(c) = tinline_task_manager::metrics::sampler::log_counts(&state, id)
    {
        crates = crates.or(Some(c.count));
        tests = tests.or(Some(c.test_targets()));
    }
    let cpu = match (cpu_user, cpu_sys) {
        (Some(u), Some(s)) => Some(u + s),
        _ => None,
    };
    let avg_cores = match (cpu, wall) {
        (Some(c), Some(w)) if w > 0.0 => Some((c / w * 100.0).round() / 100.0),
        _ => None,
    };
    let broad = tinline_task_manager::metrics::testscope::broad_flag(&argv);
    let escape = tinline_task_manager::metrics::history::recipe(&job).is_some_and(|r| r.broad_test);
    let mut hints = vec![];
    if broad == Some(true) {
        if let Some(w) = tinline_task_manager::metrics::testscope::job_warning(&job) {
            hints.push(w);
        }
        if escape {
            hints.push("submitted with --broad-test (every target built deliberately)".into());
        }
    }
    if warmth.as_deref() == Some("cold") {
        hints.push(format!(
            "cold build ({}); the next run of the same command in this checkout on {machine} should be warm",
            warmth_reason.clone().unwrap_or_default()
        ));
    }
    if let (Some(t), Some(false) | None) = (tests, broad)
        && t > 8
    {
        hints.push(format!(
            "{t} test targets were built; if you need one, name it (--lib / --test NAME)"
        ));
    }
    let hint = if hints.is_empty() {
        "nothing obviously wasteful".to_string()
    } else {
        hints.join("; ")
    };
    let fmt = |v: Option<f64>, unit: &str| v.map_or("-".to_string(), |v| format!("{v:.1}{unit}"));
    let text = format!(
        "job {id} ({}, {}) on {machine}{}: wall {}, cpu {} (avg {} cores), peak {}, {} ({}), {} crates compiled, {} test targets built{}\nhint: {hint}\n(source: {source})",
        job.spec.owner,
        phase_name(&job.phase),
        remote
            .map(|r| format!(" as remote job {r}"))
            .unwrap_or_default(),
        fmt(wall, "s"),
        fmt(cpu, "s"),
        avg_cores.map_or("-".into(), |c| format!("{c:.2}")),
        peak.map_or("-".into(), mib),
        warmth.as_deref().unwrap_or("unknown"),
        warmth_reason.as_deref().unwrap_or(""),
        crates.map_or("-".into(), |c| c.to_string()),
        tests.map_or("-".into(), |c| c.to_string()),
        match broad {
            Some(true) => ", BROAD test build",
            _ => "",
        },
    );
    Ok(Outcome {
        text,
        structured: json!({
            "id": id, "owner": job.spec.owner, "phase": phase_name(&job.phase),
            "machine": machine, "remote_job_id": remote, "source": source,
            "wall_s": wall, "cpu_s": cpu, "cpu_user_s": cpu_user, "cpu_sys_s": cpu_sys,
            "avg_cores": avg_cores, "peak_rss_bytes": peak,
            "warmth": warmth, "warmth_reason": warmth_reason,
            "crates_compiled": crates, "test_targets_built": tests,
            "broad_test_build": broad, "broad_test_escape": escape,
            "live": live, "hint": hint,
        }),
    })
}
