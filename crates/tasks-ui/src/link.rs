//! The only way the dashboard reaches the daemon: one request, one reply.
//!
//! `tasks-cli` adapts its `Client` to [`DaemonLink`]; tests use [`FakeLink`].

use std::sync::Mutex;
use tasks_daemon::api::{Request, Response};

/// Why a call failed, in words for the window's status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkError(pub String);

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A connection to the daemon. Must be callable from several threads.
pub trait DaemonLink: Send + Sync {
    /// Sends one request and returns the daemon's answer.
    fn call(&self, req: &Request) -> Result<Response, LinkError>;
}

type Handler = Box<dyn Fn(&Request) -> Result<Response, LinkError> + Send + Sync>;

/// A scripted link for tests: answers from a function and records every request.
pub struct FakeLink {
    handler: Handler,
    seen: Mutex<Vec<Request>>,
}

impl FakeLink {
    /// A link answering with `handler`.
    pub fn from_fn(
        handler: impl Fn(&Request) -> Result<Response, LinkError> + Send + Sync + 'static,
    ) -> FakeLink {
        FakeLink {
            handler: Box::new(handler),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Every request received so far, oldest first.
    pub fn requests(&self) -> Vec<Request> {
        self.seen.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl DaemonLink for FakeLink {
    fn call(&self, req: &Request) -> Result<Response, LinkError> {
        if let Ok(mut s) = self.seen.lock() {
            s.push(req.clone());
        }
        (self.handler)(req)
    }
}
