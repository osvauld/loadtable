//! Parsing and formatting of durations as printed by cargo and gradle.

/// Parses `"1m 02s"`, `"51m 03s"`, `"2h 1m 3s"`, `"3.21s"` or `"120ms"` into seconds.
pub fn parse_secs(s: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut any = false;
    for part in s.split_whitespace() {
        let split = part.find(|c: char| c.is_ascii_alphabetic())?;
        let (num, unit) = part.split_at_checked(split)?;
        let n: f64 = num.parse().ok()?;
        total += match unit {
            "h" => n * 3600.0,
            "m" => n * 60.0,
            "s" => n,
            "ms" => n / 1000.0,
            _ => return None,
        };
        any = true;
    }
    any.then_some(total)
}

/// Rounded ETA text: `"45s"`, `"4m"`, `"1h05m"`.
pub fn fmt_eta(secs: f64) -> String {
    let s = if secs.is_finite() && secs > 0.0 {
        secs
    } else {
        0.0
    };
    if s < 60.0 {
        return format!("{}s", s.round() as u64);
    }
    let mins = (s / 60.0).round() as u64;
    if mins < 60 {
        format!("{mins}m")
    } else {
        format!("{}h{:02}m", mins / 60, mins % 60)
    }
}

/// Precise duration text: `"45s"`, `"3m10s"`, `"1h02m03s"`.
pub fn fmt_precise(secs: f64) -> String {
    let s = if secs.is_finite() && secs > 0.0 {
        secs.round() as u64
    } else {
        0
    };
    match (s / 3600, (s / 60) % 60, s % 60) {
        (0, 0, sec) => format!("{sec}s"),
        (0, m, sec) => format!("{m}m{sec:02}s"),
        (h, m, sec) => format!("{h}h{m:02}m{sec:02}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_and_gradle_durations() {
        assert_eq!(parse_secs("1m 02s"), Some(62.0));
        assert_eq!(parse_secs("51m 03s"), Some(3063.0));
        assert_eq!(parse_secs("4m 3s"), Some(243.0));
        assert_eq!(parse_secs("3.21s"), Some(3.21));
        assert_eq!(parse_secs("2h 1m 3s"), Some(7263.0));
        assert_eq!(parse_secs("120ms"), Some(0.12));
        assert_eq!(parse_secs("soon"), None);
        assert_eq!(parse_secs(""), None);
    }

    #[test]
    fn formats_eta_and_precise() {
        assert_eq!(fmt_eta(240.0), "4m");
        assert_eq!(fmt_eta(44.6), "45s");
        assert_eq!(fmt_eta(3900.0), "1h05m");
        assert_eq!(fmt_eta(f64::NAN), "0s");
        assert_eq!(fmt_precise(190.0), "3m10s");
        assert_eq!(fmt_precise(5.0), "5s");
        assert_eq!(fmt_precise(3723.0), "1h02m03s");
    }
}
