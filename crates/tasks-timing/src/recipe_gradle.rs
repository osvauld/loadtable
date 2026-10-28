//! Normalisation of `gradle` / `./gradlew` command lines.

use crate::recipe::{RecipeKey, sorted};
use std::collections::BTreeSet;

/// Flags that only change output or process management.
const IGNORED: &[&str] = &[
    "--info",
    "-i",
    "--debug",
    "-d",
    "--warn",
    "-w",
    "--quiet",
    "-q",
    "--stacktrace",
    "-s",
    "--full-stacktrace",
    "-S",
    "--no-daemon",
    "--daemon",
    "--parallel",
    "--no-parallel",
    "--offline",
    "--scan",
    "--no-scan",
    "--continue",
    "--no-build-cache",
    "--build-cache",
    "--configure-on-demand",
];
/// Options whose value is a path or output setting and is dropped with the option.
const IGNORED_VALUE: &[&str] = &[
    "--console",
    "--max-workers",
    "--warning-mode",
    "-p",
    "--project-dir",
    "-g",
    "--gradle-user-home",
    "-b",
    "--build-file",
    "-c",
    "--settings-file",
    "--project-cache-dir",
    "-I",
    "--init-script",
];
/// `-P`/`-D` properties that only stamp the output or tune the JVM, not what is built: every
/// release passes a new `versionCode`, which would otherwise make each one a first run.
const STAMP_PROPS: &[&str] = &[
    "-PversionCode",
    "-PversionName",
    "-PrequireReleaseSigning",
    "-Dfile.encoding",
    "-Dorg.gradle.jvmargs",
];

/// True for a `-P`/`-D` word that only stamps the output.
pub(crate) fn is_stamp(a: &str) -> bool {
    STAMP_PROPS.contains(&a.split('=').next().unwrap_or(a))
}

pub(crate) fn parse(args: &[String]) -> RecipeKey {
    let mut k = RecipeKey::new("gradle");
    let (mut tasks, mut fl) = (BTreeSet::new(), BTreeSet::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let a = a.as_str();
        if IGNORED.contains(&a)
            || IGNORED_VALUE
                .iter()
                .any(|o| a.starts_with(&format!("{o}=")))
        {
        } else if IGNORED_VALUE.contains(&a) {
            it.next();
        } else if a == "-x" || a == "--exclude-task" {
            fl.insert(format!("x:{}", it.next().cloned().unwrap_or_default()));
        } else if a == "--tests" {
            fl.insert(format!("tests:{}", it.next().cloned().unwrap_or_default()));
        } else if let Some(v) = a.strip_prefix("--tests=") {
            fl.insert(format!("tests:{v}"));
        } else if a.starts_with("-P") || a.starts_with("-D") {
            if !is_stamp(a) {
                fl.insert(a.to_string());
            }
        } else if a.starts_with('-') {
            fl.insert(a.trim_start_matches('-').to_string());
        } else {
            tasks.insert(a.to_string());
        }
    }
    k.selection = sorted(tasks);
    k.flags = sorted(fl);
    k
}
