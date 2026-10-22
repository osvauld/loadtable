//! Command line parsing (clap derive) and the option checks clap cannot word precisely.

use crate::error::{CliError, EXIT_CODES_HELP};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use tasks_core::{MachineName, Placement};

/// `tasks2`: submit builds and tests to the tasks coordinator, watch them, read history.
#[derive(Parser, Debug)]
#[command(
    name = "tasks2",
    version,
    about = "Submit builds and tests to the tasks coordinator, watch them and read their history",
    after_help = EXIT_CODES_HELP,
    arg_required_else_help = true
)]
pub struct Cli {
    /// State directory of the daemon (default: the same resolution the daemon uses).
    #[arg(long, global = true, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// The command.
    #[command(subcommand)]
    pub command: Command,
}

/// Where a job may run. At most one of these.
#[derive(Args, Debug, Clone, Default)]
pub struct PlaceArgs {
    /// Keep the job on this machine and never reroute it.
    #[arg(long)]
    pub pin: bool,
    /// Run the job on the named peer.
    #[arg(long, value_name = "NAME")]
    pub peer: Option<String>,
    /// Let the daemon pick any capable machine.
    #[arg(long)]
    pub anywhere: bool,
    /// Run on this machine (the default).
    #[arg(long)]
    pub here: bool,
}

impl PlaceArgs {
    /// The placement, or the precise conflict and its fix (R-190).
    pub fn placement(&self, device: Option<&str>) -> Result<Placement, CliError> {
        let mut chosen: Vec<(&str, String)> = Vec::new();
        if self.pin {
            chosen.push(("--pin", "keeps the job on this machine".into()));
        }
        if let Some(p) = &self.peer {
            if p.trim().is_empty() {
                return Err(CliError::Usage(
                    "--peer needs a machine name, for example `--peer nixos`".into(),
                ));
            }
            chosen.push(("--peer", format!("sends it to {p}")));
        }
        if self.anywhere {
            chosen.push((
                "--anywhere",
                "lets the daemon choose any capable machine".into(),
            ));
        }
        if self.here {
            chosen.push(("--here", "runs it on this machine".into()));
        }
        if let [(a, ma), (b, mb), ..] = chosen.as_slice() {
            return Err(CliError::Usage(format!(
                "{a} and {b} conflict: {a} {ma}, {b} {mb}; pass only one of them"
            )));
        }
        if let (Some(dev), true) = (device, self.peer.is_some() || self.anywhere) {
            let flag = if self.peer.is_some() {
                "--peer NAME"
            } else {
                "--anywhere"
            };
            return Err(CliError::Usage(format!(
                "--device {dev} and {flag} conflict: a device job runs on the machine the device is attached to, which is this one; drop {flag} to run here, or drop --device to run elsewhere"
            )));
        }
        Ok(if self.pin {
            Placement::Pin
        } else if let Some(p) = &self.peer {
            Placement::Peer(MachineName(p.clone()))
        } else if self.anywhere {
            Placement::Anywhere
        } else {
            Placement::Here
        })
    }
}

/// Arguments of `run`, `enqueue` and `submit`.
#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// Job owner (default: $TASKS_WHO, then $USER).
    #[arg(long, value_name = "OWNER")]
    pub who: Option<String>,
    /// Where the job may run.
    #[command(flatten)]
    pub place: PlaceArgs,
    /// Run on this attached device (adb serial); the job stays on this machine.
    #[arg(long, value_name = "SERIAL")]
    pub device: Option<String>,
    /// Stop the job after this many seconds.
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,
    /// Allow a `cargo test` with a name filter but no target selector to build every target.
    #[arg(long)]
    pub broad_test: bool,
    /// Submit and print the id; do not follow the log (`run` only).
    #[arg(long)]
    pub detach: bool,
    /// The command to run, after `--`.
    #[arg(last = true, required = true, value_name = "CMD")]
    pub command: Vec<String>,
}

/// Arguments of `list`.
#[derive(Args, Debug, Clone)]
pub struct ListArgs {
    /// Only this owner.
    #[arg(long, value_name = "OWNER")]
    pub who: Option<String>,
    /// Only unfinished jobs.
    #[arg(long)]
    pub active: bool,
    /// Show only the most recent N jobs.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// One short line per job.
    #[arg(long)]
    pub brief: bool,
    /// The daemon's reply as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments of `logs`.
#[derive(Args, Debug, Clone)]
pub struct LogsArgs {
    /// The job.
    pub id: i64,
    /// The last N lines (default 100).
    #[arg(long, value_name = "N", conflicts_with_all = ["grep", "follow"])]
    pub tail: Option<usize>,
    /// Lines matching this pattern (regex subset; see --fixed).
    #[arg(long, value_name = "PATTERN", conflicts_with = "follow")]
    pub grep: Option<String>,
    /// Read the --grep pattern as a plain substring.
    #[arg(long, short = 'F', requires = "grep")]
    pub fixed: bool,
    /// Lines of context around each --grep match.
    #[arg(
        short = 'C',
        long,
        value_name = "N",
        requires = "grep",
        default_value_t = 0
    )]
    pub context: usize,
    /// Stream the log until the job ends.
    #[arg(long, short = 'f')]
    pub follow: bool,
}

/// Arguments of `log-search`.
#[derive(Args, Debug, Clone)]
pub struct LogSearchArgs {
    /// The pattern (regex subset; see --fixed).
    pub pattern: String,
    /// Read the pattern as a plain substring.
    #[arg(long, short = 'F')]
    pub fixed: bool,
    /// Only this owner.
    #[arg(long, value_name = "OWNER")]
    pub who: Option<String>,
    /// Only these jobs.
    #[arg(long = "id", value_name = "ID")]
    pub ids: Vec<i64>,
    /// Only jobs in this phase (passed, failed, running, ...).
    #[arg(long)]
    pub phase: Option<String>,
    /// Only jobs submitted since: unix seconds or a duration such as 2h.
    #[arg(long)]
    pub since: Option<String>,
    /// Only jobs submitted until: unix seconds or a duration such as 2h.
    #[arg(long)]
    pub until: Option<String>,
    /// Lines of context.
    #[arg(short = 'C', long, default_value_t = 0)]
    pub context: usize,
    /// Most matching lines in total.
    #[arg(long, value_name = "N")]
    pub max_matches: Option<usize>,
    /// Most bytes of matched text.
    #[arg(long, value_name = "BYTES")]
    pub max_bytes: Option<usize>,
}

/// Arguments of `steps`.
#[derive(Args, Debug, Clone)]
pub struct StepsArgs {
    /// The plan file (JSON: {"steps":[{"name":..,"argv":[..],"after":[..]}]}).
    pub file: PathBuf,
    /// Submitting owner.
    #[arg(long, value_name = "OWNER")]
    pub who: Option<String>,
    /// Where the steps may run.
    #[command(flatten)]
    pub place: PlaceArgs,
    /// Allow broad `cargo test` steps.
    #[arg(long)]
    pub broad_test: bool,
}

/// Arguments of `suite run`.
#[derive(Args, Debug, Clone)]
pub struct SuiteRunArgs {
    /// The suite of .tasks/run-policy.toml.
    pub name: String,
    /// Drop steps marked skip_in_quick.
    #[arg(long)]
    pub quick: bool,
    /// Submitting owner.
    #[arg(long, value_name = "OWNER")]
    pub who: Option<String>,
    /// Where the steps may run.
    #[command(flatten)]
    pub place: PlaceArgs,
    /// Allow broad `cargo test` steps.
    #[arg(long)]
    pub broad_test: bool,
}

/// `suite` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum SuiteCommand {
    /// Submit a suite of the repo's run policy as a plan.
    Run(SuiteRunArgs),
    /// List the suites of the repo's run policy.
    List,
}

/// `policy` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum PolicyCommand {
    /// Show the effective repo run policy.
    Show,
    /// Propose policy changes from measured history.
    Suggest,
}

/// Arguments of `split`.
#[derive(Args, Debug, Clone)]
pub struct SplitArgs {
    /// Cargo's `--message-format=json` output of `cargo test --no-run` (`-` reads stdin).
    #[arg(long, value_name = "FILE")]
    pub exe: PathBuf,
    /// The `cargo test ...` command to split, after `--`.
    #[arg(last = true, required = true, value_name = "CMD")]
    pub command: Vec<String>,
}

/// Arguments of `stats`.
#[derive(Args, Debug, Clone)]
pub struct StatsArgs {
    /// Only this owner. The timing history has no owners, so this reads the finished jobs
    /// instead (whole commands only, no targets).
    #[arg(long, value_name = "OWNER")]
    pub who: Option<String>,
    /// Newest samples (or finished jobs with --who) to look at per series (default 500, at
    /// most 200 samples per series from the history).
    #[arg(long, value_name = "N", default_value_t = 500)]
    pub limit: usize,
    /// Only recipes containing this text.
    #[arg(long, value_name = "TEXT")]
    pub recipe: Option<String>,
    /// Only this machine.
    #[arg(long, value_name = "MACHINE")]
    pub machine: Option<String>,
    /// Also show each test target's times.
    #[arg(long)]
    pub targets: bool,
}

/// `execution` subcommands.
#[derive(Subcommand, Debug, Clone, Copy)]
pub enum ExecutionCommand {
    /// Let queued jobs start (persists across daemon restarts).
    Start,
    /// Stop queued jobs from starting (persists across daemon restarts).
    Stop,
}

/// All commands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Submit a job, follow its log and exit with its exit code.
    Run(RunArgs),
    /// Submit a job and print its id; do not follow.
    #[command(visible_alias = "submit")]
    Enqueue(RunArgs),
    /// List jobs.
    List(ListArgs),
    /// Read a job's log.
    Logs(LogsArgs),
    /// Search the retained logs of many jobs.
    LogSearch(LogSearchArgs),
    /// Show one job.
    Get {
        /// The job.
        id: i64,
        /// The daemon's reply as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Cancel a job (only its owner may).
    Cancel {
        /// The job.
        id: i64,
        /// The asking owner (default: $TASKS_WHO, then $USER).
        #[arg(long, value_name = "OWNER")]
        who: Option<String>,
        /// Why; kept with the job and shown by `get` and `list`.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Slots, waiting and running jobs, pressure and alerts.
    Status {
        /// The daemon's reply as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Lock directory, thresholds, slot holders and where each policy value comes from.
    Admission {
        /// The daemon's reply as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Estimate how long a command takes on this machine.
    Estimate {
        /// The command, after `--`.
        #[arg(last = true, required = true, value_name = "CMD")]
        command: Vec<String>,
    },
    /// Stop starting new jobs until the daemon restarts or `resume`.
    Drain,
    /// Undo a drain.
    Resume,
    /// Start or stop letting queued jobs run.
    Execution {
        /// What to do.
        #[command(subcommand)]
        action: ExecutionCommand,
    },
    /// Submit a plan of named, dependent steps.
    Steps(StepsArgs),
    /// Suites of the repo run policy.
    Suite {
        /// What to do.
        #[command(subcommand)]
        action: SuiteCommand,
    },
    /// The repo run policy.
    Policy {
        /// What to do.
        #[command(subcommand)]
        action: PolicyCommand,
    },
    /// Split a `cargo test` into a build step and one step per test executable.
    Split(SplitArgs),
    /// Run time statistics of finished jobs per command.
    Stats(StatsArgs),
    /// The daemon: serve, install, policy-sample (arguments go to the daemon).
    #[command(disable_help_flag = true)]
    Daemon {
        /// Arguments for the daemon.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The desktop window (not built in this binary).
    Ui {
        /// Ignored.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },
    /// The MCP server (not built in this binary).
    Mcp {
        /// Ignored.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },
}

#[cfg(test)]
mod tests;
