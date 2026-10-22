//! Reading job logs: tail, grep with context, follow from an offset, and the time and phase
//! filters of the cross-job search (R-112, R-114).

use crate::api::{GrepGroup, GrepLine, MAX_CONTEXT, MAX_LOG_CHUNK, MAX_TAIL_LINES};
use crate::regex_lite::Pattern;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use tasks_core::Phase;
use thiserror::Error;

/// Longest line text returned; longer lines are cut.
const MAX_LINE_CHARS: usize = 2000;

/// Why a log request cannot be served.
#[derive(Debug, Error)]
pub enum LogError {
    /// The request is invalid (R-112 rejects bad arguments).
    #[error("{0}")]
    Bad(String),
    /// The log cannot be read.
    #[error("log {path}: {source}")]
    Io {
        /// The log file.
        path: std::path::PathBuf,
        /// The cause.
        source: std::io::Error,
    },
}

fn io_err(path: &Path, source: std::io::Error) -> LogError {
    LogError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// A substring or regex-subset matcher.
#[derive(Debug, Clone)]
pub enum Matcher {
    /// Plain substring.
    Substring(String),
    /// Regex subset.
    Regex(Pattern),
}

impl Matcher {
    /// Builds a matcher; an empty or invalid pattern is refused.
    pub fn new(pattern: &str, regex: bool) -> Result<Matcher, LogError> {
        if pattern.is_empty() {
            return Err(LogError::Bad("pattern is empty".into()));
        }
        if regex {
            Pattern::new(pattern)
                .map(Matcher::Regex)
                .map_err(|e| LogError::Bad(e.to_string()))
        } else {
            Ok(Matcher::Substring(pattern.to_string()))
        }
    }

    /// True when the line matches.
    pub fn is_match(&self, line: &str) -> bool {
        match self {
            Matcher::Substring(s) => line.contains(s.as_str()),
            Matcher::Regex(p) => p.is_match(line),
        }
    }
}

/// Checks the context size.
pub fn check_context(context: usize) -> Result<(), LogError> {
    if context > MAX_CONTEXT {
        return Err(LogError::Bad(format!(
            "context {context} is above the limit of {MAX_CONTEXT}"
        )));
    }
    Ok(())
}

/// The last `lines` lines of a log, never a torn one.
pub fn tail(path: &Path, lines: usize, complete: bool) -> Result<Vec<String>, LogError> {
    if lines == 0 || lines > MAX_TAIL_LINES {
        return Err(LogError::Bad(format!(
            "lines must be between 1 and {MAX_TAIL_LINES}"
        )));
    }
    let window = (lines as u64).saturating_mul(400).clamp(64 * 1024, 8 << 20);
    tasks_runner::tail_lines(path, lines, window, complete).map_err(|e| match e {
        tasks_runner::RunnerError::Log { path, source } => LogError::Io { path, source },
        other => LogError::Bad(other.to_string()),
    })
}

/// What a grep found.
#[derive(Debug, Default, PartialEq)]
pub struct GrepOutcome {
    /// Blocks of consecutive lines.
    pub groups: Vec<GrepGroup>,
    /// Matching lines.
    pub matches: usize,
    /// True when a bound stopped the search.
    pub truncated: bool,
    /// Bytes of line text in the groups.
    pub bytes: usize,
}

fn cut(text: &str) -> String {
    if text.chars().count() > MAX_LINE_CHARS {
        let head: String = text.chars().take(MAX_LINE_CHARS).collect();
        format!("{head}...")
    } else {
        text.to_string()
    }
}

struct Collector {
    groups: Vec<GrepGroup>,
    last_n: Option<u64>,
    bytes: usize,
}

impl Collector {
    fn push(&mut self, n: u64, text: &str, matched: bool) {
        let text = cut(text);
        self.bytes += text.len() + 16;
        let line = GrepLine { n, text, matched };
        let contiguous = self.last_n.is_some_and(|l| l + 1 == n);
        match self.groups.last_mut() {
            Some(g) if contiguous => g.lines.push(line),
            _ => self.groups.push(GrepGroup { lines: vec![line] }),
        }
        self.last_n = Some(n);
    }
}

/// Searches the whole retained log. Bounds: `max_matches` lines, `max_bytes` of text, `context`
/// lines around each match (R-112, R-114).
pub fn grep_file(
    path: &Path,
    matcher: &Matcher,
    context: usize,
    max_matches: usize,
    max_bytes: usize,
) -> Result<GrepOutcome, LogError> {
    check_context(context)?;
    let file = File::open(path).map_err(|e| io_err(path, e))?;
    let mut reader = BufReader::new(file);
    let mut col = Collector {
        groups: Vec::new(),
        last_n: None,
        bytes: 0,
    };
    let mut before: VecDeque<(u64, String)> = VecDeque::new();
    let mut after_left = 0usize;
    let mut matches = 0usize;
    let mut truncated = false;
    let mut n = 0u64;
    let mut raw = Vec::new();
    loop {
        raw.clear();
        let read = reader
            .read_until(b'\n', &mut raw)
            .map_err(|e| io_err(path, e))?;
        if read == 0 {
            break;
        }
        n += 1;
        let text = String::from_utf8_lossy(&raw);
        let text = text.trim_end_matches(['\n', '\r']);
        if matcher.is_match(text) {
            if matches >= max_matches || col.bytes >= max_bytes {
                truncated = true;
                break;
            }
            matches += 1;
            for (bn, bt) in before.drain(..) {
                col.push(bn, &bt, false);
            }
            col.push(n, text, true);
            after_left = context;
        } else if after_left > 0 {
            col.push(n, text, false);
            after_left -= 1;
        } else if context > 0 {
            before.push_back((n, cut(text)));
            if before.len() > context {
                before.pop_front();
            }
        }
    }
    Ok(GrepOutcome {
        bytes: col.bytes,
        groups: col.groups,
        matches,
        truncated,
    })
}

/// Reads up to `max_bytes` from `offset`. Returns the text, the next offset and whether the
/// end of the file was reached. A partial UTF-8 character at the end is left for the next call.
pub fn follow(
    path: &Path,
    offset: u64,
    max_bytes: Option<usize>,
) -> Result<(String, u64, bool), LogError> {
    let want = max_bytes.unwrap_or(MAX_LOG_CHUNK);
    if want == 0 || want > MAX_LOG_CHUNK {
        return Err(LogError::Bad(format!(
            "max_bytes must be between 1 and {MAX_LOG_CHUNK}"
        )));
    }
    let mut file = File::open(path).map_err(|e| io_err(path, e))?;
    let len = file.metadata().map_err(|e| io_err(path, e))?.len();
    // A log that shrank (elided at the end of the job) restarts from its end.
    let start = offset.min(len);
    file.seek(SeekFrom::Start(start))
        .map_err(|e| io_err(path, e))?;
    let mut buf = Vec::new();
    file.take(want as u64)
        .read_to_end(&mut buf)
        .map_err(|e| io_err(path, e))?;
    let keep = match std::str::from_utf8(&buf) {
        Ok(_) => buf.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => buf.len(),
    };
    buf.truncate(keep);
    let next = start + buf.len() as u64;
    Ok((
        String::from_utf8_lossy(&buf).into_owned(),
        next,
        next >= len,
    ))
}

/// A phase's stable tag.
pub fn phase_tag(p: &Phase) -> &'static str {
    match p {
        Phase::Queued => "queued",
        Phase::Waiting(_) => "waiting",
        Phase::Running => "running",
        Phase::Passed => "passed",
        Phase::Failed { .. } => "failed",
        Phase::TimedOut { .. } => "timed_out",
        Phase::NotStarted { .. } => "not_started",
        Phase::Cancelled => "cancelled",
        Phase::Interrupted => "interrupted",
    }
}

/// Parses a search time: unix seconds, or a relative duration such as `90s`, `10m`, `2h`,
/// `3d`, `1w` meaning that long before `now_ms`. Returns ms since epoch (R-114).
pub fn parse_time(text: &str, now_ms: i64) -> Result<i64, LogError> {
    let t = text.trim();
    if let Ok(secs) = t.parse::<i64>() {
        return secs
            .checked_mul(1000)
            .ok_or_else(|| LogError::Bad(format!("time {t:?} is out of range")));
    }
    let Some(unit) = t.chars().last() else {
        return Err(LogError::Bad("time is empty".into()));
    };
    let num = t.strip_suffix(unit).unwrap_or(t);
    let n: i64 = num
        .parse()
        .map_err(|_| LogError::Bad(format!("time {t:?} is neither unix seconds nor like 10m")))?;
    let unit_secs = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        'w' => 604_800,
        _ => {
            return Err(LogError::Bad(format!(
                "time {t:?}: unit must be s, m, h, d or w"
            )));
        }
    };
    if n < 0 {
        return Err(LogError::Bad(format!("time {t:?} is negative")));
    }
    n.checked_mul(unit_secs * 1000)
        .map(|ms| now_ms.saturating_sub(ms))
        .ok_or_else(|| LogError::Bad(format!("time {t:?} is out of range")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_log(name: &str, text: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tasks-logs-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("job.log");
        std::fs::write(&p, text).unwrap();
        p
    }

    const LOG: &str = "one\ntwo\nERROR three\nfour\nfive\nsix\nseven\nERROR eight\nnine\n";

    // R-112
    #[test]
    fn r112_grep_prints_matches_with_line_numbers() {
        let p = temp_log("plain", LOG);
        let m = Matcher::new("ERROR", false).unwrap();
        let r = grep_file(&p, &m, 0, 100, 1 << 20).unwrap();
        assert_eq!(r.matches, 2);
        let nums: Vec<u64> = r
            .groups
            .iter()
            .flat_map(|g| g.lines.iter().map(|l| l.n))
            .collect();
        assert_eq!(nums, vec![3, 8]);
        assert_eq!(r.groups.len(), 2, "two separate groups");
    }

    // R-112
    #[test]
    fn r112_context_marks_lines_and_separates_groups() {
        let p = temp_log("ctx", LOG);
        let m = Matcher::new("ERROR", false).unwrap();
        let r = grep_file(&p, &m, 1, 100, 1 << 20).unwrap();
        let shape: Vec<Vec<(u64, bool)>> = r
            .groups
            .iter()
            .map(|g| g.lines.iter().map(|l| (l.n, l.matched)).collect())
            .collect();
        assert_eq!(
            shape,
            vec![
                vec![(2, false), (3, true), (4, false)],
                vec![(7, false), (8, true), (9, false)],
            ]
        );
        // overlapping context merges into one group
        let r = grep_file(&p, &m, 3, 100, 1 << 20).unwrap();
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].lines.len(), 9);
    }

    // R-112
    #[test]
    fn r112_bad_arguments_are_rejected() {
        let p = temp_log("bad", LOG);
        assert!(Matcher::new("", false).is_err());
        assert!(Matcher::new("a(", true).is_err());
        let m = Matcher::new("x", false).unwrap();
        assert!(grep_file(&p, &m, MAX_CONTEXT + 1, 1, 1).is_err());
        assert!(tail(&p, 0, true).is_err());
        assert!(tail(&p, MAX_TAIL_LINES + 1, true).is_err());
        assert!(follow(&p, 0, Some(0)).is_err());
        assert!(grep_file(Path::new("/nonexistent/x.log"), &m, 0, 1, 1).is_err());
    }

    // R-114: every bound is respected.
    #[test]
    fn r114_search_respects_every_bound() {
        let big: String = (0..1000).map(|i| format!("hit {i}\n")).collect();
        let p = temp_log("bounds", &big);
        let m = Matcher::new("hit", false).unwrap();
        let r = grep_file(&p, &m, 0, 5, 1 << 20).unwrap();
        assert_eq!(r.matches, 5);
        assert!(r.truncated);
        let r = grep_file(&p, &m, 0, 1000, 200).unwrap();
        assert!(r.truncated && r.matches < 1000, "{}", r.matches);
        let r = grep_file(&p, &m, 0, 5000, 1 << 20).unwrap();
        assert!(!r.truncated);
        assert_eq!(r.matches, 1000);
        let m = Matcher::new(r"hit 9\d$", true).unwrap();
        let r = grep_file(&p, &m, 0, 5000, 1 << 20).unwrap();
        assert_eq!(r.matches, 10);
    }

    // R-114
    #[test]
    fn r114_times_accept_unix_seconds_and_relative_durations() {
        let now = 10_000_000_000;
        assert_eq!(parse_time("1700000000", now).unwrap(), 1_700_000_000_000);
        assert_eq!(parse_time("10m", now).unwrap(), now - 600_000);
        assert_eq!(parse_time("2h", now).unwrap(), now - 7_200_000);
        assert_eq!(parse_time("1d", now).unwrap(), now - 86_400_000);
        for bad in ["", "m", "10x", "-5m", "ten", "1.5h"] {
            assert!(parse_time(bad, now).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn follow_reads_from_an_offset_and_never_cuts_a_character() {
        let p = temp_log("follow", "héllo wörld\nsecond\n");
        let (a, next, done) = follow(&p, 0, Some(2)).unwrap();
        assert_eq!(a, "h", "the two-byte é is not split");
        assert_eq!(next, 1);
        assert!(!done);
        let (b, next2, done2) = follow(&p, next, None).unwrap();
        assert_eq!(b, "éllo wörld\nsecond\n");
        assert!(done2);
        let (c, next3, done3) = follow(&p, next2 + 999, None).unwrap();
        assert_eq!((c.as_str(), next3, done3), ("", next2, true));
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"third\n").unwrap();
        let (d, _, _) = follow(&p, next2, None).unwrap();
        assert_eq!(d, "third\n");
    }

    #[test]
    fn tail_returns_complete_lines() {
        let p = temp_log("tail", "a\nb\nc\npartial");
        assert_eq!(tail(&p, 2, true).unwrap(), ["c", "partial"]);
        assert_eq!(tail(&p, 2, false).unwrap(), ["b", "c"]);
    }
}
