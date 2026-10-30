//! Which folders a job from another machine may touch here (R-821).
//!
//! A paired machine sends builds, not requests to read the owner's files. Such a job runs
//! confined (see [`tasks_platform::confine`]): it reads and writes the build cache, the synced
//! copies and temp; it reads and writes the toolchains (cargo, rustup, gradle, the Android SDK,
//! Xcode's data), which download and install into themselves; it reads the system and a few
//! tool settings. Everything else in the home folder, other volumes and `/root` cannot be opened.
//! The machine's owner widens or switches it off in `[confine]` of the machine policy.

use crate::build_cache::BuildCache;
use crate::machine_policy::MachinePolicy;
use std::path::{Path, PathBuf};
use tasks_platform::Confine;

/// Toolchain folders under the home folder, read and write: they fetch and install into
/// themselves during a build.
pub const HOME_TOOLS_WRITE: &[&str] = &[
    ".cargo",
    ".rustup",
    ".gradle",
    ".android",
    "Android",
    ".npm",
    ".m2",
    ".konan",
    // the Kotlin compiler daemon keeps its run files here; without it every Gradle build of
    // Kotlin falls back to compiling in-process after a failed start (R-843)
    ".local/share/kotlin",
    "Library/Developer",
    "Library/Android",
    "Library/Caches/org.swift.swiftpm",
];

/// Tool settings and binaries under the home folder, read only.
pub const HOME_TOOLS_READ: &[&str] = &[
    ".local/bin",
    ".nix-profile",
    ".local/state/nix",
    ".gitconfig",
    ".config/git",
];

/// Environment variables naming a toolchain folder that may live elsewhere (read and write).
const ENV_TOOLS_WRITE: &[&str] = &[
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GRADLE_USER_HOME",
    "ANDROID_HOME",
    "ANDROID_SDK_ROOT",
    "ANDROID_USER_HOME",
];

/// Environment variables naming a toolchain folder that is only read.
const ENV_TOOLS_READ: &[&str] = &["JAVA_HOME"];

/// Other people's data outside the home folder: removable and mounted volumes, root's home.
fn other_closed() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["/Volumes"]
    } else {
        &["/root", "/mnt", "/media", "/run/media"]
    }
}

/// The argv prefix that runs the confine helper: this binary's `confine` subcommand. `None`
/// when the running executable cannot be found.
pub fn helper_from_current_exe() -> Option<Vec<String>> {
    let exe = std::env::current_exe().ok()?.display().to_string();
    Some(if crate::entry::is_daemon_binary(&exe) {
        vec![exe, "confine".into()]
    } else {
        vec![exe, "daemon".into(), "confine".into()]
    })
}

/// The confinement of a job from another machine: `Ok(None)` when the policy switched it off,
/// `Err` when it is on but cannot be set up (the job must then not run).
pub fn for_peer_job(
    policy: &MachinePolicy,
    cache: &BuildCache,
    env: &[(String, String)],
    helper: Option<&[String]>,
) -> Result<Option<Confine>, String> {
    if !policy.confine_peer_jobs() {
        return Ok(None);
    }
    let get = |k: &str| {
        env.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let home = get("HOME").ok_or("the daemon has no HOME, so it cannot tell what to close")?;
    let home = real(Path::new(&home)).ok_or(format!("the home folder {home} does not exist"))?;
    let helper = match helper {
        Some(h) => h.to_vec(),
        None if cfg!(target_os = "macos") => Vec::new(),
        None => return Err("the confine helper (loadtabled confine) cannot be found".into()),
    };
    let mut closed = vec![home.clone()];
    closed.extend(other_closed().iter().filter_map(|p| real(Path::new(p))));

    let mut write: Vec<PathBuf> =
        vec![cache.root().to_path_buf(), cache.peer_store().to_path_buf()];
    write.extend(HOME_TOOLS_WRITE.iter().map(|t| home.join(t)));
    write.extend(
        ENV_TOOLS_WRITE
            .iter()
            .filter_map(|k| get(k))
            .map(PathBuf::from),
    );
    write.extend(policy.confine_write().iter().map(|p| expand(&home, p)));

    let mut read: Vec<PathBuf> = HOME_TOOLS_READ.iter().map(|t| home.join(t)).collect();
    read.extend(
        ENV_TOOLS_READ
            .iter()
            .filter_map(|k| get(k))
            .map(PathBuf::from),
    );
    read.extend(policy.confine_read().iter().map(|p| expand(&home, p)));

    Ok(Some(Confine {
        helper,
        closed,
        write: existing(write),
        read: existing(read),
    }))
}

/// `~/x` and relative paths are under the home folder.
fn expand(home: &Path, p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if p == "~" => home.to_path_buf(),
        None => home.join(p),
    }
}

/// The real path (symlinks resolved, as the OS checks it), or `None` when it does not exist.
fn real(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok()
}

fn existing(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths.iter().filter_map(|p| real(p)) {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// The log line saying what a confined job may touch.
pub fn note(c: &Confine) -> String {
    let show = |ps: &[PathBuf]| {
        ps.iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "confine: this job came from another machine, so it runs confined: it cannot open \
         anything in {} except: read and write {}; read {}",
        show(&c.closed),
        show(&c.write),
        if c.read.is_empty() {
            "nothing more".to_string()
        } else {
            show(&c.read)
        }
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::testkit::TempDir;

    fn rig() -> (TempDir, BuildCache, Vec<(String, String)>) {
        let t = TempDir::new("confine");
        let home = t.path().join("home");
        for d in ["build-cache", ".cargo", ".rustup", "Photos", "work"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let store = t.path().join("state/peer-store");
        std::fs::create_dir_all(&store).unwrap();
        let cache = BuildCache::discover(&home.join("build-cache"), &store, &[]);
        let env = vec![("HOME".to_string(), home.display().to_string())];
        (t, cache, env)
    }

    fn policy(text: &str) -> MachinePolicy {
        MachinePolicy::from_text(Some(text), &|_| None)
    }

    #[test]
    fn r821_a_peer_job_is_closed_out_of_home_but_keeps_the_cache_copies_and_toolchains() {
        let (t, cache, env) = rig();
        let helper = vec!["/x/loadtabled".to_string(), "confine".to_string()];
        let c = for_peer_job(&policy(""), &cache, &env, Some(&helper))
            .unwrap()
            .unwrap();
        let home = real(&t.path().join("home")).unwrap();
        assert_eq!(c.closed[0], home);
        assert!(c.write.contains(&home.join("build-cache")));
        assert!(c.write.contains(&real(cache.peer_store()).unwrap()));
        assert!(c.write.contains(&home.join(".cargo")));
        assert!(c.write.contains(&home.join(".rustup")));
        let all: Vec<_> = c.write.iter().chain(&c.read).collect();
        assert!(!all.iter().any(|p| p.starts_with(home.join("Photos"))));
        assert!(!all.iter().any(|p| p.starts_with(home.join("work"))));
        assert!(
            !all.contains(&&home),
            "the home folder itself is never opened"
        );
        assert!(
            !all.iter().any(|p| p.ends_with(".gradle")),
            "a toolchain this machine does not have is not listed"
        );
        assert!(note(&c).contains("cannot open"));
    }

    #[test]
    fn r843_a_peer_job_may_write_the_kotlin_daemons_folder_but_not_its_neighbours() {
        let (t, cache, env) = rig();
        let home = t.path().join("home");
        std::fs::create_dir_all(home.join(".local/share/kotlin")).unwrap();
        std::fs::create_dir_all(home.join(".local/share/keyrings")).unwrap();
        let helper = vec!["/x/loadtabled".to_string(), "confine".to_string()];
        let c = for_peer_job(&policy(""), &cache, &env, Some(&helper))
            .unwrap()
            .unwrap();
        let home = real(&home).unwrap();
        assert!(c.write.contains(&home.join(".local/share/kotlin")));
        let all: Vec<_> = c.write.iter().chain(&c.read).collect();
        assert!(
            !all.iter()
                .any(|p| p.starts_with(home.join(".local/share/keyrings")))
        );
        assert!(!all.contains(&&home.join(".local/share")));
    }

    #[test]
    fn r821_the_owner_widens_it_or_switches_it_off_in_the_policy() {
        let (t, cache, env) = rig();
        let helper = vec!["/x/loadtabled".to_string(), "confine".to_string()];
        let c = for_peer_job(
            &policy("[confine]\nwrite = [\"~/work\"]\nread = [\"Photos\"]\n"),
            &cache,
            &env,
            Some(&helper),
        )
        .unwrap()
        .unwrap();
        let home = real(&t.path().join("home")).unwrap();
        assert!(c.write.contains(&home.join("work")));
        assert!(c.read.contains(&home.join("Photos")));
        let off = for_peer_job(
            &policy("[confine]\npeer_jobs = \"off\"\n"),
            &cache,
            &env,
            Some(&helper),
        )
        .unwrap();
        assert!(off.is_none());
    }

    #[test]
    fn r821_without_a_home_or_a_helper_the_job_is_refused_not_run_open() {
        let (_t, cache, env) = rig();
        assert!(for_peer_job(&policy(""), &cache, &[], None).is_err());
        if !cfg!(target_os = "macos") {
            assert!(for_peer_job(&policy(""), &cache, &env, None).is_err());
        }
    }
}
