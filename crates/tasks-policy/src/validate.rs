//! Validation of a parsed policy.

use crate::error::{PolicyError, Problem, Violation, Violations};
use crate::model::{Policy, SCHEMA_VERSION, Suite};
use std::collections::BTreeSet;
use std::path::Path;
use tasks_core::plan::{Plan, PlanError, Step};
use tasks_core::{Kind, Requirements, StepHints};

/// Longest accepted timeout: one day.
pub const MAX_TIMEOUT_SECS: u64 = 86_400;

fn name_ok(n: &str, dots: bool) -> bool {
    !n.is_empty()
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-') || (dots && c == '.'))
}

/// True when `argv` is `cargo ... test ...`.
pub(crate) fn is_cargo_test(argv: &[String]) -> bool {
    argv.first().map(|a| a.rsplit('/').next().unwrap_or(a)) == Some("cargo")
        && argv
            .iter()
            .skip(1)
            .find(|a| !a.starts_with('+') && !a.starts_with('-'))
            .map(String::as_str)
            == Some("test")
}

fn check_suite(name: &str, suite: &Suite, out: &mut Vec<Violation>) {
    let mut add = |step: Option<&str>, problem: Problem| {
        out.push(Violation {
            suite: name.to_string(),
            step: step.map(str::to_string),
            problem,
        })
    };
    if !name_ok(name, false) {
        add(None, Problem::BadName(name.to_string()));
    }
    if suite.steps.is_empty() {
        add(None, Problem::NoSteps);
    }
    for (key, note) in &suite.targets {
        if note.light && note.isolate {
            add(None, Problem::LightAndIsolate(key.clone()));
        }
        let ok = key
            .split_once(':')
            .is_some_and(|(k, n)| !k.is_empty() && !n.is_empty());
        if !ok {
            add(None, Problem::BadTargetKey(key.clone()));
        }
    }
    let mut names = BTreeSet::new();
    for s in &suite.steps {
        let sn = Some(s.name.as_str());
        if !name_ok(&s.name, true) {
            add(sn, Problem::BadName(s.name.clone()));
        } else if !names.insert(s.name.as_str()) {
            add(sn, Problem::DuplicateStep(s.name.clone()));
        }
    }
    let mut local_ok = true;
    for s in &suite.steps {
        let sn = Some(s.name.as_str());
        if s.command.first().is_none_or(|w| w.is_empty()) {
            add(sn, Problem::EmptyCommand);
            local_ok = false;
        }
        if s.command.iter().any(|a| a.contains('\0')) {
            add(sn, Problem::NulInCommand);
        }
        if let Some(t) = s.timeout_secs
            && (t == 0 || t > MAX_TIMEOUT_SECS)
        {
            add(sn, Problem::BadTimeout(t));
        }
        if s.placement.as_deref().is_some_and(|p| p.trim().is_empty()) {
            add(sn, Problem::EmptyPlacement);
        }
        if s.priority == Some(tasks_core::Priority::Blocking) {
            add(sn, Problem::BlockingInPolicy);
        }
        if s.light && s.isolate {
            add(sn, Problem::StepLightAndIsolate);
        }
        if s.split.is_some() && !is_cargo_test(&s.command) {
            add(sn, Problem::SplitNotCargoTest);
        }
        let mut seen = BTreeSet::new();
        for d in &s.after {
            if *d == s.name {
                add(sn, Problem::SelfAfter);
                local_ok = false;
            } else if !names.contains(d.as_str()) {
                add(sn, Problem::UnknownAfter(d.clone()));
                local_ok = false;
            } else if !seen.insert(d.as_str()) {
                add(sn, Problem::DuplicateAfter(d.clone()));
                local_ok = false;
            }
        }
    }
    // Cycle check: reuse the Plan validator once the local checks pass.
    if local_ok && names.len() == suite.steps.len() {
        let plan = Plan {
            steps: suite
                .steps
                .iter()
                .map(|s| Step {
                    name: s.name.clone(),
                    argv: s.command.clone(),
                    after: s.after.clone(),
                    kind: Kind::Run,
                    requirements: Requirements::default(),
                    hints: StepHints::default(),
                })
                .collect(),
        };
        if let Err(PlanError::Cycle(step)) = plan.validate() {
            add(Some(&step), Problem::Cycle);
        }
    }
}

/// Returns every problem in `policy`. `file` only labels the error.
pub fn validate(policy: &Policy, file: &Path) -> Result<(), PolicyError> {
    let mut out = Vec::new();
    if policy.version != SCHEMA_VERSION {
        out.push(Violation {
            suite: String::new(),
            step: None,
            problem: Problem::Version(policy.version),
        });
    }
    for (name, suite) in &policy.suites {
        check_suite(name, suite, &mut out);
    }
    check_sync(&policy.sync, &mut out);
    if out.is_empty() {
        Ok(())
    } else {
        Err(PolicyError::Invalid(Violations {
            file: file.to_path_buf(),
            list: out,
        }))
    }
}

fn check_sync(sync: &crate::model::SyncSection, out: &mut Vec<Violation>) {
    let mut bad = |why: String| {
        out.push(Violation {
            suite: String::new(),
            step: None,
            problem: Problem::BadSyncExtra(why),
        })
    };
    if sync.extra.len() > crate::model::MAX_SYNC_EXTRA {
        bad(format!(
            "more than {} entries",
            crate::model::MAX_SYNC_EXTRA
        ));
    }
    for raw in &sync.extra {
        let p = raw.trim_end_matches('/');
        if p.is_empty() {
            bad(format!("{raw:?} is empty"));
        } else if p.starts_with('/') || p.contains('\0') || p.contains('\\') {
            bad(format!(
                "{raw:?} must be a relative path without backslashes"
            ));
        } else if p
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == ".." || c == ".git")
        {
            bad(format!(
                "{raw:?} has an empty, `.`, `..` or `.git` component"
            ));
        }
    }
}

/// Parses TOML text (unknown keys are errors) and validates it.
pub fn parse(text: &str, file: &Path) -> Result<Policy, PolicyError> {
    let policy: Policy = toml::from_str(text).map_err(|e| PolicyError::Parse {
        file: file.to_path_buf(),
        message: e.to_string().trim_end().to_string(),
    })?;
    validate(&policy, file)?;
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Result<Policy, PolicyError> {
        parse(text, Path::new("/r/.tasks/run-policy.toml"))
    }
    fn problems(text: &str) -> Vec<Problem> {
        match p(text) {
            Err(PolicyError::Invalid(v)) => v.list.into_iter().map(|v| v.problem).collect(),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }
    const HEAD: &str = "version = 1\n[suites.s]\n";

    #[test]
    fn r314_sync_extras_parse_and_bad_ones_are_named() {
        let ok = p("version = 1\n[sync]\nextra = [\"local.properties\", \"config/*.json\", \"fixtures/\"]\n").unwrap();
        assert_eq!(ok.sync.extra.len(), 3);
        assert!(p("version = 1\n").unwrap().sync.is_empty());
        for bad in ["/etc/passwd", "../x", "a/../b", ".git/config", "", "a//b"] {
            let t = format!("version = 1\n[sync]\nextra = [{bad:?}]\n");
            let v = problems(&t);
            assert!(
                matches!(v.as_slice(), [Problem::BadSyncExtra(_)]),
                "{bad:?}: {v:?}"
            );
        }
        let many: Vec<String> = (0..65).map(|i| format!("\"f{i}\"")).collect();
        let v = problems(&format!(
            "version = 1\n[sync]\nextra = [{}]\n",
            many.join(",")
        ));
        assert!(v.iter().any(
            |p| matches!(p, Problem::BadSyncExtra(w) if w.contains("65") || w.contains("more than"))
        ));
        assert!(matches!(
            p("version = 1\n[sync]\nbogus = 1\n"),
            Err(PolicyError::Parse { .. })
        ));
    }

    #[test]
    fn r340_a_step_may_ask_for_background_but_not_blocking() {
        let step = |prio: &str| {
            format!(
                "{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[\"true\"]\npriority=\"{prio}\"\n"
            )
        };
        let ok = p(&step("background")).unwrap();
        assert_eq!(
            ok.suites["s"].steps[0].priority,
            Some(tasks_core::Priority::Background)
        );
        assert!(p(&step("normal")).is_ok());
        assert_eq!(problems(&step("blocking")), vec![Problem::BlockingInPolicy]);
        assert!(matches!(p(&step("urgent")), Err(PolicyError::Parse { .. })));
    }

    #[test]
    fn minimal_parses() {
        let t = format!("{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[\"true\"]\n");
        let pol = p(&t).unwrap();
        assert_eq!(pol.suites["s"].steps[0].name, "a");
    }

    #[test]
    fn unknown_keys_rejected_with_file() {
        let t = format!("{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[\"true\"]\nbogus=1\n");
        let e = p(&t).unwrap_err();
        assert!(matches!(e, PolicyError::Parse { .. }));
        let s = e.to_string();
        assert!(s.contains("run-policy.toml") && s.contains("bogus") && s.contains("fix"));
        assert!(matches!(
            p("version=1\nzzz=2\n"),
            Err(PolicyError::Parse { .. })
        ));
    }

    #[test]
    fn duplicate_and_unknown_after() {
        let t = format!(
            "{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[\"x\"]\n[[suites.s.steps]]\nname=\"a\"\ncommand=[\"x\"]\nafter=[\"nope\"]\n"
        );
        let v = problems(&t);
        assert!(v.contains(&Problem::DuplicateStep("a".into())));
        assert!(v.contains(&Problem::UnknownAfter("nope".into())));
    }

    #[test]
    fn cycle_empty_argv_timeout() {
        let t = format!(
            "{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[\"x\"]\nafter=[\"b\"]\n[[suites.s.steps]]\nname=\"b\"\ncommand=[\"x\"]\nafter=[\"a\"]\n"
        );
        assert!(problems(&t).contains(&Problem::Cycle));
        let t = format!("{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[]\n");
        assert_eq!(problems(&t), vec![Problem::EmptyCommand]);
        let t =
            format!("{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[\"x\"]\ntimeout_secs=999999\n");
        assert_eq!(problems(&t), vec![Problem::BadTimeout(999_999)]);
        let t = format!("{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[\"x\"]\ntimeout_secs=0\n");
        assert_eq!(problems(&t), vec![Problem::BadTimeout(0)]);
    }

    #[test]
    fn split_requires_cargo_test_and_message_names_location() {
        let t = format!(
            "{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[\"make\"]\nsplit=\"cargo-test\"\n"
        );
        let e = p(&t).unwrap_err().to_string();
        assert!(
            e.contains("run-policy.toml")
                && e.contains("\"s\"")
                && e.contains("\"a\"")
                && e.contains("fix:")
        );
    }

    // R-714
    #[test]
    fn r714_a_target_cannot_be_both_light_and_isolate() {
        let t = format!(
            "{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[\"x\"]\n[suites.s.targets.\"test:calls\"]\nlight=true\nisolate=true\n"
        );
        assert_eq!(
            problems(&t),
            vec![Problem::LightAndIsolate("test:calls".into())]
        );
    }

    // R-750
    #[test]
    fn r750_a_step_cannot_be_both_light_and_isolate() {
        let t = format!(
            "{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[\"x\"]\nlight=true\nisolate=true\n"
        );
        assert_eq!(problems(&t), vec![Problem::StepLightAndIsolate]);
        let ok = format!(
            "{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[\"x\"]\nlong=true\nisolate=true\n"
        );
        let step = &p(&ok).unwrap().suites["s"].steps[0];
        assert!(step.long && step.isolate);
    }

    #[test]
    fn bad_version_and_target_key() {
        assert_eq!(problems("version = 2\n"), vec![Problem::Version(2)]);
        let t = format!(
            "{HEAD}[[suites.s.steps]]\nname=\"a\"\ncommand=[\"x\"]\n[suites.s.targets.lifecycle]\nlong=true\n"
        );
        assert_eq!(
            problems(&t),
            vec![Problem::BadTargetKey("lifecycle".into())]
        );
    }
}

#[cfg(test)]
mod example_tests {
    use super::*;

    #[test]
    fn shipped_example_parses_and_expands() {
        let text = include_str!("../../../docs/examples/run-policy.toml");
        let p = parse(text, Path::new("docs/examples/run-policy.toml")).unwrap();
        assert_eq!(
            p.suites.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["check", "core-tests"]
        );
        let e = crate::expand::expand(&p, "core-tests", &Default::default(), &Default::default())
            .unwrap();
        assert_eq!(e.plan.steps.len(), 2);
    }
}
