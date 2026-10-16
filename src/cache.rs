//! Worker-owned compilation policy and cooperative target leases.
//! No pruning/deletion, test-result reuse, or submitted environment overrides.
use crate::Result;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt, fs::PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Default reserves. A daemon/worker may raise or lower them within validated
/// bounds via TASKS_MIN_CACHE_FREE_GIB / TASKS_MIN_STATE_FREE_MIB (see admission).
pub const MIN_FREE_BYTES: u64 = 20 * 1024 * 1024 * 1024;
pub const MIN_STATE_FREE_BYTES: u64 = 256 * 1024 * 1024;
pub const SCCACHE_LIMIT: &str = "30G";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiskSpace {
    pub path: PathBuf,
    pub available_bytes: u64,
    pub reserve_bytes: u64,
}
impl DiskSpace {
    pub fn admitted(&self) -> bool {
        self.available_bytes >= self.reserve_bytes
    }
    pub fn detail(&self) -> String {
        format!(
            "{}: {:.1} GiB available, {:.1} GiB reserve",
            self.path.display(),
            self.available_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            self.reserve_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        )
    }
}

/// Inspect the nearest existing ancestor, without creating a directory.
/// Reserve: this process's effective cache threshold (fails closed if invalid).
pub fn disk_space(path: &Path) -> Result<DiskSpace> {
    cache_space_with(path, &crate::admission::thresholds()?)
}
pub fn cache_space_with(
    path: &Path,
    thresholds: &crate::admission::Thresholds,
) -> Result<DiskSpace> {
    measure(path, thresholds.min_cache_free_bytes)
}
fn measure(path: &Path, reserve_bytes: u64) -> Result<DiskSpace> {
    if !path.is_absolute() {
        return Err("cache/disk paths must be absolute".into());
    }
    let mut existing = path;
    while !existing.try_exists()? {
        existing = existing.parent().ok_or("no existing disk ancestor")?;
    }
    let name = CString::new(existing.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(name.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(DiskSpace {
        path: path.to_path_buf(),
        available_bytes: ((stat.f_bavail as u128) * (stat.f_frsize as u128)).min(u64::MAX as u128)
            as u64,
        reserve_bytes,
    })
}

pub fn state_space(path: &Path) -> Result<DiskSpace> {
    state_space_with(path, &crate::admission::thresholds()?)
}
pub fn state_space_with(
    path: &Path,
    thresholds: &crate::admission::Thresholds,
) -> Result<DiskSpace> {
    measure(path, thresholds.min_state_free_bytes)
}

pub fn require_state_space(path: &Path) -> Result<()> {
    let space = state_space(path)?;
    if !space.admitted() {
        return Err(format!("insufficient state disk space: {}", space.detail()).into());
    }
    Ok(())
}

pub fn require_space(paths: &[&Path]) -> Result<()> {
    for path in paths {
        let space = disk_space(path)?;
        if !space.admitted() {
            return Err(format!("insufficient disk space: {}", space.detail()).into());
        }
    }
    Ok(())
}

pub fn target_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or("HOME is required for cache policy")?;
    let target = std::env::var_os("TASK_MANAGER_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(home).join("build-cache/p2p_chat/tasks/targets/self-tests")
        });
    if !target.is_absolute() {
        return Err("TASK_MANAGER_TARGET_DIR must be absolute".into());
    }
    Ok(target)
}

/// Shared by every cooperating worker using the same canonical target directory.
/// Never unlink this file: its inode must remain stable for concurrent users.
pub struct TargetLease(File);
impl TargetLease {
    pub fn try_acquire(target: &Path) -> Result<Option<Self>> {
        Self::try_open(target, true)
    }
    /// Planning may probe an existing lease, but must never create/adopt one.
    pub fn try_existing(target: &Path) -> Result<Option<Self>> {
        Self::try_open(target, false)
    }
    fn try_open(target: &Path, create: bool) -> Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(create)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(target.join(".tasks-target.lock"))?;
        if !file.metadata()?.is_file() {
            return Err("target lease must be a regular file".into());
        }
        match file.try_lock() {
            Ok(()) => Ok(Some(Self(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    pub fn file(&self) -> &File {
        &self.0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompileCache {
    pub wrapper: Option<PathBuf>,
    pub directory: PathBuf,
    pub max_size: String,
}
impl CompileCache {
    pub fn discover() -> Result<Self> {
        let home = std::env::var_os("HOME").ok_or("HOME is required for cache policy")?;
        let wrapper = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path).find_map(|dir| {
                let candidate = dir.join("sccache");
                let metadata = fs::metadata(&candidate).ok()?;
                (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                    .then(|| fs::canonicalize(candidate).ok())
                    .flatten()
            })
        });
        Ok(Self {
            wrapper,
            directory: PathBuf::from(home).join("build-cache/sccache"),
            max_size: SCCACHE_LIMIT.into(),
        })
    }
    /// Called AFTER environment sanitization. Never trust an agent/job's wrapper
    /// or cache-directory override. Missing sccache permits an explicit uncached build.
    pub fn apply(&self, command: &mut Command) {
        command
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("SCCACHE_DIR")
            .env_remove("SCCACHE_CACHE_SIZE")
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_PROFILE_DEV_DEBUG", "0")
            .env("CARGO_PROFILE_TEST_DEBUG", "0");
        if let Some(wrapper) = &self.wrapper {
            command
                .env("RUSTC_WRAPPER", wrapper)
                .env("SCCACHE_DIR", &self.directory)
                .env("SCCACHE_CACHE_SIZE", &self.max_size);
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CacheReport {
    pub thresholds: crate::admission::ThresholdReport,
    pub compile_cache: CompileCache,
    pub disks: Vec<DiskSpace>,
    pub target: PathBuf,
    pub target_lease: &'static str,
    pub target_budget_bytes: u64,
    pub cleanup_enabled: bool,
}
/// Read-only diagnostics. Does not start a daemon or create/lock a target.
pub fn report(state: &Path) -> Result<CacheReport> {
    let compile_cache = CompileCache::discover()?;
    let target = target_path()?;
    let (thresholds, _) = crate::admission::report_thresholds(
        state,
        crate::admission::thresholds(),
        crate::admission::Thresholds::overridden(|name| std::env::var_os(name)),
    )?;
    let t = &thresholds.effective;
    Ok(CacheReport {
        disks: vec![
            cache_space_with(&target, t)?,
            state_space_with(state, t)?,
            cache_space_with(&compile_cache.directory, t)?,
        ],
        thresholds,
        compile_cache,
        target,
        target_lease: "exclusive across build/run/cleanup; cooperating workers only",
        target_budget_bytes: crate::retention::TARGET_BUDGET_BYTES,
        cleanup_enabled: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn temp() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tasks-cache-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn disk_gate_has_a_precise_reserve_and_report_does_not_create_paths() {
        let mut sample = DiskSpace {
            path: "/fixture".into(),
            available_bytes: MIN_FREE_BYTES - 1,
            reserve_bytes: MIN_FREE_BYTES,
        };
        assert!(!sample.admitted());
        sample.available_bytes += 1;
        assert!(sample.admitted());
        let path = temp();
        let missing = path.join("not-created/target");
        assert!(disk_space(&missing).unwrap().available_bytes > 0);
        assert_eq!(
            state_space(&missing).unwrap().reserve_bytes,
            MIN_STATE_FREE_BYTES
        );
        assert!(!missing.exists());
        assert!(disk_space(Path::new("relative")).is_err());
        fs::remove_dir(path).unwrap();
    }
    #[test]
    fn configured_reserves_are_applied_and_reported() {
        let path = temp();
        let missing = path.join("not-created/target");
        let custom = crate::admission::Thresholds {
            min_available_memory_bytes: crate::admission::MIN_MEMORY,
            min_cache_free_bytes: 3 * 1024 * 1024 * 1024,
            min_state_free_bytes: 64 * 1024 * 1024,
        };
        let cache = cache_space_with(&missing, &custom).unwrap();
        assert_eq!(cache.reserve_bytes, 3 * 1024 * 1024 * 1024);
        let state = state_space_with(&missing, &custom).unwrap();
        assert_eq!(state.reserve_bytes, 64 * 1024 * 1024);
        assert!(!missing.exists());
        let blocked = DiskSpace {
            path: "/fixture".into(),
            available_bytes: 1024 * 1024 * 1024,
            reserve_bytes: custom.min_cache_free_bytes,
        };
        assert_eq!(
            blocked.detail(),
            "/fixture: 1.0 GiB available, 3.0 GiB reserve"
        );
        fs::remove_dir(path).unwrap();
    }
    // Parallel Git/Python tests may briefly inherit this fd between fork/exec.
    // Retry only IDLE expectations; active/exclusive assertions remain immediate.
    fn assert_released(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if TargetLease::try_acquire(path).unwrap().is_some() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "lease stayed active after all owned holders exited"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    #[test]
    fn target_lease_is_exclusive_and_clones_retain_it() {
        let path = temp();
        let first = TargetLease::try_acquire(&path).unwrap().unwrap();
        assert!(TargetLease::try_acquire(&path).unwrap().is_none());
        let inherited = first.file().try_clone().unwrap();
        drop(first);
        assert!(TargetLease::try_acquire(&path).unwrap().is_none());
        drop(inherited);
        assert_released(&path);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn lease_survives_python_subprocess_boundary_on_standard_descriptor() {
        let path = temp();
        let first = TargetLease::try_acquire(&path).unwrap().unwrap();
        let script = "import subprocess,sys; subprocess.run([sys.executable,'-c',sys.argv[1],sys.argv[2]],check=True)";
        let nested = "import pathlib,sys,time; p=pathlib.Path(sys.argv[1]); (p/'ready').touch(); deadline=time.monotonic()+10\nwhile not (p/'release').exists() and time.monotonic()<deadline: time.sleep(.02)";
        let mut child = Command::new("python3")
            .args(["-c", script, nested])
            .arg(&path)
            .stdout(first.file().try_clone().unwrap())
            .spawn()
            .unwrap();
        drop(first);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !path.join("ready").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let ready = path.join("ready").exists();
        let busy = TargetLease::try_acquire(&path).unwrap().is_none();
        fs::write(path.join("release"), "").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(ready && busy, "nested process must retain target lease");
        assert_released(&path);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn target_lease_refuses_symlink_lock_files() {
        let path = temp();
        let other = path.join("other");
        fs::write(&other, "preserve").unwrap();
        std::os::unix::fs::symlink(&other, path.join(".tasks-target.lock")).unwrap();
        assert!(TargetLease::try_acquire(&path).is_err());
        assert_eq!(fs::read_to_string(other).unwrap(), "preserve");
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn cache_policy_replaces_overrides_and_disables_disposable_incremental_output() {
        let policy = CompileCache {
            wrapper: Some("/trusted/sccache".into()),
            directory: "/cache/sccache".into(),
            max_size: SCCACHE_LIMIT.into(),
        };
        let mut command = Command::new("cargo");
        command
            .env("RUSTC_WRAPPER", "/untrusted")
            .env("SCCACHE_DIR", "/wrong");
        policy.apply(&mut command);
        let values: std::collections::BTreeMap<_, _> = command
            .get_envs()
            .map(|(k, v)| (k.to_str().unwrap(), v.and_then(|s| s.to_str())))
            .collect();
        assert_eq!(values["RUSTC_WRAPPER"], Some("/trusted/sccache"));
        assert_eq!(values["SCCACHE_DIR"], Some("/cache/sccache"));
        assert_eq!(values["SCCACHE_CACHE_SIZE"], Some("30G"));
        assert_eq!(values["CARGO_INCREMENTAL"], Some("0"));
        assert_eq!(values["CARGO_PROFILE_TEST_DEBUG"], Some("0"));
        let mut uncached = command;
        CompileCache {
            wrapper: None,
            ..policy
        }
        .apply(&mut uncached);
        assert!(
            uncached
                .get_envs()
                .any(|(k, v)| k == "RUSTC_WRAPPER" && v.is_none())
        );
    }
}
