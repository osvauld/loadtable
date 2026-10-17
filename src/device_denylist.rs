//! Devices that agents must never use, on any host.
//!
//! The built-in list cannot be shortened: configuration only ADDS entries.
//! Extra entries come from `TASKS_DENIED_DEVICES` (comma/space separated) in
//! the environment of the `tasks` process that checks (client, daemon, worker),
//! and from `$XDG_CONFIG_HOME/tasks/denied-devices` (default
//! `~/.config/tasks/denied-devices`; one serial per line, `#` comments).
//! A job's own environment never reaches these checks: the daemon and worker
//! check with their own environment before the job's command exists.
//!
//! Matching ignores ASCII case and treats `:` like `_` (the device lock name
//! form). Argv is checked by substring, so `adb -s SERIAL`, `-sSERIAL`,
//! `ANDROID_SERIAL=SERIAL` and serials inside `sh -c` text are all refused.
//! That cannot stop a command from reaching a device it does not name (plain
//! `adb install` with one device attached picks that device); see
//! deploy/worker/README.md "Test phone on the worker".
use crate::Result;
use std::path::PathBuf;

/// Built-in entries. `ZD22257P6G` is the user's personal phone (moto g52).
pub const BUILTIN: [&str; 1] = ["ZD22257P6G"];
/// Additive environment list, read by the checking `tasks` process.
pub const ENV: &str = "TASKS_DENIED_DEVICES";
/// Additive file, relative to the XDG config directory.
pub const CONFIG_FILE: &str = "tasks/denied-devices";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// As written in its source.
    pub serial: String,
    /// `built-in`, `TASKS_DENIED_DEVICES` or the config file path.
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct Denylist {
    entries: Vec<Entry>,
}

fn normalize(serial: &str) -> String {
    serial.replace(':', "_").to_ascii_uppercase()
}

fn entry(serial: &str, source: &str) -> Result<Entry> {
    if serial.len() < 6
        || serial.len() > 128
        || !serial
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    {
        return Err(format!(
            "{source}: invalid denied device serial {serial:?} (6-128 characters of \
             A-Z a-z 0-9 . _ - :); refusing to guess, fix the device denylist"
        )
        .into());
    }
    Ok(Entry {
        serial: serial.into(),
        source: source.into(),
    })
}

impl Denylist {
    /// Built-in entries only.
    pub fn builtin() -> Self {
        Self {
            entries: BUILTIN
                .iter()
                .map(|s| Entry {
                    serial: (*s).into(),
                    source: "built-in".into(),
                })
                .collect(),
        }
    }

    /// Built-in plus the given additive sources. `file` is (display path, contents).
    pub fn from_sources(env: Option<&str>, file: Option<(&str, &str)>) -> Result<Self> {
        let mut list = Self::builtin();
        if let Some(env) = env {
            for serial in env
                .split(|c: char| c == ',' || c.is_ascii_whitespace())
                .filter(|s| !s.is_empty())
            {
                list.entries.push(entry(serial, ENV)?);
            }
        }
        if let Some((path, text)) = file {
            for line in text.lines() {
                let serial = line.split('#').next().unwrap_or_default().trim();
                if !serial.is_empty() {
                    list.entries.push(entry(serial, path)?);
                }
            }
        }
        Ok(list)
    }

    /// Built-in, this process's `TASKS_DENIED_DEVICES` and the config file.
    /// An unreadable or invalid source fails closed.
    pub fn load() -> Result<Self> {
        let env = match std::env::var(ENV) {
            Ok(v) => Some(v),
            Err(std::env::VarError::NotPresent) => None,
            Err(e) => return Err(format!("{ENV}: {e}").into()),
        };
        let file = match config_path() {
            Some(path) => match std::fs::read_to_string(&path) {
                Ok(text) => Some((path.display().to_string(), text)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    return Err(format!("device denylist {}: {e}", path.display()).into());
                }
            },
            None => None,
        };
        Self::from_sources(
            env.as_deref(),
            file.as_ref().map(|(p, t)| (p.as_str(), t.as_str())),
        )
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The entry denying this exact serial (or its lock-name form), if any.
    pub fn denied(&self, serial: &str) -> Option<&Entry> {
        let serial = normalize(serial);
        self.entries.iter().find(|e| normalize(&e.serial) == serial)
    }

    /// The first entry that any argv element mentions, if any.
    pub fn mentioned(&self, argv: &[String]) -> Option<&Entry> {
        let argv: Vec<String> = argv.iter().map(|a| normalize(a)).collect();
        self.entries.iter().find(|e| {
            let serial = normalize(&e.serial);
            argv.iter().any(|a| a.contains(&serial))
        })
    }
}

/// `$XDG_CONFIG_HOME/tasks/denied-devices`, else `~/.config/tasks/denied-devices`.
pub fn config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join(CONFIG_FILE))
}

fn refusal(what: String, e: &Entry) -> Box<dyn std::error::Error + Send + Sync> {
    format!(
        "{what} denylisted device {} ({}): agents must never use it on any host. Use a \
         dedicated test device (see deploy/worker/README.md, \"Test phone on the worker\")",
        e.serial, e.source
    )
    .into()
}

/// Refuses a device serial (real or lock-name form) on the loaded denylist.
pub fn refuse_device(serial: &str) -> Result<()> {
    match Denylist::load()?.denied(serial) {
        Some(e) => Err(refusal("refusing".into(), e)),
        None => Ok(()),
    }
}

/// Refuses a command whose argv mentions a denylisted serial.
pub fn refuse_argv(argv: &[String]) -> Result<()> {
    match Denylist::load()?.mentioned(argv) {
        Some(e) => Err(refusal("refusing a command that names".into(), e)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn builtin_personal_phone_is_always_denied_in_any_case_or_lock_form() {
        let list = Denylist::builtin();
        for s in ["ZD22257P6G", "zd22257p6g", "Zd22257P6g"] {
            assert_eq!(list.denied(s).unwrap().source, "built-in", "{s}");
        }
        assert!(list.denied("9BWAY1LSDC").is_none());
        assert!(list.denied("ZD22257P6").is_none());
        // An empty or unrelated additive configuration never removes it.
        // There is no removal syntax: "-SERIAL" or "!SERIAL" is not an exception.
        for env in [None, Some(""), Some(" , "), Some("-ZD22257P6G")] {
            let list = Denylist::from_sources(env, Some(("cfg", "# nothing\n\n"))).unwrap();
            assert!(list.denied("ZD22257P6G").is_some(), "{env:?}");
        }
        assert!(Denylist::from_sources(Some("!ZD22257P6G"), None).is_err());
    }

    #[test]
    fn env_and_file_add_entries_with_comments_and_lock_forms() {
        let list = Denylist::from_sources(
            Some("SYNTH00001, synth00002\tSYNTH00003"),
            Some((
                "/cfg/denied-devices",
                "# synthetic test devices\nFILE000001  # bench phone\n\n  192.168.9.9:5555\n",
            )),
        )
        .unwrap();
        assert_eq!(list.denied("synth00002").unwrap().source, ENV);
        assert_eq!(
            list.denied("FILE000001").unwrap().source,
            "/cfg/denied-devices"
        );
        assert!(list.denied("SYNTH00003").is_some());
        // Wireless serials match their lock-name form too.
        assert!(list.denied("192.168.9.9_5555").is_some());
        assert!(list.denied("192.168.9.9:5555").is_some());
        assert!(list.denied("ZD22257P6G").is_some());
        assert_eq!(list.entries().len(), 6);
    }

    #[test]
    fn invalid_entries_fail_closed() {
        for bad in [
            "short",
            "has space?",
            "semi;colon",
            "slash/x",
            &"A".repeat(129),
        ] {
            assert!(Denylist::from_sources(Some(bad), None).is_err(), "{bad}");
            assert!(
                Denylist::from_sources(None, Some(("cfg", bad))).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn argv_mentions_are_found_anywhere_in_any_case() {
        let list = Denylist::builtin();
        for cmd in [
            argv(&["adb", "-s", "ZD22257P6G", "install", "-r", "a.apk"]),
            argv(&["adb", "-sZD22257P6G", "logcat"]),
            argv(&["env", "ANDROID_SERIAL=zd22257p6g", "adb", "shell"]),
            argv(&["sh", "-c", "adb -s ZD22257P6G logcat -d | tail"]),
            argv(&["tasks-offload", "--device", "ZD22257P6G", "--", "true"]),
        ] {
            assert!(list.mentioned(&cmd).is_some(), "{cmd:?}");
        }
        for cmd in [
            argv(&["adb", "-s", "9BWAY1LSDC", "logcat", "-d"]),
            argv(&["cargo", "test", "-j", "2"]),
        ] {
            assert!(list.mentioned(&cmd).is_none(), "{cmd:?}");
        }
    }
}
