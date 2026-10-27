//! The daemon socket client: a [`Client`] trait with a real Unix-socket implementation.
//!
//! Rules this module keeps:
//! * R-102: each attempt waits longer than the daemon's ledger busy timeout; a slow or
//!   EAGAIN read is retried inside a total budget, but only for read-only requests.
//! * R-103: a missing socket starts the daemon before anything is sent; a live socket never
//!   does; a startup failure is reported and nothing is sent.
//! * R-099 / R-100: a mutation whose reply was lost is reported as lost and never resent.
//! * R-028: a reply over the frame cap is an error, never a truncation.
//! * An unknown protocol version names both versions.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tasks_daemon::api::{
    ApiError, ErrorCode, MAX_REQUEST_BYTES, PROTOCOL_VERSION, ReplyEnvelope, Request,
    RequestEnvelope, Response,
};
use tasks_platform::{Service, ServiceStatus};

/// Reply frame cap for ordinary requests. The daemon never sends more than
/// `MAX_RESPONSE_BYTES` (4 MiB); this leaves room for a newer daemon's slack.
pub const DEFAULT_FRAME_CAP: usize = 8 << 20;
/// Frame cap for `list --json`, which asks for the whole ledger window (R-028).
pub const JSON_LIST_FRAME_CAP: usize = 32 << 20;

/// Why a call to the daemon failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The client could not work out where the daemon lives.
    #[error("{0}")]
    Setup(String),
    /// The socket was missing or dead and the daemon would not start. Nothing was sent.
    #[error("cannot start the daemon: {0}")]
    StartFailed(String),
    /// The socket could not be reached. Nothing was sent.
    #[error("cannot reach the daemon at {}: {source}", path.display())]
    Connect {
        /// The socket.
        path: PathBuf,
        /// The cause.
        source: io::Error,
    },
    /// The request may have been delivered but no reply arrived.
    #[error("no reply from the daemon ({detail})")]
    ReplyLost {
        /// What went wrong.
        detail: String,
    },
    /// The reply is larger than the client accepts (R-028).
    #[error(
        "the daemon's reply is larger than this client's frame cap of {cap} bytes and was not read; ask for less (a smaller --limit or --tail)"
    )]
    FrameTooLarge {
        /// The cap.
        cap: usize,
    },
    /// The daemon speaks another protocol version.
    #[error(
        "protocol version mismatch: this client speaks version {ours}, the daemon speaks version {}; use a loadtable built from the same release as the daemon",
        theirs.map_or_else(|| "unknown".to_string(), |v| v.to_string())
    )]
    Version {
        /// This client's version.
        ours: u32,
        /// The daemon's, when it said.
        theirs: Option<u32>,
    },
    /// The request does not fit the daemon's request limit.
    #[error("the request is {bytes} bytes; the daemon accepts at most {limit}")]
    RequestTooLarge {
        /// Size of the request line.
        bytes: usize,
        /// The limit.
        limit: usize,
    },
    /// The reply is not a valid reply.
    #[error("the daemon sent a reply this client cannot read: {0}")]
    BadReply(String),
    /// The daemon answered with a named refusal or failure.
    #[error("{}", .0.message)]
    Daemon(ApiError),
}

impl ClientError {
    /// The daemon's error code, when the daemon refused.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            ClientError::Daemon(e) => Some(e.code),
            _ => None,
        }
    }
}

/// Requests that change nothing: safe to resend (R-102).
pub fn is_read_only(req: &Request) -> bool {
    matches!(
        req,
        Request::Hello
            | Request::List { .. }
            | Request::Get { .. }
            | Request::Logs { .. }
            | Request::LogSearch { .. }
            | Request::Status
            | Request::Estimate { .. }
            | Request::JobMetrics { .. }
            | Request::History { .. }
            | Request::Audit { .. }
            | Request::AuditVerify
    )
}

/// Talks to the daemon. A fake implements it for tests.
pub trait Client: Send + Sync {
    /// Sends one request and returns the daemon's answer.
    fn call(&self, req: &Request) -> Result<Response, ClientError>;

    /// Like [`Client::call`] with a larger reply frame cap.
    fn call_capped(&self, req: &Request, _cap: usize) -> Result<Response, ClientError> {
        self.call(req)
    }
}

/// A client that fails every call with the same setup error (no HOME, bad directory).
#[derive(Debug, Clone)]
pub struct UnavailableClient(pub String);

impl Client for UnavailableClient {
    fn call(&self, _req: &Request) -> Result<Response, ClientError> {
        Err(ClientError::Setup(self.0.clone()))
    }
}

/// Starts a daemon when the socket is missing.
pub trait DaemonStarter: Send + Sync {
    /// Starts the daemon and returns once it accepts connections, or says why it cannot.
    fn start(&self) -> Result<(), String>;
}

/// Reply and retry timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// How long one attempt waits for a reply (R-102: longer than the ledger busy timeout).
    pub attempt: Duration,
    /// How long a read-only request keeps retrying.
    pub total: Duration,
    /// Pause between attempts.
    pub pause: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            attempt: Duration::from_secs(60),
            total: Duration::from_secs(180),
            pause: Duration::from_millis(200),
        }
    }
}

/// The real client: JSON lines over the daemon's Unix socket.
pub struct UnixClient {
    socket: PathBuf,
    timeouts: Timeouts,
    starter: Option<Arc<dyn DaemonStarter>>,
    cap: usize,
}

enum Attempt {
    Reply(Result<Response, ApiError>),
    /// The request cannot have been processed.
    NotSent(ClientError),
    /// The request may have been processed; no usable reply.
    Lost(String),
    Fatal(ClientError),
}

impl UnixClient {
    /// A client for `socket` with default timing and no autostart.
    pub fn new(socket: impl Into<PathBuf>) -> UnixClient {
        UnixClient {
            socket: socket.into(),
            timeouts: Timeouts::default(),
            starter: None,
            cap: DEFAULT_FRAME_CAP,
        }
    }

    /// Sets the timing.
    pub fn with_timeouts(mut self, t: Timeouts) -> UnixClient {
        self.timeouts = t;
        self
    }

    /// Sets the daemon starter used when the socket is missing (R-103).
    pub fn with_starter(mut self, s: Arc<dyn DaemonStarter>) -> UnixClient {
        self.starter = Some(s);
        self
    }

    /// The socket this client uses.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn connect(&self, started: &mut bool) -> Result<UnixStream, ClientError> {
        match UnixStream::connect(&self.socket) {
            Ok(s) => Ok(s),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                let Some(starter) = self.starter.as_ref().filter(|_| !*started) else {
                    return Err(ClientError::Connect {
                        path: self.socket.clone(),
                        source: e,
                    });
                };
                *started = true;
                starter.start().map_err(ClientError::StartFailed)?;
                UnixStream::connect(&self.socket).map_err(|source| ClientError::Connect {
                    path: self.socket.clone(),
                    source,
                })
            }
            Err(source) => Err(ClientError::Connect {
                path: self.socket.clone(),
                source,
            }),
        }
    }

    fn attempt(&self, line: &[u8], cap: usize, remaining: Duration, started: &mut bool) -> Attempt {
        let mut stream = match self.connect(started) {
            Ok(s) => s,
            Err(e @ ClientError::StartFailed(_)) => return Attempt::Fatal(e),
            Err(e) => return Attempt::NotSent(e),
        };
        let wait = self
            .timeouts
            .attempt
            .min(remaining)
            .max(Duration::from_millis(1));
        let _ = stream.set_read_timeout(Some(wait));
        let _ = stream.set_write_timeout(Some(wait));
        if let Err(e) = stream.write_all(line).and_then(|()| stream.flush()) {
            return Attempt::Lost(format!("sending the request failed: {e}"));
        }
        let deadline = Instant::now() + wait;
        let reply = match read_line(&mut stream, cap, deadline) {
            Ok(l) => l,
            Err(ReadError::TooLarge) => return Attempt::Fatal(ClientError::FrameTooLarge { cap }),
            Err(ReadError::Timeout) => {
                return Attempt::Lost(format!("no reply within {}s", wait.as_secs_f32()));
            }
            Err(ReadError::Closed) => {
                return Attempt::Lost("the daemon closed the connection".into());
            }
            Err(ReadError::Io(e)) => {
                return Attempt::Lost(format!("reading the reply failed: {e}"));
            }
        };
        parse_reply(&reply)
    }
}

fn parse_reply(line: &[u8]) -> Attempt {
    let value: serde_json::Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(e) => return Attempt::Fatal(ClientError::BadReply(format!("not JSON: {e}"))),
    };
    let v = value.get("v").and_then(serde_json::Value::as_u64);
    if v != Some(u64::from(PROTOCOL_VERSION)) {
        return Attempt::Fatal(ClientError::Version {
            ours: PROTOCOL_VERSION,
            theirs: v.and_then(|v| u32::try_from(v).ok()),
        });
    }
    match serde_json::from_value::<ReplyEnvelope>(value) {
        Ok(env) => match env.reply {
            tasks_daemon::api::Reply::Ok(r) => Attempt::Reply(Ok(r)),
            tasks_daemon::api::Reply::Err(e) if e.code == ErrorCode::UnsupportedVersion => {
                Attempt::Fatal(ClientError::Version {
                    ours: PROTOCOL_VERSION,
                    theirs: e.supported,
                })
            }
            tasks_daemon::api::Reply::Err(e) => Attempt::Reply(Err(e)),
        },
        Err(e) => Attempt::Fatal(ClientError::BadReply(e.to_string())),
    }
}

enum ReadError {
    TooLarge,
    Timeout,
    Closed,
    Io(io::Error),
}

/// Reads up to the first newline. More than `cap` bytes of line is [`ReadError::TooLarge`].
fn read_line(stream: &mut UnixStream, cap: usize, deadline: Instant) -> Result<Vec<u8>, ReadError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if Instant::now() >= deadline {
            return Err(ReadError::Timeout);
        }
        let n = match stream.read(&mut chunk) {
            Ok(0) => return Err(ReadError::Closed),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(ReadError::Timeout);
            }
            Err(e) => return Err(ReadError::Io(e)),
        };
        let got = chunk.get(..n).unwrap_or(&[]);
        match got.iter().position(|b| *b == b'\n') {
            Some(end) => {
                buf.extend_from_slice(got.get(..end).unwrap_or(&[]));
                return if buf.len() > cap {
                    Err(ReadError::TooLarge)
                } else {
                    Ok(buf)
                };
            }
            None => {
                buf.extend_from_slice(got);
                if buf.len() > cap {
                    return Err(ReadError::TooLarge);
                }
            }
        }
    }
}

impl Client for UnixClient {
    fn call(&self, req: &Request) -> Result<Response, ClientError> {
        self.call_capped(req, self.cap)
    }

    fn call_capped(&self, req: &Request, cap: usize) -> Result<Response, ClientError> {
        let mut line = serde_json::to_vec(&RequestEnvelope {
            v: PROTOCOL_VERSION,
            request: req.clone(),
        })
        .map_err(|e| ClientError::BadReply(format!("cannot encode the request: {e}")))?;
        if line.len() > MAX_REQUEST_BYTES {
            return Err(ClientError::RequestTooLarge {
                bytes: line.len(),
                limit: MAX_REQUEST_BYTES,
            });
        }
        line.push(b'\n');
        let read_only = is_read_only(req);
        let begin = Instant::now();
        let mut started = false;
        loop {
            let remaining = self.timeouts.total.saturating_sub(begin.elapsed());
            let again = begin.elapsed() + self.timeouts.pause < self.timeouts.total;
            match self.attempt(&line, cap, remaining, &mut started) {
                Attempt::Reply(Ok(r)) => return Ok(r),
                Attempt::Reply(Err(e)) => {
                    // R-101: only `connection_limit` says the request was never read. `busy`
                    // is a real answer (a shutdown refused while jobs run) and is not resent.
                    if e.code == ErrorCode::ConnectionLimit && again {
                        std::thread::sleep(self.timeouts.pause);
                        continue;
                    }
                    return Err(ClientError::Daemon(e));
                }
                Attempt::Fatal(e) => return Err(e),
                Attempt::NotSent(e) => {
                    // nothing was sent: any request may try again, within the budget, when the
                    // failure is transient (a full accept backlog is EAGAIN)
                    let transient = matches!(
                        &e,
                        ClientError::Connect { source, .. }
                            if matches!(
                                source.kind(),
                                io::ErrorKind::WouldBlock
                                    | io::ErrorKind::TimedOut
                                    | io::ErrorKind::Interrupted
                            )
                    );
                    if again && transient {
                        std::thread::sleep(self.timeouts.pause);
                        continue;
                    }
                    return Err(e);
                }
                Attempt::Lost(detail) => {
                    // R-100: a delivered mutation is never replayed
                    if read_only && again {
                        std::thread::sleep(self.timeouts.pause);
                        continue;
                    }
                    return Err(ClientError::ReplyLost { detail });
                }
            }
        }
    }
}

/// Starts `program args` detached from this process, with its output in `log`, then waits for
/// `socket` to accept connections (R-103, R-089).
pub struct ProcessStarter {
    /// The program, normally this executable.
    pub program: PathBuf,
    /// Its arguments, normally `daemon serve ...`.
    pub args: Vec<String>,
    /// Where the daemon's output goes.
    pub log: PathBuf,
    /// The socket to wait for.
    pub socket: PathBuf,
    /// How long to wait for the socket.
    pub wait: Duration,
}

impl ProcessStarter {
    fn log_tail(&self) -> String {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let from = lines.len().saturating_sub(5);
        lines.get(from..).unwrap_or(&[]).join("\n")
    }
}

impl DaemonStarter for ProcessStarter {
    fn start(&self) -> Result<(), String> {
        use std::os::unix::process::CommandExt;
        // R-089: the daemon must not inherit this process's lock or pipe descriptors.
        let _ = tasks_platform::close_on_exec_inherited();
        if let Some(dir) = self.log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .map_err(|e| format!("cannot open the daemon log {}: {e}", self.log.display()))?;
        let log2 = log
            .try_clone()
            .map_err(|e| format!("cannot open the daemon log {}: {e}", self.log.display()))?;
        let mut child = std::process::Command::new(&self.program)
            .args(&self.args)
            .stdin(std::process::Stdio::null())
            .stdout(log)
            .stderr(log2)
            .process_group(0)
            .spawn()
            .map_err(|e| format!("cannot run {}: {e}", self.program.display()))?;
        let deadline = Instant::now() + self.wait;
        loop {
            if UnixStream::connect(&self.socket).is_ok() {
                return Ok(());
            }
            if let Ok(Some(status)) = child.try_wait() {
                // another daemon may have won the race to this state directory
                if UnixStream::connect(&self.socket).is_ok() {
                    return Ok(());
                }
                return Err(format!(
                    "the daemon exited ({status}) before it listened on {}; log {}:\n{}",
                    self.socket.display(),
                    self.log.display(),
                    self.log_tail()
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the daemon did not listen on {} within {}s; see {}",
                    self.socket.display(),
                    self.wait.as_secs(),
                    self.log.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Starts the daemon through the user's service manager when a service for it is installed, so
/// that no second daemon runs behind it (R-682); with none installed it defers to `fallback`.
pub struct ServiceStarter<S: Service> {
    /// The platform's service manager.
    pub service: Arc<S>,
    /// The installed service's name.
    pub name: String,
    /// What a person calls the manager in messages ("systemd", "launchd").
    pub manager: &'static str,
    /// Starts the daemon directly when no service is installed.
    pub fallback: Arc<dyn DaemonStarter>,
    /// The socket to wait for.
    pub socket: PathBuf,
    /// How long to wait for the socket after asking.
    pub wait: Duration,
}

impl<S: Service + Send + Sync> DaemonStarter for ServiceStarter<S> {
    fn start(&self) -> Result<(), String> {
        let status = self.service.status(&self.name).map_err(|e| {
            format!(
                "daemon down; cannot ask {} about {}: {e}",
                self.manager, self.name
            )
        })?;
        match status {
            ServiceStatus::NotInstalled => return self.fallback.start(),
            // up but not listening yet: starting it again would be noise
            ServiceStatus::Running => {}
            ServiceStatus::Stopped => self
                .service
                .start(&self.name)
                .map_err(|e| format!("daemon down; asked {} to start it: {e}", self.manager))?,
        }
        let deadline = Instant::now() + self.wait;
        loop {
            if UnixStream::connect(&self.socket).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "daemon down; asked {} to start it, but it did not listen on {} within {}s \
                     (see `{}` for why)",
                    self.manager,
                    self.socket.display(),
                    self.wait.as_secs(),
                    if self.manager == "systemd" {
                        format!("journalctl --user -u {}", self.name)
                    } else {
                        "the daemon log in the state directory".to_string()
                    }
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests;
