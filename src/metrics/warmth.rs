//! Build warmth: will this job compile (almost) everything, or (almost) nothing?
//!
//! Heuristic, kept simple and explainable (`warmth_reason` says which rule fired):
//! 1. Before the job starts, the worker checks the checkout's Cargo target for
//!    the command's profile (`target[/TRIPLE]/debug|release|PROFILE`, following
//!    the cache link tasks places). Missing, or no `.fingerprint` entries:
//!    **cold**. Present: **unknown** for now (it may still rebuild a lot).
//! 2. While and after running, `Compiling ` lines in the job log are counted
//!    and compared with a reference size R for this kind of command on this
//!    machine: the median crate count of earlier cold runs, else the largest
//!    count seen in any earlier run.
//!    - count > 20% of R: **cold** (it is rebuilding most of the graph);
//!    - build finished with count <= 20% of R: **warm**;
//!    - build finished with 0 crates compiled: **warm** even without history.
//!    - otherwise it stays **unknown**.
//!
//! Non-Cargo commands and offload clients (the build runs on the worker) are
//! **unknown** with a reason saying so.
use super::Warmth;
use super::command::{CargoInvocation, cargo_invocation, is_offload};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Fraction of the reference crate count at or below which a finished build is warm.
pub const WARM_FRACTION: f64 = 0.2;
/// Written by the worker before output placement as `jobs/ID/warmth.json`.
pub const START_FILE: &str = "warmth.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StartWarmth {
    pub warmth: Warmth,
    pub reason: String,
}

/// Where Cargo will put this command's outputs for its profile.
pub fn profile_dir(inv: &CargoInvocation, cwd: &Path) -> PathBuf {
    let target = match inv.target_dir() {
        Some(dir) => cwd.join(dir),
        None => {
            let start = inv
                .manifest_path()
                .map(|m| cwd.join(m))
                .and_then(|m| m.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| cwd.to_path_buf());
            workspace_root(&start).join("target")
        }
    };
    let target = match inv.target_triple() {
        Some(triple) => target.join(triple),
        None => target,
    };
    target.join(inv.profile_dir())
}

/// Simplified Cargo workspace discovery, bounded by the Git root (nearest
/// `.git` entry): the nearest ancestor manifest with a `[workspace]` table,
/// else the nearest manifest, else `start` itself.
pub fn workspace_root(start: &Path) -> PathBuf {
    let git_root = start
        .ancestors()
        .find(|d| std::fs::symlink_metadata(d.join(".git")).is_ok());
    let mut package = None;
    for dir in start.ancestors() {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            package.get_or_insert_with(|| dir.to_path_buf());
            let workspace = std::fs::read_to_string(&manifest)
                .ok()
                .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
                .is_some_and(|t| t.contains_key("workspace"));
            if workspace {
                return dir.to_path_buf();
            }
        }
        if Some(dir) == git_root {
            break;
        }
    }
    package.unwrap_or_else(|| start.to_path_buf())
}

/// Rule 1: cold if the profile output has no fingerprints, else unknown.
pub fn at_start(argv: &[String], cwd: &Path) -> StartWarmth {
    if is_offload(argv) {
        return StartWarmth {
            warmth: Warmth::Unknown,
            reason: "offloaded: the build runs on the worker".into(),
        };
    }
    let Some(inv) = cargo_invocation(argv) else {
        return StartWarmth {
            warmth: Warmth::Unknown,
            reason: "not a Cargo command".into(),
        };
    };
    let dir = profile_dir(&inv, cwd);
    let shown = dir
        .strip_prefix(cwd)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| dir.clone());
    let fingerprints = std::fs::read_dir(dir.join(".fingerprint"))
        .map(|entries| entries.take(10_000).count())
        .unwrap_or(0);
    if fingerprints == 0 {
        StartWarmth {
            warmth: Warmth::Cold,
            reason: format!(
                "no build output for this checkout on this machine ({} has no fingerprints)",
                shown.display()
            ),
        }
    } else {
        StartWarmth {
            warmth: Warmth::Unknown,
            reason: format!(
                "build output exists ({}: {fingerprints} fingerprints); refined from Compiling lines",
                shown.display()
            ),
        }
    }
}

/// Rule 2. `reference` is R (see module docs); `build_done` is true once no
/// further `Compiling` lines can come (job finished, or past its build stage).
pub fn refine(
    start: &StartWarmth,
    compiled: u64,
    reference: Option<u64>,
    build_done: bool,
) -> (Warmth, String) {
    if start.warmth == Warmth::Cold {
        return (Warmth::Cold, start.reason.clone());
    }
    if start.reason.starts_with("not a Cargo") || start.reason.starts_with("offloaded") {
        return (start.warmth, start.reason.clone());
    }
    let limit = reference.map(|r| r as f64 * WARM_FRACTION);
    match (reference, limit) {
        (Some(r), Some(limit)) if compiled as f64 > limit => (
            Warmth::Cold,
            format!(
                "compiled {compiled} crates, more than {:.0}% of a full build ({r})",
                WARM_FRACTION * 100.0
            ),
        ),
        _ if build_done && compiled == 0 => (Warmth::Warm, "nothing was recompiled".into()),
        (Some(r), Some(_)) if build_done => (
            Warmth::Warm,
            format!(
                "compiled {compiled} crates, at most {:.0}% of a full build ({r})",
                WARM_FRACTION * 100.0
            ),
        ),
        (None, _) if build_done => (
            Warmth::Unknown,
            format!("compiled {compiled} crates; no earlier build of this kind to compare"),
        ),
        _ => (Warmth::Unknown, start.reason.clone()),
    }
}

/// Counts cargo `Compiling ` status lines (`   Compiling foo v0.1.0 (...)`) and the test
/// targets a `cargo test` built: one per distinct `Running`/`Executable` binary
/// (`     Running tests/a.rs (target/debug/deps/a-0123...)`) plus one per `Doc-tests X`.
#[derive(Clone, Debug, Default)]
pub struct CompileCounter {
    pub count: u64,
    partial: Vec<u8>,
    /// Distinct test binaries (path in parentheses) and doc-test crates seen; bounded.
    test_targets: std::collections::HashSet<String>,
}

/// Longest line prefix kept for classification (test-binary lines carry a path).
const LINE_PREFIX: usize = 1024;
const MAX_TEST_TARGETS: usize = 4096;

impl CompileCounter {
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                self.line();
            } else if self.partial.len() < LINE_PREFIX {
                self.partial.push(b);
            }
        }
    }
    fn line(&mut self) {
        let text = String::from_utf8_lossy(&self.partial);
        let text = text.trim();
        if text.starts_with("Compiling ") {
            self.count += 1;
        } else if let Some(key) = test_target_key(text)
            && self.test_targets.len() < MAX_TEST_TARGETS
        {
            self.test_targets.insert(key);
        }
        self.partial.clear();
    }
    /// Test targets seen so far (see the type docs).
    pub fn test_targets(&self) -> u64 {
        self.test_targets.len() as u64
    }
    /// Counts all complete lines of a whole text.
    pub fn count_text(text: &[u8]) -> u64 {
        Self::of_text(text).count
    }
    pub fn of_text(text: &[u8]) -> Self {
        let mut c = Self::default();
        c.feed(text);
        c
    }
}

/// `Running unittests src/lib.rs (target/debug/deps/x-hash)` / `Executable tests/a.rs (...)`
/// -> the binary path; `Doc-tests video` -> `doc:video`.
fn test_target_key(line: &str) -> Option<String> {
    if let Some(name) = line.strip_prefix("Doc-tests ") {
        return Some(format!("doc:{}", name.trim()));
    }
    let rest = line
        .strip_prefix("Running ")
        .or_else(|| line.strip_prefix("Executable "))?;
    let open = rest.rfind(" (")?;
    let path = rest[open + 2..].strip_suffix(')')?;
    (!path.is_empty()).then(|| path.to_string())
}

/// An unknown start state with the given reason.
pub fn at_start_unknown(reason: &str) -> StartWarmth {
    StartWarmth {
        warmth: Warmth::Unknown,
        reason: reason.into(),
    }
}
