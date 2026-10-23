//! Pure parsers for macOS command output and kernel values. Compiled on every
//! OS under test so they run on Linux CI too.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::{Level, PlatformError};

/// Page counters from `vm_stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VmStat {
    pub page_size: u64,
    pub free: u64,
    pub inactive: u64,
    pub purgeable: u64,
    /// Pages read back from swap since boot (0 when `vm_stat` does not print it).
    pub swapins: u64,
    /// Pages written to swap since boot.
    pub swapouts: u64,
}

fn bad(what: &str, detail: impl Into<String>) -> PlatformError {
    PlatformError::parse(what, detail)
}

/// Parses `vm_stat`. Its `Pages free` already excludes speculative pages (R-070).
pub(crate) fn parse_vm_stat(text: &str) -> Result<VmStat, PlatformError> {
    let page_size = text
        .lines()
        .next()
        .and_then(|l| l.split("page size of ").nth(1))
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok())
        .ok_or_else(|| bad("vm_stat", "missing page size"))?;
    if page_size == 0 || !page_size.is_power_of_two() {
        return Err(bad("vm_stat", format!("invalid page size {page_size}")));
    }
    let pages = |name: &str| -> Result<u64, PlatformError> {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|r| r.trim().trim_end_matches('.').parse::<u64>().ok())
            .ok_or_else(|| bad("vm_stat", format!("missing or bad {name}")))
    };
    Ok(VmStat {
        page_size,
        free: pages("Pages free:")?,
        inactive: pages("Pages inactive:")?,
        purgeable: pages("Pages purgeable:")?,
        swapins: pages("Swapins:").unwrap_or(0),
        swapouts: pages("Swapouts:").unwrap_or(0),
    })
}

/// Available = (free + inactive + purgeable) pages x page size, overflow-checked.
pub(crate) fn available_bytes(vm: &VmStat) -> Result<u64, PlatformError> {
    vm.free
        .checked_add(vm.inactive)
        .and_then(|p| p.checked_add(vm.purgeable))
        .and_then(|p| p.checked_mul(vm.page_size))
        .ok_or_else(|| bad("vm_stat", "page counters overflow"))
}

/// Parses the single unsigned integer printed by `sysctl -n NAME`.
pub(crate) fn parse_sysctl_u64(name: &str, text: &str) -> Result<u64, PlatformError> {
    text.trim()
        .parse()
        .map_err(|_| bad(name, format!("not an unsigned integer: {:?}", text.trim())))
}

/// Maps `kern.memorystatus_vm_pressure_level` (1/2/4) to a level; others fail.
pub(crate) fn parse_pressure_level(text: &str) -> Result<Level, PlatformError> {
    match parse_sysctl_u64("kern.memorystatus_vm_pressure_level", text)? {
        1 => Ok(Level::Normal),
        2 => Ok(Level::Warn),
        4 => Ok(Level::Critical),
        other => Err(bad(
            "kern.memorystatus_vm_pressure_level",
            format!("unknown level {other}"),
        )),
    }
}

/// Parses the 1-minute average from `vm.loadavg` (`{ 1.23 1.50 1.60 }`).
pub(crate) fn parse_loadavg(text: &str) -> Result<f64, PlatformError> {
    text.trim()
        .trim_start_matches('{')
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .ok_or_else(|| bad("vm.loadavg", format!("malformed: {:?}", text.trim())))
}

/// CPU contention ESTIMATE: 1-minute load average divided by logical cores.
/// Not PSI; zero cores fail closed.
pub(crate) fn cpu_estimate(load1: f64, cores: u64) -> Result<f32, PlatformError> {
    if cores == 0 {
        return Err(bad("hw.logicalcpu", "zero cores"));
    }
    Ok((load1 / cores as f64) as f32)
}

/// Process start token in microseconds, keeping sub-second resolution (R-075).
pub(crate) fn start_token(seconds: u64, micros: u64) -> Result<u64, PlatformError> {
    if micros >= 1_000_000 {
        return Err(bad("proc_bsdinfo", "start microseconds out of range"));
    }
    seconds
        .checked_mul(1_000_000)
        .and_then(|s| s.checked_add(micros))
        .ok_or_else(|| bad("proc_bsdinfo", "start time overflow"))
}

/// Parses `sysctl -n vm.swapusage`: `total = 2048.00M  used = 1024.50M  free = 1023.50M  (encrypted)`.
/// Returns (total, used) in bytes. Units are K, M or G (binary, as the tool prints them).
pub(crate) fn parse_swapusage(text: &str) -> Result<(u64, u64), PlatformError> {
    let value = |name: &str| -> Result<u64, PlatformError> {
        let rest = text
            .split(name)
            .nth(1)
            .and_then(|r| r.trim_start().strip_prefix('='))
            .map(str::trim_start)
            .ok_or_else(|| bad("vm.swapusage", format!("missing {name}")))?;
        let word = rest.split_whitespace().next().unwrap_or("");
        let (num, unit) = word.split_at(word.len().saturating_sub(1));
        let mult: f64 = match unit {
            "K" => 1024.0,
            "M" => 1024.0 * 1024.0,
            "G" => 1024.0 * 1024.0 * 1024.0,
            _ => return Err(bad("vm.swapusage", format!("bad unit in {word:?}"))),
        };
        let n: f64 = num
            .parse()
            .map_err(|_| bad("vm.swapusage", format!("bad number in {word:?}")))?;
        if !n.is_finite() || n < 0.0 {
            return Err(bad("vm.swapusage", format!("bad number in {word:?}")));
        }
        Ok((n * mult) as u64)
    };
    let total = value("total")?;
    let used = value("used")?.min(total);
    Ok((total, used))
}

/// Memory snapshot from raw values; inconsistent totals fail closed.
pub(crate) fn snapshot(total: u64, vm: &VmStat) -> Result<crate::MemorySnapshot, PlatformError> {
    let available = available_bytes(vm)?;
    if total == 0 || available > total {
        return Err(bad("memory", "inconsistent totals"));
    }
    Ok(crate::MemorySnapshot {
        total_bytes: total,
        available_bytes: available,
        swap_in_bytes: vm.swapins.saturating_mul(vm.page_size),
        swap_out_bytes: vm.swapouts.saturating_mul(vm.page_size),
        ..crate::MemorySnapshot::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VM: &str = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
        Pages free:                               1000.\n\
        Pages active:                             5000.\n\
        Pages inactive:                           2000.\n\
        Pages speculative:                         300.\n\
        Pages purgeable:                           500.\n";

    // R-357
    #[test]
    fn swap_usage_and_swap_counters_parse() {
        assert_eq!(
            parse_swapusage("total = 2048.00M  used = 1024.50M  free = 1023.50M  (encrypted)\n")
                .unwrap(),
            (2048 << 20, 1_074_266_112)
        );
        assert_eq!(
            parse_swapusage("total = 0.00M  used = 0.00M  free = 0.00M  (encrypted)").unwrap(),
            (0, 0)
        );
        assert!(parse_swapusage("junk").is_err());
        assert!(parse_swapusage("total = 1X used = 1M").is_err());
        let vm = format!(
            "{VM}Swapins:                                    10.\nSwapouts:                                   20.\n"
        );
        let v = parse_vm_stat(&vm).unwrap();
        assert_eq!((v.swapins, v.swapouts), (10, 20));
        let m = snapshot(1 << 40, &v).unwrap();
        assert_eq!(
            (m.swap_in_bytes, m.swap_out_bytes),
            (10 * 16384, 20 * 16384)
        );
        // a vm_stat without the lines gives zero counters
        assert_eq!(parse_vm_stat(VM).unwrap().swapins, 0);
    }

    // R-070
    #[test]
    fn available_uses_the_real_page_size() {
        let vm = parse_vm_stat(VM).unwrap();
        assert_eq!(vm.page_size, 16384);
        assert_eq!(available_bytes(&vm).unwrap(), 3500 * 16384);
        let s = snapshot(1 << 30, &vm).unwrap();
        assert_eq!(s.available_bytes, 3500 * 16384);
    }

    // R-070
    #[test]
    fn bad_samples_fail_closed() {
        assert!(parse_vm_stat("").is_err());
        assert!(parse_vm_stat(&VM.replace("16384", "1000")).is_err());
        assert!(parse_vm_stat(&VM.replace("Pages free:", "Pages freed:")).is_err());
        assert!(parse_vm_stat(&VM.replace("1000.", "lots.")).is_err());
        let vm = parse_vm_stat(VM).unwrap();
        assert!(snapshot(0, &vm).is_err());
        assert!(snapshot(1000, &vm).is_err(), "available above total");
        let huge = VmStat {
            free: u64::MAX,
            ..vm
        };
        assert!(available_bytes(&huge).is_err());
    }

    // R-070, R-072
    #[test]
    fn native_levels_are_mapped_and_unknown_is_an_error() {
        assert_eq!(parse_pressure_level("1\n").unwrap(), Level::Normal);
        assert_eq!(parse_pressure_level("2\n").unwrap(), Level::Warn);
        assert_eq!(parse_pressure_level("4\n").unwrap(), Level::Critical);
        for bad in ["3", "0", "", "warn", "-1"] {
            assert!(parse_pressure_level(bad).is_err(), "{bad}");
        }
    }

    // R-071
    #[test]
    fn malformed_load_and_cpu_counts_fail_closed() {
        assert_eq!(parse_loadavg("{ 2.50 1.00 0.50 }\n").unwrap(), 2.5);
        for bad in ["", "{ }", "{ abc 1 2 }", "{ -1.0 1 2 }", "{ NaN 1 2 }"] {
            assert!(parse_loadavg(bad).is_err(), "{bad}");
        }
        assert!(parse_sysctl_u64("hw.logicalcpu", "x").is_err());
        assert!(cpu_estimate(1.0, 0).is_err());
    }

    #[test]
    fn cpu_estimate_is_load_per_core() {
        assert_eq!(cpu_estimate(4.0, 8).unwrap(), 0.5);
        assert_eq!(cpu_estimate(16.0, 8).unwrap(), 2.0);
    }

    // R-075
    #[test]
    fn start_token_preserves_subsecond_generation() {
        assert_ne!(start_token(10, 1).unwrap(), start_token(10, 2).unwrap());
        assert_eq!(start_token(10, 5).unwrap(), 10_000_005);
        assert!(start_token(1, 1_000_000).is_err());
        assert!(start_token(u64::MAX, 0).is_err());
    }
}
