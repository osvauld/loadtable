//! The macOS process table: `sysinfo` for names, memory, CPU and per-process disk bytes
//! (sysinfo reads `proc_pid_rusage`, whose `ri_diskio_bytesread` and `ri_diskio_byteswritten`
//! are the storage bytes), `proc_pidinfo` for the process group and the start token.
//!
//! This path is type-checked from Linux but only runs on macOS. The disk figures are per process
//! and cumulative; the sampler turns them into per-window and per-job numbers.

use super::bsdinfo::bsd_info;
use crate::macos_parse::start_token;
use crate::sample::clean_program_name;
use crate::{PlatformError, ProcInfo};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

pub(super) fn snapshot() -> Result<Vec<ProcInfo>, PlatformError> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    let mut out = Vec::new();
    for (pid, p) in sys.processes() {
        let Ok(raw) = i32::try_from(pid.as_u32()) else {
            continue;
        };
        // a process that exits mid-read, or one we may not inspect, drops out
        let Ok(Some(info)) = bsd_info(raw) else {
            continue;
        };
        if info.zombie {
            continue;
        }
        let Ok(start) = start_token(info.start_sec, info.start_usec) else {
            continue;
        };
        let disk = p.disk_usage();
        out.push(ProcInfo {
            pid: raw,
            ppid: p
                .parent()
                .and_then(|pp| i32::try_from(pp.as_u32()).ok())
                .unwrap_or(0),
            pgid: i32::try_from(info.pgid).unwrap_or(0),
            start,
            name: clean_program_name(&p.name().to_string_lossy()),
            rss_bytes: p.memory(),
            cpu_usec: p.accumulated_cpu_time().saturating_mul(1000),
            io_read: Some(disk.total_read_bytes),
            io_write: Some(disk.total_written_bytes),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_table_lists_this_process() {
        let me = i32::try_from(std::process::id()).unwrap_or(-1);
        let t = super::snapshot().unwrap();
        assert!(t.iter().any(|p| p.pid == me));
    }
}
