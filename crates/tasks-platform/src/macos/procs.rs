//! macOS process-group accounting via `libproc` (membership, start time) and
//! `sysinfo` (CPU and RSS).

use crate::macos_parse::start_token;
use crate::{GroupId, PlatformError, Usage, groups};
use libproc::bsd_info::BSDInfo;
use libproc::proc_pid::pidinfo;
use libproc::processes::{ProcFilter, pids_by_type};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const SZOMB: u32 = 5;

fn bsd_info(pid: i32) -> Result<Option<BSDInfo>, PlatformError> {
    match pidinfo::<BSDInfo>(pid, 0) {
        Ok(info) => Ok(Some(info)),
        Err(msg) => {
            match rustix::process::Pid::from_raw(pid).map(rustix::process::test_kill_process) {
                Some(Err(rustix::io::Errno::SRCH)) | None => Ok(None),
                _ => Err(PlatformError::parse("proc_pidinfo", msg)),
            }
        }
    }
}

/// Start token of a process (microseconds since the epoch); `None` if absent.
pub(super) fn start_time(pid: i32) -> Result<Option<u64>, PlatformError> {
    match bsd_info(pid)? {
        Some(i) => start_token(i.pbi_start_tvsec, i.pbi_start_tvusec).map(Some),
        None => Ok(None),
    }
}

/// Usage of the group after verifying the leader's identity (R-075).
/// sysinfo reports one accumulated CPU figure, so it is returned as user time
/// and system time is 0.0 (the split is unavailable on this path).
pub(super) fn usage(group: GroupId) -> Result<Usage, PlatformError> {
    groups::validate_target(group.pgid)?;
    groups::check_leader(group, &start_time)?;
    let pgrp = u32::try_from(group.pgid).map_err(|_| PlatformError::InvalidGroup(group.pgid))?;
    let pids = pids_by_type(ProcFilter::ByProgramGroup { pgrpid: pgrp })
        .map_err(|m| PlatformError::parse("proc_listpids", m))?;
    let mut live = Vec::new();
    for pid in pids {
        let Ok(raw) = i32::try_from(pid) else {
            continue;
        };
        if let Some(info) = bsd_info(raw)? {
            if info.pbi_status != SZOMB {
                live.push(Pid::from_u32(pid));
            }
        }
    }
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&live),
        true,
        ProcessRefreshKind::everything(),
    );
    let mut u = Usage {
        cpu_user_s: 0.0,
        cpu_sys_s: 0.0,
        rss_bytes: 0,
        processes: 0,
    };
    for pid in &live {
        if let Some(p) = sys.process(*pid) {
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
