//! `loadtable ui`: connects the egui dashboard of `tasks-ui` to the daemon client.

use crate::client::Client;
use std::io::Write;
use std::sync::Arc;
use tasks_ui::link::{DaemonLink, LinkError};

/// A [`Client`] seen as the dashboard's [`DaemonLink`].
struct ClientLink(Arc<dyn Client>);

impl DaemonLink for ClientLink {
    fn call(
        &self,
        req: &tasks_daemon::api::Request,
    ) -> Result<tasks_daemon::api::Response, LinkError> {
        self.0.call(req).map_err(|e| LinkError(e.to_string()))
    }
}

/// Opens the window and returns the process exit code when it closes.
pub(crate) fn run(client: Arc<dyn Client>, env: &dyn Fn(&str) -> Option<String>) -> i32 {
    let who = env("USER")
        .or_else(|| env("LOGNAME"))
        .filter(|u| !u.is_empty())
        .map_or_else(|| "ui".to_string(), |u| format!("ui:{u}"));
    match tasks_ui::app::run(Arc::new(ClientLink(client)), who) {
        Ok(()) => crate::error::EXIT_OK,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "loadtable ui: {e}");
            crate::error::EXIT_FAILURE
        }
    }
}
