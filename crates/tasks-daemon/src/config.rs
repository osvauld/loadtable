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
use tasks_core::names;

/// The cache root under `$HOME` when nothing else says.
pub const DEFAULT_CACHE_DIR: &str = "build-cache";

/// Directories and names, fixed for the daemon's lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// Holds the ledger, socket, pid file, logs and the execution intent.
    pub state_dir: PathBuf,
    /// Holds `machine-policy.toml`.
    pub config_dir: PathBuf,
    /// Holds the slot lock files; the same for every client (R-231).
    pub lock_dir: PathBuf,
    /// The cache root (default `~/build-cache`): compiler cache, Gradle home and checkout
    /// outputs live here; its filesystem is checked against the disk reserve.
    pub cache_dir: PathBuf,
    /// This machine's name in placement.
    pub machine: String,
}

/// The XDG state and config homes the default directories sit in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Homes {
    /// `$XDG_STATE_HOME`, else `~/.local/state`.
    pub state: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME`, else `~/.config`.
    pub config: Option<PathBuf>,
}

impl Homes {
    /// The homes from `env`; a relative XDG variable is ignored, as the spec says.
    pub fn of(env: &dyn Fn(&str) -> Option<String>) -> Homes {
        let home = env("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
        let xdg = |var: &str, under_home: &str| -> Option<PathBuf> {
            env(var)
                .filter(|v| v.starts_with('/'))
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|h| h.join(under_home)))
        };
        Homes {
            state: xdg("XDG_STATE_HOME", ".local/state"),
            config: xdg("XDG_CONFIG_HOME", ".config"),
        }
    }
}

/// `<base>/loadtable`, or `<base>/tasks2` while only that one exists: the daemon moves it on
/// start (R-804), and until then a client still finds the daemon of the old name.
fn named_dir(base: &Path) -> PathBuf {
    let new = base.join(names::DIR_NAME);
    let old = base.join(names::OLD_DIR_NAME);
    if !new.exists() && old.is_dir() {
        old
    } else {
        new
    }
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
    /// lock directory is `--lock-dir`, then `LOADTABLE_LOCK_DIR`, then `<state_dir>/slots`. It is
    /// never derived from `TMPDIR`.
    pub fn resolve(
        args: &ConfigArgs,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<DaemonConfig, DaemonError> {
        let home = env("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
        let homes = Homes::of(env);
        let state_dir = args
            .state_dir
            .clone()
            .or_else(|| env("LOADTABLE_STATE_DIR").map(PathBuf::from))
            .or_else(|| homes.state.as_deref().map(named_dir))
            .ok_or(DaemonError::NoHome("state directory"))?;
        let config_dir = args
            .config_dir
            .clone()
            .or_else(|| env("LOADTABLE_CONFIG_DIR").map(PathBuf::from))
            .or_else(|| homes.config.as_deref().map(named_dir))
            .ok_or(DaemonError::NoHome("config directory"))?;
        let cache_dir = args
            .cache_dir
            .clone()
            .or_else(|| env("LOADTABLE_CACHE_DIR").map(PathBuf::from))
            // the earlier release's cache root, so its warm caches are reused (R-401)
            .or_else(|| home.as_ref().map(|h| h.join(DEFAULT_CACHE_DIR)))
            .unwrap_or_else(|| state_dir.clone());
        let lock_dir = args
            .lock_dir
            .clone()
            .or_else(|| env("LOADTABLE_LOCK_DIR").map(PathBuf::from))
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
            .or_else(|| env("LOADTABLE_MACHINE"))
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

/// How long [`PidLock::acquire`] waits for a lock held under its own pid to be let go.
const OWN_PID_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

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
        let t0 = std::time::Instant::now();
        loop {
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    let mut text = String::new();
                    let _ = file.seek(SeekFrom::Start(0));
                    let _ = file.read_to_string(&mut text);
                    let pid = text.trim().parse::<u32>().ok();
                    // Held under our own pid: a lock this process just dropped can still be held
                    // by a child forked a moment ago, until it execs (close-on-exec acts only
                    // then). That clears in milliseconds; a lock we really still hold does not.
                    if pid == Some(std::process::id()) && t0.elapsed() < OWN_PID_LOCK_WAIT {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    return Err(DaemonError::AlreadyRunning {
                        pid,
                        path: path.to_path_buf(),
                    });
                }
                Err(e) => return Err(DaemonError::io(path, std::io::Error::from(e))),
            }
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
        assert_eq!(c.state_dir, PathBuf::from("/home/u/.local/state/loadtable"));
        assert_eq!(
            c.lock_dir,
            PathBuf::from("/home/u/.local/state/loadtable/slots")
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
        let env = env_of(&[("HOME", "/home/u"), ("LOADTABLE_LOCK_DIR", "/var/lock/t")]);
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
        // taken again at once, even while a child another test forks holds a copy of the
        // descriptor for a moment (the own-pid wait in `acquire`)
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
