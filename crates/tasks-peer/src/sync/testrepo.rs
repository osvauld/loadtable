//! Helpers for tests that need a real git repository on disk. Not used by the daemon.
#![doc(hidden)]

use std::path::Path;
use std::process::{Command, Stdio};

/// Runs `git` in `root` with a clean environment; returns stdout.
pub fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// `git init` with a branch and no signing, in an existing directory.
pub fn init_repo(root: &Path) -> Result<(), String> {
    git(root, &["init", "-q", "-b", "main"])?;
    git(root, &["config", "commit.gpgsign", "false"])?;
    git(root, &["config", "core.autocrlf", "false"])?;
    Ok(())
}

/// Writes `body` to `rel` under `root`, creating directories.
pub fn write_file(root: &Path, rel: &str, body: impl AsRef<[u8]>) -> std::io::Result<()> {
    let p = root.join(rel);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, body)
}

/// Sets every file's mtime an hour into the past (not `.git`). A snapshot treats files edited
/// within the last second as unverified, as git does for racily clean entries; tests that
/// assert nothing is re-read age their files first.
pub fn age_all(root: &Path) -> std::io::Result<()> {
    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)?.flatten() {
            let path = e.path();
            if e.file_name() == ".git" {
                continue;
            }
            let ft = e.file_type()?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                std::fs::File::options()
                    .write(true)
                    .open(&path)?
                    .set_modified(when)?;
            }
        }
    }
    Ok(())
}

/// Sets one file's mtime an hour into the past.
pub fn age_file(root: &Path, rel: &str) -> std::io::Result<()> {
    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(root.join(rel))?
        .set_modified(when)
}

/// Ages the files, stages everything and commits it.
pub fn commit_all(root: &Path, msg: &str) -> Result<(), String> {
    age_all(root).map_err(|e| e.to_string())?;
    git(root, &["add", "-A"])?;
    git(root, &["commit", "-q", "-m", msg])?;
    Ok(())
}

/// A small repository: a few source files, committed.
pub fn sample_repo(root: &Path) -> Result<(), String> {
    init_repo(root)?;
    write_file(root, "Cargo.toml", "[package]\nname = \"demo\"\n").map_err(|e| e.to_string())?;
    write_file(root, "src/main.rs", "fn main() {}\n").map_err(|e| e.to_string())?;
    write_file(root, "tests/fixtures/run.log", "fixture log line\n").map_err(|e| e.to_string())?;
    write_file(
        root,
        ".gitignore",
        "target/\nlocal.properties\n*.secret-ignored\n",
    )
    .map_err(|e| e.to_string())?;
    commit_all(root, "initial")
}
