//! Log size caps with truncation markers, the tail reader and runner notes
//! (R-109 to R-117).

use crate::{LineSplitter, RunnerError, SplitLimits};
use std::collections::{BTreeSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Substrings that mark a line the elided middle must keep (R-110).
const IMPORTANT: &[&str] = &["Running", "test result", "FAILED", "panicked", "error"];

/// True when `line` must survive elision.
pub fn is_important(line: &str) -> bool {
    IMPORTANT.iter().any(|p| line.contains(p))
}

/// Size budget of a capped log, in bytes of line text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElisionCaps {
    /// Logs up to this size are kept whole.
    pub max_bytes: usize,
    /// Longest stored line; longer lines are split first.
    pub max_line_bytes: usize,
}

impl ElisionCaps {
    fn head(&self) -> usize {
        self.max_bytes / 4
    }
    fn tail(&self) -> usize {
        self.max_bytes / 2
    }
    fn important(&self) -> usize {
        self.max_bytes / 4
    }
}

/// Streaming head/tail eliding of lines. Keeps the head, the tail and the
/// important lines of the middle in order, around exactly one marker line, and
/// states the dropped count and the full-log path as the first and last line
/// (REQUIREMENTS 5.3). Memory is bounded by the caps.
#[derive(Debug)]
pub struct Elider {
    caps: ElisionCaps,
    full_path: PathBuf,
    head: Vec<String>,
    head_bytes: usize,
    head_closed: bool,
    tail: VecDeque<String>,
    tail_bytes: usize,
    kept: Vec<String>,
    kept_bytes: usize,
    total_lines: u64,
    total_bytes: u64,
    evicted_lines: u64,
    evicted_bytes: u64,
}

impl Elider {
    /// A new elider; `full_path` is named in the notices.
    pub fn new(caps: ElisionCaps, full_path: impl Into<PathBuf>) -> Elider {
        Elider {
            caps,
            full_path: full_path.into(),
            head: Vec::new(),
            head_bytes: 0,
            head_closed: false,
            tail: VecDeque::new(),
            tail_bytes: 0,
            kept: Vec::new(),
            kept_bytes: 0,
            total_lines: 0,
            total_bytes: 0,
            evicted_lines: 0,
            evicted_bytes: 0,
        }
    }

    /// Adds the next line (without newline).
    pub fn push(&mut self, line: String) {
        let n = line.len() + 1;
        self.total_lines += 1;
        self.total_bytes += n as u64;
        if !self.head_closed && self.head_bytes + n <= self.caps.head() {
            self.head_bytes += n;
            self.head.push(line);
            return;
        }
        self.head_closed = true;
        self.tail_bytes += n;
        self.tail.push_back(line);
        while self.tail_bytes > self.caps.tail() {
            let Some(old) = self.tail.pop_front() else {
                break;
            };
            self.tail_bytes -= old.len() + 1;
            self.evicted_lines += 1;
            self.evicted_bytes += old.len() as u64 + 1;
            if is_important(&old) {
                self.keep(old);
            }
        }
    }

    /// Keeps an important line, thinning the middle of the kept list when it
    /// grows past twice its budget so memory stays bounded.
    fn keep(&mut self, line: String) {
        self.kept_bytes += line.len() + 1;
        self.kept.push(line);
        while self.kept_bytes > self.caps.important() * 2 && self.kept.len() > 2 {
            let mid = self.kept.len() / 2;
            let gone = self.kept.remove(mid);
            self.kept_bytes -= gone.len() + 1;
        }
    }

    /// Lines kept from the middle: the first and last halves that fit the budget.
    fn kept_view(&self) -> Vec<&String> {
        let budget = self.caps.important();
        let n = self.kept.len();
        let size = |l: &String| l.len() + 1;
        if self.kept.iter().map(size).sum::<usize>() <= budget {
            return self.kept.iter().collect();
        }
        for k in (0..=n / 2).rev() {
            let front = self.kept.iter().take(k);
            let back = self.kept.iter().skip(n - k);
            let bytes: usize = front.clone().chain(back.clone()).map(size).sum();
            if bytes <= budget || k == 0 {
                return front.chain(back).collect();
            }
        }
        Vec::new()
    }

    /// Finishes: returns the capped text, or `None` when nothing was elided.
    pub fn finish(self) -> Option<String> {
        if self.evicted_lines == 0 {
            return None;
        }
        let kept = self.kept_view();
        let kept_bytes: u64 = kept.iter().map(|l| l.len() as u64 + 1).sum();
        let dropped_lines = self.evicted_lines - kept.len() as u64;
        let dropped_bytes = self.evicted_bytes.saturating_sub(kept_bytes);
        let notice = format!(
            "[loadtable: log elided, {dropped_lines} lines / {dropped_bytes} bytes dropped; full log: {}]",
            self.full_path.display()
        );
        let mut out = String::new();
        let mut line = |s: &str| {
            out.push_str(s);
            out.push('\n');
        };
        line(&notice);
        for l in &self.head {
            line(l);
        }
        line(&format!(
            "[loadtable: ... {dropped_lines} lines / {dropped_bytes} bytes elided ...]"
        ));
        for l in kept {
            line(l);
        }
        for l in &self.tail {
            line(l);
        }
        line(&notice);
        Some(out)
    }
}

/// Caps a finished log in place: when it exceeds `caps.max_bytes` it is replaced
/// by the elided view (notices, head, one marker, important lines, tail). The
/// original is first copied to `full_copy` so the whole log stays retrievable
/// (REQUIREMENTS 5.2). Returns whether anything was elided. Reads in fixed-size
/// chunks, so memory stays bounded.
pub fn cap_log_file(path: &Path, full_copy: &Path, caps: ElisionCaps) -> Result<bool, RunnerError> {
    let size = std::fs::metadata(path)
        .map_err(|e| RunnerError::log(path, e))?
        .len();
    if size <= caps.max_bytes as u64 {
        return Ok(false);
    }
    let mut file = File::open(path).map_err(|e| RunnerError::log(path, e))?;
    let mut elider = Elider::new(caps, full_copy);
    let mut splitter = LineSplitter::new(SplitLimits {
        max_line_bytes: caps.max_line_bytes,
    });
    let mut chunk = vec![0u8; 64 * 1024];
    let mut lines = Vec::new();
    loop {
        let n = file
            .read(&mut chunk)
            .map_err(|e| RunnerError::log(path, e))?;
        if n == 0 {
            break;
        }
        splitter.feed(chunk.get(..n).unwrap_or(&[]), Duration::ZERO, &mut lines);
        for l in lines.drain(..) {
            elider.push(l.text);
        }
    }
    splitter.finish(Duration::ZERO, &mut lines);
    for l in lines.drain(..) {
        elider.push(l.text);
    }
    let Some(text) = elider.finish() else {
        return Ok(false);
    };
    std::fs::rename(path, full_copy).map_err(|e| RunnerError::log(full_copy, e))?;
    std::fs::write(path, text).map_err(|e| RunnerError::log(path, e))?;
    Ok(true)
}

/// Returns up to the last `n` lines of the log, never a torn one: reads at most
/// `window` bytes from the end, drops a first line cut by the window, and drops
/// an unterminated last line unless `complete` (the job is over).
pub fn tail_lines(
    path: &Path,
    n: usize,
    window: u64,
    complete: bool,
) -> Result<Vec<String>, RunnerError> {
    tail_lines_at(path, n, window, complete).map(|(lines, _)| lines)
}

/// [`tail_lines`] and the byte offset just after the last byte it covers: where a follower
/// resumes so that no byte is read twice or skipped. An unterminated last line that was left out
/// is not covered, so the follower sees it whole once it is finished.
pub fn tail_lines_at(
    path: &Path,
    n: usize,
    window: u64,
    complete: bool,
) -> Result<(Vec<String>, u64), RunnerError> {
    let mut file = File::open(path).map_err(|e| RunnerError::log(path, e))?;
    let len = file
        .metadata()
        .map_err(|e| RunnerError::log(path, e))?
        .len();
    let start = len.saturating_sub(window);
    file.seek(SeekFrom::Start(start))
        .map_err(|e| RunnerError::log(path, e))?;
    let mut bytes = Vec::new();
    file.take(window)
        .read_to_end(&mut bytes)
        .map_err(|e| RunnerError::log(path, e))?;
    let mut end = start + bytes.len() as u64;
    if !complete && bytes.last() != Some(&b'\n') {
        end = bytes
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(start, |i| start + i as u64 + 1);
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        match text.find('\n') {
            Some(i) => text = text.split_off(i + 1),
            None => text.clear(),
        }
    }
    if !complete && !text.ends_with('\n') {
        match text.rfind('\n') {
            Some(i) => text.truncate(i + 1),
            None => text.clear(),
        }
    }
    let lines: Vec<&str> = text.lines().collect();
    let from = lines.len().saturating_sub(n);
    Ok((
        lines
            .get(from..)
            .unwrap_or(&[])
            .iter()
            .map(|s| s.to_string())
            .collect(),
        end,
    ))
}

/// Appends coordinator notes (placement decision, reroute move) to a job log,
/// each key at most once (R-116, R-117). Write the placement note before the
/// spawn so it is the top of the log.
#[derive(Debug, Default)]
pub struct LogNotes {
    written: BTreeSet<String>,
}

impl LogNotes {
    /// No notes written yet.
    pub fn new() -> LogNotes {
        LogNotes::default()
    }

    /// Appends `[loadtable] text` unless `key` was already written; true if written.
    pub fn note(&mut self, log: &Path, key: &str, text: &str) -> Result<bool, RunnerError> {
        if self.written.contains(key) {
            return Ok(false);
        }
        let flat = text.replace('\n', " ");
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| RunnerError::log(log, e))?;
        writeln!(f, "[loadtable] {flat}").map_err(|e| RunnerError::log(log, e))?;
        self.written.insert(key.to_string());
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TempDir;

    fn caps(max: usize) -> ElisionCaps {
        ElisionCaps {
            max_bytes: max,
            max_line_bytes: 200,
        }
    }

    fn elide(lines: &[String], max: usize) -> Option<String> {
        let mut e = Elider::new(caps(max), "/full/job.log");
        for l in lines {
            e.push(l.clone());
        }
        e.finish()
    }

    fn filler(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("noise line {i:04}")).collect()
    }

    #[test]
    fn short_logs_are_untouched() {
        assert!(elide(&filler(5), 10_000).is_none());
    }

    #[test]
    fn r109_long_logs_keep_head_and_tail_around_one_marker() {
        let out = elide(&filler(500), 800).unwrap();
        assert_eq!(out.matches("elided ...]").count(), 1);
        assert!(out.contains("noise line 0000"));
        assert!(out.contains("noise line 0499"));
        assert!(!out.contains("noise line 0250"));
        // Notice is both the first and the last line and names the full log.
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.first(), lines.last());
        assert!(lines[0].contains("/full/job.log"));
        assert!(lines[0].contains("dropped"));
    }

    #[test]
    fn r110_elided_middle_keeps_important_lines_in_order() {
        let mut v = filler(200);
        v[60] = "Running unittests src/lib.rs".into();
        v[61] = "test result: ok. 3 passed".into();
        v[100] = "thread 'x' panicked at src/a.rs:1".into();
        let out = elide(&v, 1200).unwrap();
        let a = out.find("Running unittests").unwrap();
        let b = out.find("test result: ok").unwrap();
        let c = out.find("panicked at").unwrap();
        assert!(a < b && b < c);
        assert!(!out.contains("noise line 0090"));
    }

    #[test]
    fn r110_many_important_lines_keep_first_and_last_halves() {
        let mut v = filler(20);
        v.extend((0..400).map(|i| format!("error number {i:04}")));
        v.extend(filler(100));
        let out = elide(&v, 1600).unwrap();
        assert!(out.contains("error number 0000"));
        assert!(out.contains("error number 0399") || out.contains("noise line 0099"));
        assert!(!out.contains("error number 0200"));
        let first = out.find("error number 0000").unwrap();
        let second = out.find("error number 0001").unwrap();
        assert!(first < second);
    }

    #[test]
    fn r109_cap_log_file_preserves_full_copy_and_writes_notice() {
        let dir = TempDir::new();
        let log = dir.path().join("job.log");
        let full = dir.path().join("job.full.log");
        let body: String = filler(600).iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&log, &body).unwrap();
        assert!(cap_log_file(&log, &full, caps(1000)).unwrap());
        assert_eq!(std::fs::read_to_string(&full).unwrap(), body);
        let capped = std::fs::read_to_string(&log).unwrap();
        assert!(capped.len() < body.len());
        assert!(capped.contains(full.to_str().unwrap()));
        // A small log is left alone.
        let small = dir.path().join("s.log");
        std::fs::write(&small, "x\n").unwrap();
        assert!(!cap_log_file(&small, &dir.path().join("s.full"), caps(1000)).unwrap());
    }

    #[test]
    fn r111_tail_never_returns_a_torn_line() {
        let dir = TempDir::new();
        let log = dir.path().join("t.log");
        std::fs::write(&log, "alpha\nbravo\ncharlie\ndelt").unwrap();
        // Running job: unterminated last line is withheld.
        assert_eq!(
            tail_lines(&log, 2, 1 << 20, false).unwrap(),
            ["bravo", "charlie"]
        );
        // Finished job: the final line is real.
        assert_eq!(
            tail_lines(&log, 2, 1 << 20, true).unwrap(),
            ["charlie", "delt"]
        );
        // Window cuts "alpha" in half: the torn head line is dropped.
        let got = tail_lines(&log, 10, 18, false).unwrap();
        assert_eq!(got, ["charlie"]);
        assert!(tail_lines(&log, 0, 100, true).unwrap().is_empty());
    }

    #[test]
    fn r116_r117_a_note_reaches_the_log_exactly_once() {
        let dir = TempDir::new();
        let log = dir.path().join("n.log");
        let mut notes = LogNotes::new();
        assert!(
            notes
                .note(&log, "placement", "placed on nixos (wait 0s)")
                .unwrap()
        );
        assert!(
            !notes
                .note(&log, "placement", "placed on nixos (wait 0s)")
                .unwrap()
        );
        assert!(
            notes
                .note(&log, "move", "moved to laptop\nreroute")
                .unwrap()
        );
        assert!(!notes.note(&log, "move", "again").unwrap());
        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            text,
            "[loadtable] placed on nixos (wait 0s)\n[loadtable] moved to laptop reroute\n"
        );
    }
}
