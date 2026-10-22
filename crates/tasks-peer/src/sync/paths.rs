//! Path and file helpers shared by sync and output fetch.

use crate::ids::Hash;
use crate::proto::MAX_PATH_BYTES;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Checks a relative path for sending or receiving: non-empty, relative, no
/// `..`, `.`, empty or NUL components, no backslash, no `.git` component, bounded.
pub fn check_rel_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("empty path".into());
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(format!("path is longer than {MAX_PATH_BYTES} bytes"));
    }
    if path.starts_with('/') {
        return Err("absolute path".into());
    }
    if path.contains('\0') || path.contains('\\') {
        return Err("path contains a NUL or backslash".into());
    }
    for c in path.split('/') {
        match c {
            "" | "." | ".." => return Err(format!("path component {c:?} is not allowed")),
            ".git" => return Err("paths inside .git are not synced".into()),
            _ => {}
        }
    }
    Ok(())
}

/// Hashes a file by streaming it. Returns the hash and the bytes read.
pub fn hash_file(path: &Path) -> std::io::Result<(Hash, u64)> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(buf.get(..k).unwrap_or(&[]));
        n += k as u64;
    }
    Ok((Hash::from_digest(h), n))
}

/// Joins a checked relative path onto a root.
pub fn join(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for c in rel.split('/') {
        p.push(c);
    }
    p
}

/// Total size in bytes of the files under `dir` (symlinks are not followed). A missing
/// directory is 0.
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r158_unsafe_paths_are_refused_with_a_reason() {
        assert!(check_rel_path("src/main.rs").is_ok());
        assert!(check_rel_path("tests/fixtures/run.log").is_ok());
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "a\\b",
            ".git/config",
            "a/.git/x",
        ] {
            assert!(check_rel_path(bad).is_err(), "{bad:?}");
        }
        assert!(check_rel_path(&"a/".repeat(600)).is_err());
    }

    #[test]
    fn hash_file_streams_and_counts() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f");
        std::fs::write(&p, b"abc").unwrap();
        let (h, n) = hash_file(&p).unwrap();
        assert_eq!((h, n), (Hash::of(b"abc"), 3));
    }

    #[test]
    fn dir_size_sums_files_and_tolerates_a_missing_directory() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/x"), b"12345").unwrap();
        std::fs::write(d.path().join("a/b/y"), b"123").unwrap();
        assert_eq!(dir_size(d.path()), 8);
        assert_eq!(dir_size(&d.path().join("nope")), 0);
    }
}
