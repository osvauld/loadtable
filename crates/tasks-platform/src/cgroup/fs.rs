//! The file operations a cgroup tree needs, behind a trait so tests can use a plain directory
//! that behaves like the parts of cgroupfs we rely on.

use std::io;
use std::path::{Path, PathBuf};

/// Reads and writes cgroup control files and makes or removes cgroup directories.
pub trait CgroupFs: Send + Sync {
    /// Reads a control file.
    fn read(&self, path: &Path) -> io::Result<String>;
    /// Writes a command or value to an existing control file (never creates one).
    fn write(&self, path: &Path, text: &str) -> io::Result<()>;
    /// Creates a cgroup directory; an existing one is fine.
    fn mkdir(&self, path: &Path) -> io::Result<()>;
    /// Removes an empty cgroup directory. `NotFound` when it is already gone.
    fn rmdir(&self, path: &Path) -> io::Result<()>;
    /// The sub-directory names (child cgroups) of a cgroup.
    fn subdirs(&self, path: &Path) -> io::Result<Vec<String>>;
}

/// The real file system. cgroupfs control files are written with `O_WRONLY` and no
/// create, and a cgroup is removed with plain `rmdir`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

impl CgroupFs for RealFs {
    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
    fn write(&self, path: &Path, text: &str) -> io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
        f.write_all(text.as_bytes())
    }
    fn mkdir(&self, path: &Path) -> io::Result<()> {
        match std::fs::create_dir(path) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            other => other,
        }
    }
    fn rmdir(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }
    fn subdirs(&self, path: &Path) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(path)? {
            let e = e?;
            if e.file_type()?.is_dir()
                && let Some(n) = e.file_name().to_str()
            {
                out.push(n.to_string());
            }
        }
        out.sort();
        Ok(out)
    }
}

/// Joins `dir` and a control file name.
pub(crate) fn file(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

#[cfg(test)]
pub(crate) mod fake {
    //! A directory that stands in for cgroupfs: `mkdir` creates the control files a kernel
    //! would, `rmdir` removes them (refusing a populated or parent cgroup), writes append (so
    //! every command written to `cgroup.subtree_control` and `cgroup.procs` can be read back)
    //! and every write is logged in order.

    use super::*;
    use std::sync::{Arc, Mutex};

    /// The control files a new cgroup directory gets.
    pub(crate) const FILES: [&str; 10] = [
        "cgroup.procs",
        "cgroup.controllers",
        "cgroup.subtree_control",
        "cgroup.kill",
        "io.stat",
        "io.pressure",
        "memory.peak",
        "memory.pressure",
        "cpu.stat",
        "cpu.pressure",
    ];

    #[derive(Debug, Clone, Default)]
    pub(crate) struct DirFs {
        /// `path: text` of every write, in order.
        pub(crate) log: Arc<Mutex<Vec<String>>>,
    }

    impl CgroupFs for DirFs {
        fn read(&self, path: &Path) -> io::Result<String> {
            std::fs::read_to_string(path)
        }
        fn write(&self, path: &Path, text: &str) -> io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
            writeln!(f, "{text}")?;
            if let Ok(mut l) = self.log.lock() {
                l.push(format!("{}: {text}", path.display()));
            }
            Ok(())
        }
        fn mkdir(&self, path: &Path) -> io::Result<()> {
            if path.is_dir() {
                return Ok(());
            }
            std::fs::create_dir(path)?;
            for f in FILES {
                // a fresh kernel cgroup reports zeros, never empty statistics
                let initial = match f {
                    "io.pressure" | "memory.pressure" => {
                        "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
                    }
                    "cpu.pressure" => "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
                    "cpu.stat" => "usage_usec 0\n",
                    "memory.peak" => "0\n",
                    _ => "",
                };
                std::fs::write(path.join(f), initial)?;
            }
            Ok(())
        }
        fn rmdir(&self, path: &Path) -> io::Result<()> {
            if !self.subdirs(path)?.is_empty() {
                return Err(io::Error::from(io::ErrorKind::ResourceBusy));
            }
            if !std::fs::read_to_string(path.join("cgroup.procs"))?
                .trim()
                .is_empty()
            {
                return Err(io::Error::from(io::ErrorKind::ResourceBusy));
            }
            std::fs::remove_dir_all(path)
        }
        fn subdirs(&self, path: &Path) -> io::Result<Vec<String>> {
            RealFs.subdirs(path)
        }
    }
}
