//! Finding and reading the policy file. All I/O sits behind [`PolicyFs`].

use crate::error::PolicyError;
use crate::model::Policy;
use crate::validate::parse;
use std::io;
use std::path::{Path, PathBuf};

/// Policy file location relative to the repo root.
pub const POLICY_PATH: &str = ".loadtable/run-policy.toml";
/// Its location before the rename to LoadTable, still read when the new one is absent (R-804).
pub const OLD_POLICY_PATH: &str = ".tasks/run-policy.toml";

/// The file system operations loading needs.
pub trait PolicyFs {
    /// Reads a file as text.
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    /// True if the path exists (file or directory).
    fn exists(&self, path: &Path) -> bool;
    /// The JSON of `cargo metadata --no-deps --format-version 1` run in `cwd`, to split a
    /// `cargo test` step into its test targets without building (R-704). A file system that
    /// cannot run cargo says so; the step then stays whole.
    fn cargo_metadata(&self, cwd: &Path) -> io::Result<String> {
        let _ = cwd;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cargo metadata is not available here",
        ))
    }
}

/// How long `cargo metadata` may take before the step is left whole. It reads manifests only;
/// more than this means cargo is installing a toolchain or waiting on a lock.
const METADATA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

fn run_cargo_metadata(cwd: &Path) -> io::Result<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // read both pipes while waiting, or a big answer fills the pipe and cargo blocks on it
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_string(&mut text);
        }
        text
    });
    let err_reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(e) = err.as_mut() {
            let _ = e.read_to_string(&mut text);
        }
        text
    });
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if started.elapsed() > METADATA_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "cargo metadata took more than {}s",
                    METADATA_TIMEOUT.as_secs()
                ),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let text = reader.join().unwrap_or_default();
    let why = err_reader.join().unwrap_or_default();
    if status.success() {
        Ok(text)
    } else {
        let first = why.lines().next().unwrap_or("no message");
        Err(io::Error::other(format!("cargo metadata failed: {first}")))
    }
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
    fn cargo_metadata(&self, cwd: &Path) -> io::Result<String> {
        run_cargo_metadata(cwd)
    }
}

/// Walks up from `cwd` to the repo root (the first directory holding `.git`) looking for the
/// policy file. The root itself is checked; nothing above it is.
pub fn find_policy(fs: &dyn PolicyFs, cwd: &Path) -> Result<PathBuf, PolicyError> {
    for dir in cwd.ancestors() {
        for rel in [POLICY_PATH, OLD_POLICY_PATH] {
            let candidate = dir.join(rel);
            if fs.exists(&candidate) {
                return Ok(candidate);
            }
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
        /// What `cargo metadata` prints; `None` makes it fail.
        pub metadata: Option<String>,
    }
    impl PolicyFs for FakeFs {
        fn cargo_metadata(&self, _cwd: &Path) -> io::Result<String> {
            self.metadata
                .clone()
                .ok_or_else(|| io::Error::other("cargo metadata failed: no manifest"))
        }
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
    fn r804_the_loadtable_policy_wins_and_the_tasks_one_is_still_read() {
        let mut fs = FakeFs::default();
        fs.files
            .insert("/r/.tasks/run-policy.toml".into(), OK.into());
        fs.dirs.push("/r/.git".into());
        let l = load(&fs, Path::new("/r/a")).unwrap();
        assert_eq!(l.file, Path::new("/r/.tasks/run-policy.toml"));
        fs.files
            .insert("/r/.loadtable/run-policy.toml".into(), OK.into());
        let l = load(&fs, Path::new("/r/a")).unwrap();
        assert_eq!(l.file, Path::new("/r/.loadtable/run-policy.toml"));
        // a nearer old file still wins over a farther new one: the nearest directory decides
        fs.files
            .insert("/r/a/.tasks/run-policy.toml".into(), OK.into());
        let l = load(&fs, Path::new("/r/a")).unwrap();
        assert_eq!(l.file, Path::new("/r/a/.tasks/run-policy.toml"));
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
