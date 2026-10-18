//! The daemon's peer runtime: its own OS thread and tokio runtime, started by `tasks daemon`
//! only when a peer config exists (`TASKS_PEER_CONFIG`, else `~/.config/tasks/peer.toml`).
//! The unix-socket request threads never touch iroh: the runtime is a CLIENT of the daemon's
//! own socket (status, submit, approve, cancel, job state), like any local tool.
use super::forward::{BridgeEvent, Probe, Sink, probe_remote, propagate_cancel, serve_bridge};
use super::host::{self, DaemonJobHost};
use super::records::{
    InboundRecord, OutboundLink, Paths, inbound_records, outbound_links, private_dir, write_json,
};
use crate::peer_link::{PeerLink, PeerView, PeersReport, write_peer_link};
use std::collections::{HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use task_manager_peer::iroh::Endpoint;
use task_manager_peer::iroh_blobs::api::Store as BlobStore;
use task_manager_peer::iroh_blobs::store::fs::FsStore;
use task_manager_peer::jobs::JobsTiming;
use task_manager_peer::server::{self, Logger, Services};
use task_manager_peer::source::{Limits, SourceService};
use task_manager_peer::status::{self, Sample, StatusHub};
use task_manager_peer::{LanReport, PeerConfig, lan_endpoint, select};

/// State shared by every task of the runtime.
pub struct Shared {
    pub paths: Paths,
    /// This daemon's own `taskd.sock`.
    pub socket: PathBuf,
    pub config: PeerConfig,
    pub ep: Endpoint,
    pub store: BlobStore,
    pub hostname: String,
    pub stall: Duration,
    pub log: Logger,
    gates: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    held: Mutex<HashMap<i64, OwnedFd>>,
}

impl Shared {
    /// Serializes sync+submit of one local job across concurrent bridge sessions.
    pub fn job_gate(&self, job: i64) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.gates.lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(gates.entry(job).or_default())
    }
    pub fn holds_checkout(&self, job: i64) -> bool {
        self.held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&job)
    }
    pub fn keep_checkout(&self, job: i64, fd: OwnedFd) {
        self.held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(job, fd);
    }
    pub fn release_checkout(&self, job: i64) {
        self.held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&job);
        self.gates
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&job);
    }
}

fn logger() -> Logger {
    Arc::new(|line: String| {
        eprintln!("{} {line}", crate::execution::now());
    })
}

/// Starts the runtime thread if configured. The returned report is what `tasks peer list`
/// shows (static part; links are read from disk per request).
pub fn start(state: &Path, socket: &Path) -> Arc<Mutex<PeersReport>> {
    let config_path = PeerConfig::default_path();
    let report = Arc::new(Mutex::new(PeersReport {
        enabled: false,
        config: config_path.as_ref().map(|p| p.display().to_string()),
        ..Default::default()
    }));
    let Some(config_path) = config_path.filter(|p| p.exists()) else {
        report.lock().unwrap().error = Some("not configured (no peer config file)".into());
        return report;
    };
    let (state, socket, out) = (state.to_owned(), socket.to_owned(), Arc::clone(&report));
    let spawned = std::thread::Builder::new()
        .name("peer".into())
        .spawn(move || {
            let log = logger();
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("peer-rt")
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    out.lock().unwrap().error = Some(format!("runtime: {e}"));
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(e) =
                    run(&config_path, &state, &socket, Arc::clone(&out), log.clone()).await
                {
                    log(format!("peer: DISABLED: {e}"));
                    let mut r = out.lock().unwrap();
                    r.enabled = false;
                    r.error = Some(e);
                }
            });
        });
    if let Err(e) = spawned {
        report.lock().unwrap().error = Some(format!("thread: {e}"));
    }
    report
}

async fn run(
    config_path: &Path,
    state: &Path,
    socket: &Path,
    report: Arc<Mutex<PeersReport>>,
    log: Logger,
) -> Result<(), String> {
    let config = PeerConfig::load(config_path)?;
    let key = config.secret_key()?;
    let ep = lan_endpoint(key, config.bind, server::alpns()).await?;
    let lan = LanReport::of(&ep)?;
    if !lan.is_relay_and_lookup_free() {
        return Err(format!("endpoint is not LAN-only: {lan:?}"));
    }
    let paths = Paths::new(state);
    private_dir(&paths.dir()).map_err(|e| e.to_string())?;
    let blobs = config.store.clone().unwrap_or_else(|| paths.blobs());
    private_dir(&blobs).map_err(|e| format!("{}: {e}", blobs.display()))?;
    let store: BlobStore = FsStore::load(&blobs)
        .await
        .map_err(|e| format!("blob store {}: {e}", blobs.display()))?
        .into();
    private_dir(&config.checkouts).map_err(|e| format!("{}: {e}", config.checkouts.display()))?;

    let status_socket = socket.to_owned();
    let source: status::SampleSource =
        Arc::new(
            move || match crate::request_status(&status_socket, Duration::from_secs(30)) {
                Ok(reply) => match (reply.status, reply.error) {
                    (Some(snapshot), _) => match serde_json::to_vec(&snapshot) {
                        Ok(bytes) => Sample::Snapshot(Arc::new(bytes)),
                        Err(e) => Sample::Error(e.to_string()),
                    },
                    (None, e) => Sample::Error(e.unwrap_or_else(|| "no snapshot".into())),
                },
                Err(e) => Sample::Error(format!("tasks status: {e}")),
            },
        );
    let host = Arc::new(DaemonJobHost::new(
        paths.clone(),
        socket.to_owned(),
        config.checkouts.clone(),
    ));
    let services = Arc::new(Services {
        allow: config.allowlist(),
        status: Some(StatusHub::new(source, config.status_interval)),
        jobs: Some((
            host,
            JobsTiming {
                heartbeat: config.heartbeat,
            },
        )),
        source: Some(Arc::new(SourceService {
            store: store.clone(),
            checkouts: config.checkouts.clone(),
            limits: Limits::default(),
        })),
    });
    tokio::spawn(server::serve(ep.clone(), services, Arc::clone(&log)));

    // Local bridge socket. We hold the daemon lock, so a leftover socket is stale.
    let bridge = paths.socket();
    if std::fs::symlink_metadata(&bridge).is_ok() {
        std::fs::remove_file(&bridge).map_err(|e| e.to_string())?;
    }
    let listener = tokio::net::UnixListener::bind(&bridge).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&bridge, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;

    let shared = Arc::new(Shared {
        paths,
        socket: socket.to_owned(),
        stall: config.link_stall,
        config: config.clone(),
        ep: ep.clone(),
        store,
        hostname: select::hostname(),
        log: Arc::clone(&log),
        gates: Mutex::default(),
        held: Mutex::default(),
    });
    {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(serve_bridge(Arc::clone(&shared), stream));
                    }
                    Err(e) => {
                        (shared.log)(format!("peer: bridge accept: {e}"));
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });
    }
    {
        let mut r = report.lock().unwrap();
        r.enabled = true;
        r.error = None;
        r.endpoint_id = Some(ep.id().to_string());
        r.bind = Some(config.bind.to_string());
        r.peers = config
            .peers
            .iter()
            .map(|p| PeerView {
                name: p.name.clone(),
                endpoint_id: p.endpoint.to_string(),
                addr: p.addr.map(|a| a.to_string()),
                role: format!("{:?}", p.role).to_lowercase(),
                approve: format!("{:?}", p.approve).to_lowercase(),
            })
            .collect();
    }
    log(format!(
        "peer: endpoint {} listening on {} (LAN-only, no relay/discovery); {} allowlisted peers: {}",
        ep.id(),
        config.bind,
        config.peers.len(),
        config
            .peers
            .iter()
            .map(|p| format!("{} ({:?})", p.name, p.role))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let mut finished = HashSet::new();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        watch_tick(&shared, &mut finished).await;
    }
}

/// Cancel propagation (local cancel -> remote cancel), abnormal local endings, and the ledger
/// link of finished forwarded jobs on both sides.
async fn watch_tick(shared: &Arc<Shared>, finished: &mut HashSet<(bool, i64)>) {
    let paths = shared.paths.clone();
    let Ok((links, inbound)) =
        tokio::task::spawn_blocking(move || (outbound_links(&paths), inbound_records(&paths)))
            .await
    else {
        return;
    };
    for mut link in links {
        let id = link.local_job_id;
        if finished.contains(&(true, id)) {
            continue;
        }
        let socket = shared.socket.clone();
        let Ok(Ok(job)) = tokio::task::spawn_blocking(move || host::job(&socket, id)).await else {
            continue;
        };
        let cancel = job.phase == crate::Phase::Cancelled
            || job.execution.as_ref().is_some_and(|e| e.cancel_requested);
        if !link.done() {
            if cancel && link.remote_job_id.is_some() {
                if let Err(e) = propagate_cancel(shared, id).await {
                    (shared.log)(format!("peer: cancel propagation for job {id}: {e}"));
                }
            } else if job.phase.terminal() && !cancel {
                // The bridge ended without a decided outcome (worker loss, killed bridge).
                // The remote job is NOT cancelled; report what is known.
                link.stage = "done".into();
                link.remote_state = Some(if link.remote_job_id.is_some() {
                    "orphaned".into()
                } else {
                    "not_submitted".into()
                });
                link.error = Some(format!(
                    "local job ended ({:?}) before the peer's result; remote job {:?} on {} was left as is",
                    job.phase, link.remote_job_id, link.peer
                ));
                link.updated_at = task_manager_peer::now_f64();
                let _ = write_json(&shared.paths.link(id), &link);
                shared.release_checkout(id);
            } else if job.phase.terminal() && cancel && link.remote_job_id.is_none() {
                link.stage = "done".into();
                link.remote_state = Some("not_submitted".into());
                link.updated_at = task_manager_peer::now_f64();
                let _ = write_json(&shared.paths.link(id), &link);
                shared.release_checkout(id);
            }
            continue;
        }
        if job.phase.terminal() && !link.ledger_linked {
            // The local job ended without the peer's result (controller restart, killed
            // bridge): ask the peer what became of its job before linking the ledgers.
            if link.remote_state.as_deref() == Some("orphaned") && link.remote_job_id.is_some() {
                if task_manager_peer::now_f64() - link.updated_at < ORPHAN_PROBE_EVERY_S {
                    continue;
                }
                if !reconcile_orphan(shared, &job, &mut link).await {
                    continue;
                }
            }
            let peer_link = outbound_ledger_link(&link);
            let state = shared.paths.state.clone();
            let linked = tokio::task::spawn_blocking(move || {
                crate::Store::open(&state.join("tasks.sqlite"))
                    .and_then(|mut s| write_peer_link(&mut s, id, &peer_link))
                    .map_err(|e| e.to_string())
            })
            .await;
            match linked {
                Ok(Ok(_)) => {
                    link.ledger_linked = true;
                    let _ = write_json(&shared.paths.link(id), &link);
                    finished.insert((true, id));
                }
                Ok(Err(e)) => (shared.log)(format!("peer: ledger link of job {id}: {e}")),
                Err(_) => {}
            }
        } else if link.ledger_linked {
            finished.insert((true, id));
        }
    }
    for mut record in inbound {
        let Some(id) = record.remote_id else { continue };
        if record.state != "submitted" || finished.contains(&(false, id)) {
            continue;
        }
        if record.ledger_linked {
            finished.insert((false, id));
            continue;
        }
        let socket = shared.socket.clone();
        let Ok(Ok(job)) = tokio::task::spawn_blocking(move || host::job(&socket, id)).await else {
            continue;
        };
        if !job.phase.terminal() {
            continue;
        }
        let peer_link = inbound_ledger_link(&record);
        let state = shared.paths.state.clone();
        let linked = tokio::task::spawn_blocking(move || {
            crate::Store::open(&state.join("tasks.sqlite"))
                .and_then(|mut s| write_peer_link(&mut s, id, &peer_link))
                .map_err(|e| e.to_string())
        })
        .await;
        if let Ok(Ok(_)) = linked {
            record.ledger_linked = true;
            let path = shared
                .paths
                .inbound_record(&record.peer_endpoint, &record.key);
            let _ = write_json(&path, &record);
            finished.insert((false, id));
        }
    }
}

/// Seconds between probes of one orphaned link's peer job.
const ORPHAN_PROBE_EVERY_S: f64 = 30.0;

/// One probe of an orphaned link. Records the outcome as the job's `remote_result` and
/// returns true once the link may be closed into the ledger (peer job terminal, or given up
/// after `remote_reconcile::MAX_AGE`); false to probe again later.
async fn reconcile_orphan(shared: &Shared, job: &crate::Job, link: &mut OutboundLink) -> bool {
    let id = link.local_job_id;
    let finished = job
        .execution
        .as_ref()
        .and_then(|e| e.finished_at)
        .unwrap_or(job.submitted_at);
    let expired =
        crate::execution::now() - finished > crate::remote_reconcile::MAX_AGE.as_secs() as i64;
    let probe = probe_remote(shared, link).await;
    let mut result = crate::execution::RemoteResult {
        transport: "iroh".into(),
        worker: link.peer.clone(),
        remote_job_id: link.remote_job_id,
        remote_phase: link.remote_phase.clone(),
        exit_code: None,
        state: "unreachable".into(),
        checked_at: crate::execution::now(),
        detail: String::new(),
    };
    let close = match probe {
        Ok(Probe::Terminal(phase, code)) => {
            link.remote_phase = Some(phase.clone());
            link.exit = code;
            link.remote_state = Some("reconciled".into());
            link.error = Some(format!(
                "local job ended before the peer's result; reconciled: peer job {:?} {phase}",
                link.remote_job_id
            ));
            result.remote_phase = Some(phase);
            result.exit_code = code;
            result.state = "terminal".into();
            result.detail = "probed after the local job was interrupted".into();
            true
        }
        Ok(Probe::Live(phase)) => {
            link.remote_phase = Some(phase.clone());
            result.remote_phase = Some(phase);
            result.state = if expired { "unknown" } else { "running" }.into();
            result.detail = "peer job still live; probed again later".into();
            expired
        }
        Err(e) => {
            result.state = if expired { "unknown" } else { "unreachable" }.into();
            result.detail = e;
            expired
        }
    };
    link.updated_at = task_manager_peer::now_f64();
    let _ = write_json(&shared.paths.link(id), link);
    let state = shared.paths.state.clone();
    let settled = result.settled();
    let note = format!(
        "\n=== tasks peer: after this job was interrupted: {} [{}]; this job stays interrupted, nothing was re-run ===\n",
        result.summary(),
        result.state
    );
    let written = tokio::task::spawn_blocking(move || {
        let written = crate::Store::open(&state.join("tasks.sqlite"))
            .and_then(|mut s| s.write_remote_result(id, &result));
        if settled && matches!(written, Ok(true)) {
            crate::worker::append_job_log(&state, id, note.as_bytes());
        }
        written.map_err(|e| e.to_string())
    })
    .await;
    if let Ok(Err(e)) = written {
        (shared.log)(format!("peer: remote result of job {id}: {e}"));
    }
    close
}

pub fn outbound_ledger_link(link: &OutboundLink) -> PeerLink {
    PeerLink {
        direction: "outbound".into(),
        peer: link.peer.clone(),
        peer_endpoint: link.peer_endpoint.clone(),
        peer_job_id: link.remote_job_id,
        owner: link.owner.clone(),
        key: link.key.clone(),
        transport: "iroh".into(),
        remote_phase: link.remote_phase.clone(),
        remote_state: link.remote_state.clone(),
        sync_s: link.sync_s,
        fetch_s: link.fetch_s,
    }
}

pub fn inbound_ledger_link(record: &InboundRecord) -> PeerLink {
    PeerLink {
        direction: "inbound".into(),
        peer: record.peer.clone(),
        peer_endpoint: record.peer_endpoint.clone(),
        peer_job_id: record.origin_job_id,
        owner: record.owner.clone(),
        key: record.key.clone(),
        transport: "iroh".into(),
        remote_phase: None,
        remote_state: None,
        sync_s: None,
        fetch_s: None,
    }
}

/// `tasks peer status-watch NAME`: the peer's status stream as JSON lines (live snapshots
/// annotated with `machine.online` + `stream`, or `offline`/`status_error` markers), with
/// reconnect backoff 1 s doubling to 30 s, until the bridge goes away.
pub async fn watch_peer(shared: &Shared, name: &str, sink: &mut Sink) {
    let Some(peer) = shared.config.peer(name).cloned() else {
        let _ = sink
            .event(&BridgeEvent::Error {
                message: format!("unknown peer {name:?}"),
            })
            .await;
        return;
    };
    let Some(addr) = peer.addr else {
        let _ = sink
            .event(&BridgeEvent::Error {
                message: format!("peer {name} has no addr"),
            })
            .await;
        return;
    };
    let to = task_manager_peer::peer_addr(peer.endpoint, addr);
    let short = peer.endpoint.to_string();
    let mut machine = peer.name.clone();
    let mut backoff = 1.0f64;
    let mut seq = 0u64;
    loop {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = status::watch_status(&shared.ep, to.clone(), shared.config.connect_timeout, {
            move |event| tx.send(event).is_ok()
        });
        tokio::pin!(watch);
        let result = loop {
            tokio::select! {
                result = &mut watch => break Some(result),
                Some(event) = rx.recv() => {
                    let line = match event {
                        status::StatusEvent::Snapshot { value, .. } => {
                            if let Some(id) = value.pointer("/machine/id").and_then(|v| v.as_str()) {
                                machine = id.to_owned();
                            }
                            seq += 1;
                            backoff = 1.0;
                            status::annotate_live(value, &short, seq, task_manager_peer::now_f64())
                        }
                        status::StatusEvent::Error { error, .. } => {
                            status::marker("status_error", &machine, &short, &error, None)
                        }
                    };
                    if sink.event(&BridgeEvent::Line { value: line }).await.is_err() {
                        break None;
                    }
                }
            }
        };
        let error = match result {
            None => return,
            Some(Ok(_)) => "stream ended".to_string(),
            Some(Err(e)) => e.to_string(),
        };
        let line = status::marker("offline", &machine, &short, &error, Some(backoff));
        if sink
            .event(&BridgeEvent::Line { value: line })
            .await
            .is_err()
        {
            return;
        }
        tokio::time::sleep(Duration::from_secs_f64(backoff)).await;
        backoff = (backoff * 2.0).min(30.0);
    }
}
