//! Recipe families: commands that cost about the same, so a command with no runs of its own can
//! be estimated from its relatives'.
//!
//! An Android build's time is mostly the native code and the Kotlin compile behind every task,
//! not the task named: a fresh `:app:testPlayDebugUnitTest` costs what a fresh
//! `:app:assemblePlayRelease` does, while `-Pabis` (how many native targets) halves or doubles
//! it. So a gradle family is the build type (debug, or release-like: release, bundle,
//! benchmark) and the `-P`/`-D` properties, whatever the flavour or mix of tasks. Families are
//! read from stored recipe strings, so runs recorded before families existed count too.
//!
//! A cargo test run's time is mostly its build, not the tests its filter picks: `cargo test -p
//! x -- r72` with no runs of its own is estimated from `cargo test -p x` and `cargo test -p x
//! -- r71` here (R-827). So a cargo family is the command without its test filters; packages,
//! features, profile and target still tell work apart. Of the 509 cargo runs recorded up to 29
//! Oct 2026, 241 of 320 command lines had run once; as families, 127 of 205.

use crate::recipe_gradle::is_stamp;

/// Bare gradle flags a recipe keeps (`--dry-run` shows as `dry-run`). Inside the recipe's
/// comma-joined flag list they tell a new flag from the next piece of a comma-valued property
/// (`-Pabis=x86_64,arm64-v8a`).
const BARE_FLAGS: &[&str] = &[
    "dry-run",
    "m",
    "rerun-tasks",
    "rerun",
    "refresh-dependencies",
    "profile",
    "configuration",
    "configuration-cache",
    "no-configuration-cache",
    "continuous",
    "t",
    "write-locks",
    "offline",
];
/// Words that make a gradle task a build: anything else (`dependencies`, `lint`, `clean`) is
/// not timed like one.
const BUILD_WORDS: &[&str] = &["assemble", "bundle", "test", "compile", "build"];
const RELEASE_WORDS: &[&str] = &["release", "bundle", "benchmark"];
const DEVICE_WORDS: &[&str] = &["connected", "install"];

/// The family of a whole-job recipe string, or `None` when it has none: tools other than
/// gradle and cargo, chains, dry runs, device runs and tasks that build nothing.
pub fn family(recipe: &str) -> Option<String> {
    if recipe.contains(" && ") {
        return None;
    }
    let mut words = recipe.split(' ');
    match words.next() {
        Some("gradle") => gradle_family(words),
        Some("cargo") => Some(cargo_family(recipe)),
        _ => None,
    }
}

/// A cargo recipe without its test filters (`filter:`, `skip:`). A command with none is its own
/// family, so it and its filtered runs share their runs.
fn cargo_family(recipe: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for w in recipe.split(' ') {
        if let Some(sel) = w.strip_prefix("sel=") {
            let kept: Vec<&str> = sel
                .split(',')
                .filter(|s| !s.starts_with("filter:") && !s.starts_with("skip:"))
                .collect();
            if !kept.is_empty() {
                out.push(format!("sel={}", kept.join(",")));
            }
        } else {
            out.push(w.to_string());
        }
    }
    out.join(" ")
}

/// Whether the tool's builds here, whatever their family, make a range worth showing (R-827):
/// gradle's do, an Android build being mostly the same work whatever the task; cargo's run from
/// a one-second check to an hour-long release build and would say nothing.
pub fn ranged(tool: &str) -> bool {
    tool == "gradle"
}

fn gradle_family(words: std::str::Split<'_, char>) -> Option<String> {
    let (mut sel, mut flags) = ("", "");
    for w in words {
        if let Some(v) = w.strip_prefix("sel=") {
            sel = v;
        } else if let Some(v) = w.strip_prefix("flags=") {
            flags = v;
        }
    }
    let tasks: Vec<String> = sel
        .split(',')
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let has = |words: &[&str]| tasks.iter().any(|t| words.iter().any(|w| t.contains(w)));
    if !has(BUILD_WORDS) || has(DEVICE_WORDS) {
        return None;
    }
    let mut props: Vec<String> = Vec::new();
    let mut bare: Vec<&str> = Vec::new();
    for piece in flags.split(',').filter(|p| !p.is_empty()) {
        if piece.starts_with("-P") || piece.starts_with("-D") {
            props.push(piece.to_string());
        } else if let Some(last) = props.last_mut()
            && !piece.contains(':')
            && !BARE_FLAGS.contains(&piece)
        {
            last.push(',');
            last.push_str(piece);
        } else {
            bare.push(piece);
        }
    }
    if bare.iter().any(|b| matches!(*b, "dry-run" | "m")) {
        return None;
    }
    props.retain(|p| !is_stamp(p));
    let class = if has(RELEASE_WORDS) {
        "release"
    } else {
        "debug"
    };
    let mut out = format!("gradle {class}");
    for p in props {
        out.push(' ');
        out.push_str(&p);
    }
    Some(out)
}

/// The tool a recipe string runs (`gradle`, `cargo`, ...): its first word.
pub fn tool(recipe: &str) -> &str {
    recipe.split(' ').next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavour_task_mix_and_stamps_do_not_change_the_family() {
        let release = family(
            "gradle sel=:app:assemblePlayRelease flags=-Pabis=x86_64,arm64-v8a,\
             -PrequireReleaseSigning=true,-PversionCode=1632,-PversionName=0.2.4",
        );
        assert_eq!(
            release.as_deref(),
            Some("gradle release -Pabis=x86_64,arm64-v8a")
        );
        assert_eq!(
            family("gradle sel=:app:assembleLiberumRelease flags=-Pabis=x86_64,arm64-v8a"),
            release
        );
        assert_eq!(
            family("gradle sel=:app:assemblePlayDebug,:app:testPlayDebugUnitTest"),
            family("gradle sel=:app:testLiberumDebugUnitTest flags=tests:com.x.WakeRingTest"),
        );
        assert_eq!(
            family("gradle sel=:app:assemblePlayBenchmark").as_deref(),
            Some("gradle release")
        );
    }

    #[test]
    fn abis_and_build_type_change_the_family() {
        let one = family("gradle sel=:app:assemblePlayDebug flags=-Pabis=arm64-v8a");
        assert_ne!(one, family("gradle sel=:app:assemblePlayDebug"));
        assert_ne!(
            one,
            family("gradle sel=:app:assemblePlayRelease flags=-Pabis=arm64-v8a")
        );
    }

    #[test]
    fn a_bare_flag_after_a_comma_valued_property_is_not_its_value() {
        assert_eq!(
            family("gradle sel=:app:assembleDebug flags=-Pabis=x86_64,arm64-v8a,rerun-tasks")
                .as_deref(),
            Some("gradle debug -Pabis=x86_64,arm64-v8a")
        );
    }

    #[test]
    fn dry_runs_device_runs_non_builds_chains_and_other_tools_have_none() {
        for r in [
            "gradle sel=:app:assembleBenchmark flags=-Pabis=arm64-v8a,dry-run",
            "gradle sel=:app:connectedDebugAndroidTest",
            "gradle sel=:app:dependencies,liberumDebugRuntimeClasspath flags=configuration",
            "gradle sel=:app:assembleDebug && gradle sel=:app:testDebugUnitTest",
            "cargo build pkg=a profile=dev && cargo test pkg=a profile=dev",
            "make all",
            "npm run build",
        ] {
            assert_eq!(family(r), None, "{r}");
        }
    }

    #[test]
    fn r827_a_cargo_command_without_its_test_filters_is_its_family() {
        let lib = "cargo test pkg=tasks-daemon profile=dev sel=lib";
        assert_eq!(
            family("cargo test pkg=tasks-daemon profile=dev sel=filter:r71,lib").as_deref(),
            Some(lib)
        );
        assert_eq!(family(lib).as_deref(), Some(lib), "its own family");
        assert_eq!(
            family("cargo test pkg=a profile=dev sel=filter:x,skip:y flags=no-fail-fast")
                .as_deref(),
            Some("cargo test pkg=a profile=dev flags=no-fail-fast"),
            "only filters: the selection goes"
        );
        // packages, features, the profile and the target are the work
        assert_ne!(
            family("cargo test pkg=a profile=dev sel=lib"),
            family("cargo test pkg=b profile=dev sel=lib")
        );
        assert_ne!(
            family("cargo build pkg=a profile=dev"),
            family("cargo build pkg=a profile=release")
        );
        assert_ne!(
            family("cargo build pkg=a feat=x profile=dev"),
            family("cargo build pkg=a profile=dev")
        );
        assert_ne!(
            family("cargo check profile=dev target=aarch64-apple-darwin"),
            family("cargo check profile=dev")
        );
        assert!(ranged("gradle") && !ranged("cargo") && !ranged("make"));
    }
}
