//! macOS process-group accounting: `proc_pidinfo` for membership and start
//! time, `sysinfo` for CPU and RSS.

use super::bsdinfo::{bsd_info, bsd_info_for_scan};
use crate::macos_parse::start_token;
use crate::{GroupId, PlatformError, Usage, groups};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

/// Start token of a process (microseconds since the epoch); `None` if absent.
pub(super) fn start_time(pid: i32) -> Result<Option<u64>, PlatformError> {
    match bsd_info(pid)? {
        Some(i) => start_token(i.start_sec, i.start_usec).map(Some),
        None => Ok(None),
    }
}

/// Usage of the group after verifying the leader's identity (R-075). Zombies
/// are not counted. sysinfo reports one accumulated CPU figure, returned as
/// user time with system time 0.0 (the split is unavailable on this path).
pub(super) fn usage(group: GroupId) -> Result<Usage, PlatformError> {
    groups::validate_target(group.pgid)?;
    groups::check_leader(group, &start_time)?;
    let pgid = u32::try_from(group.pgid).map_err(|_| PlatformError::InvalidGroup(group.pgid))?;
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    let mut u = Usage {
        cpu_user_s: 0.0,
        cpu_sys_s: 0.0,
        rss_bytes: 0,
        processes: 0,
    };
    for (pid, p) in sys.processes() {
        let Ok(raw) = i32::try_from(pid.as_u32()) else {
            continue;
        };
        let Some(info) = bsd_info_for_scan(raw)? else {
            continue;
        };
        if info.pgid == pgid && !info.zombie {
            u.cpu_user_s += p.accumulated_cpu_time() as f64 / 1000.0;
            u.rss_bytes = u.rss_bytes.saturating_add(p.memory());
            u.processes += 1;
        }
    }
    Ok(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_has_a_start_time() {
        let pid = std::process::id() as i32;
        assert!(start_time(pid).unwrap().unwrap() > 0);
    }
}
