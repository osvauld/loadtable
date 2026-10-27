//! Linux power: the mains/battery state from sysfs, and a logind sleep inhibitor.

use crate::power::{Supply, reading_from_supplies};
use crate::{InhibitGuard, PlatformError, PowerReading};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

fn read_supplies(dir: &Path) -> Vec<Supply> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let text = |p: std::path::PathBuf| {
        std::fs::read_to_string(p)
            .ok()
            .map(|t| t.trim().to_string())
    };
    entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let kind = text(p.join("type"))?;
            Some(Supply {
                kind,
                online: text(p.join("online")).map(|v| v == "1"),
                capacity: text(p.join("capacity")).and_then(|v| v.parse().ok()),
            })
        })
        .collect()
}

pub(super) fn read() -> Result<PowerReading, PlatformError> {
    Ok(reading_from_supplies(&read_supplies(Path::new(
        "/sys/class/power_supply",
    ))))
}

/// Holds a logind `idle:sleep` block inhibitor by running `systemd-inhibit` around a shell loop
/// that ends when this process or the inhibitor itself is gone, so a crashed daemon never leaves
/// the machine awake for good.
pub(super) fn inhibit(why: &str) -> Result<InhibitGuard, PlatformError> {
    let fail = |detail: String| PlatformError::Command {
        command: "systemd-inhibit".into(),
        detail,
    };
    let mut child = Command::new("systemd-inhibit")
        .args([
            "--what=idle:sleep",
            "--who=loadtabled",
            "--mode=block",
            &format!("--why={why}"),
            "sh",
            "-c",
            "while kill -0 \"$0\" 2>/dev/null && kill -0 \"$PPID\" 2>/dev/null; do sleep 5; done",
            &std::process::id().to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| fail(format!("not available: {e}")))?;
    // an inhibitor logind refuses (no session bus, no policy) exits at once
    std::thread::sleep(Duration::from_millis(100));
    match child.try_wait() {
        Ok(None) => {}
        Ok(Some(st)) => return Err(fail(format!("exited at once ({st})"))),
        Err(e) => return Err(fail(e.to_string())),
    }
    Ok(InhibitGuard::new(move || {
        let _ = child.kill();
        let _ = child.wait();
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r610_a_missing_directory_reads_as_ac() {
        let r = reading_from_supplies(&read_supplies(Path::new("/nonexistent/power_supply")));
        assert_eq!(r, PowerReading::AC);
    }

    #[test]
    fn r610_supplies_are_read_from_files() {
        let d = std::env::temp_dir().join(format!("tasks-ps-{}", std::process::id()));
        let bat = d.join("BAT0");
        let ac = d.join("AC");
        std::fs::create_dir_all(&bat).unwrap();
        std::fs::create_dir_all(&ac).unwrap();
        std::fs::write(bat.join("type"), "Battery\n").unwrap();
        std::fs::write(bat.join("capacity"), "42\n").unwrap();
        std::fs::write(ac.join("type"), "Mains\n").unwrap();
        std::fs::write(ac.join("online"), "0\n").unwrap();
        let r = reading_from_supplies(&read_supplies(&d));
        std::fs::remove_dir_all(&d).unwrap();
        assert_eq!(
            r,
            PowerReading {
                on_battery: true,
                percent: Some(42)
            }
        );
    }
}
