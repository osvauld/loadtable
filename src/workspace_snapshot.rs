//! Bounded full Rust/Android source capsules. Not an atomic filesystem snapshot,
//! hermetic toolchain capture, or sandbox. Output/cache paths are excluded only
//! during capture; adding any of them to a sealed capsule invalidates verification.
use crate::{
    Result,
    snapshot::{Snapshot, validate_revision},
};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

const MAX_FILES: usize = 5_000;
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;
const ROOTS: [&str; 3] = ["crates", "android", "scripts"];
const REQUIRED: [&str; 7] = [
    "Cargo.toml",
    "Cargo.lock",
    "android/gradlew",
    "android/settings.gradle.kts",
    "android/app/build.gradle.kts",
    "android/gradle/wrapper/gradle-wrapper.jar",
    "scripts/build_libaom.py",
];

#[derive(Debug, PartialEq)]
struct Entry {
    path: String,
    executable: bool,
    bytes: Vec<u8>,
}

fn excluded(path: &Path) -> bool {
    let name = path.to_string_lossy();
    [
        "android/build",
        "android/app/build",
        "android/.gradle",
        "android/.kotlin",
        "android/app/.cxx",
        "android/local.properties",
    ]
    .iter()
    .any(|prefix| path == Path::new(prefix) || path.starts_with(prefix))
        || path
            .components()
            .any(|part| matches!(part.as_os_str().to_str(), Some("target" | "__pycache__")))
        || name.ends_with(".pyc")
}
fn directory(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(format!("source directory is not real: {}", path.display()).into());
    }
    Ok(())
}
fn collect(root: &Path, exact: bool) -> Result<Vec<Entry>> {
    directory(root)?;
    match fs::symlink_metadata(root.join(".cargo")) {
        Ok(_) => {
            return Err(
                "workspace .cargo configuration requires a separate environment/input review"
                    .into(),
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let mut pending = Vec::new();
    for name in ROOTS {
        directory(&root.join(name))?;
        pending.push((PathBuf::from(name), 0));
    }
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain",
        "rust-toolchain.toml",
    ] {
        match fs::symlink_metadata(root.join(name)) {
            Ok(_) => pending.push((PathBuf::from(name), 0)),
            Err(error)
                if !REQUIRED.contains(&name) && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if exact {
        for item in fs::read_dir(root)? {
            let name = item?.file_name();
            if !matches!(
                name.to_str(),
                Some(
                    "Cargo.toml"
                        | "Cargo.lock"
                        | "rust-toolchain"
                        | "rust-toolchain.toml"
                        | "crates"
                        | "android"
                        | "scripts"
                )
            ) {
                return Err("unexpected capsule root entry".into());
            }
        }
    }
    let mut files = Vec::new();
    let mut bytes = 0usize;
    let mut entries = 0usize;
    while let Some((relative, depth)) = pending.pop() {
        entries += 1;
        if entries > MAX_ENTRIES || depth > 64 {
            return Err("workspace traversal limit exceeded".into());
        }
        if excluded(&relative) {
            if exact {
                return Err(
                    format!("generated/local path in capsule: {}", relative.display()).into(),
                );
            }
            continue;
        }
        let path = root.join(&relative);
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            for child in fs::read_dir(&path)? {
                if pending.len() + entries >= MAX_ENTRIES {
                    return Err("workspace traversal limit exceeded".into());
                }
                pending.push((relative.join(child?.file_name()), depth + 1));
            }
        } else if meta.is_file() {
            let text = relative
                .to_str()
                .ok_or("workspace paths must be UTF-8")?
                .to_owned();
            if text.ends_with(".jks")
                || text.ends_with(".keystore")
                || relative
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(".env"))
            {
                return Err("private signing/environment files must not be captured".into());
            }
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)?;
            let opened = file.metadata()?;
            if !opened.is_file() || opened.len() > MAX_FILE_BYTES {
                return Err("unsupported/oversized source input".into());
            }
            let mut content = Vec::new();
            (&mut file)
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut content)?;
            bytes = bytes
                .checked_add(content.len())
                .ok_or("workspace size overflow")?;
            if content.len() as u64 > MAX_FILE_BYTES
                || bytes > MAX_BYTES
                || files.len() >= MAX_FILES
            {
                return Err("workspace file/byte limit exceeded".into());
            }
            files.push(Entry {
                path: text,
                executable: opened.permissions().mode() & 0o111 != 0,
                bytes: content,
            });
        } else {
            return Err(format!("symlink/special source input: {}", relative.display()).into());
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    for required in REQUIRED {
        if !files.iter().any(|file| file.path == required) {
            return Err(format!("missing workspace input: {required}").into());
        }
    }
    manifests(&files)?;
    Ok(files)
}
fn normalized(base: &Path, relative: &str) -> Result<PathBuf> {
    let mut result = base.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => (),
            Component::ParentDir if result.pop() => (),
            _ => return Err("manifest path escapes captured workspace".into()),
        }
    }
    Ok(result)
}
fn manifests(files: &[Entry]) -> Result<()> {
    for file in files
        .iter()
        .filter(|file| file.path == "Cargo.toml" || file.path.ends_with("/Cargo.toml"))
    {
        let manifest: toml::Value = toml::from_str(std::str::from_utf8(&file.bytes)?)?;
        let base = Path::new(&file.path)
            .parent()
            .ok_or("manifest parent missing")?;
        check_paths(&manifest, base, files)?;
        if file.path == "Cargo.toml" {
            let members = manifest
                .get("workspace")
                .and_then(|w| w.get("members"))
                .and_then(|m| m.as_array())
                .ok_or("explicit workspace members required")?;
            for member in members {
                let member = normalized(
                    base,
                    member.as_str().ok_or("workspace member must be a path")?,
                )?;
                if !files
                    .iter()
                    .any(|f| Path::new(&f.path) == member.join("Cargo.toml"))
                {
                    return Err(
                        "workspace member manifest missing from capsule (globs unsupported)".into(),
                    );
                }
            }
        }
    }
    Ok(())
}
fn check_paths(value: &toml::Value, base: &Path, files: &[Entry]) -> Result<()> {
    match value {
        toml::Value::Table(table) => {
            if let Some(path) = table.get("path").and_then(toml::Value::as_str) {
                let resolved = normalized(base, path)?;
                if !files.iter().any(|file| {
                    Path::new(&file.path) == resolved
                        || Path::new(&file.path).starts_with(&resolved)
                }) {
                    return Err(
                        format!("manifest path not in capsule: {}", resolved.display()).into(),
                    );
                }
            }
            for child in table.values() {
                check_paths(child, base, files)?;
            }
        }
        toml::Value::Array(array) => {
            for child in array {
                check_paths(child, base, files)?;
            }
        }
        _ => (),
    }
    Ok(())
}
fn hash(files: &[Entry]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"tinline-workspace-capsule-v1\0");
    for file in files {
        digest.update((file.path.len() as u64).to_le_bytes());
        digest.update(file.path.as_bytes());
        digest.update([u8::from(file.executable)]);
        digest.update((file.bytes.len() as u64).to_le_bytes());
        digest.update(Sha256::digest(&file.bytes));
    }
    format!("{:x}", digest.finalize())
}
fn audited(source: &Path, revision: &str) -> Result<(PathBuf, Vec<Entry>)> {
    if !source.is_absolute() {
        return Err("workspace source must be absolute".into());
    }
    directory(source)?;
    let source = fs::canonicalize(source)?;
    validate_revision(&source, revision)?;
    let first = collect(&source, false)?;
    let second = collect(&source, false)?;
    if first != second {
        return Err("workspace changed during input capture".into());
    }
    validate_revision(&source, revision)?;
    Ok((source, second))
}
fn receipt(source: &Path, revision: &str, files: &[Entry]) -> Result<Snapshot> {
    Ok(Snapshot {
        digest: hash(files),
        revision: revision.into(),
        source: source
            .to_str()
            .ok_or("workspace root must be UTF-8")?
            .into(),
        files: files.len(),
        bytes: files.iter().map(|file| file.bytes.len() as u64).sum(),
    })
}
/// Read-only census/hash of dirty and untracked source, not build authorization.
pub fn inspect(source: &Path, revision: &str) -> Result<Snapshot> {
    let (source, files) = audited(source, revision)?;
    receipt(&source, revision, &files)
}
/// Create a NEW immutable source capsule. Caller stores its receipt separately.
/// Partial new destinations may remain on failure; never remove existing data.
pub fn capture(source: &Path, revision: &str, destination: &Path) -> Result<Snapshot> {
    let (source, files) = audited(source, revision)?;
    directory(
        destination
            .parent()
            .ok_or("capsule destination requires a parent")?,
    )?;
    let destination = fs::canonicalize(destination.parent().ok_or("missing parent")?)?.join(
        destination
            .file_name()
            .ok_or("destination must name a new directory")?,
    );
    if destination.starts_with(&source) {
        return Err("capsule destination must be outside source checkout".into());
    }
    fs::create_dir(&destination)?;
    for file in &files {
        let path = destination.join(&file.path);
        fs::create_dir_all(path.parent().ok_or("file parent missing")?)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        output.write_all(&file.bytes)?;
        output.sync_all()?;
        fs::set_permissions(
            &path,
            fs::Permissions::from_mode(if file.executable { 0o555 } else { 0o444 }),
        )?;
    }
    let result = receipt(&source, revision, &files)?;
    verify(&destination, &result.digest)?;
    seal(&destination)?;
    Ok(result)
}
/// Materialize a NEW writable tree. Outputs must be mapped only AFTER verification.
/// Never hardlink source files: Gradle/generated writes must not alter the capsule.
pub fn materialize(capsule: &Path, expected: &str, destination: &Path) -> Result<()> {
    verify(capsule, expected)?;
    let files = collect(capsule, true)?;
    if hash(&files) != expected {
        return Err("capsule changed before materialization".into());
    }
    directory(destination.parent().ok_or("build tree requires parent")?)?;
    let parent = fs::canonicalize(destination.parent().ok_or("missing parent")?)?;
    let destination = parent.join(destination.file_name().ok_or("missing tree name")?);
    if destination.starts_with(fs::canonicalize(capsule)?) {
        return Err("build tree must be outside the capsule".into());
    }
    fs::create_dir(&destination)?;
    for file in files {
        let path = destination.join(&file.path);
        fs::create_dir_all(path.parent().ok_or("missing parent")?)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        output.write_all(&file.bytes)?;
        output.sync_all()?;
        fs::set_permissions(
            &path,
            fs::Permissions::from_mode(if file.executable { 0o755 } else { 0o644 }),
        )?;
    }
    verify(&destination, expected)
}

/// Recheck selected source inputs in a writable build tree; controlled outputs are
/// excluded, not verified here. The lane separately pins output mappings/lease.
pub fn verify_inputs(source: &Path, expected: &str) -> Result<()> {
    let first = collect(source, false)?;
    let second = collect(source, false)?;
    if first != second || hash(&second) != expected {
        return Err("working source inputs changed".into());
    }
    Ok(())
}
fn seal(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            seal(&entry.path())?;
        }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o555))?;
    Ok(())
}
pub fn verify(source: &Path, expected: &str) -> Result<()> {
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid workspace digest".into());
    }
    let first = collect(source, true)?;
    let second = collect(source, true)?;
    if first != second || hash(&second) != expected {
        return Err("workspace capsule digest mismatch".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "workspace_snapshot_tests.rs"]
pub(crate) mod tests;
