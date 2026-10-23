//! Job runner: turns an admitted job into a running process group and then a
//! final outcome. It spawns through the platform `Processes` trait, follows the
//! combined log, enforces cancel, timeout and client-loss policy, and reports
//! through [`RunEvents`] so it never depends on the ledger.

mod clock;
mod env;
mod error;
mod events;
mod lease;
mod linebuf;
mod logview;
mod meter;
mod outcome;
mod policy;
mod run;
#[cfg(test)]
mod testkit;

pub use clock::{Clock, SystemClock};
pub use env::{ENV_ALLOWLIST, ENV_PREFIX_ALLOWLIST, EnvPolicy, build_env, with_dirs};
pub use error::RunnerError;
pub use events::{RunEvents, TimedLine};
pub use lease::Lease;
pub use linebuf::{LineSplitter, SplitLimits};
pub use logview::{
    Elider, ElisionCaps, LogNotes, cap_log_file, is_important, tail_lines, tail_lines_at,
};
pub use meter::Attrib;
pub use outcome::{End, Outcome};
pub use policy::{CancelState, CancelToken, Policy, parse_grace_ms};
pub use run::{JobStart, Runner, build_spawn_spec};
