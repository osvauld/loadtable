//! Whether a cargo build is warm or cold, judged from the build output directory and from what
//! the build had to compile (R-800).
//!
//! "Warm" used to mean "this machine passed the command before", which a new worktree with an
//! empty `target/` or a dependency bump also satisfied. Here a build is cold when it compiles
//! (or, before it starts, its output directory lacks) a large share of the dependencies its
//! lockfile names; a rebuild of the checkout's own crates and a few changed dependencies is
//! warm.

use crate::history::Cache;

/// The share of the lockfile's dependencies that makes a build cold: a quarter.
const COLD_SHARE_DIVISOR: u32 = 4;
/// Dependencies compiled that make a build cold when the lockfile could not be read.
const COLD_DEPS_WITHOUT_LOCK: u32 = 20;

fn bar(deps_in_lock: Option<u32>) -> u32 {
    deps_in_lock.map_or(COLD_DEPS_WITHOUT_LOCK, |n| {
        n.div_ceil(COLD_SHARE_DIVISOR).max(1)
    })
}

/// How a finished cargo run started, from the dependencies it compiled (`deps_built`) and the
/// dependencies its lockfile names (`None` when it could not be read).
pub fn run_warmth(deps_built: u32, deps_in_lock: Option<u32>) -> Cache {
    if deps_built >= bar(deps_in_lock) {
        Cache::Cold
    } else {
        Cache::Warm
    }
}

/// How a cargo run will start, from the units its output directory already holds for the
/// profile (`None`: no such directory) and the dependencies its lockfile names.
pub fn start_warmth(built_units: Option<u32>, deps_in_lock: Option<u32>) -> Cache {
    match built_units {
        Some(n) if n >= bar(deps_in_lock) => Cache::Warm,
        _ => Cache::Cold,
    }
}

/// The dependencies a `Cargo.lock` names: its packages with a `source` (registry or git), not
/// the workspace's own path crates.
pub fn lock_deps(lock_text: &str) -> u32 {
    let n = lock_text
        .lines()
        .filter(|l| l.trim_start().starts_with("source = "))
        .count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r800_a_build_that_compiles_a_quarter_of_its_dependencies_is_cold() {
        assert_eq!(run_warmth(0, Some(600)), Cache::Warm, "only own crates");
        assert_eq!(run_warmth(12, Some(600)), Cache::Warm, "a dependency bump");
        assert_eq!(run_warmth(150, Some(600)), Cache::Cold);
        assert_eq!(
            run_warmth(15, Some(15)),
            Cache::Cold,
            "a small project from scratch"
        );
        assert_eq!(run_warmth(19, None), Cache::Warm);
        assert_eq!(run_warmth(20, None), Cache::Cold);
        assert_eq!(run_warmth(0, Some(0)), Cache::Warm);
    }

    #[test]
    fn r800_an_output_directory_without_most_dependencies_starts_cold() {
        assert_eq!(start_warmth(None, Some(600)), Cache::Cold, "no target yet");
        assert_eq!(
            start_warmth(Some(30), Some(600)),
            Cache::Cold,
            "another crate's deps"
        );
        assert_eq!(start_warmth(Some(900), Some(600)), Cache::Warm);
        assert_eq!(start_warmth(Some(0), Some(0)), Cache::Cold);
        assert_eq!(start_warmth(Some(1), Some(0)), Cache::Warm);
    }

    #[test]
    fn r800_the_lockfile_counts_registry_and_git_packages_only() {
        let lock = "version = 4\n\n[[package]]\nname = \"desktop\"\nversion = \"0.2.1\"\n\n\
                    [[package]]\nname = \"serde\"\nversion = \"1.0.200\"\n\
                    source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
                    [[package]]\nname = \"iroh\"\nversion = \"1.4.1\"\n\
                    source = \"git+https://github.com/n0-computer/iroh#abc\"\n";
        assert_eq!(lock_deps(lock), 2);
    }
}
