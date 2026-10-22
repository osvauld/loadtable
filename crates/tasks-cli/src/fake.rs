//! A scripted [`Client`] for tests: records every request and answers from a function.

use crate::client::{Client, ClientError};
use std::sync::Mutex;
use tasks_daemon::api::{Request, Response};

type Handler = Box<dyn Fn(&Request) -> Result<Response, ClientError> + Send + Sync>;

/// Answers requests with `handler` and remembers them in order.
pub struct FakeClient {
    handler: Handler,
    seen: Mutex<Vec<Request>>,
}

impl FakeClient {
    /// A fake that answers with `handler`.
    pub fn from_fn(
        handler: impl Fn(&Request) -> Result<Response, ClientError> + Send + Sync + 'static,
    ) -> FakeClient {
        FakeClient {
            handler: Box::new(handler),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Every request received so far.
    pub fn requests(&self) -> Vec<Request> {
        self.seen.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl Client for FakeClient {
    fn call(&self, req: &Request) -> Result<Response, ClientError> {
        if let Ok(mut s) = self.seen.lock() {
            s.push(req.clone());
        }
        (self.handler)(req)
    }
}
