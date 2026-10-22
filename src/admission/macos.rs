//! Read-only macOS compile-admission candidate. NOT authority to launch a job.
//!
//! Darwin memory-pressure level and one-minute runnable load are distinct native
//! signals, not PSI percentages. No CPU or I/O stall metric is fabricated. The
//! worker still requires lease, supervision and recovery acceptance before this
//! candidate can replace its Unsupported backend. No locks/state are opened here.
use super::Thresholds;
use crate::Result;
use serde::Serialize;
use task_manager_platform::{MemorySource, ResourceSample};

pub const POLICY: &str = "macos-compile-candidate-v1";
/// Proposed ceiling, not an installed/default worker setting. Load is a lagging
/// scheduler-demand signal, NOT CPU utilization or a measurement of I/O stalls.
pub const MAX_LOAD_PER_LOGICAL_CPU: f64 = 0.75;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryPressure {
    Normal,
    Warning,
    Critical,
}
impl MemoryPressure {
    fn decode(raw: i32) -> Result<Self> {
        match raw {
            1 => Ok(Self::Normal),
            2 => Ok(Self::Warning),
            4 => Ok(Self::Critical),
            _ => Err(format!("unknown Darwin memory-pressure level {raw}; fails closed").into()),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Sample {
    pub resources: ResourceSample,
    pub memory_pressure: MemoryPressure,
    pub load_average_1m: f64,
    pub logical_cpus: usize,
}
impl Sample {
    /// Native reads only, without creating the supplied cache/state paths. Missing
    /// paths use the platform disk reader's nearest existing ancestor. No caching:
    /// eventual worker integration must resample before and after taking its lease.
    #[cfg(target_os = "macos")]
    pub fn read(cache: &std::path::Path, state: &std::path::Path) -> Result<Self> {
        let memory_pressure = read_memory_pressure()?;
        let mut load = 0.0;
        // SAFETY: a writable double is provided for exactly one requested sample.
        if unsafe { libc::getloadavg(&mut load, 1) } != 1 {
            return Err("Darwin one-minute load unavailable; fails closed".into());
        }
        let sample = Self {
            resources: ResourceSample::read(cache, state)?,
            memory_pressure,
            load_average_1m: load,
            logical_cpus: std::thread::available_parallelism()?.get(),
        };
        sample.validate()?;
        Ok(sample)
    }

    fn validate(&self) -> Result<()> {
        let memory = &self.resources.memory;
        if memory.source != MemorySource::MacosFreeAndPurgeable
            || memory.total_bytes == 0
            || memory.available_bytes > memory.total_bytes
        {
            return Err("invalid or non-Darwin native memory sample; fails closed".into());
        }
        if self.logical_cpus == 0 || !self.load_average_1m.is_finite() || self.load_average_1m < 0.0
        {
            return Err("invalid native load/cpu sample; fails closed".into());
        }
        Ok(())
    }

    /// Evaluate compile resources only. Existing memory/disk reserves are kept;
    /// Linux-only PSI/run-slot knobs are deliberately NOT applied to Darwin load.
    pub fn evaluate(self, thresholds: &Thresholds) -> Result<Report> {
        self.validate()?;
        if !thresholds.valid() {
            return Err("invalid admission thresholds; fails closed".into());
        }
        let blocker = match self.memory_pressure {
            MemoryPressure::Warning => Some("Darwin memory pressure is warning"),
            MemoryPressure::Critical => Some("Darwin memory pressure is critical"),
            MemoryPressure::Normal => self
                .resources
                .floor_blocker(
                    thresholds.min_available_memory_bytes,
                    thresholds.min_cache_free_bytes,
                    thresholds.min_state_free_bytes,
                )
                .or_else(|| {
                    (self.load_average_1m / self.logical_cpus as f64 > MAX_LOAD_PER_LOGICAL_CPU)
                        .then_some("Darwin one-minute load above 0.75 per logical CPU")
                }),
        };
        Ok(Report {
            policy: POLICY,
            sample: self,
            min_memory_bytes: thresholds.min_available_memory_bytes,
            min_cache_free_bytes: thresholds.min_cache_free_bytes,
            min_state_free_bytes: thresholds.min_state_free_bytes,
            max_load_per_logical_cpu: MAX_LOAD_PER_LOGICAL_CPU,
            resource_blocker: blocker,
            runtime_enabled: false,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub policy: &'static str,
    pub sample: Sample,
    pub min_memory_bytes: u64,
    pub min_cache_free_bytes: u64,
    pub min_state_free_bytes: u64,
    pub max_load_per_logical_cpu: f64,
    /// None means ONLY the candidate resource checks pass. It grants no lease,
    /// approval, execution permission, or protection from subsequent load changes.
    pub resource_blocker: Option<&'static str>,
    pub runtime_enabled: bool,
}

#[cfg(target_os = "macos")]
fn read_memory_pressure() -> Result<MemoryPressure> {
    let mut raw: i32 = 0;
    let mut size = std::mem::size_of_val(&raw);
    // SAFETY: this read-only sysctl returns an int; its byte count is checked.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.memorystatus_vm_pressure_level".as_ptr(),
            (&mut raw as *mut i32).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(format!(
            "Darwin memory-pressure read failed: {}; fails closed",
            std::io::Error::last_os_error()
        )
        .into());
    }
    if size != std::mem::size_of_val(&raw) {
        return Err("short Darwin memory-pressure sample; fails closed".into());
    }
    MemoryPressure::decode(raw)
}

#[cfg(test)]
#[path = "macos_tests.rs"]
mod tests;
