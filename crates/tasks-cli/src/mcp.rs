//! `tasks2 mcp`: the MCP server over this crate's daemon [`Client`].
//!
//! `tasks-mcp` cannot depend on this crate (this crate depends on it), so it defines its own
//! `Daemon` trait; [`ClientDaemon`] adapts a [`Client`] to it. Autostart and the retry rules
//! (R-102, R-103) therefore stay in the client, whatever front end calls it.

use crate::client::{Client, ClientError};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tasks_daemon::api::{Request, Response};
use tasks_mcp::{Config, Daemon, DaemonError, Server, Tools};

/// A [`Client`] seen as the MCP server's [`Daemon`].
pub struct ClientDaemon(pub Arc<dyn Client>);

/// How a client failure is told to the MCP server.
pub fn map_error(e: ClientError) -> DaemonError {
    match e {
        ClientError::Daemon(api) => DaemonError::Refused(api),
        ClientError::ReplyLost { detail } => DaemonError::ReplyLost(detail),
        ClientError::Setup(m) => DaemonError::Unreachable(m),
        e @ (ClientError::StartFailed(_) | ClientError::Connect { .. }) => {
            DaemonError::Unreachable(e.to_string())
        }
        other => DaemonError::Failed(other.to_string()),
    }
}

impl Daemon for ClientDaemon {
    fn call(&self, req: &Request) -> Result<Response, DaemonError> {
        self.0.call(req).map_err(map_error)
    }
}

/// Serves MCP on `input` and `output` over `client` until `input` closes.
pub fn serve<R: BufRead, W: Write + Send>(
    client: Arc<dyn Client>,
    cwd: PathBuf,
    env: &dyn Fn(&str) -> Option<String>,
    input: R,
    output: W,
) -> std::io::Result<()> {
    let config = Config::from_env(cwd, env);
    let tools = Tools::new(Arc::new(ClientDaemon(client)), config);
    Server::new(tools).serve(input, output)
}

/// Serves MCP on the process's stdin and stdout. Returns the exit code.
pub fn run_stdio(
    client: Arc<dyn Client>,
    cwd: PathBuf,
    env: &dyn Fn(&str) -> Option<String>,
) -> i32 {
    let stdin = std::io::stdin();
    match serve(client, cwd, env, stdin.lock(), std::io::stdout()) {
        Ok(()) => crate::error::EXIT_OK,
        Err(e) => {
            eprintln!("tasks2 mcp: {e}");
            crate::error::EXIT_FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::UnavailableClient;
    use crate::fake::FakeClient;
    use tasks_daemon::api::{ApiError, ErrorCode};

    struct Shared(Arc<FakeClient>);
    impl Client for Shared {
        fn call(&self, req: &Request) -> Result<Response, ClientError> {
            self.0.call(req)
        }
    }

    fn run(client: Arc<dyn Client>, input: &str) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        serve(
            client,
            PathBuf::from("/w"),
            &|k| (k == "TASKS_WHO").then(|| "me".to_string()),
            input.as_bytes(),
            &mut out,
        )
        .unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn client_failures_map_to_the_mcp_error_kinds() {
        assert!(matches!(
            map_error(ClientError::Setup("x".into())),
            DaemonError::Unreachable(_)
        ));
        assert!(matches!(
            map_error(ClientError::StartFailed("x".into())),
            DaemonError::Unreachable(_)
        ));
        assert!(matches!(
            map_error(ClientError::ReplyLost { detail: "x".into() }),
            DaemonError::ReplyLost(_)
        ));
        assert!(matches!(
            map_error(ClientError::Daemon(ApiError::new(
                ErrorCode::NotFound,
                "no"
            ))),
            DaemonError::Refused(_)
        ));
        assert!(matches!(
            map_error(ClientError::FrameTooLarge { cap: 1 }),
            DaemonError::Failed(_)
        ));
    }

    #[test]
    fn a_session_runs_over_the_cli_client() {
        let fake = Arc::new(FakeClient::from_fn(|_| {
            Err(ClientError::Daemon(ApiError::new(
                ErrorCode::NotFound,
                "no job 9",
            )))
        }));
        let replies = run(
            Arc::new(Shared(Arc::clone(&fake))),
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get","arguments":{"id":9}}}"#,
                "\n",
            ),
        );
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(replies[1]["result"]["isError"], true);
        assert_eq!(
            replies[1]["result"]["structuredContent"]["error"]["kind"],
            "not_found"
        );
        assert_eq!(fake.requests().len(), 1);
    }

    #[test]
    fn an_unavailable_daemon_is_a_clean_tool_error() {
        let replies = run(
            Arc::new(UnavailableClient("no HOME".into())),
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"status","arguments":{}}}"#,
                "\n"
            ),
        );
        assert_eq!(replies[0]["result"]["isError"], true);
        assert_eq!(
            replies[0]["result"]["structuredContent"]["error"]["kind"],
            "daemon_unreachable"
        );
    }
}
