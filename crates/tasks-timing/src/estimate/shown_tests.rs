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
    // a cold run is not given the warm time, only told it takes longer (R-823)
    let first = show(&h, Cache::Cold, &[]);
    let e = first.eta().unwrap();
    assert_eq!((&e.basis, e.median_secs), (&EtaBasis::AtLeast, 20.0));
    assert_eq!(e.text(), "over ~20s (first build here)");
    // another machine's warm run does not stand in for a cold one either
    h.add(k("nixos"), s(30.0, Cache::Warm, Outcome::Passed, 1));
    assert_eq!(
        show(&h, Cache::Cold, &[m("nixos", 8)]).eta().unwrap().basis,
        EtaBasis::AtLeast
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
    // a cold build only bounds a rebuild (R-823)
    let e = show(&h, Cache::Warm, &[]);
    let e = e.eta().unwrap();
    assert_eq!((&e.basis, e.median_secs), (&EtaBasis::AtMost, 240.0));
    assert_eq!(e.text(), "under ~4m (only cold builds so far)");
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
        low_secs: None,
        high_secs: None,
        samples: 3,
        machine: "laptop".into(),
        rough: false,
        basis: EtaBasis::Exact,
    }
}

#[test]
fn r675_past_the_usual_time_is_over_the_usual_never_zero_left() {
    let e = eta(57.0);
    assert_eq!(e.left(None, 10.0), Left::Secs(47.0));
    assert_eq!(
        e.left(None, 137.0),
        Left::Over {
            fastest_secs: 57.0,
            slowest_secs: 57.0,
            by_secs: 80.0
        }
    );
    // per-target sums that have nothing left do not turn into "0s left" either
    assert!(matches!(e.left(Some(0.0), 137.0), Left::Over { .. }));
    // while targets still see work ahead they are believed
    assert_eq!(e.left(Some(30.0), 137.0), Left::Secs(30.0));
}

#[test]
fn r837_an_estimate_is_the_fastest_to_the_slowest_recent_run() {
    // the same warm build: nothing to rebuild takes seconds, a changed crate a minute
    let mut h = MemHistory::new();
    for (i, secs) in [5.0, 9.0, 12.0, 20.0, 31.0, 56.0].into_iter().enumerate() {
        h.add(k("laptop"), s(secs, Cache::Warm, Outcome::Passed, i as u64));
    }
    let shown = show(&h, Cache::Warm, &[]);
    let e = shown.eta().unwrap();
    assert_eq!((e.low_secs, e.high_secs), (Some(5.0), Some(56.0)));
    assert_eq!(e.text(), "~5s–56s");
    assert_eq!(
        shown.to_string(),
        "~5s–56s (median 16s) from 6 passing runs on laptop"
    );
    // runs that round alike are one number
    let mut h = MemHistory::new();
    for i in 0..6u64 {
        h.add(
            k("laptop"),
            s(240.0 + i as f64, Cache::Warm, Outcome::Passed, i),
        );
    }
    assert_eq!(show(&h, Cache::Warm, &[]).eta().unwrap().text(), "~4m");
}

#[test]
fn r837_a_running_job_has_time_left_to_the_fastest_and_slowest_and_is_over_only_past_both() {
    let e = Eta {
        median_secs: 16.0,
        p90_secs: 56.0,
        low_secs: Some(5.0),
        high_secs: Some(56.0),
        samples: 6,
        machine: "laptop".into(),
        rough: false,
        basis: EtaBasis::Exact,
    };
    assert_eq!(
        e.left(None, 2.0),
        Left::Between {
            low_secs: 3.0,
            high_secs: 54.0
        }
    );
    // past the median and the fastest: still inside what recent runs took, not "over"
    assert_eq!(
        e.left(None, 30.0),
        Left::Between {
            low_secs: 0.0,
            high_secs: 26.0
        }
    );
    assert_eq!(
        e.left(None, 86.0),
        Left::Over {
            fastest_secs: 5.0,
            slowest_secs: 56.0,
            by_secs: 30.0
        }
    );
}

fn gk(recipe: &str, machine: &str) -> SampleKey {
    SampleKey {
        recipe: recipe.into(),
        target: None,
        machine: machine.into(),
    }
}
fn gshow(h: &MemHistory, recipe: &str, want: Cache) -> Shown {
    shown(h, &gk(recipe, "laptop"), &m("laptop", 8), &[], want).unwrap()
}

const REL_1631: &str = "gradle sel=:app:assemblePlayRelease flags=-Pabis=x86_64,arm64-v8a,\
                        -PrequireReleaseSigning=true,-PversionCode=1631,-PversionName=0.2.4";
const REL_LIB: &str = "gradle sel=:app:assembleLiberumRelease flags=-Pabis=x86_64,arm64-v8a";

#[test]
fn r824_a_command_with_no_runs_is_estimated_from_its_familys_runs_here() {
    let mut h = MemHistory::new();
    for (i, secs) in [1627.0, 1886.0, 772.0].into_iter().enumerate() {
        h.add(
            gk(REL_1631, "laptop"),
            s(secs, Cache::Cold, Outcome::Passed, i as u64),
        );
    }
    let e = gshow(&h, REL_LIB, Cache::Cold);
    let e = e.eta().unwrap();
    assert_eq!(
        (&e.basis, e.median_secs, e.samples),
        (&EtaBasis::Similar, 1627.0, 3)
    );
    assert!(e.rough);
    assert_eq!(e.text(), "~13m–31m (like 3 similar builds here)");
    // a family's runs of the other warmth do not make a plain estimate
    assert_ne!(
        gshow(&h, REL_LIB, Cache::Warm).eta().unwrap().basis,
        EtaBasis::Similar
    );
}

#[test]
fn r823_with_no_relative_the_tools_builds_here_give_a_range_of_the_same_warmth() {
    let mut h = MemHistory::new();
    let debug = "gradle sel=:app:assemblePlayDebug flags=-Pabis=arm64-v8a";
    for (i, (secs, cache)) in [
        (893.0, Cache::Cold),
        (1929.0, Cache::Cold),
        (22.0, Cache::Warm),
    ]
    .into_iter()
    .enumerate()
    {
        h.add(
            gk(debug, "laptop"),
            s(secs, cache, Outcome::Passed, i as u64),
        );
    }
    // a dry run is no build and stays out
    h.add(
        gk("gradle sel=:app:assembleDebug flags=dry-run", "laptop"),
        s(9.0, Cache::Cold, Outcome::Passed, 9),
    );
    let e = gshow(&h, REL_1631, Cache::Cold);
    let e = e.eta().unwrap();
    assert_eq!(
        e.basis,
        EtaBasis::Range {
            tool: "gradle".into()
        }
    );
    // the range is the fastest build to the slowest (R-837); the p90 is floored at 1.5x the
    // median (R-826)
    assert_eq!((e.low_secs, e.p90_secs), (Some(893.0), 2116.5));
    assert_eq!(e.high_secs, Some(1929.0));
    assert_eq!(e.text(), "~15m–32m (rough: gradle builds here)");
    let warm = gshow(&h, REL_1631, Cache::Warm);
    assert_eq!(warm.eta().unwrap().median_secs, 22.0, "warm builds only");
}

#[test]
fn r823_a_command_with_no_relatives_at_all_still_says_why() {
    let h = MemHistory::new();
    assert_eq!(
        gshow(&h, REL_1631, Cache::Cold),
        Shown::Absent(NoEstimate::FirstRun)
    );
}

#[test]
fn r823_the_estimate_follows_the_last_six_runs() {
    let mut h = MemHistory::new();
    for i in 0..10u64 {
        let secs = if i < 4 { 1000.0 } else { 100.0 };
        h.add(k("laptop"), s(secs, Cache::Warm, Outcome::Passed, i));
    }
    let e = show(&h, Cache::Warm, &[]);
    assert_eq!(
        (e.eta().unwrap().median_secs, e.eta().unwrap().samples),
        (100.0, RECENT)
    );
}

#[test]
fn r826_the_p90_sees_twenty_runs_and_is_never_under_one_and_a_half_times_the_median() {
    // twenty runs of 100 s; the last six decide the median
    let mut h = MemHistory::new();
    for i in 0..20u64 {
        h.add(k("laptop"), s(100.0, Cache::Warm, Outcome::Passed, i));
    }
    let shown = show(&h, Cache::Warm, &[]);
    let e = shown.eta().unwrap();
    assert_eq!((e.median_secs, e.samples), (100.0, RECENT));
    assert_eq!(
        e.p90_secs, 150.0,
        "six like runs cannot see a tail: the floor"
    );
    assert_eq!(shown.to_string(), "~2m from 6 passing runs on laptop");
    // three slow runs older than the last six: the median stays, the p90 sees them
    let mut h = MemHistory::new();
    for i in 0..20u64 {
        let secs = if (5..8).contains(&i) { 1000.0 } else { 100.0 };
        h.add(k("laptop"), s(secs, Cache::Warm, Outcome::Passed, i));
    }
    let e = show(&h, Cache::Warm, &[]);
    let e = e.eta().unwrap();
    assert_eq!((e.median_secs, e.p90_secs), (100.0, 1000.0));
    // a run older than twenty is forgotten by the p90 too
    let mut h = MemHistory::new();
    h.add(k("laptop"), s(1000.0, Cache::Warm, Outcome::Passed, 0));
    for i in 1..=20u64 {
        h.add(k("laptop"), s(100.0, Cache::Warm, Outcome::Passed, i));
    }
    assert_eq!(show(&h, Cache::Warm, &[]).eta().unwrap().p90_secs, 150.0);
    // another machine's runs are scaled, floor included
    let mut h = MemHistory::new();
    h.add(k("nixos"), s(100.0, Cache::Warm, Outcome::Passed, 1));
    let e = show(&h, Cache::Warm, &[m("nixos", 16)]);
    assert_eq!(
        (e.eta().unwrap().median_secs, e.eta().unwrap().p90_secs),
        (200.0, 300.0)
    );
}

#[test]
fn r827_a_filtered_test_run_is_estimated_from_its_unfiltered_relatives_here() {
    let mut h = MemHistory::new();
    let lib = "cargo test pkg=tasks-daemon profile=dev sel=lib";
    for (i, secs) in [40.0, 44.0, 48.0].into_iter().enumerate() {
        h.add(
            gk(lib, "laptop"),
            s(secs, Cache::Warm, Outcome::Passed, i as u64),
        );
    }
    h.add(
        gk(
            "cargo test pkg=tasks-daemon profile=dev sel=filter:r71,lib",
            "laptop",
        ),
        s(30.0, Cache::Warm, Outcome::Passed, 9),
    );
    // a new filter: the unfiltered run and the other filter's runs are its relatives
    let r72 = "cargo test pkg=tasks-daemon profile=dev sel=filter:r72,lib";
    let e = gshow(&h, r72, Cache::Warm);
    let e = e.eta().unwrap();
    assert_eq!(
        (&e.basis, e.samples, e.median_secs),
        (&EtaBasis::Similar, 4, 42.0)
    );
    assert!(e.rough);
    assert_eq!(e.text(), "~30s–48s (like 4 similar builds here)");
    // planning takes the family's runs as measured
    let p = planning(&h, &gk(r72, "laptop"), &m("laptop", 8), &[]).unwrap();
    assert_eq!((p.basis, p.median_secs), (Basis::Passed, 42.0));
    // another package's runs are no relatives, and cargo's builds here make no range
    assert_eq!(
        gshow(
            &h,
            "cargo test pkg=tasks-cli profile=dev sel=lib",
            Cache::Warm
        ),
        Shown::Absent(NoEstimate::FirstRun)
    );
    // relatives of the other warmth only bound it (R-800)
    assert_eq!(
        gshow(&h, r72, Cache::Cold).eta().unwrap().basis,
        EtaBasis::AtLeast
    );
}

#[test]
fn r823_planning_uses_the_familys_runs_before_a_built_in_default() {
    let mut h = MemHistory::new();
    h.add(
        gk(REL_1631, "laptop"),
        s(1600.0, Cache::Cold, Outcome::Passed, 1),
    );
    let p = planning(&h, &gk(REL_LIB, "laptop"), &m("laptop", 8), &[]).unwrap();
    assert_eq!((p.basis, p.median_secs), (Basis::Passed, 1600.0));
}

/// The laptop's gradle runs of 24-28 Oct 2026 as recorded (warmth unknown: it was only
/// guessed then). Every Android build of that week gets a number in the right range.
#[test]
fn r823_every_android_build_of_the_week_gets_a_number_in_its_range() {
    let runs: &[(&str, f64)] = &[
        (
            "gradle sel=:app:assemblePlayDebug flags=-Pabis=arm64-v8a",
            893.0,
        ),
        (
            "gradle sel=:app:assemblePlayDebug flags=-Pabis=arm64-v8a",
            660.0,
        ),
        (
            "gradle sel=:app:assemblePlayDebug flags=-Pabis=arm64-v8a",
            175.0,
        ),
        ("gradle sel=:app:assemblePlayBenchmark", 1080.0),
        ("gradle sel=:app:assemblePlayBenchmark", 655.0),
        ("gradle sel=:app:assemblePlayBenchmark", 374.0),
        ("gradle sel=:app:assemblePlayDebug", 1929.0),
        ("gradle sel=:app:assembleDebug", 849.0),
        (
            "gradle sel=:app:assemblePlayDebug,:app:testPlayDebugUnitTest",
            1566.0,
        ),
        (
            "gradle sel=:app:assemblePlayDebug,:app:testPlayDebugUnitTest",
            22.0,
        ),
        (REL_1631, 1627.0),
        (
            "gradle sel=:app:assemblePlayRelease flags=-Pabis=x86_64,arm64-v8a,\
             -PrequireReleaseSigning=true,-PversionCode=1632,-PversionName=0.2.4",
            1886.0,
        ),
    ];
    let mut h = MemHistory::new();
    for (i, (r, secs)) in runs.iter().enumerate() {
        h.add(
            gk(r, "laptop"),
            s(*secs, Cache::Unknown, Outcome::Passed, i as u64),
        );
    }
    // the next signed release: its family's two runs
    let next = gshow(
        &h,
        "gradle sel=:app:assemblePlayRelease flags=-Pabis=x86_64,arm64-v8a",
        Cache::Cold,
    );
    let e = next.eta().unwrap();
    assert_eq!((&e.basis, e.samples), (&EtaBasis::Similar, 2));
    assert!(
        (1500.0..=1900.0).contains(&e.median_secs),
        "{}",
        e.median_secs
    );
    // job 1470, a release build with no -P at all (took 716 s): the release-like runs
    let e = gshow(&h, "gradle sel=:app:assemblePlayRelease", Cache::Cold);
    let e = e.eta().unwrap();
    assert_eq!((&e.basis, e.samples), (&EtaBasis::Similar, 3));
    assert!(
        (500.0..=1100.0).contains(&e.median_secs),
        "{}",
        e.median_secs
    );
    // a unit test alone in a fresh worktree is a debug build of the same family
    let e = gshow(
        &h,
        "gradle sel=:app:testLiberumDebugUnitTest flags=tests:com.osvauld.p2p.WakeRingTest",
        Cache::Cold,
    );
    assert_eq!(e.eta().unwrap().basis, EtaBasis::Similar);
}
