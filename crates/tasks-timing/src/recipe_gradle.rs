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
            fl.insert(a.to_string());
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
