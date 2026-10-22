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
    Ok(MemorySnapshot {
        total_bytes,
        available_bytes,
    })
}

/// Reads and parses the host's `/proc/meminfo`.
pub(super) fn read() -> Result<MemorySnapshot, PlatformError> {
    let text = std::fs::read_to_string(PATH).map_err(|e| PlatformError::io(PATH, e))?;
    parse_meminfo(&text)
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
        assert!(read().unwrap().total_bytes > 0);
    }
}
