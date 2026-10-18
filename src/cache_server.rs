//! Compiler-cache server started OUTSIDE the admitted job.
//!
//! Admission leases are deliberately inherited by every job process so that a
//! killed or orphaned grandchild keeps the slot until it really exits. A
//! compiler-cache wrapper such as sccache, however, autostarts a long-lived
//! server on first use that daemonizes out of the job's process group and
//! (sccache <= 0.15 at least) keeps every inherited descriptor: the server of
//! the first build after a reboot then holds that job's compile lease for its
//! whole lifetime (worker jobs #154..#158, feedback lease-leak).
//!
//! Before a job's command starts, the worker therefore starts (or touches) the
//! server itself from a child that has every descriptor above stderr marked
//! close-on-exec and its own session, so the server holds no lease. The job's
//! wrapper calls then find the running server instead of spawning one. Same
//! pattern as the adb server for device jobs (device::start_adb_server).
//! Bounded and never fatal: on failure the job runs as before (and a leaked
//! holder is named by admission::holders instead of "an untracked holder").
use crate::Result;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
/// Per-job file (job directory) with the start/probe output.
pub const LOG_NAME: &str = "compiler-cache-server.log";

/// Whether this RUSTC_WRAPPER is a server-based compiler cache whose server
/// would outlive the job (sccache). Other wrappers are left alone.
pub fn is_server_wrapper(wrapper: &OsStr) -> bool {
    Path::new(wrapper)
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|n| n == "sccache" || n.starts_with("sccache-") || n.starts_with("sccache."))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `--start-server` succeeded: this call started the server.
    Started,
    /// `--start-server` failed (normally "address in use") and the follow-up
    /// `--show-stats` request reached a server (it also resets its idle timer).
    AlreadyRunning,
    Failed(String),
}
impl Outcome {
    /// One job-log line.
    pub fn note(&self) -> String {
        match self {
            Self::Started => "tasks: started the compiler-cache server outside the job (no lease \
                              descriptors)\n"
                .into(),
            Self::AlreadyRunning => "tasks: compiler-cache server already running\n".into(),
            Self::Failed(why) => format!(
                "tasks: warning: could not start the compiler-cache server outside the job \
                 ({why}); a server the build starts itself may keep this job's slot held \
                 after it ends\n"
            ),
        }
    }
}

/// Starts or touches the server with exactly `env` (the job's environment
/// without TASKS_JOB_ID), writing its output to `dir/LOG_NAME`.
pub fn ensure(wrapper: &OsStr, env: &[(OsString, OsString)], dir: &Path) -> Outcome {
    let log = dir.join(LOG_NAME);
    match run(wrapper, "--start-server", env, &log) {
        Ok(true) => return Outcome::Started,
        Ok(false) => (),
        Err(e) => return Outcome::Failed(e.to_string()),
    }
    match run(wrapper, "--show-stats", env, &log) {
        Ok(true) => Outcome::AlreadyRunning,
        Ok(false) => Outcome::Failed(format!("see {LOG_NAME} in the job directory")),
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

fn run(wrapper: &OsStr, arg: &str, env: &[(OsString, OsString)], log: &Path) -> Result<bool> {
    use std::os::unix::process::CommandExt;
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let mut cmd = Command::new(wrapper);
    cmd.arg(arg)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    crate::device::without_inherited_fds(&mut cmd);
    // Own session: not in the job's admission process group, so cancelling
    // the job cannot kill a server that other jobs are about to use.
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{} unavailable: {e}", Path::new(wrapper).display()))?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{arg} timed out after {} s", TIMEOUT.as_secs()).into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
#[path = "cache_server_tests.rs"]
mod tests;
