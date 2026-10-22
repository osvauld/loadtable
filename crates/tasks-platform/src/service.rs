//! Service unit rendering and the never-overwrite file writer, shared by both
//! operating systems. The renderers are pure so both can be tested anywhere.

use crate::{InstallPlan, PlatformError, ServiceUnit};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Rejects names that are empty or could escape the service directory.
pub fn validate_name(name: &str) -> Result<(), PlatformError> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(PlatformError::InvalidServiceName(name.to_string()))
    }
}

fn systemd_quote(arg: &str) -> String {
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Renders a systemd user unit for `unit`.
pub fn render_systemd_unit(unit: &ServiceUnit) -> String {
    let exec: Vec<String> = unit.argv.iter().map(|a| systemd_quote(a)).collect();
    format!(
        "[Unit]\nDescription=tasks workload coordinator ({name})\n\n\
         [Service]\nType=simple\nExecStart={exec}\nRestart=on-failure\nRestartSec=2\n\n\
         [Install]\nWantedBy=default.target\n",
        name = unit.name,
        exec = exec.join(" "),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Renders a launchd agent plist for `unit` (label = service name).
pub fn render_launchd_plist(unit: &ServiceUnit) -> String {
    let args: String = unit
        .argv
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{label}</string>\n\
         \x20 <key>ProgramArguments</key>\n  <array>\n{args}  </array>\n\
         \x20 <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n</dict>\n</plist>\n",
        label = xml_escape(&unit.name),
    )
}

/// Path of the unit file for `name` under `home`.
pub fn unit_path(home: &Path, name: &str, macos: bool) -> PathBuf {
    if macos {
        home.join("Library/LaunchAgents")
            .join(format!("{name}.plist"))
    } else {
        home.join(".config/systemd/user")
            .join(format!("{name}.service"))
    }
}

/// Writes `contents` to `path` unless a different file exists (`Exists`).
/// An identical existing file is left alone and reported as not written.
pub fn write_unit(
    path: &Path,
    contents: &str,
    next_command: String,
) -> Result<InstallPlan, PlatformError> {
    match std::fs::read_to_string(path) {
        Ok(existing) if existing == contents => {
            return Ok(InstallPlan {
                path: path.to_path_buf(),
                contents_written: false,
                next_command,
            });
        }
        Ok(_) => return Err(PlatformError::Exists(path.to_path_buf())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(PlatformError::io(path, e)),
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| PlatformError::io(dir, e))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                PlatformError::Exists(path.to_path_buf())
            } else {
                PlatformError::io(path, e)
            }
        })?;
    file.write_all(contents.as_bytes())
        .map_err(|e| PlatformError::io(path, e))?;
    Ok(InstallPlan {
        path: path.to_path_buf(),
        contents_written: true,
        next_command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit() -> ServiceUnit {
        ServiceUnit {
            name: "tasks-daemon".into(),
            argv: vec!["/usr/bin/tasks".into(), "daemon".into(), "a b\"%".into()],
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tasks-svc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn systemd_unit_quotes_arguments() {
        let text = render_systemd_unit(&unit());
        assert!(text.contains("ExecStart=\"/usr/bin/tasks\" \"daemon\" \"a b\\\"%%\"\n"));
        assert!(text.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_plist_escapes_xml() {
        let mut u = unit();
        u.argv.push("<&>".into());
        let text = render_launchd_plist(&u);
        assert!(text.contains("<key>Label</key>\n  <string>tasks-daemon</string>"));
        assert!(text.contains("<string>&lt;&amp;&gt;</string>"));
        assert!(text.contains("<key>RunAtLoad</key>"));
    }

    #[test]
    fn paths_follow_each_os_convention() {
        let h = Path::new("/h");
        assert_eq!(
            unit_path(h, "x", false),
            Path::new("/h/.config/systemd/user/x.service")
        );
        assert_eq!(
            unit_path(h, "x", true),
            Path::new("/h/Library/LaunchAgents/x.plist")
        );
    }

    #[test]
    fn write_never_overwrites_a_different_file() {
        let home = tmp("write");
        let path = unit_path(&home, "x", false);
        let first = write_unit(&path, "A", "run".into()).unwrap();
        assert!(first.contents_written);
        let same = write_unit(&path, "A", "run".into()).unwrap();
        assert!(!same.contents_written);
        assert!(matches!(
            write_unit(&path, "B", "run".into()),
            Err(PlatformError::Exists(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn bad_service_names_are_refused() {
        for bad in ["", "../x", "a/b", ".hidden", "a b"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        assert!(validate_name("tasks-daemon_1.x").is_ok());
    }
}
