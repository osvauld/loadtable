//! The MCP front end of the tasks coordinator: a stdio JSON-RPC server that lets coding agents
//! submit builds and tests, wait on them, read logs and check the machine.
//!
//! The server talks to the daemon through the [`Daemon`] trait, which `tasks-cli` implements
//! over its own `Client` (autostart and retry rules included) and tests implement with
//! [`FakeDaemon`].

pub mod cancel;
pub mod daemon;
pub mod defs;
pub mod rpc;
pub mod rules;
pub mod server;
pub mod text;
pub mod tools;

pub use daemon::{Daemon, DaemonError, FakeDaemon};
pub use defs::{TOOL_NAMES, tool_defs};
pub use server::{Server, negotiate_version};
pub use tools::{Config, Tools};

use std::sync::Arc;

/// Serves MCP on the process's standard input and output until stdin closes. Returns the exit
/// code. Diagnostics go to stderr; stdout carries JSON-RPC only.
pub fn run_stdio(daemon: Arc<dyn Daemon>, config: Config) -> i32 {
    let server = Server::new(Tools::new(daemon, config));
    let stdin = std::io::stdin();
    match server.serve(stdin.lock(), std::io::stdout()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("tasks mcp: {e}");
            1
        }
    }
}
