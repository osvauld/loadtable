//! Text and colour helpers of the view model. Pure; nothing here draws.
//!
//! An unknown value is spelled [`UNKNOWN`], never `0` (R-128).

/// How an unknown value is shown.
pub const UNKNOWN: &str = "-";

/// The agent of an owner: the part before the first `@` (R-204). An owner that starts with
/// `@` or has no name keeps the whole text, so a row never has an empty agent.
pub fn agent_of(owner: &str) -> &str {
    match owner.split_once('@') {
        Some((name, _)) if !name.is_empty() => name,
        _ => {
            if owner.is_empty() {
                "?"
            } else {
                owner
            }
        }
    }
}

/// Muted hues an agent can get (the theme's `a1..a4`).
pub const AGENT_HUES: usize = 4;

/// A stable hue index (0..[`AGENT_HUES`]) for an agent name: the same name always gets the
/// same one, across refreshes and runs (R-204, R-580). The theme maps it to a colour that
/// never reads as a status.
pub fn agent_hue(agent: &str) -> usize {
    // FNV-1a: stable across runs and platforms, unlike the std hasher. Its low bits mix
    // poorly for short names, so fold the high half in before taking the remainder.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in agent.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 32;
    usize::try_from(h % AGENT_HUES as u64).unwrap_or(0)
}

/// The coding-agent product an agent name mentions (`ui-claude` -> `claude`), shown as small
/// text next to the name. A name that is just the product shows nothing extra (R-583).
pub fn vendor_of(agent: &str) -> Option<&'static str> {
    const VENDORS: [&str; 4] = ["claude", "codex", "cursor", "pi"];
    let lower = agent.to_lowercase();
    if VENDORS.contains(&lower.as_str()) {
        return None;
    }
    // whole words only, so `pipeline` is not `pi`
    lower
        .split(|c: char| !c.is_alphanumeric())
        .find_map(|w| VENDORS.iter().copied().find(|v| *v == w))
}

/// `45s`, `3m05s`, `2h07m`, `3d04h`.
pub fn duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return UNKNOWN.to_string();
    }
    let s = secs.round() as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        3600..=86_399 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// `3m05s ago`.
pub fn ago(secs: u64) -> String {
    format!("{} ago", duration(secs as f64))
}

/// `just now`, `5 min ago`, `2 h ago`, `3 d ago`: how long ago, in words (R-583).
pub fn ago_words(secs: u64) -> String {
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

/// A duration without the noise of seconds once it is long: `45s`, `2m`, `1h20m`. For
/// estimates (`starts ~2m`), where the seconds are a guess anyway.
pub fn coarse(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return UNKNOWN.to_string();
    }
    let s = secs.round() as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", (s + 30) / 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// `1st`, `2nd`, `3rd`, `4th`, `11th`, `21st`.
pub fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The wall-clock time of `ms` in `tz`: `12:05`, or `Oct 16 12:05` when it is not the same
/// day as `now_ms` (R-586).
pub fn clock_in(ms: i64, now_ms: i64, tz: &jiff::tz::TimeZone) -> String {
    let at = |ms: i64| jiff::Timestamp::from_millisecond(ms).map(|t| t.to_zoned(tz.clone()));
    match (at(ms), at(now_ms)) {
        (Ok(z), Ok(now)) if z.date() == now.date() => {
            format!("{:02}:{:02}", z.hour(), z.minute())
        }
        (Ok(z), _) => z.strftime("%b %-d %H:%M").to_string(),
        _ => UNKNOWN.to_string(),
    }
}

/// [`clock_in`] for the machine's own zone.
pub fn local_clock(ms: i64, now_ms: i64) -> String {
    clock_in(ms, now_ms, &jiff::tz::TimeZone::system())
}

/// A path with the home directory written `~`.
pub fn tilde_path(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    if let Some(rest) = home.and_then(|h| path.strip_prefix(h).ok()) {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

/// Bytes with a binary unit: `512 B`, `1.5 KiB`, `6.2 GiB`.
pub fn bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    let unit = UNITS.get(u).copied().unwrap_or("B");
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {unit}")
    }
}

/// A percentage, or [`UNKNOWN`] (R-128).
pub fn percent(v: Option<f32>) -> String {
    v.map_or_else(|| UNKNOWN.to_string(), |v| format!("{v:.0}%"))
}

/// Pressure in words for a PSI percentage; `unknown` when not measured.
pub fn pressure_word(pct: Option<f32>) -> &'static str {
    match pct {
        None => "unknown",
        Some(p) if p < 5.0 => "none",
        Some(p) if p < 20.0 => "low",
        Some(p) if p < 50.0 => "moderate",
        Some(p) if p < 80.0 => "high",
        Some(_) => "severe",
    }
}

/// The command as shown: `NAME=value` words with a long value cut (display only).
pub fn command_display(argv: &[String]) -> String {
    argv.iter()
        .map(|a| quote_arg(&abbreviate_arg(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

const ENV_VALUE_MAX: usize = 40;

/// `NAME=value` with a long value cut for display. Anything else is returned unchanged.
pub fn abbreviate_arg(a: &str) -> String {
    let Some((name, value)) = a.split_once('=') else {
        return a.to_string();
    };
    let is_env = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    let n = value.chars().count();
    if !is_env || n <= ENV_VALUE_MAX {
        return a.to_string();
    }
    let head: String = value.chars().take(ENV_VALUE_MAX - 12).collect();
    format!("{name}={head}...({n} chars)")
}

fn quote_arg(a: &str) -> String {
    let plain = !a.is_empty()
        && a.chars()
            .all(|c| c.is_alphanumeric() || "-_./:=@%+,".contains(c));
    if plain {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', "'\\''"))
    }
}

/// Cuts `s` to `max` characters, ending in `...` when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(3)).collect();
    format!("{keep}...")
}

/// `2026-10-17 12:22:13Z` for milliseconds since the epoch (UTC; no time zone database).
pub fn utc_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r204_agent_of_takes_the_owner_up_to_the_at() {
        assert_eq!(agent_of("v2-ui@laptop"), "v2-ui");
        assert_eq!(agent_of("v2-ui@a@b"), "v2-ui");
        assert_eq!(agent_of("plain"), "plain");
        assert_eq!(agent_of("@host"), "@host");
        assert_eq!(agent_of(""), "?");
    }

    #[test]
    fn r580_agent_hues_are_stable_in_range_and_apart_for_the_fixture_agents() {
        let names = ["ui-claude", "core-codex", "hard-errors", "lint-pi", "lead"];
        for n in names {
            assert!(agent_hue(n) < AGENT_HUES);
            assert_eq!(agent_hue(n), agent_hue(n), "stable");
        }
        // the fixture's four agents each get their own hue, so a screenshot tells them apart
        let fixture: Vec<_> = ["ui-claude", "core-codex", "hard-errors", "lint-pi"]
            .iter()
            .map(|n| agent_hue(n))
            .collect();
        let mut sorted = fixture.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 4, "{fixture:?}");
    }

    #[test]
    fn r583_vendor_is_a_whole_word_of_the_name_and_never_the_name_itself() {
        assert_eq!(vendor_of("ui-claude"), Some("claude"));
        assert_eq!(vendor_of("v2_codex"), Some("codex"));
        assert_eq!(vendor_of("cursor-lint"), Some("cursor"));
        assert_eq!(vendor_of("pi.worker"), Some("pi"));
        assert_eq!(vendor_of("pipeline"), None);
        assert_eq!(vendor_of("hard-errors"), None);
        assert_eq!(vendor_of("claude"), None, "the name already says it");
        assert_eq!(vendor_of("Claude"), None);
    }

    #[test]
    fn r583_times_read_in_words() {
        assert_eq!(ago_words(5), "just now");
        assert_eq!(ago_words(60), "1 min ago");
        assert_eq!(ago_words(600), "10 min ago");
        assert_eq!(ago_words(7300), "2 h ago");
        assert_eq!(ago_words(200_000), "2 d ago");
        assert_eq!(coarse(45.0), "45s");
        assert_eq!(coarse(125.0), "2m");
        assert_eq!(coarse(300.0), "5m");
        assert_eq!(coarse(4800.0), "1h20m");
        assert_eq!(coarse(f64::NAN), UNKNOWN);
        assert_eq!(ordinal(1), "1st");
        assert_eq!(ordinal(2), "2nd");
        assert_eq!(ordinal(3), "3rd");
        assert_eq!(ordinal(4), "4th");
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(12), "12th");
        assert_eq!(ordinal(21), "21st");
    }

    #[test]
    fn r586_clock_times_are_local_and_name_the_day_only_when_it_differs() {
        let tz = jiff::tz::TimeZone::fixed(jiff::tz::offset(2));
        // 2026-10-17 12:22:13Z is 14:22 at +02:00
        let now = 1_792_239_733_000;
        assert_eq!(clock_in(now, now, &tz), "14:22");
        assert_eq!(clock_in(now - 900_000, now, &tz), "14:07");
        assert_eq!(clock_in(now - 86_400_000, now, &tz), "Oct 16 14:22");
        assert_eq!(clock_in(0, now, &jiff::tz::TimeZone::UTC), "Jan 1 00:00");
    }

    #[test]
    fn r586_home_is_written_as_a_tilde() {
        use std::path::Path;
        let home = Some(Path::new("/home/demo"));
        assert_eq!(
            tilde_path(Path::new("/home/demo/work/tasks"), home),
            "~/work/tasks"
        );
        assert_eq!(tilde_path(Path::new("/home/demo"), home), "~");
        assert_eq!(
            tilde_path(Path::new("/home/demo2/x"), home),
            "/home/demo2/x"
        );
        assert_eq!(tilde_path(Path::new("/srv/x"), None), "/srv/x");
    }

    #[test]
    fn durations_bytes_and_percentages_read_plainly() {
        assert_eq!(duration(0.4), "0s");
        assert_eq!(duration(59.6), "1m00s");
        assert_eq!(duration(185.0), "3m05s");
        assert_eq!(duration(7_620.0), "2h07m");
        assert_eq!(duration(f64::NAN), UNKNOWN);
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(3 << 29), "1.5 GiB");
        assert_eq!(utc_time(0), "1970-01-01 00:00:00Z");
        assert_eq!(utc_time(1_792_239_733_000), "2026-10-17 12:22:13Z");
    }

    #[test]
    fn r128_unknown_values_are_never_shown_as_zero() {
        assert_eq!(percent(None), UNKNOWN);
        assert_eq!(percent(Some(0.0)), "0%");
        assert_eq!(pressure_word(None), "unknown");
        assert_eq!(pressure_word(Some(0.0)), "none");
        assert_ne!(percent(None), percent(Some(0.0)));
    }

    #[test]
    fn long_env_values_are_cut_in_the_display_only() {
        let long = format!("TOKEN={}", "x".repeat(100));
        assert!(abbreviate_arg(&long).contains("(100 chars)"));
        assert_eq!(abbreviate_arg("A=short"), "A=short");
        assert_eq!(abbreviate_arg("not env=1"), "not env=1");
        assert_eq!(
            command_display(&["echo".into(), "a b".into()]),
            "echo 'a b'"
        );
    }
}
