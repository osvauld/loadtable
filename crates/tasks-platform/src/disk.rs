//! Free-disk measurement via `statvfs`, shared by both operating systems.

use crate::PlatformError;
use std::path::{Path, PathBuf};

/// Free bytes available to unprivileged users on the filesystem holding `path`.
/// A missing path is measured on its nearest existing ancestor; nothing is
/// created. Relative paths are refused (R-068).
pub fn free_bytes(path: &Path) -> Result<u64, PlatformError> {
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(PlatformError::InvalidPath(path.to_path_buf()));
    }
    let target = nearest_existing(path);
    let st = rustix::fs::statvfs(&target).map_err(|source| PlatformError::Syscall {
        syscall: "statvfs",
        path: Some(target.clone()),
        source,
    })?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

fn nearest_existing(path: &Path) -> PathBuf {
    let mut cur = path;
    while !cur.exists() {
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
    cur.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_has_free_space() {
        assert!(free_bytes(Path::new("/")).unwrap() > 0);
    }

    // R-068
    #[test]
    fn missing_descendants_use_existing_filesystem_without_creation() {
        let missing = std::env::temp_dir().join("tasks-platform-no/such/dir");
        assert!(free_bytes(&missing).unwrap() > 0);
        assert!(!missing.exists());
    }

    // R-068
    #[test]
    fn relative_and_nul_paths_are_refused() {
        assert!(free_bytes(Path::new("relative/dir")).is_err());
        assert!(free_bytes(Path::new("/tmp/a\0b")).is_err());
    }
}
