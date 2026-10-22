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

/// Colours the agents get, chosen to stay apart on light and dark backgrounds.
const PALETTE: [[u8; 3]; 20] = [
    [0x1f, 0x77, 0xb4],
    [0xff, 0x7f, 0x0e],
    [0x2c, 0xa0, 0x2c],
    [0xd6, 0x27, 0x28],
    [0x94, 0x67, 0xbd],
    [0x8c, 0x56, 0x4b],
    [0xe3, 0x77, 0xc2],
    [0x7f, 0x7f, 0x7f],
    [0xbc, 0xbd, 0x22],
    [0x17, 0xbe, 0xcf],
    [0x39, 0x3b, 0x79],
    [0xad, 0x49, 0x4a],
    [0x00, 0x80, 0x80],
    [0xb8, 0x86, 0x0b],
    [0x4a, 0x90, 0xd9],
    [0xc7, 0x15, 0x85],
    [0x66, 0xa6, 0x1e],
    [0xe6, 0x55, 0x00],
    [0x5f, 0x6b, 0x7a],
    [0x7b, 0x2d, 0x8e],
];

/// A stable colour for an agent name: the same name always gets the same colour, across
/// refreshes and runs (R-204).
pub fn agent_color(agent: &str) -> [u8; 3] {
    // FNV-1a: stable across runs and platforms, unlike the std hasher.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in agent.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let i = usize::try_from(h % PALETTE.len() as u64).unwrap_or(0);
    PALETTE.get(i).copied().unwrap_or([0x7f, 0x7f, 0x7f])
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
    fn r204_agent_colours_are_stable_and_distinct() {
        let names = ["claude", "codex", "v2-ui", "v2-core", "hard-errors", "lead"];
        let colours: Vec<_> = names.iter().map(|n| agent_color(n)).collect();
        for (n, c) in names.iter().zip(&colours) {
            assert_eq!(agent_color(n), *c, "stable");
        }
        for (i, a) in colours.iter().enumerate() {
            for b in colours.iter().skip(i + 1) {
                assert_ne!(a, b, "distinct for the fixture agents");
            }
        }
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
