use super::*;
use crate::estimate::Source;

fn a(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}
fn exe(pkg: &str, name: &str, kind: ExeKind) -> TestExe {
    TestExe {
        package: pkg.into(),
        name: name.into(),
        kind,
        path: format!("/t/{name}"),
    }
}
fn est(m: f64) -> Estimate {
    Estimate {
        median_secs: m,
        p90_secs: m,
        samples: 3,
        source: Source::SameMachine,
        machine: "x".into(),
        cache_fallback: false,
    }
}
fn names(p: &Plan) -> Vec<&str> {
    p.steps.iter().map(|s| s.name.as_str()).collect()
}

#[test]
fn r114_every_test_piece_names_the_unsplit_recipe_and_its_label() {
    let exes = [
        exe("p2pcore", "lifecycle", ExeKind::Test),
        exe("p2pcore", "group_call", ExeKind::Test),
    ];
    let argv = a("cargo test -p p2pcore --features dev-tools");
    let plan = split_cargo_test(&argv, &exes).unwrap();
    let unsplit = RecipeKey::from_argv(&argv, &[])[0].to_string();
    assert!(
        plan.steps[0].hints.split.is_none(),
        "the build is not a piece"
    );
    for (step, label) in plan.steps[1..]
        .iter()
        .zip(["test:lifecycle", "test:group_call"])
    {
        let origin = step.hints.split.as_ref().unwrap();
        assert_eq!(
            (origin.recipe.as_str(), origin.target.as_str()),
            (unsplit.as_str(), label)
        );
        assert_eq!(origin.target, label_of(&step.name));
    }
}

#[test]
fn builds_one_build_step_and_one_step_per_target() {
    let exes = [
        exe("p2pcore", "p2pcore", ExeKind::Lib),
        exe("p2pcore", "lifecycle", ExeKind::Test),
        exe("p2pcore", "tool", ExeKind::Bin),
    ];
    let argv = a("cargo test --release -p p2pcore --features dev-tools -- --nocapture");
    let plan = split_cargo_test(&argv, &exes).unwrap();
    plan.validate().unwrap();
    assert_eq!(
        names(&plan),
        ["build", "lib-p2pcore", "test-lifecycle", "bin-tool"]
    );
    assert_eq!(
        plan.steps[0].argv,
        a("cargo test --release -p p2pcore --features dev-tools --no-run")
    );
    assert_eq!(plan.steps[0].kind, Kind::Compile);
    assert_eq!(
        plan.steps[2].argv,
        a("cargo test --release -p p2pcore --features dev-tools --test lifecycle -- --nocapture")
    );
    assert_eq!(plan.steps[1].argv[plan.steps[1].argv.len() - 3], "--lib");
    assert!(
        plan.steps[1..]
            .iter()
            .all(|s| s.after == ["build"] && s.kind == Kind::Test)
    );
}

#[test]
fn original_selection_is_replaced_and_no_run_not_duplicated() {
    let argv = a("cargo test -p x --test a --test b --no-run");
    let plan = split_cargo_test(
        &argv,
        &[exe("x", "a", ExeKind::Test), exe("x", "b", ExeKind::Test)],
    )
    .unwrap();
    assert_eq!(
        plan.steps[0]
            .argv
            .iter()
            .filter(|x| *x == "--no-run")
            .count(),
        1
    );
    assert_eq!(plan.steps[1].argv, a("cargo test -p x --no-run --test a"));
    let plan = split_cargo_test(
        &a("cargo test --lib --bin t"),
        &[exe("x", "x", ExeKind::Lib)],
    )
    .unwrap();
    assert_eq!(plan.steps[1].argv, a("cargo test --lib"));
}

#[test]
fn multi_package_steps_are_narrowed_to_their_package() {
    let exes = [exe("a", "a", ExeKind::Lib), exe("b", "b", ExeKind::Lib)];
    let plan = split_cargo_test(&a("cargo test -p a -p b --features f"), &exes).unwrap();
    assert_eq!(plan.steps[1].argv, a("cargo test --features f -p a --lib"));
    assert_eq!(plan.steps[2].argv, a("cargo test --features f -p b --lib"));
    let ws = split_cargo_test(&a("cargo test --workspace --exclude z"), &exes).unwrap();
    assert_eq!(ws.steps[2].argv, a("cargo test -p b --lib"));
}

#[test]
fn colliding_names_stay_unique() {
    let exes = [
        exe("a", "calls", ExeKind::Test),
        exe("b", "calls", ExeKind::Test),
        exe("c", "calls", ExeKind::Test),
    ];
    let plan = split_cargo_test(&a("cargo test --workspace"), &exes).unwrap();
    plan.validate().unwrap();
    assert_eq!(
        names(&plan),
        ["build", "test-calls", "test-calls-b", "test-calls-c"]
    );
}

#[test]
fn rejects_non_cargo_test_and_empty() {
    let e = [exe("x", "x", ExeKind::Lib)];
    assert!(matches!(
        split_cargo_test(&a("cargo build"), &e),
        Err(TimingError::NotCargoTest(_))
    ));
    assert!(matches!(
        split_cargo_test(&a("make test"), &e),
        Err(TimingError::NotCargoTest(_))
    ));
    assert_eq!(split_cargo_test(&[], &e), Err(TimingError::EmptyArgv));
    assert_eq!(
        split_cargo_test(&a("cargo test"), &[]),
        Err(TimingError::NoTestExecutables)
    );
    assert!(split_cargo_test(&a("cargo +nightly test"), &e).is_ok());
}

fn plan5() -> Plan {
    let exes = [
        exe("x", "short", ExeKind::Test),
        exe("x", "long", ExeKind::Test),
        exe("x", "mid", ExeKind::Test),
        exe("x", "new", ExeKind::Test),
    ];
    split_cargo_test(&a("cargo test -p x"), &exes).unwrap()
}

#[test]
fn r707_longest_first_with_an_unknown_step_after_the_known_long_ones() {
    let mut est_map = BTreeMap::new();
    est_map.insert("test:short".to_string(), est(5.0));
    est_map.insert("test-long".to_string(), est(300.0));
    est_map.insert("test:mid".to_string(), est(60.0));
    let p = order_longest_first(&plan5(), &est_map).unwrap();
    p.validate().unwrap();
    // `new` has no figure: it counts as typical (60 s), so it follows `long` and `mid` (tied,
    // declared first) and goes before `short`
    assert_eq!(
        names(&p),
        ["build", "test-long", "test-mid", "test-new", "test-short"]
    );
}

#[test]
fn r707_a_step_hinted_long_goes_before_a_longer_estimate() {
    let mut plan = plan5();
    for s in &mut plan.steps {
        s.hints.long = s.name == "test-short";
    }
    let mut est_map = BTreeMap::new();
    est_map.insert("test:short".to_string(), est(5.0));
    est_map.insert("test-long".to_string(), est(300.0));
    let p = order_longest_first(&plan, &est_map).unwrap();
    assert_eq!(
        names(&p),
        ["build", "test-short", "test-long", "test-mid", "test-new"]
    );
}

#[test]
fn build_stays_first_and_ties_keep_order() {
    let p = order_longest_first(&plan5(), &BTreeMap::new()).unwrap();
    assert_eq!(
        names(&p),
        ["build", "test-short", "test-long", "test-mid", "test-new"]
    );
}

#[test]
fn ordering_respects_dependencies_in_general_plans() {
    let step = |n: &str, after: &[&str]| Step {
        name: n.into(),
        argv: vec!["x".into()],
        after: after.iter().map(|s| s.to_string()).collect(),
        kind: Kind::Run,
        requirements: Requirements::default(),
        hints: StepHints::default(),
    };
    let plan = Plan {
        steps: vec![step("c", &["b"]), step("b", &[]), step("a", &[])],
    };
    let mut m = BTreeMap::new();
    m.insert("c".to_string(), est(1000.0));
    m.insert("a".to_string(), est(1.0));
    m.insert("b".to_string(), est(2.0));
    let p = order_longest_first(&plan, &m).unwrap();
    assert_eq!(names(&p), ["b", "c", "a"]);
    let bad = Plan {
        steps: vec![step("a", &["a"])],
    };
    assert!(matches!(
        order_longest_first(&bad, &m),
        Err(TimingError::Plan(_))
    ));
}
