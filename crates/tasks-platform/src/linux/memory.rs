//! `/proc/meminfo` parsing.

use crate::{MemorySnapshot, PlatformError};

const PATH: &str = "/proc/meminfo";

/// Parses `MemTotal` and `MemAvailable` (R-069: never MemFree or swap).
pub fn parse_meminfo(text: &str) -> Result<MemorySnapshot, PlatformError> {
    let field = |name: &str| -> Result<u64, PlatformError> {
        let rest = text
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .ok_or_else(|| PlatformError::parse(PATH, format!("missing {name}")))?;
        let mut words = rest.split_whitespace();
        let kb: u64 = words
            .next()
            .and_then(|w| w.parse().ok())
            .ok_or_else(|| PlatformError::parse(PATH, format!("bad value for {name}")))?;
        if words.next() != Some("kB") || words.next().is_some() {
            return Err(PlatformError::parse(PATH, format!("bad unit for {name}")));
        }
        kb.checked_mul(1024)
            .ok_or_else(|| PlatformError::parse(PATH, format!("overflow in {name}")))
    };
    let total_bytes = field("MemTotal:")?;
    let available_bytes = field("MemAvailable:")?;
    if total_bytes == 0 || available_bytes > total_bytes {
        return Err(PlatformError::parse(PATH, "inconsistent totals"));
    }
    // swap is best effort: a kernel without it reports no SwapTotal, which is "no swap"
    let swap_total_bytes = field("SwapTotal:").unwrap_or(0);
    let swap_free = field("SwapFree:").unwrap_or(0);
    Ok(MemorySnapshot {
        total_bytes,
        available_bytes,
        swap_total_bytes,
        swap_used_bytes: swap_total_bytes.saturating_sub(swap_free),
        ..MemorySnapshot::default()
    })
}

/// `pswpin` and `pswpout` (pages) from `/proc/vmstat`, as bytes of `page_size`.
pub fn parse_vmstat_swap(text: &str, page_size: u64) -> (u64, u64) {
    let get = |name: &str| -> u64 {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|r| r.trim().parse::<u64>().ok())
            .map_or(0, |p| p.saturating_mul(page_size))
    };
    (get("pswpin "), get("pswpout "))
}

/// Busy and total CPU jiffies from the aggregate `cpu` line of `/proc/stat`. Busy is everything
/// but idle and iowait. `None` when the line is missing or malformed.
pub fn parse_stat_cpu(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|w| w.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    // guest time is already inside user/nice, so only the first eight columns are summed
    let total: u64 = v.iter().take(8).sum();
    let idle = v.get(3)?.saturating_add(v.get(4).copied().unwrap_or(0));
    Some((total.saturating_sub(idle), total))
}

/// Reads and parses the host's `/proc/meminfo`, `/proc/vmstat` and `/proc/stat`.
pub(super) fn read() -> Result<MemorySnapshot, PlatformError> {
    let text = std::fs::read_to_string(PATH).map_err(|e| PlatformError::io(PATH, e))?;
    let mut m = parse_meminfo(&text)?;
    if let Ok(vm) = std::fs::read_to_string("/proc/vmstat") {
        let page = rustix::param::page_size() as u64;
        (m.swap_in_bytes, m.swap_out_bytes) = parse_vmstat_swap(&vm, page);
    }
    if let Ok(st) = std::fs::read_to_string("/proc/stat")
        && let Some((busy, total)) = parse_stat_cpu(&st)
    {
        (m.cpu_busy, m.cpu_total) = (busy, total);
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "MemTotal:       16000000 kB\nMemFree:          100000 kB\n\
        MemAvailable:    8000000 kB\nSwapTotal:      4000000 kB\nSwapFree:       4000000 kB\n";

    // R-069
    #[test]
    fn memavailable_is_not_free_or_swap() {
        let m = parse_meminfo(FIXTURE).unwrap();
        assert_eq!(m.total_bytes, 16_000_000 * 1024);
        assert_eq!(m.available_bytes, 8_000_000 * 1024);
    }

    // R-357
    #[test]
    fn swap_is_read_from_meminfo_and_vmstat_and_cpu_from_stat() {
        let m = parse_meminfo(
            "MemTotal: 10 kB\nMemAvailable: 5 kB\nSwapTotal: 100 kB\nSwapFree: 40 kB\n",
        )
        .unwrap();
        assert_eq!(
            (m.swap_total_bytes, m.swap_used_bytes),
            (100 * 1024, 60 * 1024)
        );
        // no swap lines at all: no swap, not an error
        let m = parse_meminfo("MemTotal: 10 kB\nMemAvailable: 5 kB\n").unwrap();
        assert_eq!((m.swap_total_bytes, m.swap_used_bytes), (0, 0));
        assert_eq!(
            parse_vmstat_swap("pswpin 10\npswpout 3\npswpout_x 9\n", 4096),
            (40_960, 12_288)
        );
        assert_eq!(parse_vmstat_swap("nr_free 1\n", 4096), (0, 0));
        // user nice system idle iowait irq softirq steal guest guest_nice
        assert_eq!(
            parse_stat_cpu("cpu  100 0 50 800 40 5 5 0 7 0\ncpu0 1 2 3 4\n"),
            Some((160, 1000))
        );
        assert_eq!(parse_stat_cpu("cpu0 1 2 3 4\n"), None);
        assert_eq!(parse_stat_cpu("cpu  1 x 3 4\n"), None);
    }

    #[test]
    fn missing_or_bad_fields_are_errors() {
        assert!(parse_meminfo("MemTotal: 10 kB\n").is_err());
        assert!(parse_meminfo("MemTotal: 10 MB\nMemAvailable: 1 kB\n").is_err());
        assert!(parse_meminfo("MemTotal: x kB\nMemAvailable: 1 kB\n").is_err());
        assert!(parse_meminfo("MemTotal: 10 kB\nMemAvailable: 11 kB\n").is_err());
        assert!(parse_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB\n").is_err());
    }

    #[test]
    fn host_meminfo_parses() {
        let m = read().unwrap();
        assert!(m.total_bytes > 0);
        assert!(m.cpu_total > 0);
    }
}
