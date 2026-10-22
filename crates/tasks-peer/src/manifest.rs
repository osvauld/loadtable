//! Checkout manifests: the pure core of source sync, plus the thin file I/O.
//!
//! The manifest is every git-visible file: tracked, modified, and untracked
//! but not ignored. Files are never dropped by extension (the old sync lost
//! `*.log` fixtures); the only exclusions are things that must not travel:
//! ignored files (git already hides them), key material by name, in-tree
//! symlinks (R-158), and files over the byte limit. Each exclusion is
//! reported, never silent.

use crate::ids::Hash;
use crate::proto::{FileEntry, MAX_ENTRIES_PER_PAGE, MAX_MANIFEST_PAGE_BYTES, MAX_PATH_BYTES};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Why a manifest could not be built.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// Listing the files failed.
    #[error("listing the checkout failed: {0}")]
    List(String),
    /// Reading a file failed.
    #[error("reading {path}: {source}")]
    Read {
        /// The path.
        path: String,
        /// The error.
        source: std::io::Error,
    },
    /// Too many files.
    #[error("{count} files is over the limit of {limit}")]
    TooManyFiles {
        /// Seen.
        count: usize,
        /// Limit.
        limit: usize,
    },
}

/// Limits for building a manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncLimits {
    /// Largest file that is synced; larger ones are reported as skipped.
    pub max_file_bytes: u64,
    /// Most files in one manifest.
    pub max_files: usize,
}

impl Default for SyncLimits {
    fn default() -> Self {
        SyncLimits {
            max_file_bytes: 256 * 1024 * 1024,
            max_files: 200_000,
        }
    }
}

/// Lists the git-visible files of a checkout, relative to its root.
pub trait FileLister {
    /// Relative paths, `/` separated. Deleted-but-tracked files may appear;
    /// the builder skips paths that no longer exist.
    fn list(&self, root: &Path) -> Result<Vec<String>, ManifestError>;
}

/// The real lister: `git ls-files -co --exclude-standard -z`.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitLister;

impl FileLister for GitLister {
    fn list(&self, root: &Path) -> Result<Vec<String>, ManifestError> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["ls-files", "-co", "--exclude-standard", "-z"])
            .output()
            .map_err(|e| ManifestError::List(format!("could not run git: {e}")))?;
        if !out.status.success() {
            return Err(ManifestError::List(format!(
                "git ls-files failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(parse_ls_files(&out.stdout))
    }
}

/// Splits NUL-separated `ls-files` output. Non-UTF-8 names are kept lossily
/// so the builder can report them as skipped instead of losing them.
pub fn parse_ls_files(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

/// A fixed list, for tests and for callers that already know the files.
#[derive(Debug, Clone, Default)]
pub struct StaticLister(pub Vec<String>);

impl FileLister for StaticLister {
    fn list(&self, _root: &Path) -> Result<Vec<String>, ManifestError> {
        Ok(self.0.clone())
    }
}

/// Why a file was left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Looks like key material or credentials (by file name).
    Secret,
    /// An in-tree symlink (R-158).
    Symlink,
    /// Not a regular file (directory, submodule, socket).
    NotRegular,
    /// Over the byte limit.
    TooLarge {
        /// Size.
        size: u64,
        /// Limit.
        limit: u64,
    },
    /// The path is not safe to send.
    BadPath(String),
    /// Unreadable.
    Unreadable(String),
}

/// A file that was not synced, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The path as listed.
    pub path: String,
    /// Why.
    pub reason: SkipReason,
}

/// The files of a checkout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Entries by path.
    pub entries: BTreeMap<String, FileEntry>,
}

impl Manifest {
    /// Number of files.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    /// True when there are no files.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// Total bytes.
    pub fn total_bytes(&self) -> u64 {
        self.entries.values().map(|e| e.size).sum()
    }
}

/// A manifest plus what was left out.
#[derive(Debug, Clone, Default)]
pub struct ManifestReport {
    /// What will be synced.
    pub manifest: Manifest,
    /// What was not, and why.
    pub skipped: Vec<Skipped>,
}

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

/// True for file names that are key material or credentials. Names only: no
/// content scan, and nothing like `.log` or `.txt` is ever matched.
pub fn is_secret(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    if lower == ".env" || lower.starts_with(".env.") {
        let templ = ["example", "sample", "template", "dist", "defaults"];
        return !templ.iter().any(|t| lower.ends_with(t));
    }
    if matches!(
        lower.as_str(),
        ".netrc"
            | ".npmrc"
            | ".pypirc"
            | ".git-credentials"
            | "id_rsa"
            | "id_dsa"
            | "id_ecdsa"
            | "id_ed25519"
            | "credentials.json"
            | "service-account.json"
    ) {
        return true;
    }
    ["pem", "key", "p12", "pfx", "jks", "keystore", "kdbx"]
        .iter()
        .any(|ext| lower.rsplit_once('.').is_some_and(|(_, e)| e == *ext))
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

/// Builds the manifest of `root` from the lister's files.
pub fn build_manifest(
    root: &Path,
    lister: &dyn FileLister,
    limits: &SyncLimits,
) -> Result<ManifestReport, ManifestError> {
    let listed: BTreeSet<String> = lister.list(root)?.into_iter().collect();
    if listed.len() > limits.max_files {
        return Err(ManifestError::TooManyFiles {
            count: listed.len(),
            limit: limits.max_files,
        });
    }
    let mut report = ManifestReport::default();
    for rel in listed {
        let mut skip = |reason| {
            // Captures only `report.skipped`, so `report.manifest` stays usable.
            report.skipped.push(Skipped {
                path: rel.clone(),
                reason,
            })
        };
        if let Err(why) = check_rel_path(&rel) {
            skip(SkipReason::BadPath(why));
            continue;
        }
        if is_secret(&rel) {
            skip(SkipReason::Secret);
            continue;
        }
        let abs = root.join(&rel);
        let meta = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            // Tracked but deleted in the working tree: not part of the checkout.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                skip(SkipReason::Unreadable(e.to_string()));
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            skip(SkipReason::Symlink);
            continue;
        }
        if !meta.is_file() {
            skip(SkipReason::NotRegular);
            continue;
        }
        if meta.len() > limits.max_file_bytes {
            skip(SkipReason::TooLarge {
                size: meta.len(),
                limit: limits.max_file_bytes,
            });
            continue;
        }
        match hash_file(&abs) {
            Ok((hash, size)) => {
                report.manifest.entries.insert(
                    rel.clone(),
                    FileEntry {
                        path: rel.clone(),
                        hash,
                        size,
                        exec: meta.permissions().mode() & 0o111 != 0,
                    },
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => skip(SkipReason::Unreadable(e.to_string())),
        }
    }
    Ok(report)
}

/// The distinct hashes in `manifest` that are not in `have`: the want list.
pub fn want(manifest: &Manifest, have: &BTreeSet<Hash>) -> Vec<Hash> {
    let mut seen = BTreeSet::new();
    manifest
        .entries
        .values()
        .map(|e| e.hash)
        .filter(|h| !have.contains(h) && seen.insert(*h))
        .collect()
}

/// Paths in `old` that `new` no longer has: deleted files to remove.
pub fn deleted(new: &Manifest, old: &Manifest) -> Vec<String> {
    old.entries
        .keys()
        .filter(|p| !new.entries.contains_key(*p))
        .cloned()
        .collect()
}

/// Splits entries into pages that each fit one manifest message (R-156): a
/// first sync of thousands of files is never one oversized frame.
pub fn pages(manifest: &Manifest) -> Vec<Vec<FileEntry>> {
    // Leave room for the envelope and the other message fields.
    let budget = MAX_MANIFEST_PAGE_BYTES - 1024;
    let mut out: Vec<Vec<FileEntry>> = Vec::new();
    let mut cur: Vec<FileEntry> = Vec::new();
    let mut used = 0usize;
    for e in manifest.entries.values() {
        // JSON overhead per entry: keys, quotes, 64-char hash, digits.
        let cost = e.path.len() * 2 + 200;
        if !cur.is_empty() && (used + cost > budget || cur.len() >= MAX_ENTRIES_PER_PAGE) {
            out.push(std::mem::take(&mut cur));
            used = 0;
        }
        used += cost;
        cur.push(e.clone());
    }
    out.push(cur);
    out
}

/// Joins a validated relative path to a root.
pub fn join(root: &Path, rel: &str) -> PathBuf {
    root.join(rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(d) = p.parent() {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    fn lister(files: &[&str]) -> StaticLister {
        StaticLister(files.iter().map(|s| (*s).to_string()).collect())
    }

    #[test]
    fn r155_log_fixtures_are_not_dropped_by_extension() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "tests/fixtures/run.log", "line\n");
        write(d.path(), "build.log", "x");
        write(d.path(), "src/a.rs", "fn a(){}");
        write(d.path(), "data.tmp", "t");
        let r = build_manifest(
            d.path(),
            &lister(&[
                "tests/fixtures/run.log",
                "build.log",
                "src/a.rs",
                "data.tmp",
            ]),
            &SyncLimits::default(),
        )
        .unwrap();
        assert_eq!(r.manifest.len(), 4, "{:?}", r.skipped);
        assert!(r.manifest.entries.contains_key("tests/fixtures/run.log"));
        assert!(r.skipped.is_empty());
    }

    #[test]
    fn r155_secrets_by_name_are_never_listed_and_are_reported() {
        let d = tempfile::tempdir().unwrap();
        for f in [
            ".env",
            ".env.local",
            "deploy/id_ed25519",
            "tls/server.pem",
            ".env.example",
            "notes.txt",
        ] {
            write(d.path(), f, "s");
        }
        let r = build_manifest(
            d.path(),
            &lister(&[
                ".env",
                ".env.local",
                "deploy/id_ed25519",
                "tls/server.pem",
                ".env.example",
                "notes.txt",
            ]),
            &SyncLimits::default(),
        )
        .unwrap();
        let kept: Vec<&str> = r.manifest.entries.keys().map(String::as_str).collect();
        assert_eq!(kept, [".env.example", "notes.txt"]);
        assert_eq!(r.skipped.len(), 4);
        assert!(r.skipped.iter().all(|s| s.reason == SkipReason::Secret));
    }

    #[test]
    fn r158_symlinks_and_unsafe_paths_are_skipped_with_reasons() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "real.txt", "r");
        std::os::unix::fs::symlink("/etc/passwd", d.path().join("link")).unwrap();
        let r = build_manifest(
            d.path(),
            &lister(&[
                "real.txt",
                "link",
                "../escape",
                "/abs",
                "a/../b",
                ".git/config",
                "gone.txt",
            ]),
            &SyncLimits::default(),
        )
        .unwrap();
        assert_eq!(r.manifest.len(), 1);
        let reason = |p: &str| {
            r.skipped
                .iter()
                .find(|s| s.path == p)
                .map(|s| s.reason.clone())
        };
        assert_eq!(reason("link"), Some(SkipReason::Symlink));
        for p in ["../escape", "/abs", "a/../b", ".git/config"] {
            assert!(matches!(reason(p), Some(SkipReason::BadPath(_))), "{p}");
        }
        // A tracked-but-deleted file is simply not part of the checkout.
        assert_eq!(reason("gone.txt"), None);
    }

    #[test]
    fn r13_oversized_files_are_reported_with_the_limit() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "big.txt", "0123456789");
        write(d.path(), "small.txt", "0");
        let lim = SyncLimits {
            max_file_bytes: 5,
            max_files: 10,
        };
        let r = build_manifest(d.path(), &lister(&["big.txt", "small.txt"]), &lim).unwrap();
        assert_eq!(r.manifest.len(), 1);
        assert_eq!(
            r.skipped[0].reason,
            SkipReason::TooLarge { size: 10, limit: 5 }
        );
    }

    #[test]
    fn exec_bit_and_hash_are_recorded() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "run.sh", "#!/bin/sh\n");
        let p = d.path().join("run.sh");
        let mut perm = fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&p, perm).unwrap();
        write(d.path(), "plain.txt", "#!/bin/sh\n");
        let r = build_manifest(
            d.path(),
            &lister(&["run.sh", "plain.txt"]),
            &SyncLimits::default(),
        )
        .unwrap();
        let sh = &r.manifest.entries["run.sh"];
        assert!(sh.exec);
        assert!(!r.manifest.entries["plain.txt"].exec);
        assert_eq!(sh.hash, Hash::of(b"#!/bin/sh\n"));
        assert_eq!(sh.size, 10);
    }

    #[test]
    fn git_lister_includes_uncommitted_and_untracked_but_not_ignored() {
        let d = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(d.path())
                .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
                .args(args)
                .output()
        };
        // Environments without git cannot run this check; the trait keeps
        // every other test independent of git.
        let Ok(init) = git(&["init", "-q"]) else {
            return;
        };
        if !init.status.success() {
            return;
        }
        write(d.path(), ".gitignore", "ignored.txt\n");
        write(d.path(), "tracked.txt", "a");
        write(d.path(), "fixture.log", "l");
        write(d.path(), "ignored.txt", "i");
        let _ = git(&["add", ".gitignore", "tracked.txt"]);
        let _ = git(&["commit", "-q", "-m", "x", "--no-gpg-sign"]);
        write(d.path(), "tracked.txt", "modified");
        write(d.path(), "new dir/untracked file.txt", "u");
        let files = GitLister.list(d.path()).unwrap();
        let set: BTreeSet<&str> = files.iter().map(String::as_str).collect();
        assert!(set.contains("tracked.txt"));
        assert!(set.contains("fixture.log"));
        assert!(set.contains("new dir/untracked file.txt"));
        assert!(!set.contains("ignored.txt"));
    }

    #[test]
    fn parse_ls_files_handles_spaces_and_newlines() {
        let v = parse_ls_files(b"a b.txt\0dir/c\nd\0");
        assert_eq!(v, ["a b.txt", "dir/c\nd"]);
    }

    fn entry(p: &str, body: &str) -> FileEntry {
        FileEntry {
            path: p.into(),
            hash: Hash::of(body.as_bytes()),
            size: body.len() as u64,
            exec: false,
        }
    }

    #[test]
    fn r155_want_is_only_what_is_missing_and_deduplicated() {
        let mut m = Manifest::default();
        for (p, b) in [("a", "1"), ("b", "2"), ("c", "2"), ("d", "3")] {
            m.entries.insert(p.into(), entry(p, b));
        }
        let have: BTreeSet<Hash> = [Hash::of(b"1")].into();
        let w = want(&m, &have);
        assert_eq!(w.len(), 2);
        assert!(w.contains(&Hash::of(b"2")) && w.contains(&Hash::of(b"3")));
        assert!(want(&m, &w.iter().copied().chain(have).collect()).is_empty());
    }

    #[test]
    fn r155_deleted_lists_paths_gone_from_the_new_manifest() {
        let mut old = Manifest::default();
        let mut new = Manifest::default();
        for p in ["a", "b", "c"] {
            old.entries.insert(p.into(), entry(p, p));
        }
        new.entries.insert("b".into(), entry("b", "b"));
        assert_eq!(deleted(&new, &old), ["a", "c"]);
    }

    #[test]
    fn r156_thousands_of_files_page_within_the_frame_limit() {
        let mut m = Manifest::default();
        for i in 0..5000 {
            let p = format!("some/deep/directory/structure/file_{i:05}.rs");
            m.entries.insert(p.clone(), entry(&p, &p));
        }
        let ps = pages(&m);
        assert!(ps.len() > 1);
        assert_eq!(ps.iter().map(Vec::len).sum::<usize>(), 5000);
        for (i, page) in ps.iter().enumerate() {
            let body = crate::proto::Body::SyncManifestPage {
                checkout: crate::ids::CheckoutId::derive("h", Path::new("/p")),
                session: 1,
                index: i as u32,
                last: i + 1 == ps.len(),
                entries: page.clone(),
            };
            assert!(crate::proto::encode(&body).is_ok(), "page {i} too big");
        }
    }

    #[test]
    fn empty_manifest_still_has_one_page() {
        assert_eq!(pages(&Manifest::default()).len(), 1);
    }
}
