//! Start estimates for queued jobs (R-129): when would each one get a slot?
//!
//! The model is `slots` identical servers. Each running job frees its server after its
//! remaining time; each queued job, in queue order, takes the server that frees first and
//! holds it for its own estimated runtime.

/// Seconds until each queued job starts, in queue order. `None` for every job when the machine
/// has no slot of that kind. `running_remaining` has one entry per running job of the kind
/// (the short lane may make that more than `slots`; the extra jobs still count).
pub fn start_offsets(
    slots: usize,
    running_remaining: &[f64],
    queued_runtimes: &[f64],
) -> Vec<Option<f64>> {
    if slots == 0 {
        return vec![None; queued_runtimes.len()];
    }
    let clean = |v: f64| if v.is_finite() { v.max(0.0) } else { 0.0 };
    let mut free_at: Vec<f64> = running_remaining.iter().map(|r| clean(*r)).collect();
    free_at.sort_by(f64::total_cmp);
    // With more runners than slots only the latest `slots` finish times matter.
    if free_at.len() > slots {
        let drop = free_at.len() - slots;
        free_at.drain(..drop);
    }
    while free_at.len() < slots {
        free_at.insert(0, 0.0);
    }
    queued_runtimes
        .iter()
        .map(|runtime| {
            free_at.sort_by(f64::total_cmp);
            let start = free_at.first().copied().unwrap_or(0.0);
            if let Some(first) = free_at.first_mut() {
                *first = start + clean(*runtime);
            }
            Some(start)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // R-129
    #[test]
    fn r129_queue_start_math_single_and_multi_slot() {
        // one slot, 30 s left on the runner
        assert_eq!(
            start_offsets(1, &[30.0], &[10.0, 20.0, 5.0]),
            vec![Some(30.0), Some(40.0), Some(60.0)]
        );
        // two slots: one frees at 5 s, one at 30 s
        assert_eq!(
            start_offsets(2, &[30.0, 5.0], &[10.0, 10.0, 10.0]),
            vec![Some(5.0), Some(15.0), Some(25.0)]
        );
        // free slots start at once
        assert_eq!(
            start_offsets(3, &[], &[10.0, 10.0, 10.0, 10.0]),
            vec![Some(0.0), Some(0.0), Some(0.0), Some(10.0)]
        );
        // no slot of that kind on this machine
        assert_eq!(start_offsets(0, &[], &[1.0]), vec![None]);
        // junk estimates cannot poison the math
        assert_eq!(
            start_offsets(1, &[f64::NAN], &[f64::INFINITY, 1.0]),
            vec![Some(0.0), Some(0.0)]
        );
    }

    #[test]
    fn more_runners_than_slots_wait_for_the_latest_ones() {
        // a short-lane job runs beside two normal ones on a 2-slot kind
        assert_eq!(
            start_offsets(2, &[10.0, 20.0, 3.0], &[1.0]),
            vec![Some(10.0)]
        );
    }
}
