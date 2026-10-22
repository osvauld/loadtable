//! Cargo's own progress line (`Building [===>   ] 12/57: foo, bar`), which gives the exact
//! unit total of a build, unlike history's median crate count (an estimate). Cargo prints it
//! on a non-terminal stderr only with `CARGO_TERM_PROGRESS_WHEN=always` and a width; the
//! worker sets both for Cargo jobs. The line is redrawn in place with `\r`, so a raw log would
//! fill with copies: [`ProgressFilter`] drops them from the retained log (keeping the last
//! one) and the worker records the latest `N/M` in `units.json` for the sampler.
//!
//! Units are not `Compiling` lines (build scripts and fresh units count too), so `N/M` is only
//! ever shown as a pair, never mixed with the `Compiling` count.
use serde::{Deserialize, Serialize};
use std::path::Path;

/// File in the job directory with the latest [`Units`].
pub const UNITS_FILE: &str = "units.json";
/// Width Cargo draws its progress line at when stderr is not a terminal.
pub const PROGRESS_WIDTH: &str = "80";
const PREFIX: &[u8] = b"Building [";
/// Longest segment held back while it might still be a progress line or a blank redraw.
const HOLD: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Units {
    pub done: u64,
    pub total: u64,
}

/// `Building [====>   ] 12/57: foo, bar` -> 12/57. None for anything else, including an
/// impossible pair (N > M or M == 0).
pub fn parse_progress(segment: &str) -> Option<Units> {
    let rest = segment.trim().strip_prefix("Building [")?;
    let rest = &rest[rest.find("] ")? + 2..];
    let (done, rest) = split_number(rest.trim_start())?;
    let (total, rest) = split_number(rest.strip_prefix('/')?)?;
    (rest.is_empty() || rest.starts_with(':') || rest.starts_with(' '))
        .then_some(Units { done, total })
        .filter(|u| u.total > 0 && u.done <= u.total)
}

fn split_number(text: &str) -> Option<(u64, &str)> {
    let end = text.bytes().take_while(u8::is_ascii_digit).count();
    Some((text[..end].parse().ok()?, &text[end..]))
}

/// Records `units` for the sampler (temp file and rename; best effort).
pub fn write(dir: &Path, units: Units) {
    let temp = dir.join("units.tmp");
    if let Ok(bytes) = serde_json::to_vec(&units)
        && std::fs::write(&temp, bytes).is_ok()
    {
        let _ = std::fs::rename(&temp, dir.join(UNITS_FILE));
    }
}

pub fn read(dir: &Path) -> Option<Units> {
    serde_json::from_slice(&std::fs::read(dir.join(UNITS_FILE)).ok()?).ok()
}

/// What to put in the log for a chunk of command output, and the newest progress seen in it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Filtered {
    pub log: Vec<u8>,
    pub latest: Option<Units>,
}

/// Streaming filter that removes Cargo progress redraws from output. Segments end at `\r` or
/// `\n`. A progress segment is dropped, and so is the blank `\r`-terminated segment Cargo
/// draws to erase the line (only once progress has been seen); everything else passes through
/// byte for byte, delayed only while a segment could still turn out to be progress.
#[derive(Debug, Default)]
pub struct ProgressFilter {
    partial: Vec<u8>,
    /// The current segment is known not to be progress and was already emitted.
    raw: bool,
    seen: bool,
    last_line: Option<String>,
}

impl ProgressFilter {
    pub fn feed(&mut self, bytes: &[u8]) -> Filtered {
        let mut out = Filtered::default();
        for &b in bytes {
            if b == b'\r' || b == b'\n' {
                self.end_segment(b, &mut out);
            } else if self.raw {
                out.log.push(b);
            } else {
                self.partial.push(b);
                if !self.could_be_held() {
                    out.log.append(&mut self.partial);
                    self.raw = true;
                }
            }
        }
        out
    }

    /// Whether the partial segment might still become a progress line or a blank redraw.
    fn could_be_held(&self) -> bool {
        let p = &self.partial;
        p.len() <= HOLD
            && (p.iter().all(u8::is_ascii_whitespace)
                || if p.len() <= PREFIX.len() {
                    PREFIX.starts_with(p)
                } else {
                    p.starts_with(PREFIX)
                })
    }

    fn end_segment(&mut self, terminator: u8, out: &mut Filtered) {
        let segment = std::mem::take(&mut self.partial);
        let raw = std::mem::replace(&mut self.raw, false);
        if !raw {
            let text = String::from_utf8_lossy(&segment);
            if let Some(units) = parse_progress(&text) {
                self.seen = true;
                self.last_line = Some(text.trim().to_string());
                out.latest = Some(units);
                return;
            }
            if self.seen && terminator == b'\r' && segment.iter().all(u8::is_ascii_whitespace) {
                return;
            }
            out.log.extend_from_slice(&segment);
        }
        out.log.push(terminator);
    }

    /// End of stream: the held partial segment, then the last progress line (once).
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.partial);
        if let Some(units_line) = Some(String::from_utf8_lossy(&out).trim().to_string())
            .filter(|t| parse_progress(t).is_some())
        {
            out.clear();
            self.last_line = Some(units_line);
        }
        if let Some(line) = self.last_line.take() {
            if !out.is_empty() {
                out.push(b'\n');
            }
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        out
    }
}

/// Progress as shown by `tasks status` and the UI: text and a 0..=1 fraction when known.
/// Cargo's own units win. Otherwise only the count is exact: history's median (`expected`) is
/// an estimate, so it is shown as "usually ~E", and gives a fraction only while the count is
/// still below it (a build past the median has no known remainder).
pub fn describe(
    crates: u64,
    expected: Option<u64>,
    units: Option<(u64, u64)>,
) -> (Option<f32>, String) {
    if let Some((done, total)) = units.filter(|&(d, t)| t > 0 && d <= t) {
        return (
            Some(done as f32 / total as f32),
            format!("{done}/{total} units"),
        );
    }
    match expected.filter(|&e| e > 0) {
        Some(e) => (
            (crates < e).then(|| crates as f32 / e as f32),
            format!("{crates} crates (usually ~{e})"),
        ),
        None => (None, format!("{crates} crates")),
    }
}

/// Sets Cargo's progress variables on `set` unless the caller already chose a value
/// (`present` says whether a name has one; `inherited` is the daemon's own value, if any).
pub fn progress_env(
    present: impl Fn(&str) -> bool,
    inherited: impl Fn(&str) -> Option<String>,
    mut set: impl FnMut(&str, String),
) {
    for (name, default) in [
        ("CARGO_TERM_PROGRESS_WHEN", "always"),
        ("CARGO_TERM_PROGRESS_WIDTH", PROGRESS_WIDTH),
    ] {
        if !present(name) {
            set(name, inherited(name).unwrap_or_else(|| default.into()));
        }
    }
}
