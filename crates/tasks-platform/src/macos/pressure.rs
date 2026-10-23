//! macOS pressure: native memorystatus level plus a load-based CPU estimate.

use super::run;
use crate::macos_parse::{cpu_estimate, parse_loadavg, parse_pressure_level, parse_sysctl_u64};
use crate::{PlatformError, PressureSnapshot};

/// Native memory level; `cpu` is `loadavg[0] / logical cores` (an estimate, not
/// PSI); I/O and PSI percentages do not exist on macOS and are `None`.
pub(super) fn read() -> Result<PressureSnapshot, PlatformError> {
    let sysctl = |name: &str| run("/usr/sbin/sysctl", &["-n", name]);
    let memory = parse_pressure_level(&sysctl("kern.memorystatus_vm_pressure_level")?)?;
    let load = parse_loadavg(&sysctl("vm.loadavg")?)?;
    let cores = parse_sysctl_u64("hw.logicalcpu", &sysctl("hw.logicalcpu")?)?;
    Ok(PressureSnapshot {
        memory,
        memory_some: None,
        memory_full: None,
        cpu: Some(cpu_estimate(load, cores)?),
        io: None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_pressure_parses() {
        let s = super::read().unwrap();
        assert!(s.io.is_none() && s.cpu.is_some());
    }
}
