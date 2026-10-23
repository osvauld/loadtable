//! launchd agent installation and status.

use super::run;
use crate::service::{render_launchd_plist, unit_path, validate_name, write_unit};
use crate::{InstallPlan, PlatformError, ServiceStatus, ServiceUnit};
use std::path::{Path, PathBuf};

fn home() -> Result<PathBuf, PlatformError> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or(PlatformError::NoHome)
}

/// Writes the plist under `home`; loads nothing.
pub(super) fn install_in(home: &Path, unit: &ServiceUnit) -> Result<InstallPlan, PlatformError> {
    validate_name(&unit.name)?;
    let path = unit_path(home, &unit.name, true);
    let next = format!(
        "launchctl bootstrap gui/{} {}",
        rustix::process::getuid().as_raw(),
        path.display()
    );
    write_unit(&path, &render_launchd_plist(unit), next)
}

pub(super) fn install(unit: &ServiceUnit) -> Result<InstallPlan, PlatformError> {
    install_in(&home()?, unit)
}

pub(super) fn status(name: &str) -> Result<ServiceStatus, PlatformError> {
    validate_name(name)?;
    if !unit_path(&home()?, name, true).exists() {
        return Ok(ServiceStatus::NotInstalled);
    }
    let target = format!("gui/{}/{name}", rustix::process::getuid().as_raw());
    match run("/bin/launchctl", &["print", &target]) {
        Ok(text) if text.lines().any(|l| l.trim() == "state = running") => {
            Ok(ServiceStatus::Running)
        }
        // Printed but not running, or not bootstrapped: installed and stopped.
        Ok(_) | Err(PlatformError::Command { .. }) => Ok(ServiceStatus::Stopped),
        Err(e) => Err(e),
    }
}
