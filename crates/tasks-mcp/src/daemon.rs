//! The daemon as this crate sees it: one call, four kinds of failure.
//!
//! `tasks-cli` depends on this crate (for `loadtable mcp`), so this crate cannot name the CLI's
//! `Client` trait. The CLI adapts its client to [`Daemon`], which keeps the autostart and retry
//! rules (R-102, R-103) in one place: the client.

use std::sync::Mutex;
use tasks_daemon::api::{ApiError, Request, Response};

/// Why a call to the daemon failed.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DaemonError {
    /// The daemon could not be reached or started. Nothing was sent.
    #[error("{0}")]
    Unreachable(String),
    /// The request may have been delivered but no reply arrived. Never resent for mutations.
    #[error("no reply from the daemon ({0})")]
    ReplyLost(String),
    /// The daemon answered with a named refusal or failure.
    #[error("{}", .0.message)]
    Refused(ApiError),
    /// Anything else (unreadable reply, version mismatch, oversize request).
    #[error("{0}")]
    Failed(String),
}

/// Talks to the daemon. Shared between request threads.
pub trait Daemon: Send + Sync {
    /// Sends one request and returns the daemon's answer.
    fn call(&self, req: &Request) -> Result<Response, DaemonError>;
}

type Handler = Box<dyn Fn(&Request) -> Result<Response, DaemonError> + Send + Sync>;

/// A scripted [`Daemon`] for tests: records every request and answers from a function.
pub struct FakeDaemon {
    handler: Handler,
    seen: Mutex<Vec<Request>>,
}

impl FakeDaemon {
    /// A fake that answers with `handler`.
    pub fn from_fn(
        handler: impl Fn(&Request) -> Result<Response, DaemonError> + Send + Sync + 'static,
    ) -> FakeDaemon {
        FakeDaemon {
            handler: Box::new(handler),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.seen
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }
}

impl Daemon for FakeDaemon {
    fn call(&self, req: &Request) -> Result<Response, DaemonError> {
        match self.seen.lock() {
            Ok(mut s) => s.push(req.clone()),
            Err(e) => e.into_inner().push(req.clone()),
        }
        (self.handler)(req)
    }
}

impl<T: Daemon + ?Sized> Daemon for std::sync::Arc<T> {
    fn call(&self, req: &Request) -> Result<Response, DaemonError> {
        (**self).call(req)
    }
}
