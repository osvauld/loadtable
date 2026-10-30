//! Command line parsing (clap derive) and the option checks clap cannot word precisely.

use crate::error::{CliError, EXIT_CODES_HELP};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use tasks_core::{MachineName, Placement, Priority};

/// `loadtable`: submit builds and tests to the LoadTable coordinator, watch them, read history.
#[derive(Parser, Debug)]
#[command(
    name = "loadtable",
    version = tasks_daemon::VERSION,
    about = "Submit builds and tests to the LoadTable coordinator, watch them and read their history",
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
    /// True when the submitter chose a placement, `--here` included (R-730): a plan step's own
    /// placement hint applies only when none was chosen.
    pub fn given(&self) -> bool {
        self.pin || self.peer.is_some() || self.anywhere || self.here
    }

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

/// The queue class of a submission. Normal when neither flag is given.
#[derive(Args, Debug, Clone, Default)]
pub struct PriorityArgs {
    /// Queue class: `background` for work that may wait behind the rest, `blocking` when
    /// someone is waiting on the result now (needs --reason; audited), or `normal`.
    #[arg(long, value_name = "CLASS")]
    pub priority: Option<String>,
    /// Why the job is blocking (required with `--priority blocking`).
    #[arg(long, value_name = "TEXT")]
    pub reason: Option<String>,
}

impl PriorityArgs {
    /// The class and reason, or what to fix.
    pub fn resolve(&self) -> Result<(Priority, Option<String>), CliError> {
        let class = match &self.priority {
            None => Priority::Normal,
            Some(p) => p
                .parse::<Priority>()
                .map_err(|e| CliError::Usage(e.to_string()))?,
        };
        let reason = self
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty());
        match (class, reason) {
            (Priority::Blocking, None) => Err(CliError::Usage(
                "--priority blocking needs a reason: add --reason \"what is waiting on this\""
                    .into(),
            )),
            (Priority::Blocking, Some(r)) => Ok((class, Some(r.to_string()))),
            (_, Some(_)) => Err(CliError::Usage(
                "--reason goes with --priority blocking; drop it, or claim blocking".into(),
            )),
            (_, None) => Ok((class, None)),
        }
    }
}

/// Arguments of `run`, `enqueue` and `submit`.
#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// Job owner (default: $LOADTABLE_WHO, then $USER).
    #[arg(long, value_name = "OWNER")]
    pub who: Option<String>,
    /// Where the job may run.
    #[command(flatten)]
    pub place: PlaceArgs,
    /// Queue class.
    #[command(flatten)]
    pub class: PriorityArgs,
    /// Run on this attached device (adb serial); the job stays on this machine.
    #[arg(long, value_name = "SERIAL")]
    pub device: Option<String>,
    /// Only machines with this OS may run it (`linux` or `macos`). An `--anywhere` job without
    /// it takes the repo run policy's `[placement] os`.
    #[arg(long, value_name = "OS")]
    pub os: Option<String>,
    /// Stop the job after this many seconds.
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,
    /// Allow a `cargo test` with a name filter but no target selector to build every target.
    #[arg(long)]
    pub broad_test: bool,
    /// Run a `sh -c`/`bash -c` script of several commands as one job instead of having it
    /// refused (it then gets no timing history or estimate; prefer steps or a suite).
    #[arg(long)]
    pub allow_script: bool,
    /// Bring this output file back after the job ran on a peer (path relative to the checkout
    /// root; repeatable). A job that ran here already has it.
    #[arg(long, value_name = "REL")]
    pub fetch: Vec<String>,
    /// The job uses little CPU but runs long (timers, network): it ignores the CPU pressure gate
    /// and takes a place in the small light pool instead of a run slot.
    #[arg(long)]
    pub light: bool,
    /// If this `cargo test` fails because the machine was busy, run the failed tests once more,
    /// alone, and say whether they passed.
    #[arg(long)]
    pub retry_alone: bool,
    /// The job opens a window or waits on a person (a GUI, a REPL): it runs as long as it is
    /// used, so it gets no estimate or "longer than recent runs" warning and its runs teach no timings.
    #[arg(long)]
    pub interactive: bool,
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
    /// One line per job, plan steps too, instead of a plan as one line with its steps.
    #[arg(long)]
    pub flat: bool,
    /// Under each plan list every step, passed ones too.
    #[arg(long, conflicts_with = "flat")]
    pub all_steps: bool,
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

/// Arguments of `wait`.
#[derive(Args, Debug, Clone)]
pub struct WaitArgs {
    /// The jobs.
    #[arg(required = true, value_name = "ID")]
    pub ids: Vec<i64>,
    /// Give up after this many seconds (exit 124); the jobs are left alone.
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,
    /// Log lines to print when a job ends (0 prints none).
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub tail: usize,
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
    /// Queue class of every step.
    #[command(flatten)]
    pub class: PriorityArgs,
    /// Allow broad `cargo test` steps.
    #[arg(long)]
    pub broad_test: bool,
}

/// Arguments of `suite run`.
#[derive(Args, Debug, Clone)]
pub struct SuiteRunArgs {
    /// The suite of .loadtable/run-policy.toml.
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
    /// Queue class of every step (without it each step keeps the class its policy gives it).
    #[command(flatten)]
    pub class: PriorityArgs,
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

/// `identity` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum IdentityCommand {
    /// Print this machine's DID, name and node id.
    Show {
        /// Also print the backup phrase (only to a terminal; it restores this identity).
        #[arg(long)]
        mnemonic: bool,
        /// Print the public parts as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// `pair` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum PairCommand {
    /// Print a one-use pairing ticket for another machine to redeem.
    Invite {
        /// What the other machine may do here: submit, observe, admin (comma separated).
        #[arg(long, value_name = "ROLES", default_value = "submit,observe")]
        role: String,
        /// The other machine grants the same roles back, in the same exchange.
        #[arg(long)]
        both_ways: bool,
        /// How long the ticket stays valid: 90s, 15m, 2h (default 15m, at most 24h).
        #[arg(long, value_name = "TIME")]
        ttl: Option<String>,
    },
    /// Redeem a ticket printed by `pair invite` on another machine.
    Accept {
        /// The `tasks1.…` text.
        ticket: String,
    },
    /// Invalidate every unredeemed ticket.
    Reset,
}

/// `peers` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum PeersCommand {
    /// Revoke a peer's grants, close its live connections and stop forwarding to it.
    Revoke {
        /// The peer's name, as `loadtable peers` shows it.
        name: String,
    },
}

/// `audit` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum AuditCommand {
    /// Walk the audit chain, check the checkpoint signatures and report the first break.
    Verify {
        /// The daemon's reply as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Sign a checkpoint of the audit chain now.
    Checkpoint,
}

/// Arguments of `audit`: with no subcommand, show rows (newest first).
#[derive(Args, Debug, Clone)]
#[command(args_conflicts_with_subcommands = true)]
pub struct AuditArgs {
    /// `verify` or `checkpoint`; without one, list the rows.
    #[command(subcommand)]
    pub sub: Option<AuditCommand>,
    /// Only rows since: a duration such as 2h or 3d, unix seconds, or a UTC date such as
    /// 2026-10-22 or 2026-10-22T14:30.
    #[arg(long, value_name = "DUR|TIME")]
    pub since: Option<String>,
    /// Only rows until: the same forms as --since.
    #[arg(long, value_name = "DUR|TIME")]
    pub until: Option<String>,
    /// Only rows naming this paired machine (its name or DID) as the peer or the actor.
    #[arg(long, value_name = "NAME")]
    pub peer: Option<String>,
    /// Only rows about this job.
    #[arg(long, value_name = "ID")]
    pub job: Option<i64>,
    /// Only this action, or a family: `job` shows every job.* row.
    #[arg(long = "action", value_name = "ACTION")]
    pub action_filter: Option<String>,
    /// Most rows (default 100).
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// The daemon's reply as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments of `stalls`.
#[derive(Args, Debug, Clone)]
pub struct StallsArgs {
    /// Only trips since: a duration such as 2h or 3d, unix seconds, or a UTC date such as
    /// 2026-10-22 or 2026-10-22T14:30.
    #[arg(long, value_name = "DUR|TIME")]
    pub since: Option<String>,
    /// Only trips until: the same forms as --since.
    #[arg(long, value_name = "DUR|TIME")]
    pub until: Option<String>,
    /// Most trips (default 20).
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// The audit rows as JSON lines, details included.
    #[arg(long)]
    pub json: bool,
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
    /// Block until jobs end, then print their last log lines and results; exits like `run`.
    Wait(WaitArgs),
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
    /// Live CPU, memory, disk and process groups of a running job.
    Metrics {
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
        /// The asking owner (default: $LOADTABLE_WHO, then $USER).
        #[arg(long, value_name = "OWNER")]
        who: Option<String>,
        /// Why; kept with the job and shown by `get` and `list`.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Change the queue class of a job that has not started (only its owner may). Never
    /// preempts a running job.
    Prioritize {
        /// The job.
        id: i64,
        /// The asking owner (default: $LOADTABLE_WHO, then $USER).
        #[arg(long, value_name = "OWNER")]
        who: Option<String>,
        /// The new class: blocking, normal or background.
        #[arg(long, value_name = "CLASS")]
        priority: String,
        /// Why (required for blocking).
        #[arg(long, value_name = "TEXT")]
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
    /// Build caches: the compiler wrapper in use, the cache root and its disk against the
    /// reserve. Reads the machine's files; needs no daemon.
    Cache,
    /// This machine's identity.
    Identity {
        /// What to do.
        #[command(subcommand)]
        action: IdentityCommand,
    },
    /// Pair with another machine by ticket.
    Pair {
        /// What to do.
        #[command(subcommand)]
        action: PairCommand,
    },
    /// Paired machines: DID, roles each way, grant expiry and link path.
    Peers {
        /// What to do; without one, list the peers.
        #[command(subcommand)]
        action: Option<PeersCommand>,
        /// The daemon's reply as JSON.
        #[arg(long)]
        json: bool,
    },
    /// The audit log: who did what, when; `audit verify` checks it was not altered.
    Audit(AuditArgs),
    /// Stalls explained: each recorded pressure trip with the busiest job, transfer and
    /// outside process at that moment.
    Stalls(StallsArgs),
    /// The desktop window (this build says whether it is built in).
    #[cfg_attr(feature = "ui", command(about = "The desktop window (built in)"))]
    #[cfg_attr(
        not(feature = "ui"),
        command(about = "The desktop window (not built in this binary)")
    )]
    Ui {
        /// Ignored.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },
    /// The MCP server (this build says whether it is built in).
    #[cfg_attr(feature = "mcp", command(about = "The MCP server (built in)"))]
    #[cfg_attr(
        not(feature = "mcp"),
        command(about = "The MCP server (not built in this binary)")
    )]
    Mcp {
        /// Ignored.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },
}

#[cfg(test)]
mod tests;
