//! macOS memory via `sysctl hw.memsize` and `vm_stat`.

use super::run;
use crate::macos_parse::{parse_sysctl_u64, parse_vm_stat, snapshot};
use crate::{MemorySnapshot, PlatformError};

/// Total from `hw.memsize`; available = (free + inactive + purgeable) pages x page size.
pub(super) fn read() -> Result<MemorySnapshot, PlatformError> {
    let total = parse_sysctl_u64(
        "hw.memsize",
        &run("/usr/sbin/sysctl", &["-n", "hw.memsize"])?,
    )?;
    let vm = parse_vm_stat(&run("/usr/bin/vm_stat", &[])?)?;
    snapshot(total, &vm)
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_memory_parses() {
        assert!(super::read().unwrap().total_bytes > 0);
    }
}
