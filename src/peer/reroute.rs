//! The daemon side of rerouting (`crate::reroute`): a task of the peer runtime that re-scores
//! this machine's waiting jobs every `Policy::interval`, and early when a worker peer comes
//! online or the compile slot holder's ETA grows by more than `reroute::ETA_GROWTH_S`. A move
//! is one [`Store::claim_move`] (the job becomes the local mirror of a forward to the peer, same
//! job id); the worker then runs the forward through the bridge like any forwarded job, and
//! puts the original spec back if that forward submitted nothing (`reroute::after_stage`).
//!
//! Each round runs on a blocking thread with its own ledger connection and talks to this
//! daemon through its socket (status) like every other client; nothing here holds the
//! request threads' store lock.
use super::records::{OutboundLink, Paths, inbound_records, read_json};
use super::runtime::Shared;
use crate::peer_link::{PeersReport, STREAM_OFFLINE};
use crate::placement::{self, Peer};
use crate::reroute::{
    self, Action as Verb, Candidate, Claim, ENABLE_ENV, Forwarded, Policy, PullAction, PullVerdict,
    Reroute, STATE_MOVING, STATE_PULLED_BACK, TEST_TICKS_ENV, Verdict,
};
use crate::{Job, Store};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the cheap triggers (peer online, holder ETA) are looked at.
const PROBE: Duration = Duration::from_secs(5);

/// Starts the reroute task when enabled and at least one configured peer runs jobs.
pub fn start(shared: &Arc<Shared>, report: &Arc<Mutex<PeersReport>>) {
    let log = Arc::clone(&shared.log);
    let policy = match Policy::from_env() {
        Ok(p) => p,
        Err(e) => {
            log(format!("reroute: DISABLED: {e}"));
            return;
        }
    };
    if !policy.enabled {
        log(format!("reroute: disabled ({ENABLE_ENV})"));
        return;
    }
    if !shared.config.peers.iter().any(|p| p.role.runs_my_jobs()) {
        log("reroute: off (no worker/both peer configured)".into());
        return;
    }
    log(format!(
        "reroute: on: re-scoring waiting jobs every {}s; a job moves when a peer finishes it >= {}x and >= {} sooner; pull-back {}{}",
        policy.interval.as_secs_f64(),
        policy.min_ratio,
        placement::duration(policy.min_gain_s),
        if policy.pull_back { "on" } else { "off" },
        if policy.force { " [TEST FORCE]" } else { "" }
    ));
    let ticks = test_ticks_dir();
    let (shared, report) = (Arc::clone(shared), Arc::clone(report));
    tokio::spawn(async move {
        match ticks {
            Some(dir) => ticked(shared, report, policy, dir).await,
            None => periodic(shared, report, policy).await,
        }
    });
}

fn test_ticks_dir() -> Option<PathBuf> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var_os(TEST_TICKS_ENV)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// The worker/both peers as placement sees them (same rules as `placement::gather`).
pub fn peers_of(report: &PeersReport) -> Vec<Peer> {
    report
        .peers
        .iter()
        .filter(|p| matches!(p.role.as_str(), "worker" | "both"))
        .map(
            |view| match report.snapshots.iter().find(|s| s.name == view.name) {
                Some(s) => Peer::from_report(s),
                None => Peer {
                    name: view.name.clone(),
                    role: view.role.clone(),
                    state: STREAM_OFFLINE.into(),
                    error: Some("no address configured".into()),
                    stale: true,
                    ..Default::default()
                },
            },
        )
        .collect()
}

/// The runtime's report with ages and stale flags computed for now (the in-memory copy only
/// gets them when a reply is served).
fn fresh(report: &Mutex<PeersReport>) -> PeersReport {
    let mut r = report.lock().unwrap_or_else(|p| p.into_inner()).clone();
    r.refresh(task_manager_peer::now_f64());
    r
}

fn online_peers(report: &Mutex<PeersReport>) -> HashSet<String> {
    fresh(report)
        .snapshots
        .iter()
        .filter(|s| s.online() && !s.stale)
        .map(|s| s.name.clone())
        .collect()
}

async fn periodic(shared: Arc<Shared>, report: Arc<Mutex<PeersReport>>, policy: Policy) {
    let mut last_round: Option<Instant> = None;
    let mut online = online_peers(&report);
    let mut holders: HashMap<i64, (f64, Instant)> = HashMap::new();
    // Jobs whose first "stays" verdict was logged (one line per job, not one per round).
    let mut told: HashSet<i64> = HashSet::new();
    let mut last_pull: Option<Instant> = None;
    let mut told_pull: HashSet<i64> = HashSet::new();
    loop {
        tokio::time::sleep(PROBE).await;
        if policy.pull_back && last_pull.is_none_or(|t| t.elapsed() >= policy.interval) {
            last_pull = Some(Instant::now());
            pull_back_round(&shared, &report, &policy, &mut told_pull).await;
        }
        let now_online = online_peers(&report);
        let came_online: Vec<String> = now_online.difference(&online).cloned().collect();
        online = now_online;
        let due = last_round.is_none_or(|t| t.elapsed() >= policy.interval);
        let (s, r, p) = (Arc::clone(&shared), Arc::clone(&report), policy.clone());
        let mut seen = std::mem::take(&mut holders);
        let ran = tokio::task::spawn_blocking(move || {
            let outcome = round(&s, &r, &p, |snapshot| {
                let grew = holder_eta_grew(snapshot, &mut seen);
                (due || !came_online.is_empty() || grew.is_some()).then(|| {
                    match (came_online.first(), grew) {
                        (Some(peer), _) => format!("peer {peer} came online"),
                        (None, Some(why)) => why,
                        (None, None) => "periodic".into(),
                    }
                })
            });
            (outcome, seen)
        })
        .await;
        let Ok((outcome, seen)) = ran else { continue };
        holders = seen;
        match outcome {
            Ok(Some(o)) => {
                last_round = Some(Instant::now());
                for line in o.log_lines() {
                    (shared.log)(line);
                }
                for v in &o.verdicts {
                    if v.moves().is_none() && told.insert(v.job) {
                        (shared.log)(format!("reroute: job {} {} ({})", v.job, v.line, o.trigger));
                    }
                }
            }
            Ok(None) => {}
            Err(e) => (shared.log)(format!("reroute: round failed: {e}")),
        }
    }
}

/// Test mode: one round per new file in `dir`, answered by `NAME.done` (the round as JSON).
async fn ticked(
    shared: Arc<Shared>,
    report: Arc<Mutex<PeersReport>>,
    policy: Policy,
    dir: PathBuf,
) {
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| !n.ends_with(".done") && !n.ends_with(".tmp"))
            .filter(|n| !dir.join(format!("{n}.done")).exists())
            .collect();
        names.sort();
        for name in names {
            let (s, r, p) = (Arc::clone(&shared), Arc::clone(&report), policy.clone());
            let outcome = tokio::task::spawn_blocking(move || {
                round(&s, &r, &p, |_| Some("test tick".into()))
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));
            let body = match &outcome {
                Ok(Some(o)) => {
                    for line in o.log_lines() {
                        (shared.log)(line);
                    }
                    serde_json::to_value(o).unwrap_or_default()
                }
                Ok(None) => serde_json::json!({"skipped": true}),
                Err(e) => serde_json::json!({"error": e}),
            };
            if policy.pull_back {
                pull_back_round(&shared, &report, &policy, &mut HashSet::new()).await;
            }
            let tmp = dir.join(format!("{name}.tmp"));
            if std::fs::write(&tmp, serde_json::to_vec_pretty(&body).unwrap_or_default()).is_ok() {
                let _ = std::fs::rename(&tmp, dir.join(format!("{name}.done")));
            }
        }
    }
}

/// A compile slot holder whose remaining ETA is now well above what it was (minus the time
/// that passed) means the jobs behind it wait longer than last scored. Updates `seen`.
fn holder_eta_grew(
    snapshot: &crate::metrics::Snapshot,
    seen: &mut HashMap<i64, (f64, Instant)>,
) -> Option<String> {
    let mut grew = None;
    let mut next = HashMap::new();
    for job in snapshot.jobs.iter().filter(|j| {
        j.resource == crate::metrics::SlotKind::Compile && j.phase == crate::Phase::Building
    }) {
        let Some(remaining) = job.estimate.as_ref().map(|e| e.remaining_s) else {
            continue;
        };
        if let Some((before, at)) = seen.get(&job.id) {
            let expected = before - at.elapsed().as_secs_f64();
            if remaining - expected > reroute::ETA_GROWTH_S && grew.is_none() {
                grew = Some(format!(
                    "slot holder #{} ETA grew to ~{}",
                    job.id,
                    placement::duration(remaining)
                ));
            }
        }
        next.insert(job.id, (remaining, Instant::now()));
    }
    *seen = next;
    grew
}

#[derive(Debug, Default, Serialize)]
pub struct Moved {
    pub job: i64,
    pub peer: String,
    pub line: String,
    /// `claimed`, or why the claim was refused / failed.
    pub claim: String,
}

/// What one round saw and did.
#[derive(Debug, Default, Serialize)]
pub struct Round {
    pub trigger: String,
    pub verdicts: Vec<Verdict>,
    pub moves: Vec<Moved>,
}

impl Round {
    fn log_lines(&self) -> Vec<String> {
        self.moves
            .iter()
            .map(|m| {
                if m.claim == "claimed" {
                    format!("reroute: job {}: {}", m.job, m.line)
                } else {
                    format!(
                        "reroute: job {} stays: the move to {} was not claimed ({})",
                        m.job, m.peer, m.claim
                    )
                }
            })
            .collect()
    }
}

/// The local candidates (pins and inbound peer jobs resolved), with their ledger jobs.
fn candidates(state: &Path, store: &Store) -> crate::Result<Vec<(Candidate, Job)>> {
    let inbound: HashSet<i64> = inbound_records(&Paths::new(state))
        .into_iter()
        .filter_map(|r| r.remote_id)
        .collect();
    let mut out = vec![];
    for job in store.waiting_jobs()? {
        if !job.execution.as_ref().is_some_and(|e| e.local_command) {
            continue;
        }
        let pinned = if inbound.contains(&job.id) {
            Some("submitted by a peer: it runs where its submitter placed it".into())
        } else {
            reroute::pinned(state, job.id)
        };
        let admitted = reroute::admitted_here(state, job.id);
        if let Some(c) = Candidate::from_job(&job, pinned, admitted) {
            out.push((c, job));
        }
    }
    Ok(out)
}

/// One re-scoring. `go` sees the local snapshot and says why to score now (None: not now).
/// Returns None when nothing was scored (no eligible job, or `go` declined).
fn round(
    shared: &Shared,
    report: &Mutex<PeersReport>,
    policy: &Policy,
    go: impl FnOnce(&crate::metrics::Snapshot) -> Option<String>,
) -> crate::Result<Option<Round>> {
    let state = &shared.paths.state;
    let mut store = Store::open(&state.join("tasks.sqlite"))?;
    let found = candidates(state, &store)?;
    let eligible = found
        .iter()
        .filter(|(c, _)| reroute::ineligible(c).is_none())
        .count();
    if eligible == 0 && !policy.force {
        return Ok(None);
    }
    let local = crate::request_status(&shared.socket, Duration::from_secs(30))?
        .status
        .ok_or("daemon returned no status")?;
    let Some(trigger) = go(&local) else {
        return Ok(None);
    };
    let peers = peers_of(&fresh(report));
    let local_jobs =
        crate::admission::submission_max_cargo_jobs().unwrap_or(placement::DEFAULT_JOBS);
    let list: Vec<Candidate> = found.iter().map(|(c, _)| c.clone()).collect();
    // Missing-capability marks (health): a peer that lacks what a job needs is skipped.
    let caps = reroute::capabilities(state, &peers);
    let verdicts = reroute::decide(&list, &local, local_jobs, &peers, policy, caps.as_ref());
    let canonical = std::fs::canonicalize(state)?;
    let exe = std::env::current_exe()?;
    let mut moves = vec![];
    for v in &verdicts {
        let Verb::Move { peer, argv, .. } = &v.action else {
            continue;
        };
        let Some((_, job)) = found.iter().find(|(c, _)| c.id == v.job) else {
            continue;
        };
        let claim = move_one(
            &mut store,
            state,
            &canonical,
            &exe,
            &local.machine.id,
            job,
            v,
            peer,
            argv,
        );
        moves.push(Moved {
            job: v.job,
            peer: peer.clone(),
            line: v.line.clone(),
            claim: match claim {
                Ok(Claim::Claimed) => "claimed".into(),
                Ok(Claim::Refused(why)) => why,
                Err(e) => format!("error: {e}"),
            },
        });
    }
    Ok(Some(Round {
        trigger,
        verdicts,
        moves,
    }))
}

#[allow(clippy::too_many_arguments)]
fn move_one(
    store: &mut Store,
    state: &Path,
    canonical: &Path,
    exe: &Path,
    here: &str,
    job: &Job,
    verdict: &Verdict,
    peer: &str,
    argv: &[String],
) -> crate::Result<Claim> {
    let moved = reroute::moved_spec(&job.spec, argv, peer, canonical, exe)?;
    let there = verdict
        .peers
        .iter()
        .find(|s| s.peer == peer)
        .map(|s| s.total_s);
    let record = Reroute {
        state: STATE_MOVING.into(),
        moved_from: here.to_owned(),
        moved_to: peer.to_owned(),
        reason: verdict.line.clone(),
        at: crate::execution::now(),
        moves: 0,
        local_s: Some(verdict.local_wait_s + verdict.local_run_s),
        peer_s: there,
        remote_job_id: None,
        note: None,
        returned_at: None,
        log_pending: true,
        original_spec: None,
    };
    let claim = store.claim_move(state, job.id, &job.spec, &moved, &record)?;
    if claim == Claim::Claimed {
        // Into the job log now if its directory exists, else when the worker creates it.
        reroute::flush_pending_line(state, store, job.id);
    }
    Ok(claim)
}

// ---- pull-back ----------------------------------------------------------------------------------

/// The running forwards of this machine (bridges of `tasks peer-forward`), with what the link
/// and placement records say about them.
fn forwards(state: &Path, store: &Store) -> crate::Result<Vec<(Forwarded, Job)>> {
    let mut out = vec![];
    for job in store.building_jobs()? {
        if !job.execution.as_ref().is_some_and(|e| e.local_command) {
            continue;
        }
        let link: Option<OutboundLink> = read_json(&Paths::new(state).link(job.id));
        let remote = link.map_or((None, None, None), |l| {
            (l.remote_job_id, l.remote_phase, l.remote_state)
        });
        let requested = placement::recorded(state, job.id)
            .and_then(|r| r["requested"].as_str().map(str::to_owned));
        let pinned = reroute::pinned(state, job.id);
        if let Some(f) = Forwarded::from_job(&job, pinned, requested, remote) {
            out.push((f, job));
        }
    }
    Ok(out)
}

/// One pull-back pass: decides on a blocking thread, then for a pull cancels the peer's copy
/// over the peer link and only on a CONFIRMED cancel restores the local spec. Any doubt leaves
/// the job as it is (the bridge keeps following the peer's copy): never run twice.
async fn pull_back_round(
    shared: &Arc<Shared>,
    report: &Arc<Mutex<PeersReport>>,
    policy: &Policy,
    told: &mut HashSet<i64>,
) {
    let (s, r, p) = (Arc::clone(shared), Arc::clone(report), policy.clone());
    let decided = tokio::task::spawn_blocking(move || pull_back_decide(&s, &r, &p)).await;
    let (verdicts, jobs, here) = match decided {
        Ok(Ok(Some(d))) => d,
        Ok(Ok(None)) => return,
        Ok(Err(e)) => return (shared.log)(format!("reroute: pull-back round failed: {e}")),
        Err(_) => return,
    };
    for v in &verdicts {
        let PullAction::Pull {
            peer,
            argv,
            blocker,
        } = &v.action
        else {
            if told.insert(v.job) {
                (shared.log)(format!("reroute: forwarded job {} {}", v.job, v.line));
            }
            continue;
        };
        let Some(job) = jobs.iter().find(|j| j.id == v.job).cloned() else {
            continue;
        };
        let state = shared.paths.state.clone();
        // Before the cancel: the worker must wait for the spec change when the bridge ends.
        let marker = reroute::pull_back_marker(&state, job.id);
        if std::fs::write(&marker, b"").is_err() {
            continue;
        }
        let cancelled = super::forward::propagate_cancel(shared, job.id).await;
        let link = read_json::<OutboundLink>(&shared.paths.link(job.id));
        if !cancel_confirmed(&cancelled, link.as_ref()) {
            let _ = std::fs::remove_file(&marker);
            (shared.log)(format!(
                "reroute: job {} stays on {peer}: the cancel of its copy is not confirmed ({}); nothing restored",
                job.id,
                cancelled
                    .err()
                    .unwrap_or_else(|| "no terminal phase".into())
            ));
            continue;
        }
        let (line, argv, peer, blocker, reason, id, here) = (
            format!(
                "tasks: pulled back from {peer}: {blocker}; running here (compile slot free)\n"
            ),
            argv.clone(),
            peer.clone(),
            blocker.clone(),
            v.line.clone(),
            job.id,
            here.clone(),
        );
        let (here_s, there_s) = (v.here_s, v.there_s);
        let claimed = tokio::task::spawn_blocking(move || -> crate::Result<Claim> {
            let mut store = Store::open(&state.join("tasks.sqlite"))?;
            let pulled = reroute::pulled_spec(&job.spec, &argv)?;
            let record = Reroute {
                state: STATE_PULLED_BACK.into(),
                moved_from: peer,
                moved_to: here,
                reason,
                at: crate::execution::now(),
                moves: 0,
                local_s: Some(here_s),
                peer_s: Some(there_s),
                remote_job_id: None,
                note: Some(blocker),
                returned_at: None,
                log_pending: false,
                original_spec: None,
            };
            let claim = store.claim_pull_back(id, &job.spec, &pulled, &record)?;
            if claim == Claim::Claimed {
                let _ = reroute::append_job_log(&state, id, &line);
            }
            let _ = std::fs::remove_file(reroute::pull_back_marker(&state, id));
            Ok(claim)
        })
        .await;
        (shared.log)(match claimed {
            Ok(Ok(Claim::Claimed)) => format!("reroute: job {id}: {}", v.line),
            Ok(Ok(Claim::Refused(why))) => format!(
                "reroute: job {id}: its copy on the peer was cancelled but the pull-back was refused ({why})"
            ),
            Ok(Err(e)) => format!("reroute: job {id}: pull-back failed after the cancel: {e}"),
            Err(e) => format!("reroute: job {id}: pull-back failed after the cancel: {e}"),
        });
    }
}

/// Whether the peer's copy is certainly cancelled: the cancel call succeeded AND the link
/// record shows the remote job terminal as `cancelled` (not passed/failed, not still live).
/// Anything else is uncertain and restores nothing.
fn cancel_confirmed(call: &Result<(), String>, link: Option<&OutboundLink>) -> bool {
    call.is_ok()
        && link.is_some_and(|l| {
            l.remote_state.as_deref() == Some("terminal")
                && l.remote_phase.as_deref() == Some("cancelled")
        })
}

type Decided = (Vec<PullVerdict>, Vec<Job>, String);

fn pull_back_decide(
    shared: &Shared,
    report: &Mutex<PeersReport>,
    policy: &Policy,
) -> crate::Result<Option<Decided>> {
    let state = &shared.paths.state;
    let store = Store::open(&state.join("tasks.sqlite"))?;
    let found = forwards(state, &store)?;
    if !found
        .iter()
        .any(|(f, _)| reroute::pull_back_ineligible(f).is_none())
    {
        return Ok(None);
    }
    let local = crate::request_status(&shared.socket, Duration::from_secs(30))?
        .status
        .ok_or("daemon returned no status")?;
    let peers = peers_of(&fresh(report));
    let local_jobs =
        crate::admission::submission_max_cargo_jobs().unwrap_or(placement::DEFAULT_JOBS);
    let list: Vec<Forwarded> = found.iter().map(|(f, _)| f.clone()).collect();
    let verdicts = reroute::decide_pull_back(&list, &local, local_jobs, &peers, policy);
    let jobs = found.into_iter().map(|(_, j)| j).collect();
    Ok(Some((verdicts, jobs, local.machine.id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(state: &str, phase: &str) -> OutboundLink {
        OutboundLink {
            remote_state: Some(state.into()),
            remote_phase: Some(phase.into()),
            ..Default::default()
        }
    }

    #[test]
    fn only_a_confirmed_cancel_lets_the_job_come_back() {
        let ok = Ok(());
        assert!(cancel_confirmed(&ok, Some(&link("terminal", "cancelled"))));
        // Uncertain: the call failed, the record is missing, or the copy is not cancelled.
        let err = Err("cancel of job 77 on nixos: timed out".to_string());
        assert!(!cancel_confirmed(
            &err,
            Some(&link("terminal", "cancelled"))
        ));
        assert!(!cancel_confirmed(&ok, None));
        assert!(!cancel_confirmed(
            &ok,
            Some(&link("attached", "waiting_build"))
        ));
        assert!(!cancel_confirmed(
            &ok,
            Some(&link("uncertain", "cancelled"))
        ));
        // It finished on the peer meanwhile: the bridge delivers that result.
        assert!(!cancel_confirmed(&ok, Some(&link("terminal", "passed"))));
    }
}
