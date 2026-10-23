//! What a machine row says, decided here and only painted by the window: the memory level in
//! plain words, RAM used against total, and how the busy cores split between the jobs (R-644,
//! R-646).

use crate::format::UNKNOWN;

/// The stall percentage from which a machine is `tight` when its own limit is not known: the
/// shipped policy default (`memory.some_warn`).
pub const DEFAULT_STALL_LIMIT: f32 = 10.0;

/// A machine's memory in the words every surface uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemLevel {
    /// Plenty: nothing waits for memory.
    Ok,
    /// Stalling enough that a new compile job waits.
    Tight,
    /// Stalling badly.
    Critical,
    /// Not measured.
    Unknown,
}

impl MemLevel {
    /// `ok`, `tight`, `critical` or `unknown`.
    pub fn word(self) -> &'static str {
        match self {
            MemLevel::Ok => "ok",
            MemLevel::Tight => "tight",
            MemLevel::Critical => "critical",
            MemLevel::Unknown => "unknown",
        }
    }
}

/// The level of a machine's memory. The machine's own level wins (Linux derives it from its
/// stall readings against its policy, macOS reads the system's native level: `normal` is ok,
/// `warn` tight). An older peer that sent only a stall percentage is judged against `limit`
/// (or the default): at or above it is tight, at or above three times it critical, which is
/// the shipped 10 % / 30 % pair (R-646).
pub fn memory_level(level: Option<&str>, stall: Option<f32>, limit: Option<f32>) -> MemLevel {
    match level {
        Some("normal") => return MemLevel::Ok,
        Some("warn") => return MemLevel::Tight,
        Some("critical") => return MemLevel::Critical,
        _ => {}
    }
    let Some(s) = stall else {
        return MemLevel::Unknown;
    };
    let limit = limit.filter(|l| *l > 0.0).unwrap_or(DEFAULT_STALL_LIMIT);
    if s >= limit * 3.0 {
        MemLevel::Critical
    } else if s >= limit {
        MemLevel::Tight
    } else {
        MemLevel::Ok
    }
}

/// Bytes in GiB for a RAM figure: `8.0`, or MiB under a GiB.
fn gib_one(b: u64) -> String {
    if b < 1 << 30 {
        format!("{:.0} MiB", b as f64 / (1u64 << 20) as f64)
    } else {
        format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64)
    }
}

/// The size of the machine's memory: `16 GiB`, `15.8 GiB`, `512 MiB`.
pub fn gib_total(b: u64) -> String {
    let g = b as f64 / (1u64 << 30) as f64;
    if g < 1.0 {
        format!("{:.0} MiB", b as f64 / (1u64 << 20) as f64)
    } else if (g - g.round()).abs() < 0.05 {
        format!("{g:.0} GiB")
    } else {
        format!("{g:.1} GiB")
    }
}

/// RAM in use against its size: `8.0 of 16 GiB used`. With only the size known it says only
/// that (`64 GiB total`); with only what is free, that.
pub fn ram_used(total: Option<u64>, available: Option<u64>) -> String {
    match (total, available) {
        (Some(t), Some(a)) => {
            let used = t.saturating_sub(a);
            // the unit is said once when both figures are in GiB
            let u = gib_one(used);
            let t_text = gib_total(t);
            match (u.strip_suffix(" GiB"), t_text.ends_with(" GiB")) {
                (Some(n), true) => format!("{n} of {t_text} used"),
                _ => format!("{u} of {t_text} used"),
            }
        }
        (Some(t), None) => format!("{} total", gib_total(t)),
        (None, Some(a)) => format!("{} free", gib_one(a)),
        (None, None) => UNKNOWN.to_string(),
    }
}

/// One running job's share of the machine's CPU.
#[derive(Debug, Clone, PartialEq)]
pub struct CpuShare {
    /// `#101 ui-claude`.
    pub label: String,
    /// The agent's hue index, when the job is one of this window's rows.
    pub hue: Option<usize>,
    /// Cores in use.
    pub cores: f64,
}

/// How a machine's cores are in use: by each running job and by everything else.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CpuLoad {
    /// The jobs, busiest first.
    pub jobs: Vec<CpuShare>,
    /// Cores in use outside the jobs, when the machine's total use is known.
    pub outside: Option<f64>,
    /// Logical cores of the machine, when known.
    pub cores: Option<f64>,
}

/// A piece of the CPU bar.
#[derive(Debug, Clone, PartialEq)]
pub struct BarPiece {
    /// Share of the bar's full width, 0..=1.
    pub frac: f32,
    /// The job's hue, or `None` for work outside the jobs.
    pub hue: Option<usize>,
    /// Its label.
    pub label: String,
}

impl CpuLoad {
    /// The bar's pieces from the left: each job, then the rest. Fractions are of the machine's
    /// cores, or of the cores in use when the total is not known, and never add up to more
    /// than 1.
    pub fn pieces(&self) -> Vec<BarPiece> {
        let used: f64 =
            self.jobs.iter().map(|j| j.cores).sum::<f64>() + self.outside.unwrap_or(0.0);
        let scale = match self.cores {
            Some(c) if c > 0.0 => c.max(used),
            _ => used,
        };
        if scale <= 0.0 {
            return Vec::new();
        }
        let mut out: Vec<BarPiece> = self
            .jobs
            .iter()
            .filter(|j| j.cores > 0.0)
            .map(|j| BarPiece {
                frac: (j.cores / scale) as f32,
                hue: j.hue,
                label: j.label.clone(),
            })
            .collect();
        if let Some(o) = self.outside.filter(|o| *o > 0.0) {
            out.push(BarPiece {
                frac: (o / scale) as f32,
                hue: None,
                label: "outside jobs".to_string(),
            });
        }
        out
    }

    /// The bar's tooltip: `#101 5.4 cores, #102 1.1 cores, outside jobs 1.0 cores of 10`.
    pub fn tooltip(&self) -> String {
        let mut parts: Vec<String> = self
            .jobs
            .iter()
            .map(|j| format!("{}: {:.1} cores", j.label, j.cores))
            .collect();
        if let Some(o) = self.outside {
            parts.push(format!("outside jobs: {o:.1} cores"));
        }
        if parts.is_empty() {
            return "No job is using this machine's CPU.".to_string();
        }
        let total = self
            .cores
            .map(|c| format!("\nof {c:.0} cores in all"))
            .unwrap_or_default();
        format!("{}{total}", parts.join("\n"))
    }
}

/// The cores in use outside the jobs: the machine's total use less what the jobs use, never
/// below zero; `None` when the total is not known.
pub fn outside_jobs(total_in_use: Option<f64>, jobs: &[CpuShare]) -> Option<f64> {
    let sum: f64 = jobs.iter().map(|j| j.cores).sum();
    total_in_use.map(|t| (t - sum).max(0.0))
}

/// CPU busy percent from cores in use and the machine's cores, when both are known.
pub fn busy_from_cores(in_use: Option<f64>, cores: Option<f64>) -> Option<f32> {
    match (in_use, cores) {
        (Some(u), Some(c)) if c > 0.0 => Some((u / c * 100.0).clamp(0.0, 100.0) as f32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // R-646
    #[test]
    fn r646_the_machines_own_level_decides_and_a_bare_percentage_is_judged_by_its_limit() {
        assert_eq!(memory_level(Some("normal"), Some(50.0), None), MemLevel::Ok);
        assert_eq!(memory_level(Some("warn"), None, None), MemLevel::Tight);
        assert_eq!(
            memory_level(Some("critical"), None, None),
            MemLevel::Critical
        );
        assert_eq!(memory_level(None, None, None), MemLevel::Unknown);
        assert_eq!(memory_level(Some("unknown"), None, None), MemLevel::Unknown);
        // no level: at the limit is tight, three times it critical
        assert_eq!(memory_level(None, Some(9.9), Some(10.0)), MemLevel::Ok);
        assert_eq!(memory_level(None, Some(10.0), Some(10.0)), MemLevel::Tight);
        assert_eq!(memory_level(None, Some(29.9), None), MemLevel::Tight);
        assert_eq!(memory_level(None, Some(30.0), None), MemLevel::Critical);
        assert_eq!(
            memory_level(None, Some(20.0), Some(5.0)),
            MemLevel::Critical
        );
        assert_eq!(MemLevel::Tight.word(), "tight");
    }

    // R-643
    #[test]
    fn r643_ram_says_what_is_used_and_only_what_is_known() {
        assert_eq!(
            ram_used(Some(16 << 30), Some(8 << 30)),
            "8.0 of 16 GiB used"
        );
        assert_eq!(
            ram_used(Some(64 << 30), Some(51 << 30)),
            "13.0 of 64 GiB used"
        );
        assert_eq!(ram_used(Some(64 << 30), None), "64 GiB total");
        assert_eq!(ram_used(None, Some(3 << 29)), "1.5 GiB free");
        assert_eq!(ram_used(None, None), UNKNOWN);
        assert_eq!(
            ram_used(Some(512 << 20), Some(128 << 20)),
            "384 MiB of 512 MiB used"
        );
        // available above total (a racy reading) is not a negative use
        assert_eq!(
            ram_used(Some(4 << 30), Some(5 << 30)),
            "0 MiB of 4 GiB used"
        );
    }

    // R-644
    #[test]
    fn r644_the_cpu_bar_splits_the_cores_between_the_jobs_and_the_rest() {
        let jobs = vec![
            CpuShare {
                label: "#101 ui".into(),
                hue: Some(0),
                cores: 5.0,
            },
            CpuShare {
                label: "#102 core".into(),
                hue: Some(1),
                cores: 1.0,
            },
        ];
        let load = CpuLoad {
            outside: outside_jobs(Some(7.0), &jobs),
            jobs,
            cores: Some(10.0),
        };
        assert_eq!(load.outside, Some(1.0));
        let p = load.pieces();
        let fr: Vec<f32> = p.iter().map(|x| x.frac).collect();
        assert_eq!(fr, vec![0.5, 0.1, 0.1]);
        assert_eq!(p[2].hue, None);
        assert!(load.tooltip().contains("#101 ui: 5.0 cores"));
        assert!(load.tooltip().contains("of 10 cores in all"));
    }

    #[test]
    fn r644_the_bar_never_overflows_and_an_idle_or_unknown_machine_has_no_pieces() {
        let busy = CpuLoad {
            jobs: vec![CpuShare {
                label: "#1 a".into(),
                hue: None,
                cores: 12.0,
            }],
            outside: Some(2.0),
            cores: Some(8.0),
        };
        let total: f32 = busy.pieces().iter().map(|p| p.frac).sum();
        assert!((total - 1.0).abs() < 1e-5, "{total}");
        assert!(CpuLoad::default().pieces().is_empty());
        assert_eq!(
            CpuLoad::default().tooltip(),
            "No job is using this machine's CPU."
        );
        // the total is unknown: the bar shows shares of what is in use
        let rel = CpuLoad {
            jobs: vec![CpuShare {
                label: "#1 a".into(),
                hue: None,
                cores: 1.0,
            }],
            outside: Some(1.0),
            cores: None,
        };
        assert_eq!(
            rel.pieces().iter().map(|p| p.frac).collect::<Vec<_>>(),
            vec![0.5, 0.5]
        );
        assert_eq!(outside_jobs(None, &[]), None);
        assert_eq!(busy_from_cores(Some(4.0), Some(16.0)), Some(25.0));
        assert_eq!(busy_from_cores(Some(40.0), Some(16.0)), Some(100.0));
        assert_eq!(busy_from_cores(None, Some(16.0)), None);
    }
}
