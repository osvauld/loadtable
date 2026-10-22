//! Native pressure acquisition only; thresholds, queues and flock leases stay shared.
use super::Pressure;
use crate::Result;
#[cfg(any(target_os = "linux", test))]
use std::path::Path;

pub(super) fn pressure() -> Result<Pressure> {
    #[cfg(target_os = "linux")]
    return Pressure::read_from(Path::new("/proc"));
    #[cfg(not(target_os = "linux"))]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "native macOS admission policy is not implemented; Linux PSI is unavailable; admission fails closed",
    ).into())
}

/// Also compiled for fixture tests on macOS, never used there as a live pressure source.
#[cfg(any(target_os = "linux", test))]
pub(super) fn read_from(proc: &Path) -> Result<Pressure> {
    let read = |rel: &str| -> Result<String> {
        let path = proc.join(rel);
        std::fs::read_to_string(&path).map_err(|e| {
            if rel.starts_with("pressure/") {
                format!(
                    "{}: {e}. Admission requires kernel pressure stall information (PSI): \
                     a kernel with CONFIG_PSI=y, booted with psi=1 if it was built with \
                     CONFIG_PSI_DEFAULT_DISABLED=y. Admission fails closed without it",
                    path.display()
                )
            } else {
                format!("{}: {e}", path.display())
            }
            .into()
        })
    };
    Pressure::parse(
        &read("meminfo")?,
        &read("pressure/cpu")?,
        &read("pressure/memory")?,
        &read("pressure/io")?,
    )
}
