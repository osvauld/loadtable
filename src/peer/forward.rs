//! Sending side of a forwarded job, run inside the daemon's peer runtime for one bridge
//! connection (`tasks peer-forward` -> `peer.sock`).
//!
//! 1. Reads the LOCAL job from the ledger: it must be an active, approved `local.command` in the
//!    offload slot whose argv is `tasks peer-forward ...` for this state directory. The bridge
//!    only names the job id; what is forwarded comes from the approved record.
//! 2. Syncs the checkout (`tasks-offload` file selection and checkout id) over `source/0` under
//!    the shared per-checkout client lock, then submits over `jobs/0` with an idempotency key
//!    stored in the link record BEFORE the first attempt. A lost reply is retried with the same
//!    key: the peer never creates a second job for it.
//! 3. Attaches by remote id and streams the peer's log into the bridge (= this job's log).
//!    A lost link makes the remote state UNCERTAIN (the local job keeps running); the session
//!    re-attaches by id from the last delivered offset until `reattach_timeout`.
//! 4. Ends with the remote exit code: passed 0, failed its code, cancelled 130, interrupted or
//!    still uncertain after the window 255, refused 2, peer offline before submission 255.
//!
//! Cancels are propagated by the runtime's watcher (local cancel -> remote cancel); a remote
//! cancel arrives here as `Done{cancelled}`.
use super::host;
use super::records::{OutboundLink, read_json, write_json};
use super::runtime::Shared;
use crate::local_command::{Recipe, Resource};
use crate::peer_link::{ForwardArgs, ForwardResource};
use serde::{Deserialize, Serialize};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use task_manager_peer::jobs::{
    self, AttachEvent, JobsReply, JobsRequest, RemoteResource, SubmitRequest,
};
use task_manager_peer::source::{CheckoutIdentity, Limits};
use task_manager_peer::{ClientError, PeerEntry, artifact, iroh, select};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// Bridge protocol on `peer.sock`: one JSON request line, then JSON event lines; a `log`
/// event is followed by exactly `size` raw bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BridgeRequest {
    Forward { job_id: i64, offset: u64 },
    Cancel { job_id: i64 },
    Watch { peer: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BridgeEvent {
    Hello {
        reattach_s: u64,
    },
    Note {
        text: String,
    },
    Log {
        offset: u64,
        size: u64,
    },
    State {
        remote_job_id: Option<i64>,
        remote_state: String,
        remote_phase: Option<String>,
    },
    Done {
        exit_code: i32,
        remote_state: String,
        remote_phase: Option<String>,
        message: String,
    },
    Ok,
    Error {
        message: String,
    },
    Line {
        value: serde_json::Value,
    },
}

pub const EXIT_REFUSED: i32 = 2;
pub const EXIT_TRANSPORT: i32 = 255;
pub const EXIT_CANCELLED: i32 = 130;
pub const EXIT_FETCH: i32 = 1;

pub struct Sink {
    writer: OwnedWriteHalf,
}

impl Sink {
    pub async fn event(&mut self, event: &BridgeEvent) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        self.writer.write_all(&line).await
    }
    pub async fn note(&mut self, text: impl Into<String>) -> std::io::Result<()> {
        self.event(&BridgeEvent::Note { text: text.into() }).await
    }
    pub async fn log(&mut self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.event(&BridgeEvent::Log {
            offset,
            size: bytes.len() as u64,
        })
        .await?;
        self.writer.write_all(bytes).await
    }
}

/// Serves one bridge connection.
pub async fn serve_bridge(shared: Arc<Shared>, stream: UnixStream) {
    let (read, writer) = stream.into_split();
    let mut sink = Sink { writer };
    let request = match read_request(read).await {
        Ok(r) => r,
        Err(e) => {
            let _ = sink.event(&BridgeEvent::Error { message: e }).await;
            return;
        }
    };
    match request {
        BridgeRequest::Forward { job_id, offset } => {
            if let Err(e) = forward(&shared, job_id, offset, &mut sink).await {
                (shared.log)(format!("peer: forward of job {job_id}: {e}"));
                let _ = sink.event(&BridgeEvent::Error { message: e }).await;
            }
        }
        BridgeRequest::Cancel { job_id } => {
            let reply = match propagate_cancel(&shared, job_id).await {
                Ok(()) => BridgeEvent::Ok,
                Err(message) => BridgeEvent::Error { message },
            };
            let _ = sink.event(&reply).await;
        }
        BridgeRequest::Watch { peer } => {
            super::runtime::watch_peer(&shared, &peer, &mut sink).await
        }
    }
}

async fn read_request(read: OwnedReadHalf) -> Result<BridgeRequest, String> {
    let mut line = String::new();
    let mut reader = BufReader::new(read.take(64 * 1024));
    tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
        .await
        .map_err(|_| "no request".to_string())?
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| format!("bad bridge request: {e}"))
}

fn save(shared: &Shared, link: &mut OutboundLink) {
    link.updated_at = task_manager_peer::now_f64();
    // The watcher records a sent cancel concurrently; never drop that fact.
    if let Some(disk) = read_json::<OutboundLink>(&shared.paths.link(link.local_job_id)) {
        link.cancel_sent |= disk.cancel_sent;
    }
    if let Err(e) = write_json(&shared.paths.link(link.local_job_id), link) {
        (shared.log)(format!(
            "peer: cannot write link record of job {}: {e}",
            link.local_job_id
        ));
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("task failed: {e}"))?
}

/// The forwarded local job and its arguments, from the approved ledger record.
pub fn forwarded_job(shared: &Shared, job: &crate::Job) -> Result<ForwardArgs, String> {
    let recipe = Recipe::from_spec(&job.spec).map_err(|e| e.to_string())?;
    if recipe.resource != Resource::Offload {
        return Err(format!("job {} is not in the offload slot", job.id));
    }
    let args = ForwardArgs::from_recipe_argv(&recipe.argv)
        .ok_or_else(|| format!("job {} is not a forwarded job", job.id))?
        .map_err(|e| e.to_string())?;
    let mine = std::fs::canonicalize(&shared.paths.state).map_err(|e| e.to_string())?;
    if std::fs::canonicalize(&args.state).ok() != Some(mine) {
        return Err(format!(
            "job {} forwards through another state directory ({})",
            job.id,
            args.state.display()
        ));
    }
    Ok(args)
}

fn new_key(shared: &Shared, job: i64) -> String {
    let seed = format!(
        "{}:{}:{}:{:?}",
        shared.hostname,
        job,
        std::process::id(),
        std::time::SystemTime::now()
    );
    let host: String = shared
        .hostname
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(48)
        .collect();
    format!(
        "{host}:{job}:{}",
        &task_manager_peer::sha256_hex(seed.as_bytes())[..16]
    )
}

/// `${XDG_STATE_HOME:-~/.local/state}/tasks-offload/locks/<id>.lock`: the SAME per-checkout
/// client lock as `tasks-offload`, so an ssh offload and a peer forward of one checkout never
/// rewrite the worker copy under each other.
fn checkout_lock_path(cid: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("tasks-offload/locks").join(format!("{cid}.lock")))
}

fn try_flock(path: &Path) -> Result<Option<OwnedFd>, String> {
    if let Some(dir) = path.parent() {
        super::records::private_dir(dir).map_err(|e| e.to_string())?;
    }
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let fd = OwnedFd::from(file);
    // SAFETY: valid descriptor owned above.
    if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(Some(fd))
    } else {
        Ok(None)
    }
}

/// Maps the peer's terminal phase to this job's exit code and remote state.
pub fn exit_of(done: &jobs::AttachDone) -> (i32, &'static str) {
    match done.phase.as_str() {
        "passed" => (0, "terminal"),
        "cancelled" => (EXIT_CANCELLED, "terminal"),
        "interrupted" => (EXIT_TRANSPORT, "interrupted"),
        _ => (done.exit_code.filter(|c| *c != 0).unwrap_or(1), "terminal"),
    }
}

struct Finish {
    exit: i32,
    remote_state: &'static str,
    message: String,
}

async fn finish(
    shared: &Shared,
    link: &mut OutboundLink,
    sink: &mut Sink,
    end: Finish,
) -> Result<(), String> {
    link.exit = Some(end.exit);
    link.remote_state = Some(end.remote_state.into());
    link.stage = "done".into();
    link.error = (end.exit != 0).then(|| end.message.clone());
    save(shared, link);
    shared.release_checkout(link.local_job_id);
    let _ = sink
        .event(&BridgeEvent::Done {
            exit_code: end.exit,
            remote_state: end.remote_state.into(),
            remote_phase: link.remote_phase.clone(),
            message: end.message,
        })
        .await;
    Ok(())
}

fn peer_target(
    shared: &Shared,
    args: &ForwardArgs,
) -> Result<(PeerEntry, iroh::EndpointAddr), String> {
    let peer = shared
        .config
        .peer(&args.peer)
        .ok_or_else(|| {
            format!(
                "unknown peer {:?} (not in {})",
                args.peer,
                shared.config.path.display()
            )
        })?
        .clone();
    if !peer.role.runs_my_jobs() {
        return Err(format!(
            "peer {} has role {:?}; only role worker/both runs forwarded jobs",
            peer.name, peer.role
        ));
    }
    let addr = peer
        .addr
        .ok_or_else(|| format!("peer {} has no addr to dial", peer.name))?;
    Ok((
        peer.clone(),
        task_manager_peer::peer_addr(peer.endpoint, addr),
    ))
}

async fn local_cancel_requested(shared: &Shared, job_id: i64) -> bool {
    let socket = shared.socket.clone();
    blocking(move || host::job(&socket, job_id))
        .await
        .is_ok_and(|j| {
            j.phase == crate::Phase::Cancelled
                || j.execution.as_ref().is_some_and(|e| e.cancel_requested)
        })
}

async fn forward(
    shared: &Arc<Shared>,
    job_id: i64,
    offset: u64,
    sink: &mut Sink,
) -> Result<(), String> {
    let reattach = shared.config.reattach_timeout;
    let _ = sink
        .event(&BridgeEvent::Hello {
            reattach_s: reattach.as_secs(),
        })
        .await;
    let socket = shared.socket.clone();
    let job = blocking(move || host::job(&socket, job_id)).await?;
    let args = forwarded_job(shared, &job)?;
    let path = shared.paths.link(job_id);
    let existing: Option<OutboundLink> = read_json(&path);
    if let Some(link) = existing.as_ref().filter(|l| l.done()) {
        // The bridge re-attached after the outcome was decided: report it again.
        let _ = sink
            .event(&BridgeEvent::Done {
                exit_code: link.exit.unwrap_or(EXIT_TRANSPORT),
                remote_state: link.remote_state.clone().unwrap_or_default(),
                remote_phase: link.remote_phase.clone(),
                message: link.error.clone().unwrap_or_default(),
            })
            .await;
        return Ok(());
    }
    if !job.phase.active() {
        return Err(format!("job {job_id} is not running ({:?})", job.phase));
    }
    let mut link = existing.clone().unwrap_or_else(|| OutboundLink {
        version: 1,
        local_job_id: job_id,
        peer: args.peer.clone(),
        key: new_key(shared, job_id),
        owner: format!("{}@{}", args.who, shared.hostname),
        stage: "syncing".into(),
        started_at: task_manager_peer::now_f64(),
        ..Default::default()
    });
    let (peer, addr) = match peer_target(shared, &args) {
        Ok(t) => t,
        Err(e) => {
            return finish(
                shared,
                &mut link,
                sink,
                Finish {
                    exit: EXIT_REFUSED,
                    remote_state: "not_submitted",
                    message: e,
                },
            )
            .await;
        }
    };
    link.peer_endpoint = peer.endpoint.to_string();
    if existing.is_none() {
        save(shared, &mut link);
    }

    // ---- sync + submit, once per job (a concurrent re-attaching session waits here) ----
    let gate = shared.job_gate(job_id);
    let guard = gate.lock().await;
    if let Some(updated) = read_json::<OutboundLink>(&path) {
        link = updated;
    }
    if link.remote_job_id.is_none() {
        if existing.is_some() && link.stage != "syncing" {
            // Restarted mid-submit: ask the peer what this key became before anything else.
            match jobs::call(
                &shared.ep,
                addr.clone(),
                &JobsRequest::Lookup {
                    key: link.key.clone(),
                },
                shared.config.connect_timeout,
            )
            .await
            {
                Ok(JobsReply::Found {
                    remote_id: Some(id),
                }) => {
                    link.remote_job_id = Some(id);
                    link.stage = "running".into();
                    save(shared, &mut link);
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = sink
                        .note(format!("lookup of key {} on {}: {e}", link.key, peer.name))
                        .await;
                }
            }
        }
        if link.remote_job_id.is_none()
            && let Err(end) =
                sync_and_submit(shared, &job, &args, &peer, &addr, &mut link, sink).await
        {
            drop(guard);
            return finish(shared, &mut link, sink, end).await;
        }
    }
    drop(guard);
    let remote_id = link.remote_job_id.ok_or("no remote job id")?;
    attach_until_done(
        shared, &args, &peer, &addr, remote_id, offset, &mut link, sink,
    )
    .await
}

async fn sync_and_submit(
    shared: &Arc<Shared>,
    job: &crate::Job,
    args: &ForwardArgs,
    peer: &PeerEntry,
    addr: &iroh::EndpointAddr,
    link: &mut OutboundLink,
    sink: &mut Sink,
) -> Result<(), Finish> {
    let refused = |message: String| Finish {
        exit: EXIT_REFUSED,
        remote_state: "not_submitted",
        message,
    };
    let source = PathBuf::from(&job.spec.source);
    let src = source.clone();
    let top = blocking(move || select::git_toplevel(&src))
        .await
        .map_err(refused)?;
    let source = std::fs::canonicalize(&source).map_err(|e| refused(e.to_string()))?;
    let sub = source
        .strip_prefix(&top)
        .map_err(|_| refused("job cwd is outside its git checkout".into()))?
        .to_str()
        .ok_or_else(|| refused("non-UTF-8 cwd".into()))?
        .to_owned();
    let top_str = top
        .to_str()
        .ok_or_else(|| refused("non-UTF-8 checkout".into()))?
        .to_owned();
    let cid = select::checkout_id(&shared.hostname, &top_str);
    link.checkout = Some(cid.clone());
    link.sub = sub.clone();
    // Shared client lock of this checkout, held until the forwarded job is done.
    if !shared.holds_checkout(job.id)
        && let Some(lock_path) = checkout_lock_path(&cid)
    {
        let mut said = false;
        loop {
            let p = lock_path.clone();
            match blocking(move || try_flock(&p)).await.map_err(refused)? {
                Some(fd) => {
                    shared.keep_checkout(job.id, fd);
                    break;
                }
                None if !said => {
                    said = true;
                    let _ = sink
                        .note("waiting for another offload of this checkout to finish (its worker copy is in use)")
                        .await;
                }
                None => {}
            }
            if local_cancel_requested(shared, job.id).await {
                return Err(Finish {
                    exit: EXIT_CANCELLED,
                    remote_state: "not_submitted",
                    message: "cancelled before submission".into(),
                });
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    let t = Instant::now();
    let top2 = top.clone();
    let selection = blocking(move || select::select(&top2, &Limits::default()))
        .await
        .map_err(|e| refused(format!("source: {e}")))?;
    let _ = sink
        .note(format!(
            "syncing {} files ({} bytes; {} excluded, {} tracked-but-deleted) of {} to {} ({}) as checkout {cid}",
            selection.manifest.files.len(),
            selection.bytes,
            selection.excluded_count,
            selection.deleted_tracked,
            top.display(),
            peer.name,
            peer.endpoint.fmt_short(),
        ))
        .await;
    let identity = CheckoutIdentity {
        host: shared.hostname.clone(),
        toplevel: top_str,
    };
    let pushed = task_manager_peer::source::sync_push(
        &shared.ep,
        addr.clone(),
        &top,
        &selection.manifest,
        &cid,
        &identity,
        shared.config.connect_timeout.max(Duration::from_secs(1)) * 30,
    )
    .await;
    let report = match pushed {
        Ok(r) => r,
        Err(ClientError::Offline(e)) => {
            return Err(Finish {
                exit: EXIT_TRANSPORT,
                remote_state: "not_submitted",
                message: format!(
                    "peer {} ({}) is offline or unreachable: {e}; nothing was submitted",
                    peer.name,
                    addr.id.fmt_short()
                ),
            });
        }
        Err(ClientError::Refused(e)) => {
            return Err(refused(format!(
                "peer {} refused the source sync: {e}",
                peer.name
            )));
        }
        Err(ClientError::Local(e)) => return Err(refused(e)),
        Err(e) => {
            return Err(Finish {
                exit: EXIT_TRANSPORT,
                remote_state: "not_submitted",
                message: format!(
                    "source sync to {} failed: {e}; nothing was submitted",
                    peer.name
                ),
            });
        }
    };
    link.sync_s = Some(t.elapsed().as_secs_f64());
    link.blobs_sent = Some(report.blobs_sent);
    let r = &report.reply;
    let _ = sink
        .note(format!(
            "synced in {:.2}s: {} blobs ({} bytes) sent; {} written, {} unchanged, {} deleted{}",
            t.elapsed().as_secs_f64(),
            report.blobs_sent,
            report.bytes_sent,
            r.written,
            r.unchanged,
            r.deleted,
            if r.deleted_paths.is_empty() {
                String::new()
            } else {
                format!(" ({})", r.deleted_paths.join(", "))
            }
        ))
        .await;
    if local_cancel_requested(shared, job.id).await {
        return Err(Finish {
            exit: EXIT_CANCELLED,
            remote_state: "not_submitted",
            message: "cancelled before submission".into(),
        });
    }
    link.stage = "submitting".into();
    save(shared, link);
    let request = JobsRequest::Submit(SubmitRequest {
        key: link.key.clone(),
        origin_host: shared.hostname.clone(),
        origin_job_id: Some(job.id),
        owner: link.owner.clone(),
        checkout: cid,
        sub,
        argv: args.argv.clone(),
        resource: match &args.resource {
            ForwardResource::Compile => RemoteResource::Compile,
            ForwardResource::Run => RemoteResource::Run,
            ForwardResource::Device(s) => RemoteResource::Device(s.clone()),
        },
    });
    let deadline = Instant::now() + shared.config.reattach_timeout;
    let mut delivered_maybe = false;
    loop {
        match jobs::call(
            &shared.ep,
            addr.clone(),
            &request,
            shared.config.connect_timeout,
        )
        .await
        {
            Ok(JobsReply::Submitted {
                remote_id,
                existing,
                approved,
                phase,
            }) => {
                link.remote_job_id = Some(remote_id);
                link.remote_phase = Some(phase.clone());
                link.remote_state = Some("attached".into());
                link.stage = "running".into();
                save(shared, link);
                let _ = sink
                    .note(format!(
                        "{} job {remote_id} on {} (owner {}, phase {phase}{}){}",
                        if existing {
                            "found the already submitted"
                        } else {
                            "submitted as"
                        },
                        peer.name,
                        link.owner,
                        if approved {
                            ""
                        } else {
                            ", WAITING FOR APPROVAL on the peer"
                        },
                        if existing { "; not resubmitted" } else { "" },
                    ))
                    .await;
                let _ = sink
                    .event(&BridgeEvent::State {
                        remote_job_id: Some(remote_id),
                        remote_state: "attached".into(),
                        remote_phase: Some(phase),
                    })
                    .await;
                return Ok(());
            }
            Ok(JobsReply::Refused { reason }) if reason.starts_with("UNCERTAIN") => {
                let _ = sink
                    .note(format!(
                        "peer {}: {reason}; retrying the same key",
                        peer.name
                    ))
                    .await;
            }
            Ok(JobsReply::Refused { reason }) => {
                return Err(Finish {
                    exit: EXIT_REFUSED,
                    remote_state: "refused",
                    message: format!("refused by peer {}: {reason}", peer.name),
                });
            }
            Ok(other) => {
                return Err(Finish {
                    exit: EXIT_TRANSPORT,
                    remote_state: "uncertain",
                    message: format!("unexpected reply to submit: {other:?}"),
                });
            }
            Err(ClientError::Refused(e)) => {
                return Err(refused(format!("refused by peer {}: {e}", peer.name)));
            }
            Err(ClientError::Offline(e)) if !delivered_maybe => {
                return Err(Finish {
                    exit: EXIT_TRANSPORT,
                    remote_state: "not_submitted",
                    message: format!(
                        "peer {} went offline at submit ({e}); nothing was submitted",
                        peer.name
                    ),
                });
            }
            Err(e) => {
                delivered_maybe = true;
                link.remote_state = Some("uncertain".into());
                save(shared, link);
                let _ = sink
                    .note(format!(
                        "UNCERTAIN: submit to {} may have been delivered ({e}); retrying with the same key {} (never submits twice)",
                        peer.name, link.key
                    ))
                    .await;
            }
        }
        if Instant::now() >= deadline {
            return Err(Finish {
                exit: EXIT_TRANSPORT,
                remote_state: "uncertain",
                message: format!(
                    "UNCERTAIN: no confirmed submission on {} within {:?}; key {} (query the peer before retrying)",
                    peer.name, shared.config.reattach_timeout, link.key
                ),
            });
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn attach_until_done(
    shared: &Arc<Shared>,
    args: &ForwardArgs,
    peer: &PeerEntry,
    addr: &iroh::EndpointAddr,
    remote_id: i64,
    mut offset: u64,
    link: &mut OutboundLink,
    sink: &mut Sink,
) -> Result<(), String> {
    let started = Instant::now();
    let mut lost_since: Option<Instant> = None;
    let mut backoff = Duration::from_millis(250);
    loop {
        // Log bytes go through a channel: the attach callback is synchronous.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Out>();
        let gone = Arc::new(AtomicBool::new(false));
        let attach = {
            let gone = Arc::clone(&gone);
            let tx = tx.clone();
            jobs::attach(
                &shared.ep,
                addr.clone(),
                remote_id,
                offset,
                shared.config.connect_timeout,
                shared.stall,
                move |event| {
                    if gone.load(Ordering::SeqCst) {
                        return Err("bridge gone".into());
                    }
                    let out = match event {
                        AttachEvent::Attached(phase) => Out::Attached(phase.to_owned()),
                        AttachEvent::Phase(phase) => Out::Phase(phase.to_owned()),
                        AttachEvent::Log { offset, bytes } => Out::Log(offset, bytes.to_vec()),
                    };
                    tx.send(out).map_err(|_| "bridge gone".to_string())
                },
            )
        };
        drop(tx);
        tokio::pin!(attach);
        let result = loop {
            tokio::select! {
                result = &mut attach => break result,
                Some(out) = rx.recv() => {
                    if deliver(shared, link, sink, &mut offset, &mut lost_since, remote_id, out).await.is_err() {
                        gone.store(true, Ordering::SeqCst);
                    }
                }
            }
        };
        // Deliver what arrived before the attach ended.
        while let Ok(out) = rx.try_recv() {
            if deliver(
                shared,
                link,
                sink,
                &mut offset,
                &mut lost_since,
                remote_id,
                out,
            )
            .await
            .is_err()
            {
                gone.store(true, Ordering::SeqCst);
            }
        }
        if gone.load(Ordering::SeqCst) {
            // The bridge (this job's command) is gone: a cancel is the watcher's business;
            // otherwise a re-attaching bridge continues from its own offset.
            return Ok(());
        }
        match result {
            Ok(done) => {
                let (exit, remote_state) = exit_of(&done);
                link.remote_phase = Some(done.phase.clone());
                link.run_s = Some(started.elapsed().as_secs_f64());
                let mut message = match done.phase.as_str() {
                    "passed" => format!("job {remote_id} on {} passed", peer.name),
                    "cancelled" => format!("job {remote_id} on {} was CANCELLED there", peer.name),
                    phase => format!(
                        "job {remote_id} on {} {phase} (exit {}){}",
                        peer.name,
                        done.exit_code.map_or("-".into(), |c| c.to_string()),
                        if done.detail.is_empty() {
                            String::new()
                        } else {
                            format!(": {}", done.detail)
                        }
                    ),
                };
                let mut exit = exit;
                if exit == 0 && !args.fetch.is_empty() {
                    match fetch(shared, args, addr, link, sink).await {
                        Ok(dest) => message.push_str(&format!("; fetched into {dest}")),
                        Err(e) => {
                            exit = EXIT_FETCH;
                            message.push_str(&format!("; FETCH FAILED: {e}"));
                        }
                    }
                }
                return finish(
                    shared,
                    link,
                    sink,
                    Finish {
                        exit,
                        remote_state,
                        message,
                    },
                )
                .await;
            }
            Err(ClientError::Local(_)) => return Ok(()),
            Err(ClientError::Refused(e)) => {
                return finish(
                    shared,
                    link,
                    sink,
                    Finish {
                        exit: EXIT_TRANSPORT,
                        remote_state: "uncertain",
                        message: format!(
                            "peer {} refused to attach job {remote_id}: {e}",
                            peer.name
                        ),
                    },
                )
                .await;
            }
            Err(e) => {
                // A new loss episode (the first, or after a successful re-attach, which clears
                // `lost_since`): say so once and start the backoff again.
                let new_episode = lost_since.is_none();
                let since = *lost_since.get_or_insert_with(Instant::now);
                if new_episode {
                    backoff = Duration::from_millis(250);
                    link.remote_state = Some("uncertain".into());
                    save(shared, link);
                    let _ = sink
                        .note(format!(
                            "UNCERTAIN: link to {} lost ({e}); job {remote_id} may still be running there; re-attaching by id (never resubmitted)",
                            peer.name
                        ))
                        .await;
                    let _ = sink
                        .event(&BridgeEvent::State {
                            remote_job_id: Some(remote_id),
                            remote_state: "uncertain".into(),
                            remote_phase: link.remote_phase.clone(),
                        })
                        .await;
                }
                if since.elapsed() >= shared.config.reattach_timeout {
                    return finish(
                        shared,
                        link,
                        sink,
                        Finish {
                            exit: EXIT_TRANSPORT,
                            remote_state: "uncertain",
                            message: format!(
                                "UNCERTAIN: could not re-attach to job {remote_id} on {} within {:?}; it may still be running there (query it on the peer; not resubmitting)",
                                peer.name, shared.config.reattach_timeout
                            ),
                        },
                    )
                    .await;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}

enum Out {
    Attached(String),
    Phase(String),
    Log(u64, Vec<u8>),
}

async fn deliver(
    shared: &Shared,
    link: &mut OutboundLink,
    sink: &mut Sink,
    offset: &mut u64,
    lost_since: &mut Option<Instant>,
    remote_id: i64,
    out: Out,
) -> std::io::Result<()> {
    match out {
        Out::Attached(phase) => {
            if let Some(since) = lost_since.take() {
                sink.note(format!(
                    "re-attached to job {remote_id} on {} after {:.1}s (phase {phase}); continuing from log offset {offset}",
                    link.peer,
                    since.elapsed().as_secs_f64()
                ))
                .await?;
            }
            link.remote_state = Some("attached".into());
            link.remote_phase = Some(phase.clone());
            save(shared, link);
            sink.event(&BridgeEvent::State {
                remote_job_id: Some(remote_id),
                remote_state: "attached".into(),
                remote_phase: Some(phase),
            })
            .await
        }
        Out::Phase(phase) => {
            link.remote_phase = Some(phase.clone());
            save(shared, link);
            sink.event(&BridgeEvent::State {
                remote_job_id: Some(remote_id),
                remote_state: "attached".into(),
                remote_phase: Some(phase),
            })
            .await
        }
        Out::Log(at, bytes) => {
            if at != *offset {
                return Ok(());
            }
            sink.log(at, &bytes).await?;
            *offset += bytes.len() as u64;
            Ok(())
        }
    }
}

/// `--fetch REL...` after a passed job: artifacts into the NEW directory
/// `~/build-cache/offload/<who>/<local job id>` through this daemon's blob store.
async fn fetch(
    shared: &Arc<Shared>,
    args: &ForwardArgs,
    addr: &iroh::EndpointAddr,
    link: &mut OutboundLink,
    sink: &mut Sink,
) -> Result<String, String> {
    let t = Instant::now();
    link.stage = "fetching".into();
    save(shared, link);
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let dest = PathBuf::from(home)
        .join("build-cache/offload")
        .join(&args.who)
        .join(link.local_job_id.to_string());
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let checkout = link.checkout.clone().ok_or("no checkout")?;
    let _ = sink
        .note(format!(
            "fetching {} into {}",
            args.fetch.join(", "),
            dest.display()
        ))
        .await;
    let report = artifact::fetch_artifacts(
        &shared.ep,
        addr.clone(),
        &checkout,
        &args.fetch,
        &shared.store,
        &dest,
        shared.config.connect_timeout.max(Duration::from_secs(1)) * 30,
    )
    .await
    .map_err(|e| e.to_string())?;
    link.fetch_s = Some(t.elapsed().as_secs_f64());
    link.fetch_dest = Some(report.dest.display().to_string());
    let _ = sink
        .note(format!(
            "fetched {} files ({} blobs, {} bytes received{})",
            report.manifest.files.len(),
            report.blobs_received,
            report.bytes_received,
            if report.skipped.is_empty() {
                String::new()
            } else {
                format!("; skipped {:?}", report.skipped)
            }
        ))
        .await;
    Ok(report.dest.display().to_string())
}

/// What a read-only probe of a remote job found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Terminal there: phase and exit code (0 only if passed).
    Terminal(String, Option<i32>),
    /// Still live there (its phase).
    Live(String),
}

/// Read-only status of the peer job a local link points to: attaches far past the end of its
/// log, so no log bytes travel, and stops at the first answer unless the job is already
/// terminal (the peer then sends `Done` at once). Never submits, cancels or re-runs.
pub async fn probe_remote(shared: &Shared, link: &OutboundLink) -> Result<Probe, String> {
    let remote_id = link.remote_job_id.ok_or("no remote job id")?;
    let (peer, addr) = shared
        .config
        .peer(&link.peer)
        .and_then(|p| p.addr.map(|a| (p.clone(), a)))
        .ok_or_else(|| format!("peer {} no longer configured", link.peer))?;
    let addr = task_manager_peer::peer_addr(peer.endpoint, addr);
    let seen = Arc::new(std::sync::Mutex::new(None::<String>));
    let probe_seen = Arc::clone(&seen);
    let attached = jobs::attach(
        &shared.ep,
        addr,
        remote_id,
        1 << 62,
        shared.config.connect_timeout,
        shared.stall,
        move |event| match event {
            jobs::AttachEvent::Attached(phase) | jobs::AttachEvent::Phase(phase) => {
                *probe_seen.lock().unwrap_or_else(|p| p.into_inner()) = Some(phase.to_owned());
                if matches!(phase, "passed" | "failed" | "cancelled" | "interrupted") {
                    Ok(())
                } else {
                    Err("probe: live".into())
                }
            }
            jobs::AttachEvent::Log { .. } => Ok(()),
        },
    )
    .await;
    let phase = seen.lock().unwrap_or_else(|p| p.into_inner()).clone();
    match attached {
        Ok(done) => {
            let code = if done.phase == "passed" {
                Some(0)
            } else {
                done.exit_code.filter(|c| *c != 0)
            };
            Ok(Probe::Terminal(done.phase, code))
        }
        Err(ClientError::Local(why)) if why.starts_with("probe:") => {
            Ok(Probe::Live(phase.unwrap_or_else(|| "unknown".into())))
        }
        Err(e) => Err(format!("job {remote_id} on {}: {e}", link.peer)),
    }
}

/// Sends the remote cancel for a local job whose cancel was requested. Idempotent: repeated
/// until the peer reports a terminal phase, which then closes the link record (the bridge of
/// a cancelled job is gone, so nobody else will).
pub async fn propagate_cancel(shared: &Shared, job_id: i64) -> Result<(), String> {
    let path = shared.paths.link(job_id);
    let mut link: OutboundLink = read_json(&path).ok_or("no link record")?;
    let Some(remote_id) = link.remote_job_id else {
        return Ok(()); // not submitted yet: the session checks the cancel before submitting
    };
    if link.done() {
        return Ok(());
    }
    let (peer, addr) = shared
        .config
        .peer(&link.peer)
        .and_then(|p| p.addr.map(|a| (p.clone(), a)))
        .ok_or_else(|| format!("peer {} no longer configured", link.peer))?;
    let addr = task_manager_peer::peer_addr(peer.endpoint, addr);
    let reply = jobs::call(
        &shared.ep,
        addr,
        &JobsRequest::Cancel { remote_id },
        shared.config.connect_timeout,
    )
    .await;
    let phase = match reply {
        Ok(JobsReply::Cancelled { phase }) => phase,
        Ok(JobsReply::Refused { reason }) => {
            return Err(format!("peer refused cancel of job {remote_id}: {reason}"));
        }
        Ok(other) => return Err(format!("unexpected reply to cancel: {other:?}")),
        Err(e) => return Err(format!("cancel of job {remote_id} on {}: {e}", link.peer)),
    };
    if !link.cancel_sent {
        (shared.log)(format!(
            "peer: cancelled job {remote_id} on {} for local job {job_id} (now {phase})",
            link.peer
        ));
    }
    // Re-read: the forward session may have closed the record meanwhile.
    if let Some(current) = read_json::<OutboundLink>(&path) {
        link = current;
    }
    link.cancel_sent = true;
    if link.done() {
        save(shared, &mut link);
        return Ok(());
    }
    if matches!(
        phase.as_str(),
        "passed" | "failed" | "cancelled" | "interrupted"
    ) {
        link.remote_phase = Some(phase);
        link.remote_state = Some("terminal".into());
        link.stage = "done".into();
        link.exit = Some(EXIT_CANCELLED);
        link.error = Some("cancelled locally; the peer job was cancelled".into());
        shared.release_checkout(job_id);
    }
    save(shared, &mut link);
    Ok(())
}
