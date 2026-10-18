//! Receiving side of forwarded jobs: a peer's `submit` becomes an ordinary submission on this
//! daemon's own unix socket (same validation as `tasks enqueue`, this process's device denylist
//! and Cargo `-j` cap), approved and started only if the peer is configured `approve = "auto"`
//! (the equivalent of the ssh `tasks run` the offload client used to send). Nothing here
//! executes anything: the worker does, through admission, exactly as for a local job.
use super::records::{InboundRecord, Paths, read_json, write_json};
use crate::local_command::{Recipe, Resource};
use crate::{Action, ClientTimeouts, Job, Phase, Response, request_with_reconnect_timeouts};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use task_manager_peer::config::Approve;
use task_manager_peer::jobs::{JobHost, JobState, RemoteResource, SubmitRequest, Submitted};
use task_manager_peer::{PeerEntry, source};

pub struct DaemonJobHost {
    pub paths: Paths,
    pub socket: PathBuf,
    pub checkouts: PathBuf,
    submit_lock: Mutex<()>,
}

/// The daemon calls itself: no auto-start, generous timeouts (the store mutex may be busy).
pub fn ask(socket: &std::path::Path, action: Action) -> Result<Response, String> {
    request_with_reconnect_timeouts(
        socket,
        action,
        || Err("daemon socket is not answering".into()),
        ClientTimeouts {
            reply: Duration::from_secs(60),
            total: Duration::from_secs(120),
        },
    )
    .map_err(|e| e.to_string())
}

pub fn job(socket: &std::path::Path, id: i64) -> Result<Job, String> {
    let r = ask(socket, Action::Job(id))?;
    if let Some(e) = r.error {
        return Err(e);
    }
    r.jobs
        .into_iter()
        .find(|j| j.id == id)
        .ok_or_else(|| format!("job {id} not found"))
}

pub fn phase_name(phase: &Phase) -> String {
    serde_json::to_value(phase)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{phase:?}"))
}

/// The exit code a finished job reports (`Phase::reported_exit_code`; 0 only if passed),
/// else 128+signal of its last stage. `None` while live.
pub fn exit_code(job: &Job) -> Option<i32> {
    let stages = job
        .execution
        .as_ref()
        .map_or(&[][..], |e| e.stages.as_slice());
    job.phase.reported_exit_code(stages).or_else(|| {
        job.phase
            .terminal()
            .then(|| stages.last().and_then(|s| s.signal.map(|s| 128 + s)))
            .flatten()
    })
}

pub fn job_state(job: &Job) -> JobState {
    JobState {
        phase: phase_name(&job.phase),
        terminal: job.phase.terminal(),
        exit_code: exit_code(job),
        detail: job
            .execution
            .as_ref()
            .map(|e| e.detail.clone())
            .unwrap_or_default(),
    }
}

impl DaemonJobHost {
    pub fn new(paths: Paths, socket: PathBuf, checkouts: PathBuf) -> Self {
        Self {
            paths,
            socket,
            checkouts,
            submit_lock: Mutex::new(()),
        }
    }

    fn record_of(&self, peer: &PeerEntry, id: i64) -> Result<InboundRecord, String> {
        let dir = self.paths.inbound(&peer.endpoint.to_string());
        let entries = std::fs::read_dir(&dir).map_err(|_| format!("UnknownJob({id})"))?;
        entries
            .flatten()
            .filter_map(|e| read_json::<InboundRecord>(&e.path()))
            .find(|r| r.remote_id == Some(id))
            .ok_or_else(|| format!("UnknownJob({id}): not submitted by this peer"))
    }

    /// `checkouts/<checkout>/<sub>`, created like `mkdir -p`, inside the checkout, and only for
    /// a checkout this peer synced.
    fn cwd(&self, peer: &PeerEntry, request: &SubmitRequest) -> Result<PathBuf, String> {
        source::check_checkout_name(&request.checkout).map_err(|e| e.to_string())?;
        let identity: serde_json::Value =
            read_json(&source::identity_path(&self.checkouts, &request.checkout))
                .ok_or_else(|| format!("UnknownCheckout({}): sync it first", request.checkout))?;
        if identity.get("peer").and_then(|p| p.as_str()) != Some(&peer.endpoint.to_string()) {
            return Err(format!("CheckoutOwnedByAnotherPeer({})", request.checkout));
        }
        let top = std::fs::canonicalize(self.checkouts.join(&request.checkout))
            .map_err(|e| format!("checkout {}: {e}", request.checkout))?;
        let cwd = if request.sub.is_empty() {
            top.clone()
        } else {
            let parts = source::check_path(&request.sub, &source::Limits::default())
                .map_err(|e| format!("sub: {e}"))?;
            // `mkdir -p` without following a symlink planted inside the checkout.
            let mut cur = top.clone();
            for part in parts {
                cur.push(part);
                match std::fs::symlink_metadata(&cur) {
                    Ok(m) if m.is_dir() => {}
                    Ok(_) => {
                        return Err(format!(
                            "UnsafePath(sub {:?}: {} is not a plain directory)",
                            request.sub,
                            cur.display()
                        ));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        std::fs::create_dir(&cur).map_err(|e| format!("sub: {e}"))?
                    }
                    Err(e) => return Err(format!("sub: {e}")),
                }
            }
            std::fs::canonicalize(&cur).map_err(|e| format!("sub: {e}"))?
        };
        if !cwd.starts_with(&top) {
            return Err("UnsafePath(sub escapes the checkout)".into());
        }
        Ok(cwd)
    }

    fn approve(&self, peer: &PeerEntry, id: i64) -> Result<bool, String> {
        if peer.approve != Approve::Auto {
            return Ok(false);
        }
        let r = ask(&self.socket, Action::ApproveCommand(id))?;
        if let Some(e) = r.error {
            return Err(e);
        }
        // A peer's submission never overrides this machine's `tasks stop-worker`.
        if crate::worker::stopped(&self.paths.state) {
            eprintln!("peer: job {id} approved; execution is stopped here (tasks stop-worker)");
            return Ok(true);
        }
        if let Err(e) = ask(&self.socket, Action::StartWorker).and_then(|r| match r.error {
            Some(e) => Err(e),
            None => Ok(()),
        }) {
            eprintln!("peer: job {id} approved but start-worker failed: {e}");
        }
        Ok(true)
    }

    fn existing(&self, peer: &PeerEntry, record: &InboundRecord) -> Result<Submitted, String> {
        let id = record.remote_id.ok_or("record without id")?;
        let job = job(&self.socket, id)?;
        // A recorded but still unapproved job of an auto peer (e.g. the daemon stopped between
        // insert and approval) is approved now; approval of a queued job is idempotent.
        let approved = if job.phase == Phase::Queued {
            self.approve(peer, id)?
        } else {
            true
        };
        Ok(Submitted {
            remote_id: id,
            existing: true,
            approved,
            phase: phase_name(&job.phase),
        })
    }
}

impl JobHost for DaemonJobHost {
    fn submit(&self, peer: &PeerEntry, request: &SubmitRequest) -> Result<Submitted, String> {
        let _serial = self.submit_lock.lock().unwrap_or_else(|p| p.into_inner());
        let endpoint = peer.endpoint.to_string();
        let path = self.paths.inbound_record(&endpoint, &request.key);
        let previous: Option<InboundRecord> = read_json(&path);
        let spec = match &previous {
            Some(record) if record.key != request.key => {
                return Err("KeyCollision".into());
            }
            Some(record) if record.state == "submitted" => return self.existing(peer, record),
            Some(record) if record.state == "refused" => {
                return Err(record.refused.clone().unwrap_or_else(|| "Refused".into()));
            }
            Some(record) => {
                // `submitting`: a previous attempt stopped between intent and confirmation.
                // Adopt the job it created, if any; several candidates are never guessed.
                let jobs = ask(&self.socket, Action::List)?.jobs;
                match crate::submit_candidates(&jobs, &record.spec, record.since).as_slice() {
                    [] => record.spec.clone(),
                    [id] => {
                        let mut adopted = record.clone();
                        adopted.state = "submitted".into();
                        adopted.remote_id = Some(*id);
                        write_json(&path, &adopted).map_err(|e| e.to_string())?;
                        return self.existing(peer, &adopted);
                    }
                    many => {
                        return Err(format!(
                            "UNCERTAIN: key {} matches several jobs {many:?}; not submitting again",
                            request.key
                        ));
                    }
                }
            }
            None => {
                if request.owner.is_empty()
                    || request.owner.len() > 128
                    || request.owner.chars().any(|c| c.is_control())
                {
                    return Err("InvalidOwner".into());
                }
                let cwd = self.cwd(peer, request)?;
                let resource = match &request.resource {
                    RemoteResource::Compile => Resource::Compile,
                    RemoteResource::Run => Resource::Run,
                    RemoteResource::Device(serial) => Resource::Device(
                        crate::device::lock_serial(serial).map_err(|e| e.to_string())?,
                    ),
                };
                // The same client-side checks as `tasks enqueue` on this host: argv bounds,
                // nested admission helpers, Cargo -j cap, never-use devices (built-in plus this
                // daemon's TASKS_DENIED_DEVICES and config file).
                Recipe {
                    broad_test: request.broad_test,
                    argv: request.argv.clone(),
                    resource,
                }
                .spec(request.owner.clone(), cwd)
                .map_err(|e| format!("Refused: {e}"))?
            }
        };
        let mut record = InboundRecord {
            version: 1,
            key: request.key.clone(),
            peer: peer.name.clone(),
            peer_endpoint: endpoint,
            origin_host: request.origin_host.clone(),
            origin_job_id: request.origin_job_id,
            owner: request.owner.clone(),
            state: "submitting".into(),
            remote_id: None,
            since: crate::execution::now(),
            spec: spec.clone(),
            refused: None,
            ledger_linked: false,
        };
        write_json(&path, &record).map_err(|e| format!("inbound record: {e}"))?;
        let response = match ask(&self.socket, Action::Submit(spec)) {
            Ok(r) => r,
            // The intent stays: a retry with this key reconciles instead of submitting twice.
            Err(e) => {
                return Err(format!(
                    "UNCERTAIN: submit reply lost ({e}); retry the same key"
                ));
            }
        };
        let id = match (response.error, response.submitted_id) {
            (None, Some(id)) => id,
            (error, _) => {
                let _ = std::fs::remove_file(&path);
                return Err(format!(
                    "Refused: {}",
                    error.unwrap_or_else(|| "no submitted_id".into())
                ));
            }
        };
        record.state = "submitted".into();
        record.remote_id = Some(id);
        write_json(&path, &record).map_err(|e| format!("inbound record: {e}"))?;
        let approved = match self.approve(peer, id) {
            Ok(approved) => approved,
            Err(reason) => {
                // Approval re-validates with the daemon's own policy; a refusal cancels the
                // never-started proposal so it cannot be approved later by accident.
                let _ = ask(&self.socket, Action::Cancel(id));
                record.state = "refused".into();
                record.refused = Some(format!("Refused: {reason}"));
                let _ = write_json(&path, &record);
                return Err(format!("Refused: {reason}"));
            }
        };
        let phase = job(&self.socket, id)
            .map(|j| phase_name(&j.phase))
            .unwrap_or_else(|_| "queued".into());
        Ok(Submitted {
            remote_id: id,
            existing: false,
            approved,
            phase,
        })
    }

    fn state(&self, peer: &PeerEntry, id: i64) -> Result<JobState, String> {
        self.record_of(peer, id)?;
        Ok(job_state(&job(&self.socket, id)?))
    }

    fn read_log(
        &self,
        peer: &PeerEntry,
        id: i64,
        offset: u64,
        max: usize,
    ) -> Result<Vec<u8>, String> {
        self.record_of(peer, id)?;
        let mut file = match std::fs::File::open(self.paths.job_log(id)) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.to_string()),
        };
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        file.take(max as u64)
            .read_to_end(&mut buf)
            .map_err(|e| e.to_string())?;
        Ok(buf)
    }

    fn cancel(&self, peer: &PeerEntry, id: i64) -> Result<JobState, String> {
        self.record_of(peer, id)?;
        let current = job(&self.socket, id)?;
        if !current.phase.terminal() {
            let r = ask(&self.socket, Action::Cancel(id))?;
            if let Some(e) = r.error {
                return Err(e);
            }
        }
        Ok(job_state(&job(&self.socket, id)?))
    }

    /// Read-only, bounded search of this daemon's job logs for a host peer (every job of this
    /// ledger; the query's bounds are clamped as for a local search). Forwarded jobs of this
    /// machine name their own peer as the machine.
    fn search_logs(
        &self,
        peer: &PeerEntry,
        query: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let query: crate::log_search::Query =
            serde_json::from_value(query.clone()).map_err(|e| format!("BadQuery({e})"))?;
        let response = ask(&self.socket, Action::List)?;
        if let Some(e) = response.error {
            return Err(e);
        }
        let hostname = task_manager_peer::select::hostname();
        let machine_of = crate::log_search::local_machine_of(&self.paths.state, &hostname);
        let report = crate::log_search::search(
            &self.paths.state,
            &response.jobs,
            &query,
            &hostname,
            crate::execution::now(),
            &machine_of,
            &|_| true,
        )?;
        eprintln!(
            "{} peer: log search for {}: {} matches in {} jobs ({} bytes)",
            crate::execution::now(),
            peer.name,
            report.matches.len(),
            report.jobs_scanned,
            report.bytes_scanned
        );
        serde_json::to_value(report).map_err(|e| e.to_string())
    }

    fn lookup(&self, peer: &PeerEntry, key: &str) -> Result<Option<i64>, String> {
        let record: Option<InboundRecord> =
            read_json(&self.paths.inbound_record(&peer.endpoint.to_string(), key));
        Ok(record.and_then(|r| r.remote_id))
    }
}
