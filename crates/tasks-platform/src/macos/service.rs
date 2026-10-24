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

/// The launchd target of the user agent `name` for user `uid`.
pub(super) fn target(uid: u32, name: &str) -> String {
    format!("gui/{uid}/{name}")
}

/// Starts the agent if launchd has it loaded. A plist that is not loaded is reported, not
/// bootstrapped: loading it is the operator's decision.
pub(super) fn start(name: &str) -> Result<(), PlatformError> {
    validate_name(name)?;
    let uid = rustix::process::getuid().as_raw();
    let path = unit_path(&home()?, name, true);
    run("/bin/launchctl", &["kickstart", &target(uid, name)])
        .map(|_| ())
        .map_err(|e| match e {
            PlatformError::Command { command, detail } => PlatformError::Command {
                command,
                detail: format!(
                    "{detail}; if the agent is not loaded, run `launchctl bootstrap gui/{uid} {}`",
                    path.display()
                ),
            },
            other => other,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r683_the_kickstart_target_is_the_gui_domain_label() {
        assert_eq!(target(501, "tasks2d"), "gui/501/tasks2d");
    }
}
