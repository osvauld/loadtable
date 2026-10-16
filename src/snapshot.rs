//! Bounded snapshots of the trusted, standalone task-manager test workspace.
//!
//! Two content inventories detect ordinary concurrent edits, not all filesystem
//! races. Read-only permissions discourage accidents; this is neither an atomic
//! Git/filesystem snapshot nor a sandbox against a hostile process with our UID.
use crate::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

const MAX_FILES: usize = 256;
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const MAX_DEPTH: usize = 32;
const SCHEMA: &[u8] = b"tinline-task-manager-source-snapshot-v1\0";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    pub digest: String,
    pub revision: String,
    pub source: String,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    path: String,
    bytes: Vec<u8>,
    hash: [u8; 32],
}

/// Capture dirty and untracked Rust files, with the manifest and lockfile,
/// flattening tools/task-manager into a *new* destination directory.
/// Its parent must already exist. Failures may leave a partial new directory;
/// no pre-existing destination or original source data is overwritten/deleted.
pub fn capture(source: &Path, revision: &str, dest: &Path) -> Result<Snapshot> {
    if !source.is_absolute() {
        return Err("snapshot source must be an absolute Git repository root".into());
    }
    check_directory(source)?;
    let source = fs::canonicalize(source)?;
    validate_revision(&source, revision)?;
    let tools = source.join("tools");
    check_directory(&tools)?;
    let root = tools.join("task-manager");
    check_directory(&root)?;
    let first = inventory(&root, false)?;
    let second = inventory(&root, false)?;
    if first != second {
        return Err("snapshot source changed between inventories; retry capture".into());
    }
    validate_revision(&source, revision)?;
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    check_directory(parent)?;
    let parent = fs::canonicalize(parent)?;
    let name = dest
        .file_name()
        .ok_or("snapshot destination must name a new directory")?;
    let dest = parent.join(name);
    if dest.starts_with(&root) {
        return Err("snapshot destination must not be inside the selected source tree".into());
    }
    let source_text = source
        .to_str()
        .ok_or("source path must be UTF-8")?
        .to_owned();
    fs::create_dir(&dest)?; // Deliberately fails even for an existing empty directory.
    fs::create_dir(dest.join("src"))?;
    fs::create_dir(dest.join("tests"))?;
    for entry in &second {
        let path = dest.join(&entry.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(&entry.bytes)?;
        file.sync_all()?;
        make_readonly(&path)?;
    }
    let digest = inventory_digest(&second);
    verify(&dest, &digest)?;
    seal_directories(&dest)?;
    Ok(Snapshot {
        digest,
        revision: revision.to_owned(),
        source: source_text,
        files: second.len(),
        bytes: second.iter().map(|e| e.bytes.len() as u64).sum(),
    })
}

/// Verify the exact allowlisted inventory. Metadata, build outputs and all
/// non-Rust files below src/tests are rejected, not silently ignored.
/// Job metadata and Cargo target outputs belong outside this snapshot folder.
pub fn verify(dest: &Path, expected_digest: &str) -> Result<()> {
    if expected_digest.len() != 64 || !expected_digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid snapshot digest".into());
    }
    let first = inventory(dest, true)?;
    let second = inventory(dest, true)?;
    if first != second || inventory_digest(&second) != expected_digest {
        return Err("snapshot inventory digest mismatch or concurrent edit".into());
    }
    Ok(())
}

fn git(source: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()?;
    if !output.status.success() {
        return Err("cannot inspect snapshot source Git repository".into());
    }
    Ok(String::from_utf8(output.stdout)?
        .trim_end_matches(['\r', '\n'])
        .to_owned())
}

pub(crate) fn validate_revision(source: &Path, revision: &str) -> Result<()> {
    if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("revision must be a full hexadecimal Git HEAD".into());
    }
    if git(source, &["rev-parse", "HEAD"])? != revision {
        return Err("requested revision is not current source HEAD".into());
    }
    let top = git(source, &["rev-parse", "--show-toplevel"])?;
    if fs::canonicalize(top)? != source {
        return Err("snapshot source is not the Git repository root".into());
    }
    Ok(())
}

fn check_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_dir() {
        return Err(format!(
            "not a real directory (symlinks forbidden): {}",
            path.display()
        )
        .into());
    }
    Ok(())
}

fn inventory(root: &Path, exact: bool) -> Result<Vec<Entry>> {
    check_directory(root)?;
    let mut entries = Vec::new();
    let mut bytes = 0;
    let mut scanned = 0;
    // Even source-only exclusions must be absent, not merely omitted.
    for forbidden in ["build.rs", ".cargo"] {
        match fs::symlink_metadata(root.join(forbidden)) {
            Ok(_) => return Err(format!("forbidden snapshot input: {forbidden}").into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    if exact {
        for item in fs::read_dir(root)? {
            let item = item?;
            if !matches!(
                item.file_name().to_str(),
                Some("Cargo.toml" | "Cargo.lock" | "src" | "tests")
            ) {
                return Err("unexpected entry at snapshot root".into());
            }
        }
    }
    for name in ["Cargo.toml", "Cargo.lock"] {
        read_entry(root, Path::new(name), &mut entries, &mut bytes)?;
    }
    check_manifest(&entries[0].bytes)?;
    for name in ["src", "tests"] {
        check_directory(&root.join(name))?;
        let before = entries.len();
        walk(
            root,
            Path::new(name),
            exact,
            0,
            &mut scanned,
            &mut bytes,
            &mut entries,
        )?;
        if entries.len() == before {
            return Err(format!("missing Rust files in {name}").into());
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

// Parse the manifest without invoking Cargo. The source contract is closed to
// local Rust targets plus registry dependencies resolved from the captured lock.
// Dependency build scripts still run and are not a security sandbox.
fn check_manifest(bytes: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(bytes)?;
    let manifest: toml::Value = toml::from_str(text)?;
    if !manifest
        .get("workspace")
        .and_then(toml::Value::as_table)
        .is_some_and(|t| t.is_empty())
    {
        return Err("snapshot requires a structurally empty standalone workspace".into());
    }
    fn closed(value: &toml::Value) -> bool {
        match value {
            toml::Value::Table(table) => table.iter().all(|(key, value)| {
                !matches!(
                    key.as_str(),
                    "path" | "git" | "patch" | "replace" | "build" | "workspace"
                ) && closed(value)
            }),
            toml::Value::Array(array) => array.iter().all(closed),
            _ => true,
        }
    }
    // Bin/test paths must be within the captured tree; validate these separately.
    for section in [
        "dependencies",
        "dev-dependencies",
        "build-dependencies",
        "target",
    ] {
        if manifest.get(section).is_some_and(|value| !closed(value)) {
            return Err(
                "path/git/workspace dependencies are outside the captured source contract".into(),
            );
        }
    }
    for forbidden in ["patch", "replace"] {
        if manifest.get(forbidden).is_some() {
            return Err("dependency overrides are outside the snapshot contract".into());
        }
    }
    for section in ["bin", "test", "bench", "example"] {
        if let Some(entries) = manifest.get(section).and_then(toml::Value::as_array) {
            for entry in entries {
                if let Some(path) = entry.get("path").and_then(toml::Value::as_str) {
                    let path = Path::new(path);
                    if path.is_absolute()
                        || path
                            .components()
                            .any(|c| matches!(c, std::path::Component::ParentDir))
                        || path.extension().is_none_or(|e| e != "rs")
                        || !matches!(
                            path.components()
                                .next()
                                .and_then(|c| c.as_os_str().to_str()),
                            Some("src" | "tests")
                        )
                    {
                        return Err("manifest target outside captured Rust tree".into());
                    }
                }
            }
        }
    }
    if let Some(path) = manifest
        .get("lib")
        .and_then(|l| l.get("path"))
        .and_then(toml::Value::as_str)
        && (!path.starts_with("src/")
            || Path::new(path)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            || !path.ends_with(".rs"))
    {
        return Err("library target outside captured Rust tree".into());
    }
    let mut workspace = false;
    let mut section = "";
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            section = line;
            if line == "[workspace]" {
                workspace = true;
            }
            if line.starts_with("[workspace.") {
                return Err("snapshot workspace must be standalone and empty".into());
            }
        } else {
            if section == "[workspace]" {
                return Err("snapshot workspace must be standalone and empty".into());
            }
            if section == "[package]"
                && line.split('=').next().is_some_and(|key| {
                    matches!(key.trim().trim_matches(['\'', '"']), "build" | "workspace")
                })
            {
                return Err(
                    "custom package build scripts or external workspaces are not captured".into(),
                );
            }
        }
    }
    if !workspace {
        return Err("manifest requires an explicit standalone [workspace]".into());
    }
    Ok(())
}

fn walk(
    root: &Path,
    relative: &Path,
    exact: bool,
    depth: usize,
    scanned: &mut usize,
    bytes: &mut u64,
    entries: &mut Vec<Entry>,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err("snapshot directory nesting limit exceeded".into());
    }
    // Bound traversal too: ignored files/empty directories cannot cause an
    // unbounded inventory even though only Rust files count toward MAX_FILES.
    let mut children = Vec::new();
    for child in fs::read_dir(root.join(relative))? {
        *scanned += 1;
        if *scanned > MAX_ENTRIES {
            return Err("snapshot traversal limit exceeded".into());
        }
        children.push(child?);
    }
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let path = relative.join(child.file_name());
        let name = child.file_name();
        if name == ".cargo" || name == "build.rs" {
            return Err("forbidden snapshot tree entry".into());
        }
        if path.to_str().is_none_or(|p| p.len() > 4096) {
            return Err("invalid or oversized snapshot path".into());
        }
        let kind = fs::symlink_metadata(child.path())?.file_type();
        if kind.is_dir() {
            walk(root, &path, exact, depth + 1, scanned, bytes, entries)?;
        } else if !kind.is_file() {
            return Err("symlink or special file in snapshot tree".into());
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            read_entry(root, &path, entries, bytes)?;
        } else if exact {
            return Err("unexpected non-Rust file in snapshot tree".into());
        }
    }
    Ok(())
}

fn read_entry(
    root: &Path,
    relative: &Path,
    entries: &mut Vec<Entry>,
    bytes: &mut u64,
) -> Result<()> {
    if entries.len() >= MAX_FILES {
        return Err("snapshot exceeds 256 files".into());
    }
    let path = root.join(relative);
    let meta = fs::symlink_metadata(&path)?;
    if !meta.file_type().is_file() {
        return Err("snapshot input is not a regular file".into());
    }
    let remaining = MAX_BYTES - *bytes;
    if meta.len() > remaining {
        return Err("snapshot exceeds 4 MiB".into());
    }
    let mut content = Vec::new();
    File::open(&path)?
        .take(remaining + 1)
        .read_to_end(&mut content)?;
    if content.len() as u64 > remaining {
        return Err("snapshot exceeds 4 MiB".into());
    }
    if !fs::symlink_metadata(&path)?.file_type().is_file() {
        return Err("snapshot input changed file type".into());
    }
    *bytes += content.len() as u64;
    entries.push(Entry {
        path: relative
            .components()
            .map(|part| {
                part.as_os_str()
                    .to_str()
                    .ok_or("snapshot path must be UTF-8")
            })
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("/"),
        hash: Sha256::digest(&content).into(),
        bytes: content,
    });
    Ok(())
}

fn inventory_digest(entries: &[Entry]) -> String {
    let mut digest = Sha256::new();
    digest.update(SCHEMA);
    digest.update((entries.len() as u64).to_be_bytes());
    for entry in entries {
        digest.update((entry.path.len() as u64).to_be_bytes());
        digest.update(entry.path.as_bytes());
        digest.update((entry.bytes.len() as u64).to_be_bytes());
        digest.update(&entry.bytes);
    }
    format!("{:x}", digest.finalize())
}

fn make_readonly(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() & !0o222);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

fn seal_directories(path: &Path) -> Result<()> {
    for child in fs::read_dir(path)? {
        let child = child?;
        if child.file_type()?.is_dir() {
            seal_directories(&child.path())?;
        }
    }
    make_readonly(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "task-snapshot-unit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::create_dir(path.join("src")).unwrap();
            fs::create_dir(path.join("tests")).unwrap();
            fs::write(
                path.join("Cargo.toml"),
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace]\n",
            )
            .unwrap();
            fs::write(path.join("Cargo.lock"), "version = 4\n").unwrap();
            fs::write(path.join("src/lib.rs"), "// dirty source\n").unwrap();
            fs::write(path.join("tests/one.rs"), "// untracked test\n").unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn deterministic_content_inventory_detects_edits_additions_and_paths() {
        let f = Fixture::new();
        let first = inventory(&f.0, false).unwrap();
        assert_eq!(first, inventory(&f.0, false).unwrap());
        let original = inventory_digest(&first);
        fs::write(f.0.join("src/lib.rs"), "// other source\n").unwrap();
        assert_ne!(first, inventory(&f.0, false).unwrap());
        assert_ne!(original, inventory_digest(&inventory(&f.0, false).unwrap()));
        fs::write(f.0.join("tests/new.rs"), "// added\n").unwrap();
        let added = inventory_digest(&inventory(&f.0, false).unwrap());
        fs::rename(f.0.join("tests/new.rs"), f.0.join("tests/renamed.rs")).unwrap();
        assert_ne!(added, inventory_digest(&inventory(&f.0, false).unwrap()));
    }

    #[test]
    fn verification_is_exact_and_detects_tampering() {
        let f = Fixture::new();
        let digest = inventory_digest(&inventory(&f.0, true).unwrap());
        verify(&f.0, &digest).unwrap();
        fs::write(f.0.join("job.json"), "{}").unwrap();
        assert!(verify(&f.0, &digest).is_err());
        fs::remove_file(f.0.join("job.json")).unwrap();
        fs::write(f.0.join("src/notes.txt"), "ignored only in source").unwrap();
        assert!(inventory(&f.0, false).is_ok());
        assert!(verify(&f.0, &digest).is_err());
        fs::remove_file(f.0.join("src/notes.txt")).unwrap();
        fs::write(f.0.join("tests/one.rs"), "tampered").unwrap();
        assert!(verify(&f.0, &digest).is_err());
    }

    #[test]
    fn rejects_missing_oversized_and_too_many_files() {
        let f = Fixture::new();
        fs::remove_file(f.0.join("Cargo.lock")).unwrap();
        assert!(inventory(&f.0, false).is_err());
        fs::write(f.0.join("Cargo.lock"), "version = 4").unwrap();
        File::create(f.0.join("src/huge.rs"))
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(inventory(&f.0, false).is_err());
        fs::remove_file(f.0.join("src/huge.rs")).unwrap();
        // Individually admissible files must still fail the aggregate bound.
        for name in ["src/a.rs", "src/b.rs"] {
            File::create(f.0.join(name))
                .unwrap()
                .set_len(MAX_BYTES / 2)
                .unwrap();
        }
        assert!(inventory(&f.0, false).is_err());
        fs::remove_file(f.0.join("src/a.rs")).unwrap();
        fs::remove_file(f.0.join("src/b.rs")).unwrap();
        for i in 0..MAX_FILES {
            fs::write(f.0.join(format!("tests/{i}.rs")), "").unwrap();
        }
        assert!(inventory(&f.0, false).is_err());
    }

    #[test]
    fn requires_standalone_workspace_and_forbids_build_configuration() {
        let f = Fixture::new();
        for manifest in [
            "[package]\nname = \"x\"",
            "[workspace]\nmembers = [\"other\"]",
            "[workspace]\n[package]\nbuild = \"other.rs\"",
        ] {
            fs::write(f.0.join("Cargo.toml"), manifest).unwrap();
            assert!(inventory(&f.0, false).is_err());
        }
        for manifest in [
            "[workspace]\n[dependencies]\nx = { path = \"../external\" }",
            "[workspace]\n[dependencies]\nx = { git = \"https://fixture.invalid/x\" }",
            "[workspace]\n[patch.crates-io]\nx = { version = \"1\" }",
            "[workspace]\n[[bin]]\nname = \"external\"\npath = \"../outside.rs\"",
            "[workspace]\n[lib]\npath = \"src/../../outside.rs\"",
            "[workspace] #comment\nmembers = [\"outside\"]",
        ] {
            fs::write(f.0.join("Cargo.toml"), manifest.replace("\\\"", "\"")).unwrap();
            assert!(inventory(&f.0, false).is_err());
        }
        fs::write(f.0.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(f.0.join("build.rs"), "").unwrap();
        assert!(inventory(&f.0, false).is_err());
        fs::remove_file(f.0.join("build.rs")).unwrap();
        fs::create_dir(f.0.join(".cargo")).unwrap();
        assert!(inventory(&f.0, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_special_files_without_reading_them() {
        let f = Fixture::new();
        let socket = std::os::unix::net::UnixListener::bind(f.0.join("tests/socket.rs")).unwrap();
        assert!(inventory(&f.0, false).is_err());
        drop(socket);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_even_for_non_selected_files() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        symlink("lib.rs", f.0.join("src/link.txt")).unwrap();
        assert!(inventory(&f.0, false).is_err());
        fs::remove_file(f.0.join("src/link.txt")).unwrap();
        fs::remove_file(f.0.join("Cargo.lock")).unwrap();
        symlink("Cargo.toml", f.0.join("Cargo.lock")).unwrap();
        assert!(inventory(&f.0, false).is_err());
    }
}
