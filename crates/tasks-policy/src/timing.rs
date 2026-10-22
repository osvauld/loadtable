//! Glue to `tasks-timing`: recipe strings and per-machine estimates for policy steps.

use std::collections::BTreeMap;
use tasks_core::Kind;
use tasks_timing::estimate::{Estimate, MachineInfo, Source, estimate};
use tasks_timing::{History, RecipeKey, SampleKey, TimingError};

/// Timing history plus the machines to look at.
#[derive(Clone, Copy)]
pub struct Timing<'a> {
    /// Where samples come from.
    pub history: &'a dyn History,
    /// Machine names to consult, in preference order.
    pub machines: &'a [String],
}

/// Test-target labels (`test:lifecycle`) known for each cargo-test step, keyed `suite/step`.
/// The caller fills it from the last `--no-run` executable listing.
pub type Candidates = BTreeMap<String, Vec<String>>;

/// The key used in [`Candidates`].
pub fn candidate_key(suite: &str, step: &str) -> String {
    format!("{suite}/{step}")
}

/// The recipe string history is stored under, and the kind it implies, for a command.
pub fn recipe_of(argv: &[String]) -> Option<(String, Kind)> {
    RecipeKey::from_argv(argv, &[])
        .first()
        .map(|k| (k.to_string(), k.kind()))
}

/// Estimates with real samples behind them, one per machine that has any. Cancelled samples and
/// kind-wide fallbacks are already excluded by `tasks_timing::estimate`.
pub fn measure(
    timing: &Timing<'_>,
    recipe: &str,
    target: Option<&str>,
) -> Result<Vec<Estimate>, TimingError> {
    let mut out = Vec::new();
    for m in timing.machines {
        let key = SampleKey {
            recipe: recipe.to_string(),
            target: target.map(str::to_string),
            machine: m.clone(),
        };
        let info = MachineInfo {
            name: m.clone(),
            cores: 1,
        };
        let e = estimate(timing.history, &key, &info, &[])?;
        if e.samples > 0 && e.source == Source::SameMachine {
            out.push(e);
        }
    }
    Ok(out)
}

/// The estimate with the most samples (first machine wins ties).
pub fn primary(mut all: Vec<Estimate>) -> Option<Estimate> {
    all.reverse();
    all.into_iter().max_by_key(|e| e.samples)
}

#[cfg(test)]
pub(crate) mod fixture {
    use tasks_timing::{Cache, MemHistory, Outcome, Sample, SampleKey};

    /// Recipe string of the p2p_chat core test command used across the tests.
    pub const RECIPE: &str = "cargo test pkg=p2pcore feat=dev-tools profile=dev";

    /// Median seconds per target from `cargo-test-4254.txt`.
    pub const T4254: &[(&str, f64)] = &[
        ("test:lifecycle", 115.55),
        ("test:group_call", 112.36),
        ("test:link", 57.02),
        ("test:idle_close", 47.78),
        ("test:private_rotation", 37.10),
        ("test:device_list_push", 27.19),
        ("test:blocking_acceptance", 23.53),
        ("test:restore_ticket", 25.06),
        ("test:calls", 19.84),
        ("test:accounts", 0.89),
        ("test:devices", 4.65),
        ("test:link_expiry", 6.17),
        ("test:tickets", 5.64),
    ];

    pub fn sample(secs: f64, at: u64) -> Sample {
        Sample {
            secs,
            cache: Cache::Warm,
            outcome: Outcome::Passed,
            at_ms: at,
        }
    }

    /// Adds `n` samples of `secs` (with a little spread) for a target on a machine.
    pub fn seed(
        h: &mut MemHistory,
        recipe: &str,
        target: Option<&str>,
        machine: &str,
        secs: f64,
        n: u64,
    ) {
        for i in 0..n {
            let wobble = 1.0 + (i as f64 - 2.0) * 0.01;
            h.add(
                SampleKey {
                    recipe: recipe.into(),
                    target: target.map(str::to_string),
                    machine: machine.into(),
                },
                sample(secs * wobble, 1000 + i),
            );
        }
    }

    /// The 4254 run, five times, on nixos.
    pub fn history_4254() -> MemHistory {
        let mut h = MemHistory::new();
        for (t, s) in T4254 {
            seed(&mut h, RECIPE, Some(t), "nixos", *s, 5);
        }
        h
    }
}
