//! Faithful Rust port of the stable-checkout prune rules of `scripts/offload_agent.py`, which in
//! turn uses `scripts/worker_source.py`'s path rules. The constants below MUST equal the Python
//! ones (`tests/sync_checkout.rs` compares them by running Python, and runs both implementations
//! on one fixture). Change both sides together.
//!
//! A path in a stable checkout is *prunable* (may be deleted when absent from the manifest) only
//! if worker_source would accept it as source: not one of the preserved cache-output link names,
//! accepted by `check_path`, and not an output directory beside a build marker. Everything else
//! (`target/`, `android/build`, marker-adjacent `target`/`build`, credentials, caches, logs, ...)
//! is kept and never descended. Symlinks are never followed: a link is a leaf entry.

use std::collections::BTreeSet;
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::source::{SourceError, open_dir_at, read_dir_names, stat_at};

/// `worker_source.FORBIDDEN_PARTS`: denied as ANY path component (lowercased).
pub const FORBIDDEN_PARTS: &[&str] = &[
    ".git",
    ".hg",
    ".jj",
    ".svn",
    ".gradle",
    ".kotlin",
    ".cxx",
    "__pycache__",
    "node_modules",
    ".cache",
    ".android",
    ".avd",
    ".ssh",
    ".aws",
    ".gnupg",
    ".config",
    ".local",
    "credentials",
    "secrets",
    "captures",
    ".cargo",
    ".rustup",
    "local.properties",
    "gradle.properties",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".docker",
    ".kube",
];
/// Component prefixes denied anywhere (`p.startswith(('.env', 'id_rsa', ...))`).
pub const FORBIDDEN_PREFIXES: &[&str] = &[".env", "id_rsa", "id_ed25519", "id_ecdsa"];
/// `worker_source.TOP_LEVEL_OUTPUT_PARTS`: denied as the FIRST component.
pub const TOP_LEVEL_OUTPUT_PARTS: &[&str] = &[
    "target", "build", "dist", "out", "outputs", "sdk", "ndk", "cache", "caches",
];
/// `worker_source.MARKED_OUTPUT_DIRS`: output dir name -> markers in the same parent directory.
pub const MARKED_OUTPUT_DIRS: &[(&str, &[&str])] = &[
    (
        "build",
        &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
    ),
    ("target", &["Cargo.toml"]),
];
/// `worker_source.FORBIDDEN_SUFFIXES` (last component, lowercased).
pub const FORBIDDEN_SUFFIXES: &[&str] = &[
    ".pem",
    ".key",
    ".jks",
    ".keystore",
    ".p12",
    ".pfx",
    ".pyc",
    ".sqlite",
    ".sqlite3",
    ".db",
    ".log",
    ".sqlite-wal",
    ".sqlite-shm",
    ".sqlite3-wal",
    ".sqlite3-shm",
    ".db-wal",
    ".db-shm",
];
/// Exact last-component names denied (lowercased).
pub const FORBIDDEN_NAMES: &[&str] = &[
    "credentials.json",
    "credentials.toml",
    "token",
    "tokens.json",
];
/// `offload_agent.PRESERVED`: tasks' cache-output link names, kept on prune.
pub const PRESERVED: &[&str] = &["target", "android/build", "android/app/build"];
/// `worker_source.Limits().max_depth` (the agent uses default limits).
const MAX_DEPTH: usize = 32;

/// `worker_source._path` with default limits: Ok(parts) for an acceptable source path.
pub fn source_path(value: &str) -> Result<Vec<&str>, SourceError> {
    let parts: Vec<&str> = value.split('/').collect();
    if value.is_empty()
        || value.len() > 1024
        || parts.len() > MAX_DEPTH
        || value.contains('\\')
        || value.chars().any(|c| (c as u32) < 32 || c as u32 == 127)
        || parts.iter().any(|p| matches!(*p, "" | "." | ".."))
    {
        return Err(SourceError::UnsafePath(format!(
            "invalid, escaping or oversized source path {value:?}"
        )));
    }
    let lower: Vec<String> = parts.iter().map(|p| p.to_lowercase()).collect();
    let last = lower.last().expect("non-empty");
    let excluded = lower.iter().any(|p| {
        FORBIDDEN_PARTS.contains(&p.as_str()) || FORBIDDEN_PREFIXES.iter().any(|x| p.starts_with(x))
    }) || TOP_LEVEL_OUTPUT_PARTS.contains(&lower[0].as_str())
        || FORBIDDEN_SUFFIXES.iter().any(|s| last.ends_with(s))
        || FORBIDDEN_NAMES.contains(&last.as_str());
    if excluded {
        return Err(SourceError::UnsafePath(format!(
            "excluded credential/cache/output path {value:?}"
        )));
    }
    Ok(parts)
}

/// `worker_source._marked_output`: `name` is an output dir beside a build marker (stat only,
/// no follow). Any error other than ENOENT refuses, as in Python.
pub fn is_marked_output(parent: RawFd, name: &str) -> Result<bool, SourceError> {
    let lower = name.to_lowercase();
    let Some((_, markers)) = MARKED_OUTPUT_DIRS.iter().find(|(n, _)| *n == lower) else {
        return Ok(false);
    };
    for marker in *markers {
        let c = CString::new(*marker).expect("static marker");
        match stat_at(parent, &c) {
            Ok(_) => return Ok(true),
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => continue,
            Err(e) => {
                return Err(SourceError::Io(format!(
                    "cannot establish build-marker absence for {name:?}; refusing: {e}"
                )));
            }
        }
    }
    Ok(false)
}

/// `offload_agent._prunable`.
pub fn prunable(rel: &str, parent: RawFd, name: &str) -> Result<bool, SourceError> {
    if PRESERVED.contains(&rel) || source_path(rel).is_err() {
        return Ok(false);
    }
    Ok(!is_marked_output(parent, name)?)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct PruneReport {
    /// Sorted; directories carry a trailing `/` (as the Python agent reports them).
    pub pruned: Vec<String>,
    /// Entries kept because they are not source (outputs, excluded names); not descended.
    pub kept_non_source: usize,
}

fn is_dir(mode: libc::mode_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFDIR
}

/// `offload_agent.apply`'s prune pass over the open checkout `root`: deletes entries absent from
/// `keep` (relative file paths) that are prunable, top-down, never descending into kept
/// directories and never following symlinks; then removes emptied non-needed directories,
/// deepest first.
pub fn prune(root: &OwnedFd, keep: &BTreeSet<String>) -> Result<PruneReport, SourceError> {
    let mut needed_dirs = BTreeSet::new();
    for p in keep {
        let parts: Vec<&str> = p.split('/').collect();
        for i in 1..parts.len() {
            needed_dirs.insert(parts[..i].join("/"));
        }
    }
    let mut report = PruneReport::default();
    let mut candidates = Vec::new();
    // Explicit stack of (relative dir, its fd); top-down like os.fwalk(topdown=True).
    let mut stack: Vec<(String, OwnedFd)> =
        vec![(String::new(), root.try_clone().map_err(io("dup root"))?)];
    while let Some((base, dir)) = stack.pop() {
        let names = read_dir_names(dir.as_raw_fd()).map_err(io(&base))?;
        for cname in names {
            let Ok(name) = cname.to_str().map(str::to_owned) else {
                // Non-UTF-8 names fail worker_source's UTF-8 rule: never source, kept.
                report.kept_non_source += 1;
                continue;
            };
            let rel = if base.is_empty() {
                name.clone()
            } else {
                format!("{base}/{name}")
            };
            let st = match stat_at(dir.as_raw_fd(), &cname) {
                Ok(st) => st,
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => continue,
                Err(e) => return Err(SourceError::Io(format!("stat {rel:?}: {e}"))),
            };
            if is_dir(st.st_mode) {
                if !prunable(&rel, dir.as_raw_fd(), &name)? {
                    report.kept_non_source += 1;
                    continue;
                }
                let child = open_dir_at(dir.as_raw_fd(), &cname).map_err(io(&rel))?;
                if !needed_dirs.contains(&rel) {
                    candidates.push(rel.clone());
                }
                stack.push((rel, child));
            } else {
                // Regular files, symlinks (never followed) and special files are leaves.
                if keep.contains(&rel) {
                    continue;
                }
                if !prunable(&rel, dir.as_raw_fd(), &name)? {
                    report.kept_non_source += 1;
                    continue;
                }
                unlink_at(dir.as_raw_fd(), &cname, 0).map_err(io(&rel))?;
                report.pruned.push(rel);
            }
        }
    }
    candidates.sort_by_key(|r| std::cmp::Reverse(r.matches('/').count()));
    for rel in candidates {
        let c = CString::new(rel.as_bytes()).map_err(|_| SourceError::UnsafePath(rel.clone()))?;
        // Relative multi-component path under the root fd: every component was reached
        // no-follow during the walk; rmdir itself never follows a final symlink. Fails (kept)
        // while the directory still holds kept non-source content.
        if unlink_at(root.as_raw_fd(), &c, libc::AT_REMOVEDIR).is_ok() {
            report.pruned.push(format!("{rel}/"));
        }
    }
    report.pruned.sort();
    Ok(report)
}

/// Convenience for tests: prune a directory given by path (the directory itself must not be a
/// symlink).
pub fn prune_dir(dir: &Path, keep: &BTreeSet<String>) -> Result<PruneReport, SourceError> {
    let parent = dir.parent().unwrap_or(Path::new("/"));
    let parent = crate::source::open_root(parent)?;
    let name = CString::new(dir.file_name().unwrap_or_default().as_bytes())
        .map_err(|_| SourceError::UnsafePath(dir.display().to_string()))?;
    let root = open_dir_at(parent.as_raw_fd(), &name).map_err(io(&dir.display().to_string()))?;
    prune(&root, keep)
}

fn unlink_at(dir: RawFd, name: &CStr, flags: libc::c_int) -> std::io::Result<()> {
    // SAFETY: valid NUL-terminated name.
    let ret = unsafe { libc::unlinkat(dir, name.as_ptr(), flags) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn io(context: &str) -> impl Fn(std::io::Error) -> SourceError + '_ {
    move |e| SourceError::Io(format!("{context}: {e}"))
}
