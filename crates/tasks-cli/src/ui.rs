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

/// R-832: why the window cannot open in this shell, before winit says it in its own words: on
/// Linux a session with no display (an SSH shell, a headless worker) has none of the variables a
/// window needs. macOS needs none of them.
pub(crate) fn no_display(
    linux: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<&'static str> {
    let set = |k: &&str| env(k).is_some_and(|v| !v.is_empty());
    if !linux
        || ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
            .iter()
            .any(set)
    {
        return None;
    }
    Some(
        "no display here (DISPLAY and WAYLAND_DISPLAY are not set), so the window cannot open; \
         run `loadtable ui` in your desktop session, or use `loadtable status` and \
         `loadtable list --active` from this shell",
    )
}

/// Opens the window and returns the process exit code when it closes.
pub(crate) fn run(client: Arc<dyn Client>, env: &dyn Fn(&str) -> Option<String>) -> i32 {
    if let Some(why) = no_display(cfg!(target_os = "linux"), env) {
        let _ = writeln!(std::io::stderr(), "loadtable ui: {why}");
        return crate::error::EXIT_FAILURE;
    }
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

#[cfg(test)]
mod tests {
    use super::no_display;

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            let (_, v) = vars.iter().find(|(n, _)| *n == k)?;
            Some((*v).to_string())
        }
    }

    #[test]
    fn r832_a_linux_shell_without_a_display_is_told_plainly_and_where_to_go() {
        let why =
            no_display(true, &env_of(&[("USER", "abe"), ("DISPLAY", "")])).unwrap_or_default();
        assert!(why.contains("no display here"), "{why}");
        assert!(why.contains("loadtable status"), "{why}");
    }

    #[test]
    fn r832_any_display_variable_or_macos_lets_the_window_try() {
        for var in ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"] {
            assert_eq!(no_display(true, &env_of(&[(var, ":0")])), None, "{var}");
        }
        assert_eq!(no_display(false, &env_of(&[])), None);
    }
}
