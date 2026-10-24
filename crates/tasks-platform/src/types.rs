//! Plain data types shared by the platform traits.

use std::path::PathBuf;

/// Memory totals in bytes, with the swap and CPU counters read alongside.
///
/// The counters are cumulative since boot (or since the first read on macOS), so a rate needs two
/// readings: see [`HostRates::between`]. They default to zero where the host does not report them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemorySnapshot {
    /// Total memory.
    pub total_bytes: u64,
    /// Memory available to new work.
    pub available_bytes: u64,
    /// Swap size (0 when there is none).
    pub swap_total_bytes: u64,
    /// Swap in use.
    pub swap_used_bytes: u64,
    /// Bytes read back from swap since boot.
    pub swap_in_bytes: u64,
    /// Bytes written out to swap since boot.
    pub swap_out_bytes: u64,
    /// CPU time spent working (all cores, any unit as long as `cpu_total` uses the same).
    pub cpu_busy: u64,
    /// CPU time in total, idle included.
    pub cpu_total: u64,
}

/// Rates between two [`MemorySnapshot`] readings.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HostRates {
    /// CPU busy percent over the interval, when the counters moved.
    pub cpu_pct: Option<f32>,
    /// Bytes per second read back from swap.
    pub swap_in_per_s: Option<u64>,
    /// Bytes per second written out to swap.
    pub swap_out_per_s: Option<u64>,
}

impl HostRates {
    /// Rates from `prev` to `now`, `elapsed_ms` apart. A counter that went backwards (reboot,
    /// wrap) or an empty interval gives `None` for that figure.
    pub fn between(prev: &MemorySnapshot, now: &MemorySnapshot, elapsed_ms: u64) -> HostRates {
        let per_s = |a: u64, b: u64| {
            if elapsed_ms == 0 || b < a {
                None
            } else {
                Some((b - a).saturating_mul(1000) / elapsed_ms)
            }
        };
        let cpu_pct = match (
            now.cpu_total.checked_sub(prev.cpu_total),
            now.cpu_busy.checked_sub(prev.cpu_busy),
        ) {
            (Some(total), Some(busy)) if total > 0 => {
                Some((busy.min(total) as f64 * 100.0 / total as f64) as f32)
            }
            _ => None,
        };
        HostRates {
            cpu_pct,
            swap_in_per_s: per_s(prev.swap_in_bytes, now.swap_in_bytes),
            swap_out_per_s: per_s(prev.swap_out_bytes, now.swap_out_bytes),
        }
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Level::Normal => "normal",
            Level::Warn => "warn",
            Level::Critical => "critical",
        })
    }
}

/// Coarse pressure level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// No pressure.
    Normal,
    /// Elevated pressure.
    Warn,
    /// Severe pressure.
    Critical,
}

/// System pressure reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PressureSnapshot {
    /// Memory pressure level (from PSI thresholds on Linux, the native level on macOS).
    pub memory: Level,
    /// Memory PSI `some` avg10 percent (Linux only).
    pub memory_some: Option<f32>,
    /// Memory PSI `full` avg10 percent (Linux only).
    pub memory_full: Option<f32>,
    /// CPU pressure: PSI `some` avg10 percent on Linux; on macOS an ESTIMATE:
    /// the 1-minute load average divided by logical cores (a ratio where 1.0 means
    /// one runnable task per core, not a percentage), which is not PSI.
    pub cpu: Option<f32>,
    /// I/O PSI `some` avg10 percent (Linux only; `None` on macOS).
    pub io: Option<f32>,
}

/// Thresholds mapping PSI memory percentages (avg10) to a [`Level`].
/// A value at or above a threshold reaches that level. Nothing is hard-coded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PressureThresholds {
    /// `some` avg10 at or above this is `Warn`.
    pub memory_some_warn: f32,
    /// `some` avg10 at or above this is `Critical`.
    pub memory_some_critical: f32,
    /// `full` avg10 at or above this is `Warn`.
    pub memory_full_warn: f32,
    /// `full` avg10 at or above this is `Critical`.
    pub memory_full_critical: f32,
}

impl PressureThresholds {
    /// Rejects non-finite, negative or inverted (warn above critical) values.
    pub fn validate(&self) -> Result<(), crate::PlatformError> {
        let pairs = [
            (
                "memory_some",
                self.memory_some_warn,
                self.memory_some_critical,
            ),
            (
                "memory_full",
                self.memory_full_warn,
                self.memory_full_critical,
            ),
        ];
        for (name, warn, critical) in pairs {
            let ok = warn.is_finite() && critical.is_finite() && warn >= 0.0 && warn <= critical;
            if !ok {
                return Err(crate::PlatformError::InvalidThresholds(format!(
                    "{name}: warn={warn} critical={critical}"
                )));
            }
        }
        Ok(())
    }

    /// Maps memory PSI readings to a level.
    pub fn level(&self, some: f32, full: f32) -> Level {
        if some >= self.memory_some_critical || full >= self.memory_full_critical {
            Level::Critical
        } else if some >= self.memory_some_warn || full >= self.memory_full_warn {
            Level::Warn
        } else {
            Level::Normal
        }
    }
}

/// Process group identity: the group id plus the start time of its leader
/// (process id equals group id). The start time guards against PID reuse.
/// Units are OS-specific (Linux clock ticks since boot, macOS microseconds
/// since the epoch); treat it as an opaque token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupId {
    /// Process group id (equals the leader pid).
    pub pgid: i32,
    /// Opaque start time of the leader.
    pub start_time: u64,
}

/// Signal to send to a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Polite termination request.
    Term,
    /// Forced kill.
    Kill,
    /// Stop (pause).
    Stop,
    /// Continue.
    Cont,
}

/// How to start a process group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// Command and arguments; the first element is the program.
    pub argv: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Complete environment; the parent environment is cleared.
    pub env: Vec<(String, String)>,
    /// File receiving stdout and stderr.
    pub log_path: PathBuf,
    /// A cgroup's `cgroup.procs` file: the child writes its own pid into it before it execs,
    /// so it and everything it starts are in the cgroup from the first instruction. `None`
    /// spawns directly.
    pub cgroup_procs: Option<PathBuf>,
    /// Niceness added to the whole process group right after the spawn (0 leaves it). Used for
    /// compile jobs where the job cgroup has no `cpu.weight` (macOS, no cpu controller).
    pub nice: i32,
}

/// How much CPU a job gets against the others on the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuShare {
    /// `cpu.weight` for the job's cgroup (kernel default 100); `None` leaves the default.
    pub weight: Option<u32>,
    /// Niceness added to the job's process group when the weight could not be set (no cgroup,
    /// or no cpu controller): the only lever macOS has.
    pub nice: i32,
}

/// The shell script that places a child in a cgroup and then becomes the real command. `$0` is
/// the `cgroup.procs` file and `"$@"` the command. The write may fail (the cgroup vanished, the
/// file is not writable); the command still runs and the caller notices the missing placement.
pub const PLACE_SCRIPT: &str = "{ echo $$ >\"$0\"; } 2>/dev/null; exec \"$@\"";

/// Wraps `argv` so that the process joins the cgroup behind `procs` before it execs `argv`.
///
/// Why a shell and not `pre_exec`: `CommandExt::pre_exec` is `unsafe` (the closure runs between
/// fork and exec in a possibly multi-threaded process) and the workspace denies `unsafe`. Writing
/// the pid from the parent after the spawn would leave a window in which the child could fork
/// children that stay outside the cgroup. `clone3` with `CLONE_INTO_CGROUP` needs `unsafe` too.
/// The shell writes its own pid and then `exec`s, so pid, process-group leadership and start time
/// are the command's, and nothing the command starts is ever outside the cgroup.
pub fn place_in_cgroup(procs: &std::path::Path, argv: &[String]) -> Vec<String> {
    let mut out = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        PLACE_SCRIPT.to_string(),
        procs.display().to_string(),
    ];
    out.extend(argv.iter().cloned());
    out
}

/// A spawned process group leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Child {
    /// Leader pid.
    pub pid: i32,
    /// Process group id.
    pub group: GroupId,
}

/// Resource usage of a process group.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    /// User CPU seconds.
    pub cpu_user_s: f64,
    /// System CPU seconds.
    pub cpu_sys_s: f64,
    /// Resident memory in bytes.
    pub rss_bytes: u64,
    /// Number of processes.
    pub processes: u32,
}

/// A background service definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceUnit {
    /// Service name.
    pub name: String,
    /// Command and arguments.
    pub argv: Vec<String>,
}

/// What `Service::install` did and what the user should run next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// The unit or plist file path.
    pub path: PathBuf,
    /// True when the file was written now; false when an identical file existed.
    pub contents_written: bool,
    /// Command the user should run to enable or start the service.
    pub next_command: String,
}

/// State of an installed service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
    /// Not installed.
    NotInstalled,
    /// Installed but stopped.
    Stopped,
    /// Running.
    Running,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(busy: u64, total: u64, swin: u64, swout: u64) -> MemorySnapshot {
        MemorySnapshot {
            cpu_busy: busy,
            cpu_total: total,
            swap_in_bytes: swin,
            swap_out_bytes: swout,
            ..MemorySnapshot::default()
        }
    }

    // R-357
    #[test]
    fn r357_rates_come_from_two_readings_and_a_counter_that_went_back_gives_none() {
        let r = HostRates::between(&snap(100, 1000, 0, 0), &snap(400, 2000, 8192, 4096), 2000);
        assert_eq!(r.cpu_pct, Some(30.0));
        assert_eq!(
            (r.swap_in_per_s, r.swap_out_per_s),
            (Some(4096), Some(2048))
        );
        // nothing moved: zero swap traffic, but the CPU interval is empty so no percentage
        let r = HostRates::between(&snap(5, 50, 7, 7), &snap(5, 50, 7, 7), 1000);
        assert_eq!(r.cpu_pct, None);
        assert_eq!((r.swap_in_per_s, r.swap_out_per_s), (Some(0), Some(0)));
        // a reboot or wrap: the counters went backwards
        let r = HostRates::between(&snap(900, 9000, 99, 99), &snap(1, 10, 1, 1), 1000);
        assert_eq!(r, HostRates::default());
        // no time passed
        assert_eq!(
            HostRates::between(&snap(0, 0, 0, 0), &snap(5, 10, 5, 5), 0).swap_in_per_s,
            None
        );
        // busy never exceeds total
        let r = HostRates::between(&snap(0, 0, 0, 0), &snap(50, 10, 0, 0), 1000);
        assert_eq!(r.cpu_pct, Some(100.0));
    }
}
