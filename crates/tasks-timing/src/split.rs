//! Splitting a broad `cargo test` into a build step plus one step per test target.

use crate::error::TimingError;
use crate::estimate::Estimate;
use crate::exe::{ExeKind, TestExe};
use crate::recipe::basename;
use std::collections::{BTreeMap, BTreeSet};
use tasks_core::plan::{Plan, Step};
use tasks_core::{Kind, Requirements};

/// Flags (with a value, if the bool is true) that select which targets run.
const SELECTORS: &[(&str, bool)] = &[
    ("--lib", false),
    ("--bins", false),
    ("--doc", false),
    ("--tests", false),
    ("--benches", false),
    ("--examples", false),
    ("--all-targets", false),
    ("--bin", true),
    ("--test", true),
    ("--bench", true),
    ("--example", true),
];
/// Flags that choose packages.
const PACKAGE_FLAGS: &[(&str, bool)] = &[
    ("--workspace", false),
    ("--all", false),
    ("--exclude", true),
    ("-p", true),
    ("--package", true),
];

fn is_flag(a: &str, table: &[(&str, bool)]) -> Option<bool> {
    table.iter().find(|(n, _)| a == *n).map(|(_, v)| *v)
}

fn has_value_form(a: &str, table: &[(&str, bool)]) -> bool {
    table
        .iter()
        .any(|(n, v)| *v && a.starts_with(&format!("{n}=")))
}

fn strip(head: &[String], table: &[(&str, bool)]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = head.iter();
    while let Some(a) = it.next() {
        match is_flag(a, table) {
            Some(true) => {
                it.next();
            }
            Some(false) => {}
            None if has_value_form(a, table) => {}
            None => out.push(a.clone()),
        }
    }
    out
}

fn package_count(head: &[String]) -> usize {
    head.iter()
        .filter(|a| matches!(a.as_str(), "-p" | "--package") || a.starts_with("--package="))
        .count()
}

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Splits `cargo test ...` into a `build` step (`--no-run`) and one step per test executable.
///
/// Every test step is the original argv with the target selection replaced by `--test NAME`,
/// `--lib` or `--bin NAME`, and `after: ["build"]`. All other flags are identical so the build is
/// shared. When the original selects several packages (or the workspace), each step is also
/// narrowed to its own `-p PACKAGE` so a `--lib` step does not run every package's lib tests.
pub fn split_cargo_test(
    argv: &[String],
    test_executables: &[TestExe],
) -> Result<Plan, TimingError> {
    if argv.is_empty() {
        return Err(TimingError::EmptyArgv);
    }
    let tool = argv.first().map(|a| basename(a)).unwrap_or("");
    let sub = argv
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('+') && !a.starts_with('-'));
    if tool != "cargo" || sub.map(String::as_str) != Some("test") {
        return Err(TimingError::NotCargoTest(argv.join(" ")));
    }
    if test_executables.is_empty() {
        return Err(TimingError::NoTestExecutables);
    }
    let dd = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    let head = argv.get(..dd).unwrap_or(argv);
    let tail = argv.get(dd..).unwrap_or(&[]);
    let narrow = package_count(head) > 1 || head.iter().any(|a| a == "--workspace" || a == "--all");
    let base = strip(head, SELECTORS);
    let base = if narrow {
        strip(&base, PACKAGE_FLAGS)
    } else {
        base
    };

    let mut build = head.to_vec();
    if !build.iter().any(|a| a == "--no-run") {
        build.push("--no-run".into());
    }
    let mut steps = vec![Step {
        name: "build".into(),
        argv: build,
        after: Vec::new(),
        kind: Kind::Compile,
        requirements: Requirements::default(),
    }];
    let mut used: BTreeSet<String> = BTreeSet::from(["build".to_string()]);
    for exe in test_executables {
        let (prefix, sel): (&str, Vec<String>) = match exe.kind {
            ExeKind::Lib => ("lib", vec!["--lib".into()]),
            ExeKind::Test => ("test", vec!["--test".into(), exe.name.clone()]),
            ExeKind::Bin => ("bin", vec!["--bin".into(), exe.name.clone()]),
            ExeKind::Bench => ("bench", vec!["--bench".into(), exe.name.clone()]),
            ExeKind::Example => ("example", vec!["--example".into(), exe.name.clone()]),
        };
        let mut name = clean(&format!("{prefix}-{}", exe.name));
        if used.contains(&name) {
            name = clean(&format!("{name}-{}", exe.package));
        }
        let mut n = 2;
        while used.contains(&name) {
            name = format!("{name}-{n}");
            n += 1;
        }
        used.insert(name.clone());
        let mut argv = base.clone();
        if narrow {
            argv.extend(["-p".to_string(), exe.package.clone()]);
        }
        argv.extend(sel);
        argv.extend(tail.iter().cloned());
        steps.push(Step {
            name,
            argv,
            after: vec!["build".into()],
            kind: Kind::Test,
            requirements: Requirements::default(),
        });
    }
    let plan = Plan { steps };
    plan.validate()?;
    Ok(plan)
}

/// History label for a step name made by [`split_cargo_test`]: `test-lifecycle` is `test:lifecycle`.
pub fn label_of(step: &str) -> String {
    step.replacen('-', ":", 1)
}

/// Reorders steps so that, among those whose dependencies are done, the longest run first.
///
/// `estimates` is looked up by step name, then by label (`test:lifecycle`). Steps with no estimate
/// count as longer than any estimate; ties keep their original order. Dependencies always precede
/// dependents, so the plan stays valid.
pub fn order_longest_first(
    plan: &Plan,
    estimates: &BTreeMap<String, Estimate>,
) -> Result<Plan, TimingError> {
    plan.validate()?;
    let weight = |name: &str| {
        estimates
            .get(name)
            .or_else(|| estimates.get(&label_of(name)))
            .map_or(f64::INFINITY, |e| e.median_secs)
    };
    let mut done: BTreeSet<&str> = BTreeSet::new();
    let mut out: Vec<Step> = Vec::with_capacity(plan.steps.len());
    while out.len() < plan.steps.len() {
        let mut best: Option<(&Step, f64)> = None;
        for s in plan
            .steps
            .iter()
            .filter(|s| !done.contains(s.name.as_str()))
        {
            if s.after.iter().all(|d| done.contains(d.as_str())) {
                let w = weight(&s.name);
                if best.is_none_or(|(_, bw)| w > bw) {
                    best = Some((s, w));
                }
            }
        }
        let Some((s, _)) = best else { break };
        done.insert(s.name.as_str());
        out.push(s.clone());
    }
    let ordered = Plan { steps: out };
    ordered.validate()?;
    Ok(ordered)
}

#[cfg(test)]
mod tests;
