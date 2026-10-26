use super::*;
use crate::history::MemHistory;

fn m(name: &str, cores: u32) -> MachineInfo {
    MachineInfo {
        name: name.into(),
        cores,
    }
}
fn k(machine: &str) -> SampleKey {
    SampleKey {
        recipe: "cargo build".into(),
        target: None,
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
fn show(h: &MemHistory, want: Cache, others: &[MachineInfo]) -> Shown {
    shown(h, &k("laptop"), &m("laptop", 8), others, want).unwrap()
}

#[test]
fn r660_failed_and_cancelled_runs_never_make_an_estimate() {
    let mut h = MemHistory::new();
    h.add(k("laptop"), s(10.0, Cache::Warm, Outcome::Failed, 1));
    h.add(k("laptop"), s(10.0, Cache::Warm, Outcome::Cancelled, 2));
    assert_eq!(
        show(&h, Cache::Warm, &[]),
        Shown::Absent(NoEstimate::NoPassingRun)
    );
    // with a passing run among them only the passing one counts
    h.add(k("laptop"), s(100.0, Cache::Warm, Outcome::Passed, 3));
    let eta = show(&h, Cache::Warm, &[]);
    assert_eq!(eta.eta().map(|e| e.median_secs), Some(100.0));
}

#[test]
fn r661_no_run_at_all_is_a_first_run_and_only_failures_say_no_passing_run() {
    let mut h = MemHistory::new();
    assert_eq!(
        show(&h, Cache::Cold, &[m("nixos", 12)]),
        Shown::Absent(NoEstimate::FirstRun)
    );
    h.add(k("nixos"), s(10.0, Cache::Cold, Outcome::Failed, 1));
    assert_eq!(
        show(&h, Cache::Cold, &[m("nixos", 12)]),
        Shown::Absent(NoEstimate::NoPassingRun)
    );
    assert_eq!(NoEstimate::FirstRun.to_string(), "first run");
    assert_eq!(NoEstimate::NoPassingRun.to_string(), "no passing run yet");
}

#[test]
fn r662_a_first_build_here_does_not_borrow_a_warm_time() {
    let mut h = MemHistory::new();
    h.add(k("laptop"), s(20.0, Cache::Warm, Outcome::Passed, 1));
    // a cold run is not bounded by a warm one
    assert_eq!(
        show(&h, Cache::Cold, &[]),
        Shown::Absent(NoEstimate::FirstBuildHere)
    );
    // another machine's warm run does not stand in for a cold one either
    h.add(k("nixos"), s(30.0, Cache::Warm, Outcome::Passed, 1));
    assert_eq!(
        show(&h, Cache::Cold, &[m("nixos", 8)]),
        Shown::Absent(NoEstimate::FirstBuildHere)
    );
    // another machine's cold run does, labelled rough
    h.add(k("nixos"), s(240.0, Cache::Cold, Outcome::Passed, 2));
    let e = show(&h, Cache::Cold, &[m("nixos", 8)]);
    let e = e.eta().unwrap();
    assert!(e.rough && e.machine == "nixos" && e.median_secs == 240.0);
}

#[test]
fn r800_a_warm_run_is_timed_by_warm_runs_only_and_a_cold_one_by_cold_runs() {
    let mut h = MemHistory::new();
    h.add(k("laptop"), s(240.0, Cache::Cold, Outcome::Passed, 1));
    // a cold build says nothing about a rebuild
    assert_eq!(
        show(&h, Cache::Warm, &[]),
        Shown::Absent(NoEstimate::NoWarmRunHere)
    );
    assert_eq!(
        NoEstimate::NoWarmRunHere.to_string(),
        "no warm run here yet"
    );
    h.add(k("laptop"), s(20.0, Cache::Warm, Outcome::Passed, 2));
    h.add(k("laptop"), s(22.0, Cache::Warm, Outcome::Passed, 3));
    let median = |want| show(&h, want, &[]).eta().map(|e| e.median_secs);
    assert_eq!(
        median(Cache::Warm),
        Some(21.0),
        "the cold build is not mixed in"
    );
    assert_eq!(
        median(Cache::Cold),
        Some(240.0),
        "nor the rebuilds into a cold build"
    );
    // a warmth only guessed from history takes the warm runs, and any passing run before one
    assert_eq!(median(Cache::Unknown), Some(21.0));
    let mut c = MemHistory::new();
    c.add(k("laptop"), s(240.0, Cache::Cold, Outcome::Passed, 1));
    assert_eq!(
        show(&c, Cache::Unknown, &[]).eta().map(|e| e.median_secs),
        Some(240.0)
    );
    // runs of unknown warmth stand in only while there is no run of the wanted warmth
    let mut u = MemHistory::new();
    u.add(k("laptop"), s(250.0, Cache::Unknown, Outcome::Passed, 1));
    u.add(k("laptop"), s(1.0, Cache::Unknown, Outcome::Passed, 2));
    u.add(k("laptop"), s(18.0, Cache::Warm, Outcome::Passed, 3));
    assert_eq!(
        show(&u, Cache::Warm, &[]).eta().map(|e| e.median_secs),
        Some(18.0)
    );
}

#[test]
fn r662_history_from_before_warmth_was_kept_counts_for_either() {
    let mut h = MemHistory::new();
    h.add(k("laptop"), s(50.0, Cache::Unknown, Outcome::Passed, 1));
    assert!(show(&h, Cache::Cold, &[]).eta().is_some());
    assert!(show(&h, Cache::Warm, &[]).eta().is_some());
}

#[test]
fn r663_another_machines_passing_runs_are_labelled_rough_and_scaled() {
    let mut h = MemHistory::new();
    h.add(k("nixos"), s(240.0, Cache::Warm, Outcome::Passed, 1));
    let e = show(&h, Cache::Warm, &[m("nixos", 16)]);
    let eta = e.eta().unwrap();
    assert!(eta.rough);
    assert_eq!(eta.median_secs, 480.0);
    assert_eq!(eta.text(), "~8m (rough, from nixos's runs)");
    assert!(e.to_string().starts_with("~8m (rough, from nixos's runs)"));
    // this machine's own runs win and are not rough
    h.add(k("laptop"), s(60.0, Cache::Warm, Outcome::Passed, 2));
    let own = show(&h, Cache::Warm, &[m("nixos", 16)]);
    assert!(!own.eta().unwrap().rough);
    assert_eq!(own.eta().unwrap().text(), "~1m");
}

#[test]
fn r664_a_guess_for_planning_is_a_different_type_and_never_a_shown_estimate() {
    let h = MemHistory::new();
    let key = k("laptop");
    let p = planning(
        &h,
        &SampleKey {
            recipe: "cargo build".into(),
            ..key.clone()
        },
        &m("laptop", 8),
        &[],
    )
    .unwrap();
    assert_eq!(p.basis, Basis::BuiltIn);
    assert!(!p.is_measured());
    assert_eq!(p.median_secs, 300.0);
    // the same empty history shows nothing
    assert_eq!(
        shown(&h, &key, &m("laptop", 8), &[], Cache::Cold).unwrap(),
        Shown::Absent(NoEstimate::FirstRun)
    );
    // failed-only history still plans (ordering needs a number) but is not measured
    let mut f = MemHistory::new();
    f.add(key.clone(), s(5.0, Cache::Warm, Outcome::Failed, 1));
    let p = planning(&f, &key, &m("laptop", 8), &[]).unwrap();
    assert_eq!((p.basis, p.median_secs), (Basis::Guess, 5.0));
    // passing runs are measured
    f.add(key.clone(), s(50.0, Cache::Warm, Outcome::Passed, 2));
    let p = planning(&f, &key, &m("laptop", 8), &[]).unwrap();
    assert_eq!((p.basis, p.median_secs), (Basis::Passed, 50.0));
}

#[test]
fn r665_the_wanted_warmth_is_cold_until_this_machine_has_a_passing_run() {
    let none: Vec<Sample> = Vec::new();
    assert_eq!(wanted_cache(&none), Cache::Cold);
    let failed = [s(1.0, Cache::Cold, Outcome::Failed, 1)];
    assert_eq!(wanted_cache(&failed), Cache::Cold);
    let passed = [s(1.0, Cache::Cold, Outcome::Passed, 1)];
    assert_eq!(wanted_cache(&passed), Cache::Warm);
}

#[test]
fn r660_a_history_that_cannot_be_read_is_an_error_not_an_estimate() {
    let r = shown(
        &MemHistory::failing(),
        &k("laptop"),
        &m("laptop", 8),
        &[],
        Cache::Warm,
    );
    assert!(r.is_err());
}

fn eta(median: f64) -> Eta {
    Eta {
        median_secs: median,
        p90_secs: median * 2.0,
        samples: 3,
        machine: "laptop".into(),
        rough: false,
    }
}

#[test]
fn r675_past_the_usual_time_is_over_the_usual_never_zero_left() {
    let e = eta(57.0);
    assert_eq!(e.left(None, 10.0), Left::Secs(47.0));
    assert_eq!(
        e.left(None, 137.0),
        Left::Over {
            usual_secs: 57.0,
            by_secs: 80.0
        }
    );
    // per-target sums that have nothing left do not turn into "0s left" either
    assert!(matches!(e.left(Some(0.0), 137.0), Left::Over { .. }));
    // while targets still see work ahead they are believed
    assert_eq!(e.left(Some(30.0), 137.0), Left::Secs(30.0));
}
