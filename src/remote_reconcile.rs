//! After a controller restart (or a lost worker), an offload job that was live here ends
//! `interrupted`, while its remote half may have kept running and finished on the worker.
//! This module links the two and records what the worker says, without re-running anything:
//!
//! * [`interrupted_note`]: the remote job id and worker, appended to the interrupted detail
//!   from what the client recorded (the `tasks-offload` sidecar, or the peer link record).
//! * [`start`] (daemon thread, ssh offloads): for recently interrupted `tasks-offload` jobs,
//!   asks the job's own client, read-only, `tasks-offload --reconcile-local-job ID` (same
//!   worker/key/owner arguments the job was approved with) for the remote job's phase, exit
//!   code and retained log, then records it as `execution.remote_result` (JSON merge; the local
//!   phase stays `interrupted`) and appends the remote log to the local job log.
//!   Peer forwards (`tasks peer-forward`) are reconciled by the peer runtime instead.
//!
//! Never submits, cancels or re-runs work, never changes a job's phase.
use crate::execution::{RemoteResult, now};
use crate::local_command::{Recipe, Resource};
use crate::{Job, Phase, Result, Store};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Interrupted jobs older than this are settled as `unknown` instead of being asked about.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// How often one job's remote half is asked about while it is still running or unreachable.
pub const RETRY: Duration = Duration::from_secs(120);
const ROUND: Duration = Duration::from_secs(30);
const FIRST_ROUND: Duration = Duration::from_secs(5);
/// Bound on one reconcile client run (it has its own shorter ssh timeout).
const CLIENT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CLIENT_STDOUT: u64 = 256 * 1024;
const PER_ROUND: usize = 4;
const SIDECAR_MAX: u64 = 64 * 1024;

/// The client name that has a reconcile mode.
const RECONCILE_CLIENT: &str = "tasks-offload";

/// `$HOME/.local/state/tasks-offload/jobs/<id>.json`: where a `tasks-offload` run under the
/// worker writes its sidecar (the worker passes only `HOME` among the XDG variables).
pub fn sidecar_path(id: i64) -> Option<PathBuf> {
    Some(sidecar_dir()?.join(format!("{id}.json")))
}

pub fn sidecar_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/state/tasks-offload/jobs"))
}

/// The sidecar fields this module needs (cluster-snapshot.md, version 1).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sidecar {
    pub stage: String,
    pub worker: Option<String>,
    pub remote_job_id: Option<i64>,
    pub remote_phase: Option<String>,
    pub exit: Option<i32>,
}

pub fn read_sidecar(path: &Path, id: i64) -> Option<Sidecar> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(SIDECAR_MAX)
        .read_to_end(&mut bytes)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if v["version"].as_i64() != Some(1) || v["local_job_id"].as_i64() != Some(id) {
        return None;
    }
    Some(Sidecar {
        stage: v["stage"].as_str().unwrap_or("").to_owned(),
        worker: v["worker"].as_str().map(str::to_owned),
        remote_job_id: v["remote_job_id"].as_i64().filter(|n| *n > 0),
        remote_phase: v["remote_phase"].as_str().map(str::to_owned),
        exit: v["exit"].as_i64().and_then(|n| i32::try_from(n).ok()),
    })
}

fn recipe(job: &Job) -> Option<Recipe> {
    (job.spec.suite == crate::local_command::SUITE)
        .then(|| Recipe::from_spec_unvalidated(&job.spec).ok())
        .flatten()
        .filter(|r| r.resource == Resource::Offload)
}

/// `--worker VALUE` from an offload client argv.
fn flag(argv: &[String], name: &str) -> Option<String> {
    let end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    argv[..end]
        .iter()
        .position(|a| a == name)
        .and_then(|i| argv.get(i + 1))
        .cloned()
}

fn client_index(argv: &[String]) -> Option<usize> {
    argv.iter().position(|a| {
        Path::new(a)
            .file_name()
            .is_some_and(|n| n == RECONCILE_CLIENT)
    })
}

/// What an interrupted offload/forward job's detail adds: the remote job and where to look.
pub fn interrupted_note(job: &Job) -> Option<String> {
    let recipe = recipe(job)?;
    if let Some(Ok(args)) = crate::peer_link::ForwardArgs::from_recipe_argv(&recipe.argv) {
        let link: Option<serde_json::Value> = std::fs::read(
            args.state
                .join("peer/links")
                .join(format!("{}.json", job.id)),
        )
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
        let remote = link.as_ref().and_then(|l| l["remote_job_id"].as_i64());
        return Some(match remote {
            Some(r) => format!(
                " Forwarded to peer {} as its job {r}; the peer runtime records that job's result here (remote_result) when it can reach the peer.",
                args.peer
            ),
            None => format!(
                " Forwarded to peer {}; no remote job id was recorded (it may not have been submitted).",
                args.peer
            ),
        });
    }
    let worker = flag(&recipe.argv, "--worker").unwrap_or_else(|| "the worker".into());
    let sidecar = sidecar_path(job.id).and_then(|p| read_sidecar(&p, job.id));
    Some(
        match sidecar.and_then(|s| s.remote_job_id.map(|r| (r, s.stage))) {
            Some((remote, stage)) => format!(
                " Offloaded to {worker} as its job {remote} (client last at stage {stage}); this daemon asks the worker for that job's result and records it here (remote_result, appended log). Check `tasks logs {}` before resubmitting.",
                job.id
            ),
            None => format!(
                " Offloaded to {worker}; no remote job id was recorded by the client, a remote job may still exist: check `tasks list` on {worker} before resubmitting."
            ),
        },
    )
}

/// Candidate for the ssh reconciler: interrupted `tasks-offload` job, not a peer forward,
/// without a settled `remote_result`.
fn candidate(job: &Job) -> Option<Recipe> {
    if job.phase != Phase::Interrupted {
        return None;
    }
    let recipe = recipe(job)?;
    if crate::peer_link::is_forward_argv(&recipe.argv) {
        return None;
    }
    let settled = job
        .execution
        .as_ref()
        .and_then(|e| e.remote_result.as_ref())
        .is_some_and(RemoteResult::settled);
    (!settled).then_some(recipe)
}

/// Output of one `tasks-offload --reconcile-local-job` run: the log text it printed, and the
/// parsed `{"reconcile": {...}}` last line.
#[derive(Debug, Default)]
pub struct ClientReport {
    pub log: String,
    pub remote_job_id: Option<i64>,
    pub remote_phase: Option<String>,
    pub exit_code: Option<i32>,
    pub state: String,
    pub detail: String,
}

/// Parses the client's stdout: everything before the last line is remote log text.
pub fn parse_client_output(stdout: &str) -> Option<ClientReport> {
    let trimmed = stdout.trim_end_matches('\n');
    let (log, last) = match trimmed.rfind('\n') {
        Some(i) => (&trimmed[..=i], &trimmed[i + 1..]),
        None => ("", trimmed),
    };
    let v: serde_json::Value = serde_json::from_str(last).ok()?;
    let r = v.get("reconcile")?;
    Some(ClientReport {
        log: log.to_owned(),
        remote_job_id: r["remote_job_id"].as_i64(),
        remote_phase: r["remote_phase"].as_str().map(str::to_owned),
        exit_code: r["exit_code"].as_i64().and_then(|n| i32::try_from(n).ok()),
        state: r["state"].as_str().unwrap_or("unknown").to_owned(),
        detail: r["detail"].as_str().unwrap_or("").to_owned(),
    })
}

/// Runs the job's own client in reconcile mode (read-only on the worker). `Err` is a local
/// failure (spawn, timeout, no JSON), with the client's stderr tail.
fn run_client(argv: &[String], id: i64) -> std::result::Result<ClientReport, String> {
    let i = client_index(argv).ok_or("no tasks-offload client in argv")?;
    let mut full: Vec<String> = argv[..=i].to_vec();
    full.push("--reconcile-local-job".into());
    full.push(id.to_string());
    full.extend(argv[i + 1..].iter().cloned());
    let mut cmd = Command::new(&full[0]);
    cmd.args(&full[1..]).current_dir("/").env_clear();
    for name in ["HOME", "PATH", "LANG", "XDG_RUNTIME_DIR"] {
        if let Some(v) = std::env::var_os(name) {
            cmd.env(name, v);
        }
    }
    use std::os::unix::process::CommandExt;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", full[0]))?;
    let out = child.stdout.take().ok_or("no stdout")?;
    let err = child.stderr.take().ok_or("no stderr")?;
    let reader = |mut r: Box<dyn Read + Send>, max: u64| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut r).take(max).read_to_end(&mut buf);
            let _ = std::io::copy(&mut r, &mut std::io::sink());
            buf
        })
    };
    let out = reader(Box::new(out), MAX_CLIENT_STDOUT);
    let err = reader(Box::new(err), 64 * 1024);
    let deadline = Instant::now() + CLIENT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            _ => {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                break None;
            }
        }
    };
    let stdout = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    let tail = |s: &str| {
        let s = s.trim();
        let mut start = s.len().saturating_sub(300);
        while !s.is_char_boundary(start) {
            start += 1;
        }
        s[start..].to_owned()
    };
    let Some(status) = status else {
        return Err(format!(
            "reconcile client timed out after {}s",
            CLIENT_TIMEOUT.as_secs()
        ));
    };
    match parse_client_output(&stdout) {
        Some(report) => Ok(report),
        None if stderr.contains("unrecognized arguments") => Err(format!(
            "UNSUPPORTED: this tasks-offload client has no --reconcile-local-job (older release): {}",
            tail(&stderr)
        )),
        None => Err(format!(
            "reconcile client exited {status} without a result: {}",
            tail(&stderr)
        )),
    }
}

fn result(worker: &str, remote: Option<i64>, state: &str, detail: String) -> RemoteResult {
    RemoteResult {
        transport: "ssh".into(),
        worker: worker.into(),
        remote_job_id: remote,
        remote_phase: None,
        exit_code: None,
        state: state.into(),
        checked_at: now(),
        detail,
    }
}

/// Decides one job's record. `ask` runs the reconcile client (injected for tests).
pub fn reconcile_job(
    job: &Job,
    sidecar: Option<Sidecar>,
    age: Duration,
    ask: impl FnOnce(&[String]) -> std::result::Result<ClientReport, String>,
) -> Option<(RemoteResult, Option<String>)> {
    let recipe = candidate(job)?;
    let worker = flag(&recipe.argv, "--worker")
        .or_else(|| sidecar.as_ref().and_then(|s| s.worker.clone()))
        .unwrap_or_else(|| "?".into());
    let owner = format!(
        "{}@<this host>",
        flag(&recipe.argv, "--who").unwrap_or_default()
    );
    let Some(sidecar) = sidecar else {
        return Some((
            result(
                &worker,
                None,
                "unknown",
                format!(
                    "no tasks-offload sidecar for this job (older client or no TASKS_JOB_ID); look on {worker} for owner {owner}"
                ),
            ),
            None,
        ));
    };
    let Some(remote) = sidecar.remote_job_id else {
        return Some((
            result(
                &worker,
                None,
                "unknown",
                format!(
                    "the client had not seen a remote job id (stage {}); a remote job may still exist: check tasks list on {worker} for owner {owner}",
                    sidecar.stage
                ),
            ),
            None,
        ));
    };
    if sidecar.stage == "done" && sidecar.remote_phase.is_some() {
        // The client finished and recorded the result; only the local completion was lost.
        let phase = sidecar.remote_phase.clone();
        let exit = sidecar.exit.filter(|_| phase.is_some());
        let mut r = result(
            &worker,
            Some(remote),
            "terminal",
            "taken from the client's final sidecar record (it finished; the local completion was lost)".into(),
        );
        r.exit_code = if phase.as_deref() == Some("passed") {
            Some(0)
        } else {
            exit.filter(|c| *c != 0)
        };
        r.remote_phase = phase;
        return Some((r, None));
    }
    if age > MAX_AGE {
        return Some((
            result(
                &worker,
                Some(remote),
                "unknown",
                format!(
                    "not reconciled within {}h; check it by hand: ssh {worker} '~/.local/bin/tasks logs {remote}'",
                    MAX_AGE.as_secs() / 3600
                ),
            ),
            None,
        ));
    }
    match ask(&recipe.argv) {
        Ok(report) => {
            let state = match report.state.as_str() {
                "terminal" | "running" | "unknown" => report.state.clone(),
                _ => "unreachable".into(),
            };
            let mut r = result(&worker, Some(remote), &state, report.detail);
            r.remote_phase = report.remote_phase;
            r.exit_code = if r.remote_phase.as_deref() == Some("passed") {
                Some(0)
            } else {
                report.exit_code.filter(|c| *c != 0)
            };
            let log = (state == "terminal" && !report.log.is_empty()).then_some(report.log);
            Some((r, log))
        }
        Err(e) if e.starts_with("UNSUPPORTED") => Some((
            result(
                &worker,
                Some(remote),
                "unknown",
                format!("{e}; check it by hand: ssh {worker} '~/.local/bin/tasks logs {remote}'"),
            ),
            None,
        )),
        Err(e) => Some((result(&worker, Some(remote), "unreachable", e), None)),
    }
}

fn record(state: &Path, job: &Job, r: &RemoteResult, log: Option<String>) -> Result<()> {
    let mut store = Store::open(&state.join("tasks.sqlite"))?;
    if !store.write_remote_result(job.id, r)? {
        return Ok(());
    }
    if r.settled() || log.is_some() {
        let mut text = format!(
            "\n=== tasks: controller-side reconciliation after this job was interrupted: {} [{}]; this job stays interrupted, nothing was re-run ===\n",
            r.summary(),
            r.state
        );
        if !r.detail.is_empty() {
            text.push_str(&format!("tasks: {}\n", r.detail));
        }
        if let Some(log) = log {
            text.push_str(&format!(
                "--- retained log of remote job {} on {} (as the worker returns it) ---\n{log}",
                r.remote_job_id.unwrap_or(0),
                r.worker
            ));
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("--- end of remote log ---\n");
        }
        crate::worker::append_job_log(state, job.id, text.as_bytes());
    }
    Ok(())
}

/// One reconciler round over the ledger. `attempts` rate-limits per job.
pub fn round(state: &Path, attempts: &mut HashMap<i64, Instant>) -> Result<usize> {
    match sidecar_dir() {
        Some(dir) => round_in(state, &dir, attempts),
        None => Ok(0),
    }
}

/// [`round`] with the sidecar directory given (tests).
pub fn round_in(
    state: &Path,
    sidecars: &Path,
    attempts: &mut HashMap<i64, Instant>,
) -> Result<usize> {
    let jobs = Store::open(&state.join("tasks.sqlite"))?.jobs()?;
    let mut done = 0;
    for job in jobs.iter().rev() {
        if done >= PER_ROUND {
            break;
        }
        if candidate(job).is_none() {
            continue;
        }
        let finished = job
            .execution
            .as_ref()
            .and_then(|e| e.finished_at)
            .unwrap_or(job.submitted_at);
        let age = Duration::from_secs(now().saturating_sub(finished).max(0) as u64);
        // Old history is left alone (no burst of records on a first start); only a job this
        // reconciler already started on is settled once it is past MAX_AGE.
        let started = job
            .execution
            .as_ref()
            .is_some_and(|e| e.remote_result.is_some());
        if age > MAX_AGE && !started {
            continue;
        }
        if attempts.get(&job.id).is_some_and(|t| t.elapsed() < RETRY) {
            continue;
        }
        attempts.insert(job.id, Instant::now());
        let sidecar = read_sidecar(&sidecars.join(format!("{}.json", job.id)), job.id);
        if let Some((r, log)) = reconcile_job(job, sidecar, age, |argv| run_client(argv, job.id)) {
            if let Err(e) = record(state, job, &r, log) {
                eprintln!("reconcile: job {}: could not record: {e}", job.id);
            } else {
                eprintln!("reconcile: job {}: {} [{}]", job.id, r.summary(), r.state);
            }
            done += 1;
        }
    }
    Ok(done)
}

/// The daemon's reconciler thread (own ledger connections, never holds the daemon's store).
pub fn start(state: PathBuf) {
    let _ = std::thread::Builder::new()
        .name("reconcile".into())
        .spawn(move || {
            let mut attempts = HashMap::new();
            std::thread::sleep(FIRST_ROUND);
            loop {
                if let Err(e) = round(&state, &mut attempts) {
                    eprintln!("reconcile: round failed: {e}");
                }
                std::thread::sleep(ROUND);
            }
        });
}

#[cfg(test)]
#[path = "remote_reconcile_tests.rs"]
mod tests;
