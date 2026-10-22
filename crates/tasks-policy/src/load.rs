//! Finding and reading the policy file. All I/O sits behind [`PolicyFs`].

use crate::error::PolicyError;
use crate::model::Policy;
use crate::validate::parse;
use std::io;
use std::path::{Path, PathBuf};

/// Policy file location relative to the repo root.
pub const POLICY_PATH: &str = ".tasks/run-policy.toml";

/// The file system operations loading needs.
pub trait PolicyFs {
    /// Reads a file as text.
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    /// True if the path exists (file or directory).
    fn exists(&self, path: &Path) -> bool;
}

/// The real file system.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealFs;

impl PolicyFs for RealFs {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

/// Walks up from `cwd` to the repo root (the first directory holding `.git`) looking for the
/// policy file. The root itself is checked; nothing above it is.
pub fn find_policy(fs: &dyn PolicyFs, cwd: &Path) -> Result<PathBuf, PolicyError> {
    for dir in cwd.ancestors() {
        let candidate = dir.join(POLICY_PATH);
        if fs.exists(&candidate) {
            return Ok(candidate);
        }
        if fs.exists(&dir.join(".git")) {
            break;
        }
    }
    Err(PolicyError::NotFound(cwd.to_path_buf()))
}

/// A loaded policy with its source text, which the suggester needs to build a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// Where it was read from.
    pub file: PathBuf,
    /// The raw text.
    pub text: String,
    /// The validated policy.
    pub policy: Policy,
}

/// Finds, reads, parses and validates the policy for `cwd`.
pub fn load(fs: &dyn PolicyFs, cwd: &Path) -> Result<Loaded, PolicyError> {
    let file = find_policy(fs, cwd)?;
    let text = fs.read_to_string(&file).map_err(|e| PolicyError::Read {
        file: file.clone(),
        message: e.to_string(),
    })?;
    let policy = parse(&text, &file)?;
    Ok(Loaded { file, text, policy })
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::BTreeMap;

    /// In-memory file system: files and directories.
    #[derive(Default)]
    pub struct FakeFs {
        pub files: BTreeMap<PathBuf, String>,
        pub dirs: Vec<PathBuf>,
    }
    impl PolicyFs for FakeFs {
        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing"))
        }
        fn exists(&self, path: &Path) -> bool {
            self.files.contains_key(path) || self.dirs.iter().any(|d| d == path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeFs;
    use super::*;

    const OK: &str =
        "version = 1\n[suites.s]\n[[suites.s.steps]]\nname=\"a\"\ncommand=[\"true\"]\n";

    #[test]
    fn walks_up_to_root() {
        let mut fs = FakeFs::default();
        fs.files
            .insert("/r/.tasks/run-policy.toml".into(), OK.into());
        fs.dirs.push("/r/.git".into());
        let l = load(&fs, Path::new("/r/crates/x/src")).unwrap();
        assert_eq!(l.file, Path::new("/r/.tasks/run-policy.toml"));
        assert_eq!(l.policy.suites.len(), 1);
    }

    #[test]
    fn stops_at_repo_root() {
        let mut fs = FakeFs::default();
        fs.files.insert("/.tasks/run-policy.toml".into(), OK.into());
        fs.dirs.push("/r/.git".into());
        assert!(matches!(
            load(&fs, Path::new("/r/a")),
            Err(PolicyError::NotFound(_))
        ));
    }

    #[test]
    fn invalid_file_reports() {
        let mut fs = FakeFs::default();
        fs.files.insert(
            "/r/.tasks/run-policy.toml".into(),
            "version = 1\nx = 1\n".into(),
        );
        assert!(matches!(
            load(&fs, Path::new("/r")),
            Err(PolicyError::Parse { .. })
        ));
    }
}
