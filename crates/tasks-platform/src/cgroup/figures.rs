//! Parsers for the cgroup v2 statistics files.

use crate::PlatformError;

/// Bytes and operations summed over every device in `io.stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IoTotals {
    /// Bytes read.
    pub rbytes: u64,
    /// Bytes written.
    pub wbytes: u64,
    /// Read operations.
    pub rios: u64,
    /// Write operations.
    pub wios: u64,
}

/// Cumulative stall times from a cgroup pressure file (`total=` in microseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PsiTotals {
    /// Time some task was stalled.
    pub some_us: u64,
    /// Time all tasks were stalled at once; absent on files without a `full` line.
    pub full_us: Option<u64>,
}

/// Parses `io.stat`: one line per device, `8:0 rbytes=1 wbytes=2 rios=3 wios=4 dbytes=0 dios=0`.
/// An empty file (no I/O yet) is all zeros; a value that is not a number is an error.
pub fn parse_io_stat(text: &str) -> Result<IoTotals, PlatformError> {
    let mut t = IoTotals::default();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        for word in line.split_whitespace().skip(1) {
            let Some((key, value)) = word.split_once('=') else {
                continue;
            };
            let slot = match key {
                "rbytes" => &mut t.rbytes,
                "wbytes" => &mut t.wbytes,
                "rios" => &mut t.rios,
                "wios" => &mut t.wios,
                _ => continue,
            };
            let v = value
                .parse::<u64>()
                .map_err(|_| PlatformError::parse("io.stat", format!("bad value in {line:?}")))?;
            *slot = slot.saturating_add(v);
        }
    }
    Ok(t)
}

/// Parses the `total=` of the `some` and `full` lines of a cgroup `*.pressure` file.
pub fn parse_psi_totals(text: &str) -> Result<PsiTotals, PlatformError> {
    let mut some = None;
    let mut full = None;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut words = line.split_whitespace();
        let kind = words.next().unwrap_or_default();
        let total = words
            .find_map(|w| w.strip_prefix("total="))
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| PlatformError::parse("psi", format!("bad line {line:?}")))?;
        match kind {
            "some" => some = Some(total),
            "full" => full = Some(total),
            _ => {}
        }
    }
    Ok(PsiTotals {
        some_us: some.ok_or_else(|| PlatformError::parse("psi", "missing `some` line"))?,
        full_us: full,
    })
}

/// The `usage_usec` line of `cpu.stat` (user plus system CPU time).
pub fn parse_cpu_usage(text: &str) -> Result<u64, PlatformError> {
    text.lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .ok_or_else(|| PlatformError::parse("cpu.stat", "no usage_usec line"))
}

/// A file holding one number (`memory.peak`, `memory.current`); `max` counts as no value.
pub fn parse_single_u64(what: &str, text: &str) -> Result<Option<u64>, PlatformError> {
    let t = text.trim();
    if t == "max" {
        return Ok(None);
    }
    t.parse::<u64>()
        .map(Some)
        .map_err(|_| PlatformError::parse(what.to_string(), format!("not a number: {t:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // R-325
    #[test]
    fn r325_io_stat_sums_devices_and_tolerates_empty() {
        let t = parse_io_stat(
            "259:0 rbytes=100 wbytes=200 rios=3 wios=4 dbytes=0 dios=0\n\
             8:16 rbytes=1 wbytes=2 rios=1 wios=1 dbytes=9 dios=9\n",
        )
        .unwrap();
        assert_eq!(
            t,
            IoTotals {
                rbytes: 101,
                wbytes: 202,
                rios: 4,
                wios: 5
            }
        );
        assert_eq!(parse_io_stat("").unwrap(), IoTotals::default());
        assert!(parse_io_stat("8:0 rbytes=x\n").is_err());
    }

    // R-325
    #[test]
    fn r325_pressure_totals_with_and_without_a_full_line() {
        let both = "some avg10=0.00 avg60=0.00 avg300=0.00 total=1500\n\
                    full avg10=0.00 avg60=0.00 avg300=0.00 total=700\n";
        assert_eq!(
            parse_psi_totals(both).unwrap(),
            PsiTotals {
                some_us: 1500,
                full_us: Some(700)
            }
        );
        let cpu = "some avg10=1.00 avg60=0.00 avg300=0.00 total=9\n";
        assert_eq!(parse_psi_totals(cpu).unwrap().full_us, None);
        assert!(parse_psi_totals("full avg10=0 total=3\n").is_err());
    }

    // R-325
    #[test]
    fn r325_cpu_and_single_value_files() {
        let stat = "usage_usec 42\nuser_usec 30\nsystem_usec 12\n";
        assert_eq!(parse_cpu_usage(stat).unwrap(), 42);
        assert!(parse_cpu_usage("user_usec 1\n").is_err());
        assert_eq!(
            parse_single_u64("memory.peak", "12345\n").unwrap(),
            Some(12345)
        );
        assert_eq!(parse_single_u64("memory.max", "max\n").unwrap(), None);
        assert!(parse_single_u64("memory.peak", "x").is_err());
    }
}
