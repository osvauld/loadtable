//! A job placed on a peer that is asleep or away waits for it instead of being refused (R-600).
//! A MacBook worker idle-sleeps within a minute and wakes for a trackpad touch or a probe; the
//! job that arrives in between should land when it is back. The wait is bounded by a grace
//! period (a machine-policy setting) and never outlasts the job's own timeout.

use std::time::Duration;

/// What a job waiting for a peer does this pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Keep waiting; the text is the job's wait text.
    Wait(String),
    /// The grace period is over; the text is why the job ends not started.
    GiveUp(String),
}

/// Everything the judgement needs, in plain values so it is testable.
pub(crate) struct Waiting<'a> {
    pub peer: &'a str,
    /// When the peer was last seen, ms since epoch; `None` when never (since this daemon started).
    pub last_seen_ms: Option<i64>,
    /// When the job was submitted: the wait is counted from there, so it survives a restart.
    pub submitted_ms: i64,
    pub now_ms: i64,
    /// The machine policy's grace.
    pub grace: Duration,
    /// The job's own timeout; the wait never outlasts it.
    pub timeout: Option<Duration>,
}

/// Seconds east of UTC at a time, for the clock times in the texts.
pub(crate) type Offset<'a> = &'a dyn Fn(i64) -> i32;

/// `16:53` in the machine's local time.
pub(crate) fn hhmm(ms: i64, offset: Offset<'_>) -> String {
    let local = ms.div_euclid(1000) + i64::from(offset(ms.div_euclid(1000)));
    let day = local.rem_euclid(86_400);
    format!("{:02}:{:02}", day / 3600, (day % 3600) / 60)
}

/// `10m`, `90s`, `1h02m`.
pub(crate) fn dur_text(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        match s % 60 {
            0 => format!("{}m", s / 60),
            r => format!("{}m{r:02}s", s / 60),
        }
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Keep waiting, or give up.
pub(crate) fn judge(w: &Waiting<'_>, offset: Offset<'_>) -> Verdict {
    let grace = w.timeout.map_or(w.grace, |t| t.min(w.grace));
    let grace_ms = i64::try_from(grace.as_millis()).unwrap_or(i64::MAX);
    let deadline = w.submitted_ms.saturating_add(grace_ms);
    let seen = w.last_seen_ms.map(|t| hhmm(t, offset));
    if w.now_ms >= deadline {
        let last = seen.map_or("never seen".to_string(), |s| format!("last seen {s}"));
        return Verdict::GiveUp(format!(
            "{} stayed unreachable for {}; {last}",
            w.peer,
            dur_text(grace)
        ));
    }
    let state = seen.map_or(
        "unreachable (not seen since this daemon started)".to_string(),
        |s| format!("asleep or unreachable since {s}"),
    );
    Verdict::Wait(format!(
        "waiting for {}: {state} (gives up at {})",
        w.peer,
        hhmm(deadline, offset)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(_: i64) -> i32 {
        0
    }

    fn w(now_ms: i64) -> Waiting<'static> {
        Waiting {
            peer: "mac",
            // 16:53:00 UTC on some day
            last_seen_ms: Some(((16 * 60 + 53) * 60) * 1000),
            submitted_ms: ((16 * 60 + 54) * 60) * 1000,
            now_ms,
            grace: Duration::from_secs(600),
            timeout: None,
        }
    }

    #[test]
    fn r600_the_wait_text_names_the_peer_when_it_went_away_and_when_it_gives_up() {
        let now = ((16 * 60 + 55) * 60) * 1000;
        assert_eq!(
            judge(&w(now), &utc),
            Verdict::Wait(
                "waiting for mac: asleep or unreachable since 16:53 (gives up at 17:04)".into()
            )
        );
    }

    #[test]
    fn r600_after_the_grace_the_job_ends_with_a_clear_reason() {
        let now = ((17 * 60 + 4) * 60) * 1000;
        assert_eq!(
            judge(&w(now), &utc),
            Verdict::GiveUp("mac stayed unreachable for 10m; last seen 16:53".into())
        );
    }

    #[test]
    fn r600_the_wait_never_outlasts_the_jobs_own_timeout() {
        let mut x = w(((16 * 60 + 56) * 60) * 1000);
        x.timeout = Some(Duration::from_secs(90));
        // submitted 16:54:00, timeout 90 s: gives up at 16:55:30, so at 16:56 it is over
        assert_eq!(
            judge(&x, &utc),
            Verdict::GiveUp("mac stayed unreachable for 1m30s; last seen 16:53".into())
        );
        x.now_ms = ((16 * 60 + 54) * 60) * 1000 + 1000;
        assert!(matches!(judge(&x, &utc), Verdict::Wait(t) if t.contains("gives up at 16:55")));
    }

    #[test]
    fn r600_a_zone_offset_moves_the_clock_times() {
        let ist = |_: i64| 5 * 3600 + 1800;
        let now = ((16 * 60 + 55) * 60) * 1000;
        let Verdict::Wait(t) = judge(&w(now), &ist) else {
            panic!("should wait");
        };
        assert!(
            t.contains("since 22:23") && t.contains("gives up at 22:34"),
            "{t}"
        );
    }

    #[test]
    fn r600_a_peer_never_seen_has_its_own_wording() {
        let mut x = w(((16 * 60 + 55) * 60) * 1000);
        x.last_seen_ms = None;
        assert!(matches!(judge(&x, &utc), Verdict::Wait(t) if t.contains("not seen since")));
        x.now_ms = ((17 * 60 + 5) * 60) * 1000;
        assert!(matches!(judge(&x, &utc), Verdict::GiveUp(t) if t.ends_with("never seen")));
    }
}
