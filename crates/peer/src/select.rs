//! Sender-side source selection, ported from `scripts/tasks_offload.py` (`build_manifest`) so a
//! forwarded job sees the same files as an offloaded one:
//!
//! * candidates: `git ls-files -z -c -o --exclude-standard` in the Git top level (tracked and
//!   untracked-but-not-ignored files; `.gitignore`d files such as local env files are never
//!   sent);
//! * worker_source path rules ([`crate::prune::source_path`]): credential/cache/output names are
//!   EXCLUDED (reported, not an error), invalid/escaping paths are errors;
//! * no-follow descent; a directory beside a build marker (`target` next to `Cargo.toml`,
//!   `build` next to `build.gradle*`) is an excluded output;
//! * tracked-but-deleted files are counted and skipped; nested repositories/submodules,
//!   special files and in-tree symlinks are refused (the manifest carries regular files only);
//! * the checkout id is `tasks-offload`'s: `sha256(hostname NUL toplevel)[:16]`, so a forwarded
//!   job reuses the worker's warm checkout of the same laptop path.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use iroh_blobs::Hash;
use sha2::{Digest, Sha256};

use crate::prune;
use crate::source::{
    Limits, Manifest, ManifestFile, cstr, open_dir_at, open_file_at, open_root, stat_at,
    validate_manifest,
};

/// `tasks_offload.checkout_id`.
pub fn checkout_id(hostname: &str, top: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(hostname.as_bytes());
    digest.update([0u8]);
    digest.update(top.as_bytes());
    crate::hex(&digest.finalize())[..16].to_owned()
}

/// `/proc/sys/kernel/hostname` (what `socket.gethostname()` returns on Linux).
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_owned())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

pub fn git_toplevel(cwd: &Path) -> Result<PathBuf, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{} is not inside a git checkout (git rev-parse --show-toplevel failed)",
            cwd.display()
        ));
    }
    let text = String::from_utf8(out.stdout).map_err(|_| "non-UTF-8 checkout path".to_string())?;
    std::fs::canonicalize(text.trim_end_matches('\n')).map_err(|e| e.to_string())
}

fn git_paths(top: &Path) -> Result<Vec<Vec<u8>>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(top)
        .args(["ls-files", "-z", "-c", "-o", "--exclude-standard"])
        .output()
        .map_err(|e| format!("git ls-files: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut paths: Vec<Vec<u8>> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[derive(Debug, Clone)]
pub struct Selection {
    pub manifest: Manifest,
    /// Paths excluded by the credential/cache/output rules (first 200).
    pub excluded: Vec<String>,
    pub excluded_count: usize,
    /// Tracked files missing from the worktree (skipped, not deleted remotely by name).
    pub deleted_tracked: usize,
    pub bytes: u64,
}

enum Parent {
    Open(std::os::fd::OwnedFd),
    Excluded,
    Missing,
}

fn open_parent(root: &std::os::fd::OwnedFd, parts: &[&str], path: &str) -> Result<Parent, String> {
    let mut cur = root.try_clone().map_err(|e| e.to_string())?;
    for part in &parts[..parts.len() - 1] {
        if prune::is_marked_output(cur.as_raw_fd(), part).map_err(|e| format!("{path}: {e}"))? {
            return Ok(Parent::Excluded);
        }
        let name = cstr(part).map_err(|e| e.to_string())?;
        cur = match open_dir_at(cur.as_raw_fd(), &name) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(Parent::Missing),
            Err(e) => {
                return Err(format!(
                    "{path}: parent is a symlink or not a directory ({e})"
                ));
            }
        };
    }
    Ok(Parent::Open(cur))
}

/// Hashes the permitted source set of the checkout at `top` (a canonical Git top level).
pub fn select(top: &Path, limits: &Limits) -> Result<Selection, String> {
    let root = open_root(top).map_err(|e| e.to_string())?;
    let mut files = Vec::new();
    let mut excluded = Vec::new();
    let mut excluded_count = 0usize;
    let mut deleted_tracked = 0usize;
    let mut total = 0u64;
    for raw in git_paths(top)? {
        let path = String::from_utf8(raw.clone()).map_err(|_| {
            format!(
                "{:?}: non-UTF-8 path unsupported",
                String::from_utf8_lossy(&raw)
            )
        })?;
        if path.ends_with('/') {
            return Err(format!(
                "{path}: nested git repository or submodule; unsupported"
            ));
        }
        let parts = match prune::source_path(&path) {
            Ok(parts) => parts,
            Err(e) if e.to_string().contains("excluded") => {
                excluded_count += 1;
                if excluded.len() < 200 {
                    excluded.push(path);
                }
                continue;
            }
            Err(e) => return Err(format!("{path}: {e}")),
        };
        let parent = match open_parent(&root, &parts, &path)? {
            Parent::Open(fd) => fd,
            Parent::Excluded => {
                excluded_count += 1;
                if excluded.len() < 200 {
                    excluded.push(path);
                }
                continue;
            }
            Parent::Missing => {
                deleted_tracked += 1;
                continue;
            }
        };
        let last = cstr(parts[parts.len() - 1]).map_err(|e| e.to_string())?;
        let st = match stat_at(parent.as_raw_fd(), &last) {
            Ok(st) => st,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                deleted_tracked += 1;
                continue;
            }
            Err(e) => return Err(format!("{path}: {e}")),
        };
        match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => {}
            libc::S_IFLNK => {
                return Err(format!(
                    "{path}: in-tree symlinks are not supported by the peer transport (its manifest \
                     carries regular files only); use tasks-offload --transport ssh"
                ));
            }
            libc::S_IFDIR => {
                return Err(format!(
                    "{path}: is a directory (git submodule or nested repository); unsupported"
                ));
            }
            _ => return Err(format!("{path}: special file unsupported")),
        }
        if st.st_size as u64 > limits.max_file_bytes {
            return Err(format!(
                "{path}: file exceeds {} bytes",
                limits.max_file_bytes
            ));
        }
        let mut file =
            open_file_at(parent.as_raw_fd(), &last).map_err(|e| format!("{path}: {e}"))?;
        let mut content = Vec::with_capacity(st.st_size as usize);
        use std::io::Read;
        (&mut file)
            .take(limits.max_file_bytes + 1)
            .read_to_end(&mut content)
            .map_err(|e| format!("{path}: {e}"))?;
        let after = file.metadata().map_err(|e| format!("{path}: {e}"))?;
        if content.len() as u64 != after.len() {
            return Err(format!(
                "{path}: changed while hashing; keep the checkout stable and retry"
            ));
        }
        use std::os::unix::fs::PermissionsExt;
        total += content.len() as u64;
        files.push(ManifestFile {
            path,
            size: content.len() as u64,
            executable: after.permissions().mode() & 0o111 != 0,
            sha256: crate::sha256_hex(&content),
            iroh_blobs_hash: Some(Hash::new(&content).to_hex()),
        });
        if files.len() > limits.max_files {
            return Err(format!("source set exceeds {} files", limits.max_files));
        }
        if total > limits.max_bytes {
            return Err(format!("source set exceeds {} bytes", limits.max_bytes));
        }
    }
    files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let manifest = Manifest::from_files_unchecked(files);
    validate_manifest(&manifest, limits).map_err(|e| format!("manifest: {e}"))?;
    Ok(Selection {
        manifest,
        excluded,
        excluded_count,
        deleted_tracked,
        bytes: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_id_matches_tasks_offload() {
        // python3 -c "import hashlib;print(hashlib.sha256(b'kurukkan\0/home/u/repo').hexdigest()[:16])"
        let mut d = Sha256::new();
        d.update(b"kurukkan\0/home/u/repo");
        let expected = crate::hex(&d.finalize())[..16].to_owned();
        assert_eq!(checkout_id("kurukkan", "/home/u/repo"), expected);
        assert_eq!(checkout_id("a", "/b").len(), 16);
    }
}
