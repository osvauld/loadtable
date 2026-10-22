//! The daemon process: startup, the local socket server and the scheduler loop.
//!
//! JSON lines over a Unix domain socket in the state directory, one std thread per connection
//! (no async runtime). A closed or slow client never changes a job (R-104, R-105, R-106): the
//! connection thread only reads a request, asks the scheduler and writes a reply.

use crate::api::{ApiError, ErrorCode, MAX_REQUEST_BYTES, Reply, parse_request, render_reply};
use crate::audited::{AuditSink, AuditedLedger};
use crate::config::{DaemonConfig, PidLock, raise_nofile};
use crate::error::DaemonError;
use crate::forwarder::{Forwarder, NoForwarder};
use crate::machine_policy::MachinePolicy;
use crate::platform::{Ledger, Platform};
use crate::scheduler::{Scheduler, SchedulerParts};
use crate::startup::{StartupReport, reconcile};
use crate::trust_runtime::{LocalOnly, PeerContext, PeerRuntimeFactory};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tasks_identity::{Keystore, Signer};
use tasks_ledger::{AuditLedger, AuditLog, RetryPolicy, SqliteStore, SystemClock, retry_busy};

/// Most connections served at once.
pub const MAX_CONNECTIONS: usize = 64;
/// A connection idle this long is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// A client that does not read its reply this long is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How to start a daemon.
pub struct DaemonOptions {
    /// Directories and machine name.
    pub config: DaemonConfig,
    /// The daemon's own environment, read once by the caller.
    pub env: Vec<(String, String)>,
    /// Peers; `NoForwarder` until the peer crate plugs in.
    pub forwarder: Arc<dyn Forwarder>,
    /// The operating system; `None` means the real one.
    pub platform: Option<Platform>,
    /// Starts identity, trust and the peer transport once the state is locked. Defaults to
    /// identity and pairing bookkeeping without a transport ([`LocalOnly`]).
    pub peer_runtime: Option<Box<dyn PeerRuntimeFactory>>,
}

impl DaemonOptions {
    /// Real platform, no peers, the process environment.
    pub fn new(config: DaemonConfig) -> DaemonOptions {
        DaemonOptions {
            config,
            env: std::env::vars().collect(),
            forwarder: Arc::new(NoForwarder),
            platform: None,
            peer_runtime: Some(Box::new(LocalOnly)),
        }
    }
}

/// A started daemon: the state directory is locked, the ledger reconciled, the socket bound.
pub struct Daemon {
    sch: Arc<Scheduler>,
    listener: UnixListener,
    _pid: PidLock,
    _peer_keepalive: Option<Box<dyn std::any::Any + Send>>,
    /// What startup reconciliation did.
    pub startup: StartupReport,
}

fn sleep(d: Duration) {
    std::thread::sleep(d);
}

fn binds(path: &Path) -> Result<UnixListener, DaemonError> {
    // We hold the pid lock, so a socket file left here is stale.
    if path.exists() {
        std::fs::remove_file(path).map_err(|e| DaemonError::io(path, e))?;
    }
    let l = UnixListener::bind(path).map_err(|e| DaemonError::io(path, e))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| DaemonError::io(path, e))?;
    Ok(l)
}

impl Daemon {
    /// Starts: marks inherited descriptors close-on-exec (R-089), takes the one-daemon lock,
    /// loads the machine policy, opens the ledger with retries, reconciles jobs left running
    /// by a previous daemon without ever re-running them (R-008), and binds the socket.
    pub fn start(opts: DaemonOptions) -> Result<Daemon, DaemonError> {
        tasks_platform::close_on_exec_inherited()?;
        let cfg = opts.config;
        cfg.create_dirs()?;
        let pid = PidLock::acquire(&cfg.pid_path())?;
        let env = opts.env;
        let lookup = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let policy = MachinePolicy::load(&cfg.policy_path(), &lookup);
        raise_nofile(policy.nofile_target());
        let store = retry_busy(&RetryPolicy::default(), &sleep, || {
            SqliteStore::open(&cfg.db_path())
        })?;
        // The machine identity signs the audit log, so it is opened here, once, and handed on.
        let keystore = Keystore::open_or_create(&cfg.state_dir)
            .map_err(|e| DaemonError::Identity(e.to_string()))?;
        let identity = Arc::new(keystore.identity().clone());
        let store = Arc::new(store);
        let log = AuditLog::new(
            store.clone() as Arc<dyn AuditLedger>,
            Arc::new(SystemClock),
            Signer::did(&*identity),
        )
        .with_signer(identity.clone());
        log.set_checkpoint_every(policy.audit_checkpoint_every());
        let audit = AuditSink::new(Arc::new(log));
        let ledger: Arc<dyn Ledger> = Arc::new(AuditedLedger::new(store, audit.clone()));
        // Identity, trust and the transport start only now: the state is locked, so a stray
        // second daemon never binds an endpoint or touches the keystore.
        let ctx = PeerContext {
            identity,
            created: keystore.created(),
            audit: audit.clone(),
        };
        let peer_rt = match opts.peer_runtime {
            Some(f) => Some(f.start(&cfg, &policy, &ctx)?),
            None => None,
        };
        let forwarder = peer_rt
            .as_ref()
            .and_then(|r| r.forwarder.clone())
            .unwrap_or(opts.forwarder);
        let platform = opts
            .platform
            .unwrap_or_else(|| Platform::native(&cfg.lock_dir));
        let startup = reconcile(&*ledger, &*platform.procs, policy.cancel_grace(), &sleep)?;
        let enabled = std::fs::read_to_string(cfg.intent_path())
            .map(|t| t.trim() != "stopped")
            .unwrap_or(true);
        let listener = binds(&cfg.socket_path())?;
        let sch = Scheduler::new(SchedulerParts {
            cfg,
            policy,
            ledger,
            platform,
            clock: Arc::new(SystemClock),
            forwarder,
            parent_env: env,
            execution_enabled: enabled,
            runner_poll: Duration::from_millis(50),
        });
        sch.set_audit(audit);
        let mut keepalive = None;
        if let Some(rt) = peer_rt {
            sch.set_trust(rt.trust);
            for hook in rt.maintenance {
                sch.add_maintenance(hook);
            }
            if let Some(serve) = rt.serving {
                let store = crate::peer_link::serve::build_store(
                    &sch,
                    sch.config().state_dir.join("peer-store"),
                );
                sch.add_maintenance(crate::peer_link::serve::sweep_hook(&sch, store.clone()));
                serve(crate::peer_link::serve::build_receiver_with(&sch, store));
            }
            keepalive = Some(rt.keepalive);
        }
        Ok(Daemon {
            sch,
            listener,
            _pid: pid,
            _peer_keepalive: keepalive,
            startup,
        })
    }

    /// The receiver for forwarded jobs sent by this daemon's allowlisted peers; hand it to a
    /// transport's serving side. Copies of peers' checkouts live under `<state>/peer-store`.
    pub fn receiver(&self) -> Arc<tasks_peer::Receiver> {
        crate::peer_link::serve::build_receiver(
            &self.sch,
            self.sch.config().state_dir.join("peer-store"),
        )
    }

    /// The scheduler, for tests and embedding.
    pub fn scheduler(&self) -> &Arc<Scheduler> {
        &self.sch
    }

    /// Serves until a shutdown request is accepted (never while jobs run).
    pub fn run(self) -> Result<(), DaemonError> {
        let socket = self.sch.config().socket_path();
        let listener = self
            .listener
            .try_clone()
            .map_err(|e| DaemonError::io(&socket, e))?;
        let accept_sch = self.sch.clone();
        let accept = std::thread::Builder::new()
            .name("accept".into())
            .spawn(move || accept_loop(&accept_sch, &listener))
            .map_err(|e| DaemonError::io(&socket, e))?;
        while !self.sch.is_shutdown() {
            self.sch.tick();
            self.sch.wake().wait(self.sch.policy().tick());
        }
        // unblock the accept loop, which sees the shutdown flag
        let _ = UnixStream::connect(&socket);
        let _ = accept.join();
        let _ = std::fs::remove_file(&socket);
        Ok(())
    }
}

struct ConnGuard(Arc<AtomicUsize>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept_loop(sch: &Arc<Scheduler>, listener: &UnixListener) {
    let active = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        if sch.is_shutdown() {
            break;
        }
        let Ok(stream) = conn else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            let mut s = stream;
            let busy = render_reply(Reply::Err(ApiError::new(
                ErrorCode::ConnectionLimit,
                format!(
                    "too many connections (limit {MAX_CONNECTIONS}); the request was not processed"
                ),
            )));
            let _ = writeln!(s, "{busy}");
            continue;
        }
        let guard = ConnGuard(active.clone());
        let sch = sch.clone();
        let spawned = std::thread::Builder::new()
            .name("conn".into())
            .spawn(move || {
                serve_connection(&sch, stream);
                drop(guard);
            });
        if spawned.is_err() {
            // the guard moved into the failed closure and was dropped with it
            continue;
        }
    }
}

/// Reads requests from one client until it closes, answering each.
pub fn serve_connection(sch: &Arc<Scheduler>, stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut out = stream;
    loop {
        let mut line = Vec::new();
        let limit = MAX_REQUEST_BYTES as u64 + 1;
        let n = match reader.by_ref().take(limit).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if n > MAX_REQUEST_BYTES && !line.ends_with(b"\n") {
            let e = ApiError::new(
                ErrorCode::TooLarge,
                format!("request exceeds {MAX_REQUEST_BYTES} bytes"),
            );
            let _ = writeln!(out, "{}", render_reply(Reply::Err(e)));
            return; // cannot resynchronise after an oversize line
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        let reply = match parse_request(&line) {
            Ok(req) => sch.handle(req),
            Err(e) => Reply::Err(e),
        };
        // A failed write means the client is gone; the job it asked about is unaffected.
        if writeln!(out, "{}", render_reply(reply)).is_err() {
            return;
        }
    }
}
