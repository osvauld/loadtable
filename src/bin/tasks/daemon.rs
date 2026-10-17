use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::{
    fs::PermissionsExt,
    net::{UnixListener, UnixStream},
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tinline_task_manager::metrics::{StatusResponse, sampler::Sampler};
use tinline_task_manager::peer_link::{PeersReport, PeersResponse};
use tinline_task_manager::{
    Action, PROTOCOL_VERSION, Request, Response, Result, Store, handle, state_dir, worker,
};

/// Connections beyond this are refused so a flood cannot spawn unbounded threads.
const MAX_CONNECTIONS: usize = 64;

pub fn run() -> Result<()> {
    let state = state_dir();
    fs::create_dir_all(&state)?;
    if fs::symlink_metadata(&state)?.file_type().is_symlink() {
        return Err("state directory must not be a symlink".into());
    }
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
    let lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join("daemon.lock"))?;
    lock.try_lock()
        .map_err(|_| "another daemon owns this state directory")?;
    let socket = state.join("taskd.sock");
    // We hold the daemon lock, so no live daemon owns a leftover socket: it is stale.
    if fs::symlink_metadata(&socket).is_ok() {
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let mut ledger = Store::open(&state.join("tasks.sqlite"))?;
    // Only reconcile after acquiring worker.lock; a live supervisor retains that lease.
    let worker_lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join("worker.lock"))?;
    if worker_lock.try_lock().is_ok() {
        ledger.reconcile_interrupted()?;
        worker_lock.unlock()?;
    }
    let store = Arc::new(Mutex::new(ledger));
    let active = Arc::new(AtomicUsize::new(0));
    // In-memory machine/job sampling for `tasks status` (2 s ticks, own ledger connection).
    let sampler = Sampler::start(state.clone());
    // Submission policy: report it (and any error loudly) once at startup; submissions read
    // it fresh, and an error there falls back to the built-in defaults (never disables it).
    let policy = tinline_task_manager::policy::effective();
    for line in policy.summary().lines() {
        eprintln!("{} {line}", tinline_task_manager::execution::now());
    }
    // Peer link (iroh) in its own thread/runtime, only when a peer config exists. It talks to
    // this daemon through the socket below like any client; nothing here waits for it.
    let peers = tinline_task_manager::peer::runtime::start(&state, &socket);
    eprintln!(
        "taskd: socket={} (execution requires per-job approval and explicit local worker)",
        socket.display()
    );
    for incoming in listener.incoming() {
        let mut stream = match incoming {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("accept: {error}");
                continue;
            }
        };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            reply(
                &mut stream,
                &error_response("daemon busy: too many connections"),
            );
            continue;
        }
        let store = store.clone();
        let active = active.clone();
        let sampler = sampler.clone();
        let peers = peers.clone();
        std::thread::spawn(move || {
            serve(stream, &store, &sampler, &peers);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

/// Socket I/O happens without the store lock; only `handle` is serialized.
fn serve(
    mut stream: UnixStream,
    store: &Mutex<Store>,
    sampler: &Sampler,
    peers: &Mutex<PeersReport>,
) {
    let response = (|| -> Result<Option<Response>> {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
        let mut line = String::new();
        let read = BufReader::new((&mut stream).take(64 * 1024)).read_line(&mut line)?;
        // Connect-and-close is the liveness probe used by `ensure_daemon`; nothing to answer.
        if read == 0 {
            return Ok(None);
        }
        if !line.ends_with('\n') {
            return Err("incomplete or oversized request".into());
        }
        let request: Request = serde_json::from_str(&line)?;
        // SQLite rolls back an interrupted transaction, so a poisoned lock still guards a
        // consistent store.
        let mut store = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // A dead worker may have gone away while this daemon stayed alive. The stage
        // supervisor/command retains worker.lock through cleanup; never reconcile sooner.
        let state = state_dir();
        let lease = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state.join("worker.lock"))?;
        if lease.try_lock().is_ok() {
            store.reconcile_interrupted()?;
            lease.unlock()?;
        }
        let action = request.action.clone();
        let valid = request.version == PROTOCOL_VERSION;
        if valid && matches!(action, Action::Status) {
            // Own reply shape; the ledger is only read, then released before assembly.
            let jobs = store.jobs();
            drop(store);
            let reply = match jobs {
                Ok(jobs) => StatusResponse {
                    version: PROTOCOL_VERSION,
                    error: None,
                    status: Some(sampler.snapshot(&jobs)),
                },
                Err(error) => StatusResponse {
                    version: PROTOCOL_VERSION,
                    error: Some(error.to_string()),
                    status: None,
                },
            };
            write_line(&mut stream, &reply);
            return Ok(None);
        }
        if valid && matches!(action, Action::Peers) {
            drop(store);
            let mut report = peers.lock().unwrap_or_else(|p| p.into_inner()).clone();
            report.refresh(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
            );
            let paths = tinline_task_manager::peer::records::Paths::new(&state);
            let tail = |mut v: Vec<serde_json::Value>| {
                let skip = v.len().saturating_sub(50);
                v.drain(..skip);
                v
            };
            report.forwarded = tail(
                tinline_task_manager::peer::records::outbound_links(&paths)
                    .iter()
                    .filter_map(|l| serde_json::to_value(l).ok())
                    .collect(),
            );
            report.inbound = tail(
                tinline_task_manager::peer::records::inbound_records(&paths)
                    .iter()
                    .filter_map(|r| serde_json::to_value(r).ok())
                    .collect(),
            );
            write_line(
                &mut stream,
                &PeersResponse {
                    version: PROTOCOL_VERSION,
                    error: None,
                    peers: Some(report),
                },
            );
            return Ok(None);
        }
        let mut response = if valid && matches!(action, Action::StartWorker | Action::Logs(_)) {
            handle(
                &mut store,
                Request {
                    version: PROTOCOL_VERSION,
                    action: Action::List,
                },
            )
        } else {
            handle(&mut store, request)
        };
        drop(store);
        if valid {
            let extra = match action {
                Action::StartWorker => worker::start_detached(),
                Action::Logs(id) => worker::logs(id).map(|log| response.log = Some(log)),
                _ => Ok(()),
            };
            if let Err(error) = extra {
                response.error = Some(error.to_string());
            }
        }
        response.execution_enabled = worker::running(&state_dir());
        Ok(Some(response))
    })()
    .unwrap_or_else(|error| Some(error_response(&error.to_string())));
    if let Some(response) = response {
        reply(&mut stream, &response);
    }
}

fn error_response(error: &str) -> Response {
    Response {
        version: PROTOCOL_VERSION,
        execution_enabled: false,
        submitted_id: None,
        jobs: Vec::new(),
        estimates: Vec::new(),
        error: Some(error.to_string()),
        log: None,
    }
}

fn reply(stream: &mut UnixStream, response: &Response) {
    write_line(stream, response);
}

fn write_line<T: serde::Serialize>(stream: &mut UnixStream, response: &T) {
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    if let Err(error) = serde_json::to_writer(&mut *stream, response)
        .and_then(|_| stream.write_all(b"\n").map_err(serde_json::Error::io))
    {
        eprintln!("reply: {error}");
    }
}
