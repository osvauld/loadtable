//! Live metrics of a running job: the samples the UI draws as graphs, the process groups of a
//! job, and the pure arithmetic that turns two samples into rates. Plain data: nothing here is
//! platform-specific, so it travels over the peer protocol unchanged.

use crate::Attribution;
use serde::{Deserialize, Serialize};

/// One reading of a running job. Counters are cumulative; `mem_bytes` is the level at that
/// moment. `None` is "not measured here".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricSample {
    /// When, ms since the epoch.
    pub t_ms: i64,
    /// CPU time so far, microseconds.
    #[serde(default)]
    pub cpu_usec: Option<u64>,
    /// Memory in use now (not the peak).
    #[serde(default)]
    pub mem_bytes: Option<u64>,
    /// Bytes read from storage so far.
    #[serde(default)]
    pub io_read_bytes: Option<u64>,
    /// Bytes written to storage so far.
    #[serde(default)]
    pub io_write_bytes: Option<u64>,
    /// Time some task stalled on I/O so far, microseconds (cgroup only).
    #[serde(default)]
    pub io_some_us: Option<u64>,
}

/// The processes of one program in a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcGroup {
    /// Program base name; never arguments or environment. `others` rows carry a count label.
    pub name: String,
    /// How many processes.
    pub count: u32,
    /// CPU in cores over the last sample interval; `None` before two readings exist.
    #[serde(default)]
    pub cpu_cores: Option<f64>,
    /// Resident memory of the group, bytes.
    pub rss_bytes: u64,
    /// True for the row that sums the groups beyond the cap.
    #[serde(default)]
    pub others: bool,
}

/// Rates between two consecutive samples. A counter that went backwards (a reset) or an empty
/// interval gives `None` for that figure.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rates {
    /// CPU in cores (CPU seconds per wall second).
    pub cores: Option<f64>,
    /// Bytes per second read.
    pub read_bps: Option<f64>,
    /// Bytes per second written.
    pub write_bps: Option<f64>,
    /// Share of the interval some task stalled on I/O, 0 to 100.
    pub io_stall_pct: Option<f64>,
}

/// The per-second change of a cumulative counter over `dt_ms`, in units of `scale` per second.
fn delta_per_s(prev: Option<u64>, cur: Option<u64>, dt_ms: i64, scale: f64) -> Option<f64> {
    let (p, c) = (prev?, cur?);
    if dt_ms <= 0 || c < p {
        return None;
    }
    Some((c - p) as f64 * 1000.0 / dt_ms as f64 / scale)
}

/// Rates from `prev` to `cur`.
pub fn rates(prev: &MetricSample, cur: &MetricSample) -> Rates {
    let dt = cur.t_ms.saturating_sub(prev.t_ms);
    Rates {
        cores: delta_per_s(prev.cpu_usec, cur.cpu_usec, dt, 1e6),
        read_bps: delta_per_s(prev.io_read_bytes, cur.io_read_bytes, dt, 1.0),
        write_bps: delta_per_s(prev.io_write_bytes, cur.io_write_bytes, dt, 1.0),
        // microseconds stalled per second of wall time is parts per million
        io_stall_pct: delta_per_s(prev.io_some_us, cur.io_some_us, dt, 1e4)
            .map(|p| p.clamp(0.0, 100.0)),
    }
}

/// The median of the known peaks (newest first, `None` for a run that measured none).
pub fn median_peak(peaks: &[Option<u64>]) -> Option<u64> {
    let mut known: Vec<u64> = peaks.iter().copied().flatten().collect();
    if known.is_empty() {
        return None;
    }
    known.sort_unstable();
    let n = known.len();
    let mid = n / 2;
    if n % 2 == 1 {
        known.get(mid).copied()
    } else {
        let (a, b) = (known.get(mid - 1).copied()?, known.get(mid).copied()?);
        Some(a / 2 + b / 2 + (a % 2 + b % 2) / 2)
    }
}

/// What the daemon says about a job's metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobMetricsView {
    /// How exact the figures are; `None` when nothing was sampled.
    pub quality: Option<Attribution>,
    /// Oldest first, at most [`MAX_SAMPLES`].
    pub samples: Vec<MetricSample>,
    /// The live process groups, busiest first, ending with an `others` row when cut.
    pub procs: Vec<ProcGroup>,
    /// Median peak memory of this command's past runs on this machine, when history has it.
    #[serde(default)]
    pub usual_peak_mem_bytes: Option<u64>,
    /// Why there is little or nothing to show (finished job, history dropped).
    #[serde(default)]
    pub note: Option<String>,
    /// Where the figures were measured when not on this machine ("on mac, remote job #9").
    #[serde(default)]
    pub source: Option<String>,
}

/// Samples kept per job: 30 minutes at one every two seconds.
pub const MAX_SAMPLES: usize = 900;

#[cfg(test)]
mod tests {
    use super::*;

    fn s(t_ms: i64, cpu: u64, rd: u64, wr: u64, stall: u64) -> MetricSample {
        MetricSample {
            t_ms,
            cpu_usec: Some(cpu),
            mem_bytes: Some(1),
            io_read_bytes: Some(rd),
            io_write_bytes: Some(wr),
            io_some_us: Some(stall),
        }
    }

    #[test]
    fn rates_come_from_the_counter_deltas() {
        let r = rates(
            &s(1_000, 0, 0, 0, 0),
            &s(3_000, 5_000_000, 20_000_000, 4_000_000, 500_000),
        );
        assert_eq!(r.cores, Some(2.5));
        assert_eq!(r.read_bps, Some(10_000_000.0));
        assert_eq!(r.write_bps, Some(2_000_000.0));
        assert_eq!(r.io_stall_pct, Some(25.0));
    }

    #[test]
    fn a_counter_reset_or_an_empty_interval_gives_none() {
        let r = rates(&s(0, 100, 100, 100, 100), &s(2_000, 50, 200, 200, 50));
        assert_eq!((r.cores, r.io_stall_pct), (None, None));
        assert!(r.read_bps.is_some());
        assert_eq!(
            rates(&s(5, 1, 1, 1, 1), &s(5, 9, 9, 9, 9)),
            Rates::default()
        );
        assert_eq!(
            rates(&s(9, 1, 1, 1, 1), &s(5, 9, 9, 9, 9)),
            Rates::default()
        );
    }

    #[test]
    fn stall_is_clamped_and_unmeasured_counters_stay_none() {
        let r = rates(&s(0, 0, 0, 0, 0), &s(1_000, 0, 0, 0, 5_000_000));
        assert_eq!(r.io_stall_pct, Some(100.0));
        let mut a = s(0, 0, 0, 0, 0);
        a.cpu_usec = None;
        assert_eq!(rates(&a, &s(1_000, 9, 0, 0, 0)).cores, None);
    }

    #[test]
    fn the_median_peak_skips_unmeasured_runs() {
        assert_eq!(median_peak(&[]), None);
        assert_eq!(median_peak(&[None, None]), None);
        assert_eq!(median_peak(&[Some(9), None, Some(1), Some(5)]), Some(5));
        assert_eq!(median_peak(&[Some(2), Some(4)]), Some(3));
        assert_eq!(
            median_peak(&[Some(u64::MAX), Some(u64::MAX)]),
            Some(u64::MAX)
        );
    }
}
