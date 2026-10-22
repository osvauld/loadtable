use super::*;
use crate::history::MemHistory;

fn m(name: &str, cores: u32) -> MachineInfo {
    MachineInfo {
        name: name.into(),
        cores,
    }
}
fn k(recipe: &str, target: Option<&str>, machine: &str) -> SampleKey {
    SampleKey {
        recipe: recipe.into(),
        target: target.map(Into::into),
        machine: machine.into(),
    }
}
fn s(secs: f64, cache: Cache, outcome: Outcome, at_ms: u64) -> Sample {
    Sample {
        secs,
        cache,
        outcome,
        at_ms,
    }
}
fn pass(secs: f64, at_ms: u64) -> Sample {
    s(secs, Cache::Warm, Outcome::Passed, at_ms)
}

#[test]
fn r118_learns_from_first_sample() {
    let mut h = MemHistory::new();
    h.add(k("cargo test", None, "nixos"), pass(240.0, 1));
    let e = estimate(&h, &k("cargo test", None, "nixos"), &m("nixos", 8), &[]).unwrap();
    assert_eq!((e.median_secs, e.p90_secs, e.samples), (240.0, 240.0, 1));
    assert_eq!(e.source, Source::SameMachine);
}

#[test]
fn r118_recent_runs_outweigh_old_ones() {
    let mut h = MemHistory::new();
    for i in 0..30 {
        h.add(k("r", None, "a"), pass(100.0, i));
    }
    for i in 30..50 {
        h.add(k("r", None, "a"), pass(10.0, i));
    }
    let e = estimate(&h, &k("r", None, "a"), &m("a", 4), &[]).unwrap();
    assert_eq!(e.median_secs, 10.0);
    assert_eq!(e.samples, WINDOW);
}

#[test]
fn r118_history_is_separate_per_target_and_machine() {
    let mut h = MemHistory::new();
    h.add(k("r", Some("test:a"), "x"), pass(5.0, 1));
    h.add(k("r", Some("test:b"), "x"), pass(50.0, 1));
    let a = estimate(&h, &k("r", Some("test:a"), "x"), &m("x", 4), &[]).unwrap();
    let b = estimate(&h, &k("r", Some("test:b"), "x"), &m("x", 4), &[]).unwrap();
    let whole = estimate(&h, &k("r", None, "x"), &m("x", 4), &[]).unwrap();
    assert_eq!((a.median_secs, b.median_secs), (5.0, 50.0));
    assert_eq!(whole.samples, 0);
}

#[test]
fn median_of_even_count_and_p90() {
    let mut h = MemHistory::new();
    for (i, v) in [10.0, 20.0, 30.0, 40.0].iter().enumerate() {
        h.add(k("r", None, "a"), pass(*v, i as u64));
    }
    let e = estimate(&h, &k("r", None, "a"), &m("a", 4), &[]).unwrap();
    assert_eq!((e.median_secs, e.p90_secs), (25.0, 40.0));
}

#[test]
fn r119_prefers_same_warmth_then_falls_back_with_a_note() {
    let mut h = MemHistory::new();
    h.add(k("r", None, "a"), s(10.0, Cache::Warm, Outcome::Passed, 1));
    h.add(k("r", None, "a"), s(600.0, Cache::Cold, Outcome::Passed, 2));
    let key = k("r", None, "a");
    let cold = estimate_with_cache(&h, &key, &m("a", 4), &[], Some(Cache::Cold)).unwrap();
    assert_eq!((cold.median_secs, cold.cache_fallback), (600.0, false));
    let unknown = estimate_with_cache(&h, &key, &m("a", 4), &[], Some(Cache::Unknown)).unwrap();
    assert!(unknown.cache_fallback);
    assert_eq!(unknown.samples, 2);
    assert!(unknown.to_string().contains("no runs with matching warmth"));
}

#[test]
fn cancelled_excluded_failed_only_used_when_nothing_passed() {
    let mut h = MemHistory::new();
    h.add(
        k("r", None, "a"),
        s(1.0, Cache::Warm, Outcome::Cancelled, 1),
    );
    h.add(k("r", None, "a"), s(5.0, Cache::Warm, Outcome::Failed, 2));
    let e = estimate(&h, &k("r", None, "a"), &m("a", 4), &[]).unwrap();
    assert_eq!((e.median_secs, e.samples), (5.0, 1));
    h.add(k("r", None, "a"), pass(30.0, 3));
    let e = estimate(&h, &k("r", None, "a"), &m("a", 4), &[]).unwrap();
    assert_eq!((e.median_secs, e.samples), (30.0, 1));
}

#[test]
fn only_cancelled_samples_fall_to_default() {
    let mut h = MemHistory::new();
    h.add(
        k("cargo test -p x", None, "a"),
        s(1.0, Cache::Warm, Outcome::Cancelled, 1),
    );
    let e = estimate(&h, &k("cargo test -p x", None, "a"), &m("a", 4), &[]).unwrap();
    assert_eq!(
        (e.source, e.samples, e.median_secs),
        (Source::KindDefault, 0, 180.0)
    );
}

#[test]
fn other_machine_scaled_by_core_ratio() {
    let mut h = MemHistory::new();
    h.add(k("r", None, "big"), pass(100.0, 1));
    h.add(k("r", None, "big"), pass(100.0, 2));
    h.add(k("r", None, "mid"), pass(1.0, 1));
    let e = estimate(
        &h,
        &k("r", None, "small"),
        &m("small", 4),
        &[m("mid", 8), m("big", 16)],
    )
    .unwrap();
    assert_eq!(e.median_secs, 400.0);
    assert_eq!(
        e.source,
        Source::OtherMachine {
            name: "big".into(),
            scaled_by: 4.0
        }
    );
    assert_eq!(
        e.to_string(),
        "ETA 7m (p90 7m) from 2 runs on big, scaled x4.0"
    );
}

#[test]
fn same_machine_wins_over_other() {
    let mut h = MemHistory::new();
    h.add(k("r", None, "me"), pass(10.0, 1));
    h.add(k("r", None, "big"), pass(999.0, 1));
    let e = estimate(&h, &k("r", None, "me"), &m("me", 4), &[m("big", 16)]).unwrap();
    assert_eq!(e.source, Source::SameMachine);
}

#[test]
fn kind_history_then_builtin_default() {
    let mut h = MemHistory::new();
    let e = estimate(
        &h,
        &k("gradle sel=assembleDebug", None, "a"),
        &m("a", 4),
        &[],
    )
    .unwrap();
    assert_eq!((e.median_secs, e.samples), (240.0, 0));
    assert!(e.to_string().contains("built-in default"));
    h.add(k(&kind_recipe(Kind::Package), None, "a"), pass(77.0, 1));
    let e = estimate(
        &h,
        &k("gradle sel=assembleDebug", None, "a"),
        &m("a", 4),
        &[],
    )
    .unwrap();
    assert_eq!((e.median_secs, e.samples), (77.0, 1));
}

#[test]
fn kind_guessing() {
    assert_eq!(kind_of("cargo test -p x"), Kind::Test);
    assert_eq!(kind_of("cargo check"), Kind::Compile);
    assert_eq!(kind_of("sleep 5"), Kind::Probe);
    assert_eq!(kind_of("gradle sel=connectedAndroidTest"), Kind::Device);
    assert_eq!(kind_of("gradle sel=testDebugUnitTest"), Kind::Test);
    assert_eq!(kind_of("make"), Kind::Run);
}

#[test]
fn display_matches_spec_example() {
    let e = Estimate {
        median_secs: 240.0,
        p90_secs: 360.0,
        samples: 12,
        source: Source::SameMachine,
        machine: "nixos".into(),
        cache_fallback: false,
    };
    assert_eq!(e.to_string(), "ETA 4m (p90 6m) from 12 runs on nixos");
    let one = Estimate { samples: 1, ..e };
    assert!(one.to_string().ends_with("from 1 run on nixos"));
}

#[test]
fn store_errors_propagate() {
    let r = estimate(&MemHistory::failing(), &k("r", None, "a"), &m("a", 4), &[]);
    assert!(matches!(r, Err(TimingError::History(_))));
}
