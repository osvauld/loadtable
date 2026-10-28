//! Recipe families: commands that cost about the same, so a command with no runs of its own can
//! be estimated from its relatives'.
//!
//! An Android build's time is mostly the native code and the Kotlin compile behind every task,
//! not the task named: a fresh `:app:testPlayDebugUnitTest` costs what a fresh
//! `:app:assemblePlayRelease` does, while `-Pabis` (how many native targets) halves or doubles
//! it. So a gradle family is the build type (debug, or release-like: release, bundle,
//! benchmark) and the `-P`/`-D` properties, whatever the flavour or mix of tasks. Families are
//! read from stored recipe strings, so runs recorded before families existed count too.

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
/// gradle, chains, dry runs, device runs and tasks that build nothing.
pub fn family(recipe: &str) -> Option<String> {
    if recipe.contains(" && ") {
        return None;
    }
    let mut words = recipe.split(' ');
    if words.next() != Some("gradle") {
        return None;
    }
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
            "cargo test pkg=a",
            "make all",
        ] {
            assert_eq!(family(r), None, "{r}");
        }
    }
}
