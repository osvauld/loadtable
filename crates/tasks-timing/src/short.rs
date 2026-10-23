//! What history says about a job's size, for the short lane (design §10). Pure: the samples
//! come from [`History`](crate::History), nothing here asks the submitter.
//!
//! The lane is for work that is both quick and small, and the predictions lean to the cautious
//! side so that one overrun is enough to take a recipe out of it: the duration is the *worst*
//! of the last [`RECENT_RUNS`] runs, not the median, and the memory is the worst peak of the
//! same runs. A run that overruns is not killed; it finishes, its sample is recorded, and the
//! next prediction is already above the limit.

use crate::history::{Outcome, Sample};

/// How many of the latest runs the short-lane prediction looks at.
pub const RECENT_RUNS: usize = 3;
/// Fewest passed runs a duration prediction needs. One run says too little.
pub const MIN_PASSED_RUNS: usize = 2;

/// The predicted duration in seconds: the longest of the latest [`RECENT_RUNS`] runs that were
/// not cancelled, provided at least [`MIN_PASSED_RUNS`] runs passed. `None` is "unknown", which
/// is never short.
pub fn predicted_secs(samples: &[Sample]) -> Option<f64> {
    let mut v: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.outcome != Outcome::Cancelled && s.secs.is_finite() && s.secs >= 0.0)
        .collect();
    if v.iter().filter(|s| s.outcome == Outcome::Passed).count() < MIN_PASSED_RUNS {
        return None;
    }
    v.sort_by_key(|s| std::cmp::Reverse(s.at_ms));
    v.truncate(RECENT_RUNS);
    v.iter().map(|s| s.secs).max_by(f64::total_cmp)
}

/// The predicted peak memory in bytes from the peaks of the latest runs, newest first (`None`
/// for a run that measured none): the largest known one, but only when the newest run measured
/// its own. A recipe whose latest run is unmeasured is unknown, which is never short.
pub fn predicted_peak(newest_first: &[Option<u64>]) -> Option<u64> {
    let latest = newest_first.first().copied().flatten()?;
    let worst = newest_first
        .iter()
        .take(RECENT_RUNS)
        .copied()
        .flatten()
        .max()
        .unwrap_or(latest);
    Some(worst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Cache;

    fn s(secs: f64, outcome: Outcome, at_ms: u64) -> Sample {
        Sample {
            secs,
            cache: Cache::Warm,
            outcome,
            at_ms,
        }
    }

    #[test]
    fn r342_the_duration_needs_two_passed_runs_and_is_the_worst_of_the_latest_three() {
        assert_eq!(predicted_secs(&[]), None);
        assert_eq!(predicted_secs(&[s(5.0, Outcome::Passed, 1)]), None);
        let two = [s(5.0, Outcome::Passed, 1), s(7.0, Outcome::Passed, 2)];
        assert_eq!(predicted_secs(&two), Some(7.0));
        // an old slow run drops out once three newer ones exist
        let many = [
            s(900.0, Outcome::Passed, 1),
            s(5.0, Outcome::Passed, 2),
            s(6.0, Outcome::Passed, 3),
            s(4.0, Outcome::Passed, 4),
        ];
        assert_eq!(predicted_secs(&many), Some(6.0));
    }

    #[test]
    fn r345_one_overrun_takes_the_recipe_out_of_the_lane_for_the_next_run() {
        let mut h = vec![
            s(5.0, Outcome::Passed, 1),
            s(6.0, Outcome::Passed, 2),
            s(5.0, Outcome::Passed, 3),
        ];
        assert_eq!(predicted_secs(&h), Some(6.0));
        // the overrun finishes (it is not killed) and is recorded like any run
        h.push(s(240.0, Outcome::Passed, 4));
        assert_eq!(predicted_secs(&h), Some(240.0));
        // a failed overrun counts too: a slow failure is still slow
        let mut f = h[..3].to_vec();
        f.push(s(240.0, Outcome::Failed, 4));
        assert_eq!(predicted_secs(&f), Some(240.0));
    }

    #[test]
    fn cancelled_and_junk_samples_are_ignored() {
        let h = [
            s(5.0, Outcome::Passed, 1),
            s(6.0, Outcome::Passed, 2),
            s(500.0, Outcome::Cancelled, 3),
            s(f64::NAN, Outcome::Passed, 4),
            s(-1.0, Outcome::Passed, 5),
        ];
        assert_eq!(predicted_secs(&h), Some(6.0));
    }

    #[test]
    fn r342_the_peak_is_unknown_unless_the_latest_run_measured_one() {
        assert_eq!(predicted_peak(&[]), None);
        assert_eq!(predicted_peak(&[None, Some(1 << 20)]), None);
        assert_eq!(predicted_peak(&[Some(1 << 20)]), Some(1 << 20));
        // the worst of the latest three known peaks
        assert_eq!(
            predicted_peak(&[Some(10), None, Some(300), Some(9_999)]),
            Some(300)
        );
    }
}
