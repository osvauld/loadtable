//! systemd user service installation and status.

use crate::service::{render_systemd_unit, unit_path, validate_name, write_unit};
use crate::{InstallPlan, PlatformError, ServiceStatus, ServiceUnit};
use std::path::{Path, PathBuf};
use std::process::Command;

fn home() -> Result<PathBuf, PlatformError> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or(PlatformError::NoHome)
}

/// Writes the unit under `home`; starts nothing.
pub(super) fn install_in(home: &Path, unit: &ServiceUnit) -> Result<InstallPlan, PlatformError> {
    validate_name(&unit.name)?;
    let path = unit_path(home, &unit.name, false);
    let next = format!(
        "systemctl --user daemon-reload && systemctl --user enable --now {}.service",
        unit.name
    );
    write_unit(&path, &render_systemd_unit(unit), next)
}

pub(super) fn install(unit: &ServiceUnit) -> Result<InstallPlan, PlatformError> {
    install_in(&home()?, unit)
}

/// Maps `systemctl --user is-active` output to a status for an installed unit.
pub fn parse_is_active(stdout: &str) -> ServiceStatus {
    match stdout.trim() {
        "active" | "activating" | "reloading" => ServiceStatus::Running,
        _ => ServiceStatus::Stopped,
    }
}

pub(super) fn status(name: &str) -> Result<ServiceStatus, PlatformError> {
    validate_name(name)?;
    if !unit_path(&home()?, name, false).exists() {
        return Ok(ServiceStatus::NotInstalled);
    }
    let out = Command::new("systemctl")
        .args(["--user", "is-active", name])
        .output()
        .map_err(|e| PlatformError::Command {
            command: format!("systemctl --user is-active {name}"),
            detail: e.to_string(),
        })?;
    Ok(parse_is_active(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_active_output_maps_to_status() {
        assert_eq!(parse_is_active("active\n"), ServiceStatus::Running);
        assert_eq!(parse_is_active("inactive\n"), ServiceStatus::Stopped);
        assert_eq!(parse_is_active("failed\n"), ServiceStatus::Stopped);
        assert_eq!(parse_is_active(""), ServiceStatus::Stopped);
    }

    #[test]
    fn install_writes_unit_and_returns_command_without_running_it() {
        let home = std::env::temp_dir().join(format!("tasks-lsvc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let unit = ServiceUnit {
            name: "tasks-test".into(),
            argv: vec!["/bin/true".into()],
        };
        let plan = install_in(&home, &unit).unwrap();
        assert!(plan.contents_written);
        assert_eq!(
            plan.path,
            home.join(".config/systemd/user/tasks-test.service")
        );
        assert!(
            plan.next_command
                .contains("enable --now tasks-test.service")
        );
        let other = ServiceUnit {
            name: "tasks-test".into(),
            argv: vec!["/bin/false".into()],
        };
        assert!(matches!(
            install_in(&home, &other),
            Err(PlatformError::Exists(_))
        ));
        let _ = std::fs::remove_dir_all(&home);
    }
}
