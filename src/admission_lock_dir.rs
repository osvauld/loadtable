//! Admission lock directory. The default is exactly `/tmp/claude-$EUID`: the same
//! path, and therefore the same flock inodes, as existing clients, scripts and
//! Tinline adapters. TASKS_LOCK_DIR moves it for one daemon/worker (e.g. a host
//! whose /tmp is age-cleaned). A missing directory is created 0700; a symlink, a
//! directory owned by another user or a world-writable one is refused, never
//! chmod-ed or replaced. Nothing here deletes lock files.
use crate::{Result, execution::now};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT_RECORD: AtomicU64 = AtomicU64::new(0);

/// Absolute directory for the admission flock files. Unset = `/tmp/claude-$EUID`.
pub const LOCK_DIR_ENV: &str = "TASKS_LOCK_DIR";
/// Written by each worker at start so `tasks admission` in another shell (e.g. an
/// SSH client without the unit's environment) inspects the directory the worker uses.
pub const LOCK_DIR_RECORD: &str = "admission-lock-dir.json";

fn euid() -> u32 {
    unsafe { libc::geteuid() }
}
/// The historical, compatibility-critical path. Never change this format.
pub fn default_root() -> PathBuf {
    PathBuf::from(format!("/tmp/claude-{}", euid()))
}
/// Validated, normalized lock directory from an optional raw environment value.
pub fn root_from(raw: Option<OsString>) -> Result<PathBuf> {
    let Some(raw) = raw else {
        return Ok(default_root());
    };
    let error = || -> Box<dyn std::error::Error + Send + Sync> {
        let shown: String = raw.to_string_lossy().chars().take(64).collect();
        format!(
            "{LOCK_DIR_ENV} must be an absolute UTF-8 directory path other than / \
             (no . or .. components, at most 1024 bytes); got {shown:?}"
        )
        .into()
    };
    let text = raw.to_str().ok_or_else(error)?;
    let path = Path::new(text);
    if text.is_empty()
        || text.len() > 1024
        || !path.is_absolute()
        || text.split('/').any(|part| part == "." || part == "..")
        || !path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        || path.components().count() < 2
    {
        return Err(error());
    }
    // Drops duplicate and trailing slashes so equal directories compare equal.
    Ok(path.components().collect())
}
/// This process's lock directory. Invalid configuration fails closed.
pub fn root() -> Result<PathBuf> {
    root_from(std::env::var_os(LOCK_DIR_ENV))
}
/// Creates the final component 0700 if it is missing (never its parents), then
/// verifies it. An existing directory's mode is checked, not changed.
pub(super) fn prepare_root(root: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "admission lock directory {}: parent directory does not exist; create it or fix {LOCK_DIR_ENV}",
                root.display()
            )
            .into());
        }
        Err(e) => {
            return Err(format!("admission lock directory {}: {e}", root.display()).into());
        }
    }
    check_root(root)
}
pub(super) fn check_root(root: &Path) -> Result<()> {
    let m = fs::symlink_metadata(root)
        .map_err(|e| format!("admission lock directory {}: {e}", root.display()))?;
    if !m.is_dir() {
        return Err(format!(
            "admission lock directory {} must be a real directory, not a symlink or file; refusing",
            root.display()
        )
        .into());
    }
    if m.uid() != euid() {
        return Err(format!(
            "admission lock directory {} is owned by uid {}, not this user (uid {}); refusing",
            root.display(),
            m.uid(),
            euid()
        )
        .into());
    }
    if m.mode() & 0o002 != 0 {
        return Err(format!(
            "admission lock directory {} is world-writable (mode {:o}); refusing. \
             Make it private (chmod 700) when no admission is active",
            root.display(),
            m.mode() & 0o7777
        )
        .into());
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LockDirRecord {
    pub schema: u32,
    pub pid: u32,
    pub recorded_at: i64,
    pub lock_dir: PathBuf,
}
/// Atomic, user-only diagnostic record; not authority (each process uses its own
/// environment). Lets client shells inspect and compare the worker's directory.
pub fn record_worker_lock_dir(state: &Path, lock_dir: &Path) -> Result<()> {
    let record = LockDirRecord {
        schema: 1,
        pid: std::process::id(),
        recorded_at: now(),
        lock_dir: lock_dir.to_path_buf(),
    };
    let temp = state.join(format!(
        ".{LOCK_DIR_RECORD}.{}-{}",
        std::process::id(),
        NEXT_RECORD.fetch_add(1, Ordering::Relaxed)
    ));
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    out.write_all(&serde_json::to_vec_pretty(&record)?)?;
    out.sync_all()?;
    fs::rename(&temp, state.join(LOCK_DIR_RECORD))?;
    Ok(())
}
fn read_record(state: &Path) -> Result<Option<LockDirRecord>> {
    let path = state.join(LOCK_DIR_RECORD);
    let m = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !m.is_file() || m.uid() != euid() || m.len() > 4096 {
        return Err("unsafe/oversized worker lock-directory record".into());
    }
    let record: LockDirRecord = serde_json::from_slice(&fs::read(&path)?)?;
    let valid = record.schema == 1
        && root_from(Some(record.lock_dir.clone().into_os_string()))
            .is_ok_and(|p| p == record.lock_dir);
    if !valid {
        return Err("worker lock-directory record is not a valid normalized path".into());
    }
    Ok(Some(record))
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct LockDirReport {
    pub path: PathBuf,
    /// "worker-record", "environment" or "default".
    pub source: String,
    pub is_default: bool,
    pub recorded_by_pid: Option<u32>,
    pub recorded_at: Option<i64>,
}
/// Lock directory to INSPECT from a client shell: the last worker's record if
/// valid, else this process's own. Any disagreement becomes a warning, because a
/// daemon autostarted from this shell would use this shell's directory.
pub fn report_lock_dir(
    state: &Path,
    own: Result<PathBuf>,
    own_overridden: bool,
) -> Result<(LockDirReport, Vec<String>)> {
    let mut warnings = vec![];
    let report = match read_record(state) {
        Ok(Some(record)) => {
            match &own {
                Ok(mine) if *mine != record.lock_dir => warnings.push(format!(
                    "this shell's admission lock directory {} differs from the worker's recorded {}; \
                     the worker uses its own environment, but a daemon autostarted from this shell \
                     would use {}. Set {LOCK_DIR_ENV} identically for clients and the daemon unit",
                    mine.display(),
                    record.lock_dir.display(),
                    mine.display()
                )),
                Err(e) => warnings.push(format!("this shell's {LOCK_DIR_ENV} is invalid: {e}")),
                _ => (),
            }
            LockDirReport {
                is_default: record.lock_dir == default_root(),
                path: record.lock_dir,
                source: "worker-record".into(),
                recorded_by_pid: Some(record.pid),
                recorded_at: Some(record.recorded_at),
            }
        }
        other => {
            if let Err(e) = other {
                warnings.push(format!("{}: {e}", state.join(LOCK_DIR_RECORD).display()));
            }
            let path = own?;
            LockDirReport {
                is_default: path == default_root(),
                path,
                source: if own_overridden {
                    "environment"
                } else {
                    "default"
                }
                .into(),
                recorded_by_pid: None,
                recorded_at: None,
            }
        }
    };
    if !report.is_default {
        warnings.push(format!(
            "admission lock directory {} is not the default {}: legacy clients that use the \
             default (Python buildlock helpers, Tinline adapters) are NOT coordinated with it",
            report.path.display(),
            default_root().display()
        ));
    }
    Ok((report, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    struct Dir(PathBuf);
    impl Dir {
        fn new(label: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "tasks-lock-dir-{label}-{}-{}",
                std::process::id(),
                NEXT_RECORD.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn some(text: &str) -> Option<OsString> {
        Some(text.into())
    }

    #[test]
    fn default_lock_dir_is_exactly_the_historical_path() {
        let expected = PathBuf::from(format!("/tmp/claude-{}", unsafe { libc::geteuid() }));
        assert_eq!(default_root(), expected);
        assert_eq!(root_from(None).unwrap(), expected);
        assert_eq!(LOCK_DIR_ENV, "TASKS_LOCK_DIR");
    }
    #[test]
    fn lock_dir_override_is_validated_and_normalized() {
        assert_eq!(
            root_from(some("/var/lib/tasks//locks/")).unwrap(),
            PathBuf::from("/var/lib/tasks/locks")
        );
        for bad in [
            "",
            "relative/dir",
            "/",
            "//",
            "/a/../b",
            "/a/./b",
            "./x",
            "/a/..",
        ] {
            assert!(root_from(some(bad)).is_err(), "{bad:?} accepted");
        }
        assert!(root_from(some(&format!("/{}", "a".repeat(1100)))).is_err());
        use std::os::unix::ffi::OsStringExt;
        assert!(root_from(Some(OsString::from_vec(b"/tmp/\xff".to_vec()))).is_err());
        let message = root_from(some("rel")).unwrap_err().to_string();
        assert!(message.contains("TASKS_LOCK_DIR"), "{message}");
    }
    #[test]
    fn missing_lock_dir_is_created_private() {
        let d = Dir::new("create");
        let root = d.0.join("locks");
        prepare_root(&root).unwrap();
        let mode = fs::metadata(&root).unwrap().mode() & 0o7777;
        assert_eq!(mode, 0o700, "created with {mode:o}");
        // Idempotent on an existing private directory; parents are never created.
        prepare_root(&root).unwrap();
        let message = prepare_root(&d.0.join("no/such/parent"))
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("parent directory does not exist"),
            "{message}"
        );
        assert!(!d.0.join("no").exists());
    }
    #[test]
    fn unsafe_lock_dirs_are_refused_and_left_unchanged() {
        let d = Dir::new("unsafe");
        let open = d.0.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        let message = prepare_root(&open).unwrap_err().to_string();
        assert!(message.contains("world-writable"), "{message}");
        assert_eq!(fs::metadata(&open).unwrap().mode() & 0o7777, 0o777);
        let private = d.0.join("private");
        fs::create_dir(&private).unwrap();
        let link = d.0.join("link");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        assert!(
            prepare_root(&link)
                .unwrap_err()
                .to_string()
                .contains("symlink")
        );
        if unsafe { libc::geteuid() } != 0 {
            let message = check_root(Path::new("/")).unwrap_err().to_string();
            assert!(message.contains("owned by uid 0"), "{message}");
        }
        // Group-readable/writable but not world-writable is accepted (existing hosts).
        fs::set_permissions(&private, fs::Permissions::from_mode(0o750)).unwrap();
        prepare_root(&private).unwrap();
    }
    #[test]
    fn worker_record_drives_reported_lock_dir_and_flags_disagreement() {
        let state = Dir::new("state");
        let default = default_root();
        let (report, warnings) = report_lock_dir(&state.0, Ok(default.clone()), false).unwrap();
        assert_eq!(
            (report.source.as_str(), report.is_default),
            ("default", true)
        );
        assert_eq!(report.path, default);
        assert!(warnings.is_empty(), "{warnings:?}");
        let custom = PathBuf::from("/var/tmp/tasks-locks");
        let (report, warnings) = report_lock_dir(&state.0, Ok(custom.clone()), true).unwrap();
        assert_eq!(
            (report.source.as_str(), report.is_default),
            ("environment", false)
        );
        assert_eq!(
            warnings.len(),
            1,
            "non-default must warn about legacy clients"
        );
        assert!(report_lock_dir(&state.0, Err("bad".into()), true).is_err());

        record_worker_lock_dir(&state.0, &custom).unwrap();
        let mode = fs::metadata(state.0.join(LOCK_DIR_RECORD)).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Same as the worker: no disagreement warning, only the legacy note.
        let (report, warnings) = report_lock_dir(&state.0, Ok(custom.clone()), true).unwrap();
        assert_eq!(report.source, "worker-record");
        assert_eq!(report.path, custom);
        assert_eq!(report.recorded_by_pid, Some(std::process::id()));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        // A plain SSH shell (default) inspects the worker's directory and is warned.
        let (report, warnings) = report_lock_dir(&state.0, Ok(default.clone()), false).unwrap();
        assert_eq!(report.path, custom);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("differs"), "{warnings:?}");
        // A tampered record is a warning, never trusted.
        fs::write(
            state.0.join(LOCK_DIR_RECORD),
            br#"{"schema":1,"pid":1,"recorded_at":0,"lock_dir":"relative"}"#,
        )
        .unwrap();
        let (report, warnings) = report_lock_dir(&state.0, Ok(default.clone()), false).unwrap();
        assert_eq!(report.source, "default");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }
}
