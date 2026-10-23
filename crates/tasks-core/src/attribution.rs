//! Per-job resource figures and how exact they are (design §9).
//!
//! The figures answer "what did this job cost the machine": disk bytes, peak memory, CPU time
//! and how long its tasks stalled on I/O, memory and CPU. On Linux with a delegated cgroup they
//! are read from the job's own cgroup and are exact; everywhere else they are summed over the
//! job's process tree by the sampler and are an approximation. The quality is stored with the
//! figures so nobody reads a sampled number as an exact one.

use serde::{Deserialize, Serialize};

/// How the figures were obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribution {
    /// Read from the job's own cgroup (Linux, delegated): exact, includes escaped children.
    Cgroup,
    /// Summed over the job's process tree by sampling: approximate; bytes written by a process
    /// that exited between two samples are lost.
    ProcessTree,
}

impl Attribution {
    /// A short phrase for listings.
    pub fn label(self) -> &'static str {
        match self {
            Attribution::Cgroup => "exact cgroup",
            Attribution::ProcessTree => "process tree",
        }
    }
}

/// What one job cost, with the quality of the numbers. A `None` is "not measured here" (a
/// missing cgroup controller, or a value the OS does not give per process).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFigures {
    /// How exact the numbers are.
    pub quality: Attribution,
    /// Bytes read from storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_read_bytes: Option<u64>,
    /// Bytes written to storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_write_bytes: Option<u64>,
    /// Read operations (cgroup only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_read_ops: Option<u64>,
    /// Write operations (cgroup only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_write_ops: Option<u64>,
    /// Peak memory in bytes: `memory.peak` for a cgroup, the highest summed RSS seen otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_peak_bytes: Option<u64>,
    /// CPU time (user + system) in microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_usec: Option<u64>,
    /// Time some task of the job stalled on I/O, microseconds (cgroup PSI total).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_some_us: Option<u64>,
    /// Time every task of the job stalled on I/O at once, microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_full_us: Option<u64>,
    /// Time some task stalled on memory, microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_some_us: Option<u64>,
    /// Time every task stalled on memory at once, microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_full_us: Option<u64>,
    /// Time some task waited for a CPU, microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_some_us: Option<u64>,
}

impl JobFigures {
    /// Figures with nothing measured yet.
    pub fn empty(quality: Attribution) -> JobFigures {
        JobFigures {
            quality,
            io_read_bytes: None,
            io_write_bytes: None,
            io_read_ops: None,
            io_write_ops: None,
            mem_peak_bytes: None,
            cpu_usec: None,
            io_some_us: None,
            io_full_us: None,
            mem_some_us: None,
            mem_full_us: None,
            cpu_some_us: None,
        }
    }

    /// Total storage bytes (read + written) when either is known.
    pub fn io_bytes(&self) -> Option<u64> {
        match (self.io_read_bytes, self.io_write_bytes) {
            (None, None) => None,
            (r, w) => Some(r.unwrap_or(0).saturating_add(w.unwrap_or(0))),
        }
    }

    /// One line for `get` and the UI: `read 1.2 GiB, wrote 300 MiB, peak 4.1 GiB, cpu 93 s
    /// (exact cgroup)`; parts that were not measured are left out.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(b) = self.io_read_bytes {
            parts.push(format!("read {}", human_bytes(b)));
        }
        if let Some(b) = self.io_write_bytes {
            parts.push(format!("wrote {}", human_bytes(b)));
        }
        if let Some(b) = self.mem_peak_bytes {
            parts.push(format!("peak {}", human_bytes(b)));
        }
        if let Some(us) = self.cpu_usec {
            parts.push(format!("cpu {} s", us / 1_000_000));
        }
        if let Some(us) = self.io_some_us.filter(|u| *u >= 1000) {
            parts.push(format!("io stall {} ms", us / 1000));
        }
        if parts.is_empty() {
            parts.push("nothing measured".to_string());
        }
        format!("{} ({})", parts.join(", "), self.quality.label())
    }
}

/// Bytes with a binary unit and at most one decimal: `512 B`, `1.5 MiB`, `4.1 GiB`.
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    while unit < UNITS.len() - 1 && u128::from(b) >= 1u128 << (10 * (unit + 1)) {
        unit += 1;
    }
    let tenths = (u128::from(b) * 10) >> (10 * unit);
    let name = UNITS.get(unit).copied().unwrap_or("TiB");
    if unit == 0 {
        format!("{b} B")
    } else if tenths.is_multiple_of(10) {
        format!("{} {name}", tenths / 10)
    } else {
        format!("{}.{} {name}", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_read_in_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 << 20), "5 MiB");
        assert_eq!(human_bytes((4 << 30) + (1 << 30) / 8), "4.1 GiB");
    }

    #[test]
    fn summary_names_quality_and_skips_unmeasured() {
        let mut f = JobFigures::empty(Attribution::ProcessTree);
        assert_eq!(f.summary(), "nothing measured (process tree)");
        f.io_write_bytes = Some(300 << 20);
        f.cpu_usec = Some(93_000_000);
        f.quality = Attribution::Cgroup;
        assert_eq!(f.summary(), "wrote 300 MiB, cpu 93 s (exact cgroup)");
        assert_eq!(f.io_bytes(), Some(300 << 20));
    }

    #[test]
    fn old_json_without_new_fields_still_parses() {
        let f: JobFigures = serde_json::from_str(r#"{"quality":"cgroup"}"#).unwrap();
        assert_eq!(f, JobFigures::empty(Attribution::Cgroup));
    }
}
