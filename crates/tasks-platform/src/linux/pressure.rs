//! `/proc/pressure/*` (PSI) parsing and the Linux pressure snapshot.

use crate::{PlatformError, PressureSnapshot, PressureThresholds};
use std::path::PathBuf;

/// One PSI line: the `avg10` stall percentage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PsiLine {
    /// Share of the last 10 s with stalled tasks, percent.
    pub avg10: f32,
}

/// A parsed PSI file. CPU files on older kernels have no `full` line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PsiFile {
    /// The `some` line (required).
    pub some: PsiLine,
    /// The `full` line, when present.
    pub full: Option<PsiLine>,
}

/// Parses a PSI file; a missing `some` line or a malformed value is an error.
pub fn parse_psi(text: &str) -> Result<PsiFile, PlatformError> {
    let mut some = None;
    let mut full = None;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut words = line.split_whitespace();
        let kind = words.next().unwrap_or_default();
        let avg10 = words
            .find_map(|w| w.strip_prefix("avg10="))
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .ok_or_else(|| PlatformError::parse("psi", format!("bad line {line:?}")))?;
        let parsed = PsiLine { avg10 };
        match kind {
            "some" => some = Some(parsed),
            "full" => full = Some(parsed),
            other => {
                return Err(PlatformError::parse(
                    "psi",
                    format!("unknown kind {other:?}"),
                ));
            }
        }
    }
    let some = some.ok_or_else(|| PlatformError::parse("psi", "missing `some` line"))?;
    Ok(PsiFile { some, full })
}

fn read_file(name: &str) -> Result<PsiFile, PlatformError> {
    let path = PathBuf::from("/proc/pressure").join(name);
    // R-054: a missing file names itself and the kernel requirement.
    let text = std::fs::read_to_string(&path).map_err(|e| PlatformError::PsiUnavailable {
        path: path.clone(),
        detail: e.to_string(),
    })?;
    parse_psi(&text).map_err(|e| PlatformError::PsiUnavailable {
        path,
        detail: e.to_string(),
    })
}

/// Maps already-parsed files to a snapshot (pure; R-053).
pub(super) fn snapshot_from(
    cpu: PsiFile,
    memory: PsiFile,
    io: PsiFile,
    t: &PressureThresholds,
) -> Result<PressureSnapshot, PlatformError> {
    t.validate()?;
    let full = memory
        .full
        .ok_or_else(|| PlatformError::parse("psi memory", "missing `full` line"))?;
    Ok(PressureSnapshot {
        memory: t.level(memory.some.avg10, full.avg10),
        memory_some: Some(memory.some.avg10),
        memory_full: Some(full.avg10),
        cpu: Some(cpu.some.avg10),
        io: Some(io.some.avg10),
    })
}

/// Reads the three PSI files. Any failure refuses (fail closed).
pub(super) fn read(t: &PressureThresholds) -> Result<PressureSnapshot, PlatformError> {
    snapshot_from(read_file("cpu")?, read_file("memory")?, read_file("io")?, t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Level;

    const MEM: &str = "some avg10=1.50 avg60=0.80 avg300=0.10 total=12345\n\
                       full avg10=0.25 avg60=0.10 avg300=0.00 total=999\n";
    const CPU_OLD: &str = "some avg10=12.00 avg60=3.00 avg300=1.00 total=5\n";

    fn th() -> PressureThresholds {
        PressureThresholds {
            memory_some_warn: 10.0,
            memory_some_critical: 30.0,
            memory_full_warn: 2.0,
            memory_full_critical: 5.0,
        }
    }

    #[test]
    fn parses_some_and_full() {
        let f = parse_psi(MEM).unwrap();
        assert_eq!(f.some.avg10, 1.5);
        assert_eq!(f.full.unwrap().avg10, 0.25);
        assert_eq!(parse_psi(CPU_OLD).unwrap().full, None);
    }

    // R-053
    #[test]
    fn malformed_psi_is_an_error() {
        for bad in [
            "",
            "full avg10=1.0\n",
            "some avg60=1.0\n",
            "some avg10=abc\n",
            "some avg10=-1\n",
            "some avg10=NaN\n",
            "weird avg10=1\n",
        ] {
            assert!(parse_psi(bad).is_err(), "{bad:?}");
        }
    }

    // R-053
    #[test]
    fn thresholds_map_with_inclusive_boundaries() {
        let t = th();
        assert_eq!(t.level(9.99, 1.99), Level::Normal);
        assert_eq!(t.level(10.0, 0.0), Level::Warn);
        assert_eq!(t.level(0.0, 2.0), Level::Warn);
        assert_eq!(t.level(30.0, 0.0), Level::Critical);
        assert_eq!(t.level(0.0, 5.0), Level::Critical);
        assert_eq!(t.level(f32::MAX, 0.0), Level::Critical);
    }

    // R-053
    #[test]
    fn invalid_thresholds_fail_closed() {
        let mut t = th();
        t.memory_some_warn = f32::NAN;
        let (m, c) = (parse_psi(MEM).unwrap(), parse_psi(CPU_OLD).unwrap());
        assert!(matches!(
            snapshot_from(c, m, c, &t),
            Err(PlatformError::InvalidThresholds(_))
        ));
        t = th();
        t.memory_full_warn = 9.0;
        assert!(snapshot_from(c, m, c, &t).is_err());
    }

    #[test]
    fn snapshot_combines_files() {
        let (m, c) = (parse_psi(MEM).unwrap(), parse_psi(CPU_OLD).unwrap());
        let s = snapshot_from(c, m, c, &th()).unwrap();
        assert_eq!(s.memory, Level::Normal);
        assert_eq!(s.cpu, Some(12.0));
        assert_eq!(s.memory_full, Some(0.25));
        // memory file without a full line cannot be judged
        assert!(snapshot_from(c, c, c, &th()).is_err());
    }

    // R-054
    #[test]
    fn missing_file_error_names_file_and_kernel_requirement() {
        let e = PlatformError::PsiUnavailable {
            path: "/proc/pressure/memory".into(),
            detail: "No such file".into(),
        };
        let text = e.to_string();
        assert!(text.contains("/proc/pressure/memory"));
        assert!(text.contains("CONFIG_PSI") && text.contains("psi=1"));
    }

    #[test]
    fn host_pressure_parses_or_names_the_missing_file() {
        let t = PressureThresholds {
            memory_some_warn: 10.0,
            memory_some_critical: 30.0,
            memory_full_warn: 2.0,
            memory_full_critical: 5.0,
        };
        match read(&t) {
            Ok(_) => {}
            Err(PlatformError::PsiUnavailable { path, .. }) => {
                assert!(path.starts_with("/proc/pressure"));
            }
            Err(other) => panic!("unexpected {other}"),
        }
    }
}
