//! Build caches: the compiler cache every job runs under, the stable output directories
//! outside a synced tree, and the Gradle home.
//!
//! The machine decides all of it. The cache root is the daemon's `--cache-dir` (default
//! `~/build-cache`, so the warm caches of the earlier release are reused); the sccache binary
//! is discovered once at start; the size limit and the off switch are machine-policy keys. The
//! settings are applied after the environment is sanitized, so a job can neither choose its own
//! wrapper nor a different cache directory (R-400).
//!
//! - [`BuildCache::env_policy`] turns the machine's settings into the runner's [`EnvPolicy`].
//! - [`outputs`] places `target` and the Android outputs as symlinks into the cache root, with
//!   the old release's rules: never replace, move or delete an existing entry.
//! - [`report`] is the read-only text behind `tasks2 cache`.

pub mod outputs;
pub mod report;

use std::path::{Path, PathBuf};
use tasks_runner::EnvPolicy;

/// Why a cache step failed. Only a job that cannot work without the step fails with one; the
/// cache itself is an optimization and degrades with a note.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// A file or directory could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// What was being touched.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
    /// A Gradle project has no `local.properties` and the machine has no SDK to write one from.
    #[error(
        "Gradle project {project} has no local.properties (it is git-ignored, so a fresh \
         worktree lacks it) and neither ANDROID_HOME nor ANDROID_SDK_ROOT names an existing SDK \
         directory in the task daemon's environment; create {properties} containing \
         `sdk.dir=/absolute/path/to/Android/Sdk` and resubmit"
    )]
    NoSdk {
        /// The Gradle project.
        project: PathBuf,
        /// The file that is missing.
        properties: PathBuf,
    },
    /// An Android job asked for a Cargo target directory of its own.
    #[error(
        "Android jobs refuse CARGO_TARGET_DIR: the machine keeps their Cargo output in the \
         checkout's own target link"
    )]
    TargetOverride,
}

impl CacheError {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> CacheError {
        CacheError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// The machine-policy values the cache needs (`cache.*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Wrap rustc in sccache when it is installed.
    pub sccache: bool,
    /// The sccache size limit as sccache spells it, `30G`.
    pub sccache_size: String,
}

impl Settings {
    /// The values of `policy`.
    pub fn of(policy: &crate::machine_policy::MachinePolicy) -> Settings {
        Settings {
            sccache: policy.sccache_enabled(),
            sccache_size: policy.sccache_size(),
        }
    }
}

/// Variables whose values would put a different compiler wrapper in front of rustc. The
/// machine's choice replaces them all (R-400).
const WRAPPER_VARS: &[&str] = &[
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
];
/// Every variable sccache reads begins with this.
const SCCACHE_PREFIX: &str = "SCCACHE_";
/// Where sccache lives besides `PATH`, under `$HOME`.
const SCCACHE_FALLBACK_DIRS: &[&str] = &[".cargo/bin"];
/// The directories under `$HOME` the daemon looks in for tools besides its own `PATH`, as
/// absolute paths, for the job's `PATH` too (R-543). Empty without a `HOME`.
pub fn fallback_tool_dirs(env: &[(String, String)]) -> Vec<String> {
    env.iter()
        .find(|(k, _)| k == "HOME")
        .map(|(_, h)| h.as_str())
        .filter(|h| !h.is_empty())
        .map(|home| {
            SCCACHE_FALLBACK_DIRS
                .iter()
                .map(|d| Path::new(home).join(d).to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The sccache directory and the Gradle home under the cache root.
const SCCACHE_SUBDIR: &str = "sccache";
const GRADLE_SUBDIR: &str = "gradle";
/// Job outputs under the cache root.
pub(crate) const CHECKOUTS_SUBDIR: &str = "tasks2/checkouts";

/// The first executable regular file named `sccache` in `dirs`, as an absolute path.
pub fn find_sccache(dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.into_iter().filter(|d| d.is_absolute()).find_map(|d| {
        let candidate = d.join("sccache");
        let meta = std::fs::metadata(&candidate).ok()?;
        (meta.is_file() && is_executable(&meta)).then_some(candidate)
    })
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    true
}

/// What this machine's build caches are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildCache {
    root: PathBuf,
    peer_store: PathBuf,
    sccache: Option<PathBuf>,
}

impl BuildCache {
    /// Finds sccache in the daemon's own `PATH`, then in `$HOME/.cargo/bin` (a service often
    /// lacks the user's shell `PATH`). Nothing is created.
    pub fn discover(root: &Path, peer_store: &Path, env: &[(String, String)]) -> BuildCache {
        let get = |name: &str| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        let mut dirs: Vec<PathBuf> = get("PATH")
            .map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default();
        if let Some(home) = get("HOME").filter(|h| !h.is_empty()) {
            dirs.extend(
                SCCACHE_FALLBACK_DIRS
                    .iter()
                    .map(|d| Path::new(home).join(d)),
            );
        }
        BuildCache {
            root: root.to_path_buf(),
            peer_store: peer_store.to_path_buf(),
            sccache: find_sccache(dirs),
        }
    }

    /// The same machine with a chosen sccache (or none): for tests and explicit configuration.
    #[must_use]
    pub fn with_sccache(mut self, sccache: Option<PathBuf>) -> BuildCache {
        self.sccache = sccache;
        self
    }

    /// The cache root.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Where peers' synced working copies live.
    pub fn peer_store(&self) -> &Path {
        &self.peer_store
    }
    /// The discovered sccache binary.
    pub fn sccache(&self) -> Option<&Path> {
        self.sccache.as_deref()
    }
    /// The sccache directory (`<root>/sccache`).
    pub fn sccache_dir(&self) -> PathBuf {
        self.root.join(SCCACHE_SUBDIR)
    }
    /// The Gradle home (`<root>/gradle`) when that directory exists.
    pub fn gradle_home(&self) -> Option<PathBuf> {
        let dir = self.root.join(GRADLE_SUBDIR);
        dir.is_dir().then_some(dir)
    }
    /// Where checkouts' outputs live (`<root>/tasks2/checkouts`).
    pub fn checkouts_dir(&self) -> PathBuf {
        self.root.join(CHECKOUTS_SUBDIR)
    }

    /// The wrapper in force: the binary, or why there is none.
    pub fn wrapper(&self, settings: &Settings) -> Result<&Path, String> {
        if !settings.sccache {
            return Err("sccache is off (machine policy cache.sccache)".to_string());
        }
        self.sccache().ok_or_else(|| {
            "uncached build: sccache was not found on the daemon's PATH or in ~/.cargo/bin"
                .to_string()
        })
    }

    /// The runner's environment policy: `cargo_jobs` as the scheduler chose it, then the
    /// machine's cache settings, applied after sanitization so no job can override them.
    pub fn env_policy(&self, settings: &Settings, cargo_jobs: Option<u32>) -> EnvPolicy {
        let mut force: Vec<(String, String)> = vec![
            ("CARGO_INCREMENTAL".into(), "0".into()),
            ("CARGO_PROFILE_DEV_DEBUG".into(), "0".into()),
            ("CARGO_PROFILE_TEST_DEBUG".into(), "0".into()),
        ];
        if let Ok(wrapper) = self.wrapper(settings) {
            force.push(("RUSTC_WRAPPER".into(), path_text(wrapper)));
            force.push(("SCCACHE_DIR".into(), path_text(&self.sccache_dir())));
            force.push(("SCCACHE_CACHE_SIZE".into(), settings.sccache_size.clone()));
        }
        if let Some(gradle) = self.gradle_home() {
            force.push(("GRADLE_USER_HOME".into(), path_text(&gradle)));
        }
        EnvPolicy {
            default_cargo_jobs: cargo_jobs,
            scrub: WRAPPER_VARS.iter().map(ToString::to_string).collect(),
            scrub_prefixes: vec![SCCACHE_PREFIX.to_string()],
            path_extra: Vec::new(),
            force,
        }
    }

    /// The first line of a job's output: what cache the job runs under.
    pub fn header(&self, settings: &Settings, placed: &outputs::Placed) -> String {
        let mut parts = Vec::new();
        match self.wrapper(settings) {
            Ok(_) => parts.push(format!("sccache {}", self.sccache_dir().display())),
            Err(why) => parts.push(why),
        }
        for (link, real) in &placed.links {
            let name = link.file_name().map_or_else(
                || link.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            parts.push(format!("{name} -> {}", real.display()));
        }
        for kept in &placed.preserved {
            parts.push(format!(
                "{} kept local (existing directory)",
                kept.display()
            ));
        }
        if let Some(g) = self.gradle_home() {
            parts.push(format!("gradle home {}", g.display()));
        }
        format!("cache: {}", parts.join(", "))
    }

    /// What a peer may fetch through an output link: the same names the placement creates.
    pub fn fetch_policy() -> tasks_peer::outputs::FetchPolicy {
        tasks_peer::outputs::FetchPolicy {
            allowed_links: outputs::LINK_NAMES
                .iter()
                .map(ToString::to_string)
                .collect(),
            ..tasks_peer::outputs::FetchPolicy::default()
        }
    }
}

fn path_text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod daemon_tests;
#[cfg(test)]
mod tests;
