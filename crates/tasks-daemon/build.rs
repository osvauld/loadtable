//! Stamps the build with its git commit (R-807): `LOADTABLE_VERSION` is e.g.
//! `0.1.0 (41ac455 2026-10-27)`. Without git (a source tarball) the commit comes from
//! `LOADTABLE_BUILD_COMMIT`, else it is `unknown`.

use std::path::Path;
use std::process::Command;

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    println!("cargo:rerun-if-env-changed=LOADTABLE_BUILD_COMMIT");
    println!("cargo:rerun-if-changed=build.rs");

    let stamp = match git(&dir, &["log", "-1", "--format=%h %cs"]) {
        Some(s) => {
            // rebuild when HEAD moves: HEAD itself, the branch it names, and packed refs
            for p in ["HEAD", "packed-refs"] {
                rerun_on(&dir, p);
            }
            if let Some(r) = git(&dir, &["symbolic-ref", "-q", "HEAD"]) {
                rerun_on(&dir, &r);
            }
            s
        }
        None => match std::env::var("LOADTABLE_BUILD_COMMIT") {
            Ok(c) if !c.trim().is_empty() => c.trim().chars().take(7).collect(),
            _ => "unknown".to_string(),
        },
    };
    println!("cargo:rustc-env=LOADTABLE_VERSION={version} ({stamp})");
}

/// Watch one file of the git directory, by the path git resolves for it (worktrees keep HEAD
/// apart from the shared refs). A file that does not exist is not watched: cargo would rerun
/// the script on every build.
fn rerun_on(dir: &str, name: &str) {
    if let Some(p) = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    ) && Path::new(&p).exists()
    {
        println!("cargo:rerun-if-changed={p}");
    }
}
