//! Expanding a suite into a [`tasks_core::Plan`].

use crate::error::PolicyError;
use crate::model::{Policy, SplitMode, StepDef};
use crate::timing::{Timing, measure, primary, recipe_of};
use std::collections::{BTreeMap, BTreeSet};
use tasks_core::plan::{Plan, Step};
use tasks_core::{Kind, Requirements, StepHints};
use tasks_timing::estimate::Estimate;
use tasks_timing::{TestExe, label_of, order_longest_first, split_cargo_test};

/// A suite expanded to a plan; each step carries its policy hints.
#[derive(Debug, Clone, PartialEq)]
pub struct Expansion {
    /// The validated plan.
    pub plan: Plan,
}

impl Expansion {
    /// Hints of the step named `name`, if the plan has it.
    pub fn hints(&self, name: &str) -> Option<&StepHints> {
        self.plan
            .steps
            .iter()
            .find(|s| s.name == name)
            .map(|s| &s.hints)
    }
}

/// Options for [`expand`].
#[derive(Clone, Copy, Default)]
pub struct ExpandOptions<'a> {
    /// Drop steps marked `skip_in_quick`.
    pub quick: bool,
    /// History for longest-first ordering; without it the order is the executable order.
    pub timing: Option<Timing<'a>>,
}

/// Executables per split step name, supplied by the caller from `cargo test --no-run` output.
pub type Executables = BTreeMap<String, Vec<TestExe>>;

fn estimates(
    sub: &Plan,
    recipe: &str,
    timing: Option<&Timing<'_>>,
) -> Result<BTreeMap<String, Estimate>, PolicyError> {
    let mut out = BTreeMap::new();
    let Some(t) = timing else { return Ok(out) };
    for s in sub.steps.iter().filter(|s| s.name != "build") {
        let label = label_of(&s.name);
        if let Some(e) = primary(measure(t, recipe, Some(&label))?) {
            out.insert(s.name.clone(), e);
        }
    }
    Ok(out)
}

/// Pieces of one policy step: (name, step) with dependencies still naming policy steps.
fn pieces(
    def: &StepDef,
    exes: &Executables,
    opts: &ExpandOptions<'_>,
) -> Result<Vec<Step>, PolicyError> {
    let kind = recipe_of(&def.command).map_or(Kind::Run, |(_, k)| k);
    let plain = |kind| Step {
        name: def.name.clone(),
        argv: def.command.clone(),
        after: def.after.clone(),
        kind,
        requirements: Requirements::default(),
        hints: StepHints::default(),
    };
    let list = exes.get(&def.name).filter(|l| !l.is_empty());
    let (Some(SplitMode::CargoTest), Some(list)) = (def.split, list) else {
        return Ok(vec![plain(kind)]);
    };
    let sub = split_cargo_test(&def.command, list)?;
    let recipe = recipe_of(&def.command).map(|(r, _)| r).unwrap_or_default();
    let ests = estimates(&sub, &recipe, opts.timing.as_ref())?;
    let sub = order_longest_first(&sub, &ests)?;
    let prefix = |n: &str| format!("{}.{n}", def.name);
    Ok(sub
        .steps
        .into_iter()
        .map(|mut s| {
            s.after = if s.after.is_empty() {
                def.after.clone()
            } else {
                s.after.iter().map(|a| prefix(a)).collect()
            };
            s.name = prefix(&s.name);
            s
        })
        .collect())
}

/// Expands suite `name` into a plan.
///
/// A step with `split = "cargo-test"` becomes `STEP.build` plus `STEP.test-X` / `STEP.lib-X` pieces
/// when `exes` has executables for it, longest first by history. Steps that depended on it wait for
/// every piece. Without executables the step stays whole.
pub fn expand(
    policy: &Policy,
    name: &str,
    exes: &Executables,
    opts: &ExpandOptions<'_>,
) -> Result<Expansion, PolicyError> {
    let suite = policy
        .suites
        .get(name)
        .ok_or_else(|| PolicyError::UnknownSuite {
            name: name.to_string(),
            known: policy.suites.keys().cloned().collect::<Vec<_>>().join(", "),
        })?;
    let skipped: BTreeMap<&str, &StepDef> = suite
        .steps
        .iter()
        .filter(|s| opts.quick && s.skip_in_quick)
        .map(|s| (s.name.as_str(), s))
        .collect();
    // Dependencies on skipped steps become those steps' own dependencies.
    let resolve = |after: &[String]| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut work: Vec<&String> = after.iter().collect();
        let mut seen = BTreeSet::new();
        while let Some(d) = work.pop() {
            if !seen.insert(d.as_str()) {
                continue;
            }
            match skipped.get(d.as_str()) {
                Some(s) => work.extend(s.after.iter()),
                None => out.push(d.clone()),
            }
        }
        out.sort();
        out
    };
    let mut by_step: BTreeMap<&str, Vec<Step>> = BTreeMap::new();
    for def in suite
        .steps
        .iter()
        .filter(|s| !skipped.contains_key(s.name.as_str()))
    {
        let mut ps = pieces(def, exes, opts)?;
        for p in &mut ps {
            p.after = resolve(&p.after);
        }
        by_step.insert(def.name.as_str(), ps);
    }
    let mut steps = Vec::new();
    for def in suite
        .steps
        .iter()
        .filter(|s| !skipped.contains_key(s.name.as_str()))
    {
        for mut p in by_step.get(def.name.as_str()).cloned().unwrap_or_default() {
            // Replace a dependency on a policy step by all of that step's pieces.
            p.after = p
                .after
                .iter()
                .flat_map(|a| match by_step.get(a.as_str()) {
                    Some(ps) if suite.step(a).is_some() => {
                        ps.iter().map(|x| x.name.clone()).collect::<Vec<_>>()
                    }
                    _ => vec![a.clone()],
                })
                .collect();
            let note = p
                .name
                .strip_prefix(&format!("{}.", def.name))
                .and_then(|piece| suite.targets.get(&label_of(piece)));
            p.hints = StepHints {
                placement: def.placement.clone(),
                timeout_secs: def.timeout_secs,
                isolate: note.is_some_and(|n| n.isolate),
                long: note.is_some_and(|n| n.long),
                tools: suite.tools.clone(),
                priority: def.priority.filter(|p| *p != tasks_core::Priority::Normal),
                // set by `split_cargo_test`; the suite's hints must not lose it
                split: p.hints.split.clone(),
            };
            steps.push(p);
        }
    }
    let plan = Plan { steps };
    plan.validate()?;
    Ok(Expansion { plan })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timing::fixture::{RECIPE, history_4254};
    use crate::validate::parse;
    use std::path::Path;
    use tasks_timing::ExeKind;

    const TEXT: &str = r#"
version = 1
[suites.core]
tools = ["cargo"]
[suites.core.targets."test:lifecycle"]
long = true
[suites.core.targets."test:link"]
isolate = true
[[suites.core.steps]]
name = "fmt"
command = ["cargo", "fmt", "--check"]
skip_in_quick = true
[[suites.core.steps]]
name = "tests"
command = ["cargo", "test", "-p", "p2pcore", "--features", "dev-tools"]
split = "cargo-test"
after = ["fmt"]
placement = "nixos"
[[suites.core.steps]]
name = "report"
command = ["echo", "done"]
after = ["tests"]
"#;

    fn exe(name: &str, kind: ExeKind) -> TestExe {
        TestExe {
            package: "p2pcore".into(),
            name: name.into(),
            kind,
            path: String::new(),
        }
    }
    fn exes() -> Executables {
        let list = ["accounts", "lifecycle", "link", "group_call"]
            .iter()
            .map(|n| exe(n, ExeKind::Test))
            .collect();
        BTreeMap::from([("tests".to_string(), list)])
    }
    fn pol() -> Policy {
        parse(TEXT, Path::new("p.toml")).unwrap()
    }
    fn names(e: &Expansion) -> Vec<&str> {
        e.plan.steps.iter().map(|s| s.name.as_str()).collect()
    }

    // R-340
    #[test]
    fn r340_a_background_step_hints_background_to_the_queue_and_others_hint_nothing() {
        let text = r#"
version = 1
[suites.s]
[[suites.s.steps]]
name = "warm"
command = ["cargo", "build"]
priority = "background"
[[suites.s.steps]]
name = "check"
command = ["cargo", "check"]
"#;
        let pol = parse(text, Path::new("p.toml")).unwrap();
        let e = expand(&pol, "s", &Executables::new(), &ExpandOptions::default()).unwrap();
        let hint = |n: &str| {
            e.plan
                .steps
                .iter()
                .find(|s| s.name == n)
                .and_then(|s| s.hints.priority)
        };
        assert_eq!(hint("warm"), Some(tasks_core::Priority::Background));
        assert_eq!(hint("check"), None);
    }

    #[test]
    fn unknown_suite() {
        let e = expand(
            &pol(),
            "nope",
            &Executables::new(),
            &ExpandOptions::default(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("core"));
    }

    #[test]
    fn without_executables_stays_whole() {
        let e = expand(
            &pol(),
            "core",
            &Executables::new(),
            &ExpandOptions::default(),
        )
        .unwrap();
        assert_eq!(names(&e), vec!["fmt", "tests", "report"]);
    }

    #[test]
    fn splits_and_rewires_dependents() {
        let e = expand(&pol(), "core", &exes(), &ExpandOptions::default()).unwrap();
        let n = names(&e);
        assert_eq!(n.first(), Some(&"fmt"));
        assert!(n.contains(&"tests.build") && n.contains(&"tests.test-lifecycle"));
        let report = e.plan.steps.iter().find(|s| s.name == "report").unwrap();
        assert_eq!(report.after.len(), 5);
        let build = e
            .plan
            .steps
            .iter()
            .find(|s| s.name == "tests.build")
            .unwrap();
        assert_eq!(build.after, vec!["fmt"]);
        assert_eq!(
            e.hints("tests.test-lifecycle")
                .unwrap()
                .placement
                .as_deref(),
            Some("nixos")
        );
        assert!(e.hints("tests.test-lifecycle").unwrap().long);
        assert!(e.hints("tests.test-link").unwrap().isolate);
        assert_eq!(e.plan.validate(), Ok(()));
    }

    #[test]
    fn longest_first_from_history() {
        let h = history_4254();
        let m = vec!["nixos".to_string()];
        let opts = ExpandOptions {
            quick: false,
            timing: Some(Timing {
                history: &h,
                machines: &m,
            }),
        };
        let e = expand(&pol(), "core", &exes(), &opts).unwrap();
        let tests: Vec<&str> = names(&e)
            .into_iter()
            .filter(|n| n.starts_with("tests.test-"))
            .collect();
        assert_eq!(
            tests,
            vec![
                "tests.test-lifecycle",
                "tests.test-group_call",
                "tests.test-link",
                "tests.test-accounts"
            ]
        );
        let _ = RECIPE;
    }

    #[test]
    fn quick_skips_and_reroutes() {
        let opts = ExpandOptions {
            quick: true,
            timing: None,
        };
        let e = expand(&pol(), "core", &exes(), &opts).unwrap();
        assert!(!names(&e).contains(&"fmt"));
        let build = e
            .plan
            .steps
            .iter()
            .find(|s| s.name == "tests.build")
            .unwrap();
        assert!(build.after.is_empty());
    }
}
