//! macOS memory via `sysctl hw.memsize` and `vm_stat`.

use super::run;
use crate::macos_parse::{parse_swapusage, parse_sysctl_u64, parse_vm_stat, snapshot};
use crate::{MemorySnapshot, PlatformError};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Total from `hw.memsize`; available = (free + inactive + purgeable) pages x page size.
pub(super) fn read() -> Result<MemorySnapshot, PlatformError> {
    let total = parse_sysctl_u64(
        "hw.memsize",
        &run("/usr/sbin/sysctl", &["-n", "hw.memsize"])?,
    )?;
    let vm = parse_vm_stat(&run("/usr/bin/vm_stat", &[])?)?;
    let mut m = snapshot(total, &vm)?;
    // swap size and use are best effort: a failure leaves them at zero
    if let Ok(text) = run("/usr/sbin/sysctl", &["-n", "vm.swapusage"])
        && let Ok((t, u)) = parse_swapusage(&text)
    {
        (m.swap_total_bytes, m.swap_used_bytes) = (t, u);
    }
    (m.cpu_busy, m.cpu_total) = cpu_counters();
    Ok(m)
}

/// Cumulative CPU counters in microseconds of all-core time. macOS has no counters we may read
/// without `unsafe`, so the safe `sysinfo` percentage since the previous reading is integrated
/// over the time between readings. The first reading has no interval and adds nothing.
fn cpu_counters() -> (u64, u64) {
    struct State {
        sys: sysinfo::System,
        last: Instant,
        busy_us: f64,
        total_us: f64,
    }
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    let cell = STATE.get_or_init(|| {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_usage();
        Mutex::new(State {
            sys,
            last: Instant::now(),
            busy_us: 0.0,
            total_us: 0.0,
        })
    });
    let Ok(mut st) = cell.lock() else {
        return (0, 0);
    };
    let dt = st.last.elapsed();
    // sysinfo needs a minimum interval between refreshes to give a meaningful percentage
    if dt >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL {
        st.sys.refresh_cpu_usage();
        let pct = f64::from(st.sys.global_cpu_usage()).clamp(0.0, 100.0);
        let us = dt.as_micros() as f64;
        st.busy_us += us * pct / 100.0;
        st.total_us += us;
        st.last = Instant::now();
    }
    (st.busy_us as u64, st.total_us as u64)
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_memory_parses() {
        assert!(super::read().unwrap().total_bytes > 0);
    }
}
