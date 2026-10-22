//! Shared Linux/macOS host primitives. Monitoring is not admission policy.
//!
//! `sysinfo` supplies observational readings; its seconds-resolution process start time
//! must NOT be used as cancellation identity. Native identity preserves Linux start ticks
//! and uses macOS's microsecond start timestamp. Neither makes signalling race-free by
//! itself: supervisors must still retain the unreaped group leader through cleanup.
pub mod fs;
pub mod process;
pub mod thread;

use std::time::Instant;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

#[derive(Clone, Debug)]
pub struct MachineReading {
    pub hostname: Option<String>,
    pub logical_cpus: usize,
    pub physical_cores: Option<usize>,
    /// Null until a sufficiently separated second CPU sample exists. 1.0 = one busy core.
    pub cpu_busy_cores: Option<f64>,
    pub memory_total_bytes: u64,
    /// sysinfo's OS-specific available-memory estimate; not Linux MemAvailable equivalence.
    pub memory_available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
}

/// One retained collector per feed, refreshed by the caller rather than by a hidden thread.
/// Does not enumerate processes, read command lines/environments, or spawn subprocesses.
pub struct MachineSampler {
    system: System,
    cpu_sample_at: Option<Instant>,
    cpu_busy: Option<f64>,
    hostname: Option<String>,
    physical_cores: Option<usize>,
}

impl Default for MachineSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl MachineSampler {
    pub fn new() -> Self {
        Self {
            system: System::new_with_specifics(
                RefreshKind::nothing().with_cpu(CpuRefreshKind::nothing()),
            ),
            cpu_sample_at: None,
            cpu_busy: None,
            hostname: System::host_name(),
            physical_cores: System::physical_core_count(),
        }
    }

    pub fn sample(&mut self) -> MachineReading {
        self.system
            .refresh_memory_specifics(MemoryRefreshKind::everything());
        let now = Instant::now();
        if self
            .cpu_sample_at
            .is_none_or(|last| now.duration_since(last) >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL)
        {
            self.system.refresh_cpu_usage();
            if self.cpu_sample_at.is_some() {
                self.cpu_busy = Some(
                    self.system
                        .cpus()
                        .iter()
                        .map(|cpu| f64::from(cpu.cpu_usage()) / 100.0)
                        .sum(),
                );
            }
            self.cpu_sample_at = Some(now);
        }
        MachineReading {
            hostname: self.hostname.clone(),
            logical_cpus: self.system.cpus().len(),
            physical_cores: self.physical_cores,
            cpu_busy_cores: self.cpu_busy,
            memory_total_bytes: self.system.total_memory(),
            memory_available_bytes: self.system.available_memory(),
            swap_total_bytes: self.system.total_swap(),
            swap_used_bytes: self.system.used_swap(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_machine_readings_are_bounded_and_cpu_requires_two_samples() {
        let mut sampler = MachineSampler::new();
        let first = sampler.sample();
        assert!(first.logical_cpus > 0);
        assert!(first.memory_total_bytes > 0);
        assert!(first.memory_available_bytes <= first.memory_total_bytes);
        assert!(first.swap_used_bytes <= first.swap_total_bytes);
        assert_eq!(first.cpu_busy_cores, None);
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let second = sampler.sample();
        let busy = second.cpu_busy_cores.unwrap();
        assert!(busy.is_finite());
        assert!((0.0..=second.logical_cpus as f64).contains(&busy));
    }
}
