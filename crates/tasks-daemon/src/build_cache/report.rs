//! The read-only text behind `loadtable cache`: the wrapper in use, the cache root and the disk
//! against its reserve. It starts no daemon and creates nothing.

use super::{BuildCache, Settings};
use crate::config::DaemonConfig;
use crate::machine_policy::MachinePolicy;
use std::fmt::Write;
use tasks_platform::Disk;

/// The report for the daemon configured by `cfg`, reading the policy file and the machine's
/// `env` the way the daemon does.
pub fn report(
    cfg: &DaemonConfig,
    policy: &MachinePolicy,
    env: &[(String, String)],
    disk: &dyn Disk,
) -> String {
    let cache = BuildCache::discover(&cfg.cache_dir, &cfg.state_dir.join("peer-store"), env);
    let settings = Settings::of(policy);
    let mut out = String::new();
    let _ = writeln!(out, "cache root: {}", cache.root().display());
    match cache.wrapper(&settings) {
        Ok(w) => {
            let _ = writeln!(
                out,
                "compiler cache: sccache {} (SCCACHE_DIR {}, limit {})",
                w.display(),
                cache.sccache_dir().display(),
                settings.sccache_size
            );
        }
        Err(why) => {
            let _ = writeln!(out, "compiler cache: none ({why})");
        }
    }
    match cache.gradle_home() {
        Some(g) => {
            let _ = writeln!(out, "gradle home: {}", g.display());
        }
        None => {
            let _ = writeln!(
                out,
                "gradle home: not set ({} does not exist)",
                cache.root().join("gradle").display()
            );
        }
    }
    let _ = writeln!(out, "outputs: {}", cache.checkouts_dir().display());
    let reserve = policy.cache_reserve_bytes();
    match nearest_free(cache.root(), disk) {
        Ok(free) => {
            let verdict = if free < reserve {
                "BELOW the reserve: jobs that need new cache output wait"
            } else {
                "ok"
            };
            let _ = writeln!(
                out,
                "disk: {} free, reserve {}: {verdict}",
                tasks_admission::gib(free),
                tasks_admission::gib(reserve)
            );
        }
        Err(e) => {
            let _ = writeln!(out, "disk: cannot be measured ({e})");
        }
    }
    out
}

/// Free bytes at the nearest existing ancestor of `path`: the root may not exist yet.
fn nearest_free(path: &std::path::Path, disk: &dyn Disk) -> Result<u64, String> {
    let existing = path
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or_else(|| std::path::Path::new("/"));
    disk.free_bytes(existing).map_err(|e| e.to_string())
}
