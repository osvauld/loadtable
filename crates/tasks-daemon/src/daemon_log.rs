//! The daemon's own log lines (stderr, which a service manager collects into a file). Every
//! line starts with a local timestamp so a gap or a late line can be placed in time.

use std::fmt::Display;
use tasks_platform::local_offset_secs;

/// `2026-10-24 11:47:51 tasks2d: text` for a moment given in ms since the epoch and the zone's
/// offset east of UTC at that moment.
pub(crate) fn stamped(ms: i64, offset_secs: i32, text: &str) -> String {
    let secs = ms.div_euclid(1000) + i64::from(offset_secs);
    let (y, m, d) = crate::logs::civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} tasks2d: {text}",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

/// The wall clock now, ms since the epoch (0 when the clock is before 1970).
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Writes one line to stderr, stamped with the local time. The one way the daemon prints.
pub fn say(text: impl Display) {
    let ms = now_ms();
    let offset = local_offset_secs(ms.div_euclid(1000));
    eprintln!("{}", stamped(ms, offset, &text.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r691_a_line_starts_with_the_local_date_and_time() {
        // 2026-10-24 11:47:51 UTC
        let ms = 1_792_842_471_000;
        assert_eq!(
            stamped(ms, 0, "peer mac is down"),
            "2026-10-24 11:47:51 tasks2d: peer mac is down"
        );
        // two hours east of UTC, across midnight
        let late = 1_792_881_600_000; // 2026-10-24 22:40:00 UTC
        assert_eq!(
            stamped(late, 2 * 3600, "x"),
            "2026-10-25 00:40:00 tasks2d: x"
        );
    }
}
