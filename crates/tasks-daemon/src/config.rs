//! Where the daemon keeps things, resolved once at start, and the one-daemon-per-state-dir
//! lock.
//!
//! Every directory comes from the daemon's own arguments and environment, never from a client:
//! a client's or sandbox's `TMPDIR` cannot move the slot locks (R-231).

use crate::error::DaemonError;
use rustix::fs::{FlockOperation, flock};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Directories and names, fixed for the daemon's lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// Holds the ledger, socket, pid file, logs and the execution intent.
    pub state_dir: PathBuf,
    /// Holds `machine-policy.toml`.
    pub config_dir: PathBuf,
    /// Holds the slot lock files; the same for every client (R-231).
    pub lock_dir: PathBuf,
    /// Where cache output lands; its filesystem is checked against the disk reserve.
    pub cache_dir: PathBuf,
    /// This machine's name in placement.
    pub machine: String,
}

/// Command-line overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigArgs {
    /// `--state-dir`.
    pub state_dir: Option<PathBuf>,
    /// `--config-dir`.
    pub config_dir: Option<PathBuf>,
    /// `--lock-dir`.
    pub lock_dir: Option<PathBuf>,
    /// `--cache-dir`.
    pub cache_dir: Option<PathBuf>,
    /// `--machine`.
    pub machine: Option<String>,
}

impl DaemonConfig {
    /// Resolves the directories. `env` is the daemon's own environment, read once here; the
    /// lock directory is `--lock-dir`, then `TASKS_LOCK_DIR`, then `<state_dir>/slots`. It is
    /// never derived from `TMPDIR`.
    pub fn resolve(
        args: &ConfigArgs,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<DaemonConfig, DaemonError> {
        let home = env("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
        let xdg = |var: &str, under_home: &str| -> Option<PathBuf> {
            env(var)
                .filter(|v| v.starts_with('/'))
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|h| h.join(under_home)))
        };
        let state_dir = args
            .state_dir
            .clone()
            .or_else(|| env("TASKS_STATE_DIR").map(PathBuf::from))
            .or_else(|| xdg("XDG_STATE_HOME", ".local/state").map(|p| p.join("tasks2")))
            .ok_or(DaemonError::NoHome("state directory"))?;
        let config_dir = args
            .config_dir
            .clone()
            .or_else(|| env("TASKS_CONFIG_DIR").map(PathBuf::from))
            .or_else(|| xdg("XDG_CONFIG_HOME", ".config").map(|p| p.join("tasks2")))
            .ok_or(DaemonError::NoHome("config directory"))?;
        let cache_dir = args
            .cache_dir
            .clone()
            .or_else(|| env("TASKS_CACHE_DIR").map(PathBuf::from))
            .or_else(|| xdg("XDG_CACHE_HOME", ".cache").map(|p| p.join("tasks2")))
            .unwrap_or_else(|| state_dir.clone());
        let lock_dir = args
            .lock_dir
            .clone()
            .or_else(|| env("TASKS_LOCK_DIR").map(PathBuf::from))
            .unwrap_or_else(|| state_dir.join("slots"));
        for (what, p) in [
            ("state directory", &state_dir),
            ("config directory", &config_dir),
            ("lock directory", &lock_dir),
            ("cache directory", &cache_dir),
        ] {
            if !p.is_absolute() {
                return Err(DaemonError::RelativePath(what, p.clone()));
            }
        }
        let machine = args
            .machine
            .clone()
            .or_else(|| env("TASKS_MACHINE"))
            .or_else(|| env("HOSTNAME").filter(|h| !h.is_empty()))
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "local".to_string());
        Ok(DaemonConfig {
            state_dir,
            config_dir,
            lock_dir,
            cache_dir,
            machine,
        })
    }

    /// The ledger database.
    pub fn db_path(&self) -> PathBuf {
        self.state_dir.join("ledger.sqlite")
    }
    /// The local socket.
    pub fn socket_path(&self) -> PathBuf {
        self.state_dir.join("tasks.sock")
    }
    /// The pid file whose flock enforces one daemon per state dir.
    pub fn pid_path(&self) -> PathBuf {
        self.state_dir.join("daemon.pid")
    }
    /// Job logs.
    pub fn log_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }
    /// The machine policy file.
    pub fn policy_path(&self) -> PathBuf {
        self.config_dir.join("machine-policy.toml")
    }
    /// The persisted execution intent.
    pub fn intent_path(&self) -> PathBuf {
        self.state_dir.join("execution.intent")
    }
    /// A job's log file.
    pub fn log_path(&self, id: tasks_core::JobId) -> PathBuf {
        self.log_dir().join(format!("{}.log", id.0))
    }

    /// Creates the directories the daemon writes to (state dir private to the user).
    pub fn create_dirs(&self) -> Result<(), DaemonError> {
        for d in [
            &self.state_dir,
            &self.log_dir(),
            &self.lock_dir,
            &self.cache_dir,
        ] {
            std::fs::create_dir_all(d).map_err(|e| DaemonError::io(d, e))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.state_dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }
}

/// Holds the exclusive flock on the pid file for the daemon's lifetime. The file is never
/// deleted, so its inode is stable for anyone watching the lock.
#[derive(Debug)]
pub struct PidLock {
    _file: File,
}

impl PidLock {
    /// Takes the lock and records our pid. A second daemon on the same state dir is refused
    /// with the holder's pid.
    pub fn acquire(path: &Path) -> Result<PidLock, DaemonError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| DaemonError::io(dir, e))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| DaemonError::io(path, e))?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => {
                let mut text = String::new();
                let _ = file.read_to_string(&mut text);
                let pid = text.trim().parse::<u32>().ok();
                return Err(DaemonError::AlreadyRunning {
                    pid,
                    path: path.to_path_buf(),
                });
            }
            Err(e) => return Err(DaemonError::io(path, std::io::Error::from(e))),
        }
        file.set_len(0).map_err(|e| DaemonError::io(path, e))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| DaemonError::io(path, e))?;
        write!(file, "{}", std::process::id()).map_err(|e| DaemonError::io(path, e))?;
        file.flush().map_err(|e| DaemonError::io(path, e))?;
        Ok(PidLock { _file: file })
    }
}

/// The soft open-file limit to ask for, given the current limits (R-094): the target, but never
/// above the hard limit and never below what is already set.
pub fn nofile_plan(soft: Option<u64>, hard: Option<u64>, target: u64) -> u64 {
    let want = hard.map_or(target, |h| target.min(h));
    soft.map_or(want, |s| want.max(s))
}

/// Raises the soft open-file limit toward `target` within the hard limit; jobs inherit it
/// (R-094). Returns the soft limit after the call.
pub fn raise_nofile(target: u64) -> Option<u64> {
    use rustix::process::{Resource, getrlimit, setrlimit};
    let mut lim = getrlimit(Resource::Nofile);
    let want = nofile_plan(lim.current, lim.maximum, target);
    if lim.current != Some(want) {
        lim.current = Some(want);
        if setrlimit(Resource::Nofile, lim).is_err() {
            return getrlimit(Resource::Nofile).current;
        }
    }
    getrlimit(Resource::Nofile).current
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    type EnvFn = Box<dyn Fn(&str) -> Option<String>>;

    fn env_of(pairs: &[(&str, &str)]) -> EnvFn {
        let m: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Box::new(move |k| m.get(k).cloned())
    }

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tasks-daemon-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // R-231: the lock directory never comes from TMPDIR.
    #[test]
    fn r231_lock_dir_ignores_tmpdir_and_is_resolved_from_daemon_config() {
        let env = env_of(&[("HOME", "/home/u"), ("TMPDIR", "/tmp/claude-1000")]);
        let c = DaemonConfig::resolve(&ConfigArgs::default(), &env).unwrap();
        assert_eq!(c.state_dir, PathBuf::from("/home/u/.local/state/tasks2"));
        assert_eq!(
            c.lock_dir,
            PathBuf::from("/home/u/.local/state/tasks2/slots")
        );
        assert!(!c.lock_dir.starts_with("/tmp/claude-1000"));
        // an explicit state dir moves the lock dir with it; TMPDIR still does not matter
        let args = ConfigArgs {
            state_dir: Some("/srv/t".into()),
            ..ConfigArgs::default()
        };
        let c = DaemonConfig::resolve(&args, &env).unwrap();
        assert_eq!(c.lock_dir, PathBuf::from("/srv/t/slots"));
        // only the explicit daemon-side setting overrides it
        let env = env_of(&[("HOME", "/home/u"), ("TASKS_LOCK_DIR", "/var/lock/t")]);
        let c = DaemonConfig::resolve(&ConfigArgs::default(), &env).unwrap();
        assert_eq!(c.lock_dir, PathBuf::from("/var/lock/t"));
    }

    #[test]
    fn relative_directories_are_refused() {
        let args = ConfigArgs {
            state_dir: Some("rel".into()),
            config_dir: Some("/c".into()),
            ..ConfigArgs::default()
        };
        let e = DaemonConfig::resolve(&args, &env_of(&[("HOME", "/h")])).unwrap_err();
        assert!(matches!(e, DaemonError::RelativePath("state directory", _)));
        let e = DaemonConfig::resolve(&ConfigArgs::default(), &env_of(&[])).unwrap_err();
        assert!(matches!(e, DaemonError::NoHome(_)));
    }

    #[test]
    fn second_daemon_on_the_same_state_dir_is_refused_with_the_holder_pid() {
        let dir = temp("pidlock");
        let path = dir.join("daemon.pid");
        let first = PidLock::acquire(&path).unwrap();
        let e = PidLock::acquire(&path).unwrap_err();
        match e {
            DaemonError::AlreadyRunning { pid, .. } => assert_eq!(pid, Some(std::process::id())),
            other => panic!("unexpected {other:?}"),
        }
        drop(first);
        let again = PidLock::acquire(&path).unwrap();
        drop(again);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pid_file_keeps_its_inode_across_restarts() {
        use std::os::unix::fs::MetadataExt;
        let dir = temp("inode");
        let path = dir.join("daemon.pid");
        drop(PidLock::acquire(&path).unwrap());
        let a = std::fs::metadata(&path).unwrap().ino();
        drop(PidLock::acquire(&path).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), a);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R-094
    #[test]
    fn r094_open_file_limit_is_raised_to_the_target_within_the_hard_limit() {
        assert_eq!(nofile_plan(Some(1024), Some(1_048_576), 65_536), 65_536);
        assert_eq!(nofile_plan(Some(1024), Some(4096), 65_536), 4096);
        assert_eq!(nofile_plan(Some(100_000), Some(1_048_576), 65_536), 100_000);
        assert_eq!(nofile_plan(None, None, 65_536), 65_536);
        assert!(raise_nofile(256).is_some());
    }
}
