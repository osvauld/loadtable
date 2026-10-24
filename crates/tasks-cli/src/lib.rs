//! `tasks2`: the command line client of the tasks coordinator.
//!
//! The library holds everything testable: argument parsing ([`args`]), the daemon [`client`]
//! (a trait with a Unix-socket implementation and a [`fake`]), the [`commands`], and pure
//! [`render`]ing. `main.rs` only forwards the process environment to [`real_main`].

pub mod args;
pub mod audit;
pub mod client;
pub mod commands;
pub mod device;
pub mod error;
pub mod fake;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod render;
pub mod runtime;
#[cfg(test)]
mod testutil;
pub mod trust;
#[cfg(feature = "ui")]
mod ui;

use args::{Cli, Command};
use clap::Parser;
use client::{Client, DaemonStarter, ProcessStarter, UnavailableClient, UnixClient};
use commands::Ctx;
use runtime::{Pacer, Signals};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tasks_daemon::config::{ConfigArgs, DaemonConfig};
use tasks_policy::PolicyFs;
use tasks_timing::History;

/// Whether this build carries the desktop window (R-199).
pub const UI_BUILT: bool = cfg!(feature = "ui");
/// Whether this build carries the MCP server (R-199).
pub const MCP_BUILT: bool = cfg!(feature = "mcp");

/// Builds the daemon client for parsed arguments, and says where this shell would put the slot
/// locks.
pub type ClientFor<'a> = &'a dyn Fn(&Cli) -> (Box<dyn Client>, Option<PathBuf>);

/// Everything outside the command line that a run depends on.
pub struct Deps<'a> {
    /// Environment lookup.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// The working directory.
    pub cwd: PathBuf,
    /// Milliseconds since the epoch.
    pub now_ms: &'a dyn Fn() -> i64,
    /// Waits between polls.
    pub pacer: &'a dyn Pacer,
    /// Termination signals.
    pub signals: &'a dyn Signals,
    /// Reads a file; `-` is standard input.
    pub read_input: &'a dyn Fn(&Path) -> std::io::Result<String>,
    /// The file system for the repo run policy.
    pub policy_fs: &'a dyn PolicyFs,
    /// Timing history to use instead of asking the daemon for it (`None` asks the daemon).
    pub history: Option<&'a dyn History>,
    /// The daemon client for these arguments, and where this shell would put the slot locks.
    pub client_for: ClientFor<'a>,
}

/// The client that talks to the daemon this shell resolves, starting it when missing. Paths
/// come from the daemon's own resolution, never from `TMPDIR` (R-231).
pub fn real_client(
    cli: &Cli,
    env: &dyn Fn(&str) -> Option<String>,
) -> (Box<dyn Client>, Option<PathBuf>) {
    let args = ConfigArgs {
        state_dir: cli.state_dir.clone(),
        ..ConfigArgs::default()
    };
    match DaemonConfig::resolve(&args, &|k| env(k)) {
        Ok(cfg) => {
            let program = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tasks2"));
            let mut sargs = vec!["daemon".to_string(), "serve".to_string()];
            if let Some(d) = &cli.state_dir {
                sargs.push("--state-dir".into());
                sargs.push(d.display().to_string());
            }
            let starter = ProcessStarter {
                program,
                args: sargs,
                log: cfg.state_dir.join("daemon.log"),
                socket: cfg.socket_path(),
                wait: Duration::from_secs(15),
            };
            let direct = Arc::new(starter);
            // R-682: behind an installed service, ask the service manager; never run a second
            // daemon. A custom state directory is not the service's, so it starts directly.
            let starter: Arc<dyn DaemonStarter> = if cli.state_dir.is_none() {
                Arc::new(client::ServiceStarter {
                    service: Arc::new(tasks_platform::native()),
                    name: tasks_daemon::entry::SERVICE_NAME.to_string(),
                    manager: if cfg!(target_os = "macos") {
                        "launchd"
                    } else {
                        "systemd"
                    },
                    fallback: direct,
                    socket: cfg.socket_path(),
                    wait: Duration::from_secs(15),
                })
            } else {
                direct
            };
            let client = UnixClient::new(cfg.socket_path()).with_starter(starter);
            (Box::new(client), Some(cfg.lock_dir))
        }
        Err(e) => (
            Box::new(UnavailableClient(format!(
                "cannot work out where the daemon lives ({e}); pass --state-dir DIR or set HOME"
            ))),
            None,
        ),
    }
}

/// The text for `ui` and `mcp` in a build without them (R-199).
pub fn not_built(name: &str) -> String {
    format!(
        "tasks2: built without {name}: this binary has the daemon and the command line only; rebuild with `--features {name}`"
    )
}

/// Runs `tasks2 mcp`: the MCP server on stdin and stdout.
#[cfg(feature = "mcp")]
fn run_mcp(cli: &Cli, deps: &Deps<'_>) -> i32 {
    let (client, _) = (deps.client_for)(cli);
    mcp::run_stdio(Arc::from(client), deps.cwd.clone(), deps.env)
}

/// Unreachable without the feature: `run_cli` only calls it when [`MCP_BUILT`].
#[cfg(not(feature = "mcp"))]
fn run_mcp(_cli: &Cli, _deps: &Deps<'_>) -> i32 {
    error::EXIT_USAGE
}

/// Runs `tasks2 ui`: the desktop window over the daemon this shell resolves.
#[cfg(feature = "ui")]
fn run_ui(cli: &Cli, deps: &Deps<'_>) -> i32 {
    let (client, _) = (deps.client_for)(cli);
    // R-686: the window loop never reads the flag-only handlers from `real_main`
    runtime::die_on_termination();
    ui::run(Arc::from(client), deps.env)
}

/// Unreachable without the feature: `run_cli` only calls it when [`UI_BUILT`].
#[cfg(not(feature = "ui"))]
fn run_ui(_cli: &Cli, _deps: &Deps<'_>) -> i32 {
    error::EXIT_USAGE
}

/// `tasks2 cache`: what the daemon of this shell would use, read from its configuration and the
/// machine, without starting it.
fn cache_report(
    state_dir: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let args = ConfigArgs {
        state_dir: state_dir.map(Path::to_path_buf),
        ..ConfigArgs::default()
    };
    let cfg = match DaemonConfig::resolve(&args, &|k| env(k)) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "tasks2: cannot work out the cache directory ({e})");
            return error::EXIT_USAGE;
        }
    };
    let policy = tasks_daemon::machine_policy::MachinePolicy::load(&cfg.policy_path(), &|k| env(k));
    let vars: Vec<(String, String)> = ["PATH", "HOME"]
        .iter()
        .filter_map(|k| env(k).map(|v| ((*k).to_string(), v)))
        .collect();
    let text =
        tasks_daemon::build_cache::report::report(&cfg, &policy, &vars, &tasks_platform::native());
    let _ = write!(out, "{text}");
    error::EXIT_OK
}

/// Parses and runs one command line, writing to `out` and `err`, and returns the exit code.
pub fn run_cli(args: &[String], out: &mut dyn Write, err: &mut dyn Write, deps: &Deps<'_>) -> i32 {
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            let text = e.render().to_string();
            return if e.use_stderr() {
                let _ = write!(err, "{text}");
                error::EXIT_USAGE
            } else {
                let _ = write!(out, "{text}");
                error::EXIT_OK
            };
        }
    };
    match &cli.command {
        Command::Daemon { args } => {
            return match tasks_daemon::entry::run(args) {
                Ok(()) => error::EXIT_OK,
                Err(e) => {
                    let _ = writeln!(err, "tasks2 daemon: {e}");
                    error::EXIT_FAILURE
                }
            };
        }
        Command::Cache => return cache_report(cli.state_dir.as_deref(), deps.env, out, err),
        Command::Identity { action } => {
            let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
            // building the client starts nothing; `identity` calls it only if a daemon runs
            let (client, _) = (deps.client_for)(&cli);
            return trust::identity(
                action,
                cli.state_dir.as_deref(),
                deps.env,
                Some(client.as_ref()),
                tty,
                out,
                err,
            );
        }
        Command::Mcp { .. } if MCP_BUILT => return run_mcp(&cli, deps),
        Command::Ui { .. } if UI_BUILT => return run_ui(&cli, deps),
        Command::Ui { .. } | Command::Mcp { .. } => {
            let (name, built) = if matches!(cli.command, Command::Ui { .. }) {
                ("ui", UI_BUILT)
            } else {
                ("mcp", MCP_BUILT)
            };
            // `built` is false here: a built front end returned above
            let _ = if built {
                writeln!(err, "tasks2: the {name} front end failed to start")
            } else {
                writeln!(err, "{}", not_built(name))
            };
            return error::EXIT_USAGE;
        }
        _ => {}
    }
    let (client, client_lock_dir) = (deps.client_for)(&cli);
    let mut ctx = Ctx {
        client: client.as_ref(),
        out,
        err,
        cwd: deps.cwd.clone(),
        env: deps.env,
        now_ms: deps.now_ms,
        pacer: deps.pacer,
        signals: deps.signals,
        client_lock_dir,
        read_input: deps.read_input,
        policy_fs: deps.policy_fs,
        history: deps.history,
    };
    commands::execute(&mut ctx, &cli.command)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn read_input(path: &Path) -> std::io::Result<String> {
    if path == Path::new("-") {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        Ok(s)
    } else {
        std::fs::read_to_string(path)
    }
}

/// The real process: arguments, environment, signals, standard streams. Returns the exit code.
pub fn real_main() -> i32 {
    // R-089: nothing this process inherited may leak into a daemon it starts.
    let _ = tasks_platform::close_on_exec_inherited();
    let args: Vec<String> = std::env::args().collect();
    let signals: Box<dyn Signals> = match runtime::RealSignals::install() {
        Ok(s) => Box::new(s),
        Err(_) => Box::new(runtime::NoSignals),
    };
    let env = |k: &str| std::env::var(k).ok();
    let Ok(cwd) = std::env::current_dir() else {
        let _ = writeln!(
            std::io::stderr(),
            "tasks2: the working directory is not readable"
        );
        return error::EXIT_FAILURE;
    };
    let client_for = |cli: &Cli| real_client(cli, &env);
    let deps = Deps {
        env: &env,
        cwd,
        now_ms: &now_ms,
        pacer: &runtime::SleepPacer,
        signals: signals.as_ref(),
        read_input: &read_input,
        policy_fs: &tasks_policy::RealFs,
        history: None,
        client_for: &client_for,
    };
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    run_cli(&args, &mut out, &mut err, &deps)
}
