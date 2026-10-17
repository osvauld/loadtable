//! Bounded, read-only search of retained job logs (`<state>/jobs/<id>/job.log`).
//!
//! Used by `tasks mcp` `search_logs` on this machine's ledger and, through the peer link's
//! `jobs/0` `search_logs` request, by a peer's daemon on its own ledger. Nothing is written.
//!
//! * Pattern: a plain substring (default) or a small regular-expression subset ([`Pattern`]):
//!   the `regex` crate is not in this workspace's dependency tree, so this matcher is a
//!   linear-time NFA over literals, `.`, classes (`[a-z_]`, `[^...]`, `\d \w \s` and their
//!   negations), the quantifiers `* + ?`, the anchors `^ $`, top-level `|` and at most one
//!   un-nested `( )` group without alternation or quantifier ([`Pattern::captures`]). No
//!   counted repetition or back-references (refused with a clear error).
//! * Filters: owner (exact, or the agent part before `@`), job ids, phases, a time window
//!   (a job matches when its [submitted, finished-or-now] interval overlaps it) and machine.
//! * Bounds, newest job first: `max_jobs` logs scanned, `max_matches` matches, `max_bytes`
//!   log bytes read and `max_output_bytes` of returned text. Hitting any bound sets
//!   `truncated` with the reason; nothing is silently dropped.
use crate::{Job, Phase};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

pub const DEFAULT_MAX_JOBS: usize = 200;
pub const MAX_MAX_JOBS: usize = 5000;
pub const DEFAULT_MAX_MATCHES: usize = 50;
pub const MAX_MAX_MATCHES: usize = 1000;
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_MAX_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 128 * 1024;
pub const MAX_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub const MAX_CONTEXT: usize = 10;
/// Returned lines are cut to this many bytes (on a char boundary) with a `…` marker.
pub const MAX_LINE_BYTES: usize = 1000;
/// Lines are classified on at most this prefix (longer lines still count as one line).
const MAX_SCAN_LINE: usize = 64 * 1024;
const MAX_PATTERN: usize = 1000;

/// What to search for and where. Serialized as the peer request body (opaque to the peer
/// crate), so every field has a default and unknown fields are ignored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Query {
    pub pattern: String,
    /// Treat `pattern` as the [`Pattern`] regex subset instead of a substring.
    pub regex: bool,
    pub ignore_case: bool,
    /// Exact owner, or the agent part (`who` before `@host`).
    pub owner: Option<String>,
    /// Only these job ids (empty: all).
    pub job_ids: Vec<i64>,
    /// Only these phases (snake_case names; empty: all).
    pub phases: Vec<String>,
    /// Unix seconds, already resolved (see [`parse_time`]).
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub context: usize,
    pub max_jobs: usize,
    pub max_matches: usize,
    pub max_bytes: u64,
    pub max_output_bytes: usize,
    /// Job ids to skip (a peer skips the jobs whose logs the asking machine mirrors).
    pub exclude_job_ids: Vec<i64>,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            regex: false,
            ignore_case: false,
            owner: None,
            job_ids: vec![],
            phases: vec![],
            since: None,
            until: None,
            context: 0,
            max_jobs: DEFAULT_MAX_JOBS,
            max_matches: DEFAULT_MAX_MATCHES,
            max_bytes: DEFAULT_MAX_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            exclude_job_ids: vec![],
        }
    }
}

impl Query {
    /// Clamps the bounds into their allowed ranges.
    pub fn clamped(mut self) -> Self {
        self.context = self.context.min(MAX_CONTEXT);
        self.max_jobs = self.max_jobs.clamp(1, MAX_MAX_JOBS);
        self.max_matches = self.max_matches.clamp(1, MAX_MAX_MATCHES);
        self.max_bytes = self.max_bytes.clamp(1, MAX_MAX_BYTES);
        self.max_output_bytes = self.max_output_bytes.clamp(1024, MAX_MAX_OUTPUT_BYTES);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Match {
    pub job_id: i64,
    pub owner: String,
    /// Where the job ran: this machine's id, or the peer's name for a forwarded job.
    pub machine: String,
    pub phase: String,
    /// 1-based line number in the retained log.
    pub line_number: u64,
    pub line: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// For a job forwarded to a peer: the job's id there (its log is mirrored here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_job_id: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    /// The machine that searched (its ledger and logs).
    pub machine: String,
    pub matches: Vec<Match>,
    /// Jobs passing the filters (before the `max_jobs` bound).
    pub jobs_matching_filters: usize,
    pub jobs_scanned: usize,
    pub bytes_scanned: u64,
    pub truncated: bool,
    /// Which bound stopped the search (`max_jobs`, `max_matches`, `max_bytes`,
    /// `max_output_bytes`), if any.
    pub truncated_reason: Option<String>,
    /// Logs that could not be read (job id: error), bounded.
    pub errors: Vec<String>,
}

/// Unix seconds from `text`: an integer (Unix seconds), or a duration ago such as `90s`,
/// `15m`, `2h`, `3d`, `1w` (optionally followed by ` ago`), or `now`.
pub fn parse_time(text: &str, now: i64) -> Result<i64, String> {
    let t = text.trim();
    let t = t.strip_suffix("ago").map(str::trim).unwrap_or(t);
    if t == "now" {
        return Ok(now);
    }
    if let Ok(n) = t.parse::<i64>() {
        return Ok(n);
    }
    let split = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .ok_or_else(|| format!("bad time {text:?}"))?;
    let (number, unit) = t.split_at(split);
    let n: f64 = number
        .parse()
        .map_err(|_| format!("bad time {text:?}: expected unix seconds or e.g. 2h, 30m"))?;
    let unit_s = match unit.trim() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86400.0,
        "w" | "week" | "weeks" => 604800.0,
        other => {
            return Err(format!(
                "bad time unit {other:?} in {text:?}: use s, m, h, d or w (or unix seconds)"
            ));
        }
    };
    Ok(now - (n * unit_s).round() as i64)
}

// ------------------------------------------------------------------------------------------
// Pattern: substring or a linear-time regex subset.

#[derive(Clone, Debug, PartialEq)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

#[derive(Clone, Debug, PartialEq)]
enum Atom {
    Any,
    Char(char),
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Rep {
    One,
    Star,
    Opt,
}

#[derive(Clone, Debug, PartialEq)]
struct Branch {
    start: bool,
    end: bool,
    pieces: Vec<(Atom, Rep)>,
    /// One optional capture group: pieces `[from, to)`.
    group: Option<(usize, usize)>,
}

#[derive(Clone, Debug, PartialEq)]
enum Matcher {
    Substring(String),
    Regex(Vec<Branch>),
}

/// A compiled search pattern.
#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    matcher: Matcher,
    ignore_case: bool,
}

fn item_matches(item: &ClassItem, c: char) -> bool {
    match item {
        ClassItem::Char(x) => *x == c,
        ClassItem::Range(a, b) => (*a..=*b).contains(&c),
        ClassItem::Digit(neg) => c.is_ascii_digit() != *neg,
        ClassItem::Word(neg) => (c.is_alphanumeric() || c == '_') != *neg,
        ClassItem::Space(neg) => c.is_whitespace() != *neg,
    }
}

impl Atom {
    fn matches(&self, c: char) -> bool {
        match self {
            Atom::Any => true,
            Atom::Char(x) => *x == c,
            Atom::Class { negated, items } => items.iter().any(|i| item_matches(i, c)) != *negated,
        }
    }
}

fn escape(c: char) -> Result<Atom, String> {
    let class = |item| Atom::Class {
        negated: false,
        items: vec![item],
    };
    Ok(match c {
        'd' => class(ClassItem::Digit(false)),
        'D' => class(ClassItem::Digit(true)),
        'w' => class(ClassItem::Word(false)),
        'W' => class(ClassItem::Word(true)),
        's' => class(ClassItem::Space(false)),
        'S' => class(ClassItem::Space(true)),
        't' => Atom::Char('\t'),
        c if c.is_ascii_alphanumeric() => {
            return Err(format!("unsupported escape \\{c} in this regex subset"));
        }
        c => Atom::Char(c),
    })
}

fn class_escape(c: char) -> Result<ClassItem, String> {
    Ok(match escape(c)? {
        Atom::Char(c) => ClassItem::Char(c),
        Atom::Class { items, .. } => items.into_iter().next().ok_or("empty class")?,
        Atom::Any => return Err("bad class".into()),
    })
}

fn parse_branch(text: &str, ignore_case: bool) -> Result<Branch, String> {
    let fold = |c: char| {
        if ignore_case {
            c.to_lowercase().next().unwrap_or(c)
        } else {
            c
        }
    };
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut branch = Branch {
        start: false,
        end: false,
        pieces: vec![],
        group: None,
    };
    let mut open: Option<usize> = None;
    if chars.first() == Some(&'^') {
        branch.start = true;
        i = 1;
    }
    while i < chars.len() {
        let c = chars[i];
        let atom = match c {
            '$' if i + 1 == chars.len() => {
                branch.end = true;
                i += 1;
                continue;
            }
            '.' => Atom::Any,
            '\\' => {
                i += 1;
                let e = *chars.get(i).ok_or("pattern ends with a lone backslash")?;
                match escape(e)? {
                    Atom::Char(c) => Atom::Char(fold(c)),
                    other => other,
                }
            }
            '[' => {
                i += 1;
                let negated = chars.get(i) == Some(&'^');
                if negated {
                    i += 1;
                }
                let mut items = vec![];
                let mut first = true;
                loop {
                    let c = *chars.get(i).ok_or("unterminated [ class")?;
                    if c == ']' && !first {
                        break;
                    }
                    first = false;
                    let lo = if c == '\\' {
                        i += 1;
                        class_escape(*chars.get(i).ok_or("unterminated [ class")?)?
                    } else {
                        ClassItem::Char(fold(c))
                    };
                    match (&lo, chars.get(i + 1), chars.get(i + 2)) {
                        (ClassItem::Char(a), Some('-'), Some(&b)) if b != ']' => {
                            let b = fold(b);
                            if b < *a {
                                return Err(format!("bad class range {a}-{b}"));
                            }
                            items.push(ClassItem::Range(*a, b));
                            i += 3;
                        }
                        _ => {
                            items.push(lo);
                            i += 1;
                        }
                    }
                }
                Atom::Class { negated, items }
            }
            '*' | '+' | '?' => return Err(format!("nothing to repeat before {c:?}")),
            '(' if open.is_none() && branch.group.is_none() => {
                open = Some(branch.pieces.len());
                i += 1;
                continue;
            }
            ')' if open.is_some() => {
                branch.group = Some((open.take().unwrap_or(0), branch.pieces.len()));
                i += 1;
                if matches!(chars.get(i), Some('*' | '+' | '?')) {
                    return Err("a quantifier after a group is not supported".into());
                }
                continue;
            }
            '(' | ')' | '{' | '}' => {
                return Err(format!(
                    "{c:?} is not supported here (one capture group without alternation, no \
                     nesting or counted repetition); escape it as \\{c} to match it literally"
                ));
            }
            '^' => return Err("^ is only an anchor at the start of a branch".into()),
            '$' => return Err("$ is only an anchor at the end of a branch".into()),
            c => Atom::Char(fold(c)),
        };
        i += 1;
        match chars.get(i) {
            Some('*') => {
                branch.pieces.push((atom, Rep::Star));
                i += 1;
            }
            Some('?') => {
                branch.pieces.push((atom, Rep::Opt));
                i += 1;
            }
            Some('+') => {
                branch.pieces.push((atom.clone(), Rep::One));
                branch.pieces.push((atom, Rep::Star));
                i += 1;
            }
            _ => branch.pieces.push((atom, Rep::One)),
        }
        if matches!(chars.get(i), Some('*' | '+' | '?')) {
            return Err("stacked quantifiers are not supported".into());
        }
    }
    if open.is_some() {
        return Err("unclosed ( group".into());
    }
    Ok(branch)
}

/// A simulation thread: capture start/end (char positions) of the group, if entered.
type Thread = Option<(Option<usize>, Option<usize>)>;

impl Branch {
    /// Thompson simulation: states are piece indexes; O(len(line) x len(pieces)). Without
    /// `captures` it returns at the first accepting position; with it, it keeps going and
    /// returns the longest match of the leftmost start with the group's span.
    fn run(&self, line: &[char], captures: bool) -> Option<Option<(usize, usize)>> {
        let n = self.pieces.len();
        let (g0, g1) = self.group.unwrap_or((usize::MAX, usize::MAX));
        let mark = |t: &mut (Option<usize>, Option<usize>), state: usize, pos: usize| {
            if state == g0 {
                t.0 = Some(pos);
            }
            if state == g1 {
                t.1 = Some(pos);
            }
        };
        let close = |set: &mut Vec<Thread>, pos: usize| {
            for i in 0..n {
                if let Some(t) = set[i]
                    && matches!(self.pieces[i].1, Rep::Star | Rep::Opt)
                    && set[i + 1].is_none()
                {
                    let mut t = t;
                    mark(&mut t, i + 1, pos);
                    set[i + 1] = Some(t);
                }
            }
        };
        let fresh = |pos: usize| {
            let mut t = (None, None);
            mark(&mut t, 0, pos);
            Some(t)
        };
        let mut current: Vec<Thread> = vec![None; n + 1];
        current[0] = fresh(0);
        close(&mut current, 0);
        let mut best: Option<Option<(usize, usize)>> = None;
        let accept = |set: &[Thread], at_end: bool| -> Option<Option<(usize, usize)>> {
            let t = set[n]?;
            (!self.end || at_end).then_some(match t {
                (Some(a), Some(b)) => Some((a, b)),
                _ => None,
            })
        };
        if let Some(found) = accept(&current, line.is_empty()) {
            if !captures {
                return Some(found);
            }
            best = Some(found);
        }
        for (pos, &c) in line.iter().enumerate() {
            let mut next: Vec<Thread> = vec![None; n + 1];
            for (i, slot) in current.iter().enumerate().take(n) {
                let Some(t) = *slot else { continue };
                let (atom, rep) = &self.pieces[i];
                if atom.matches(c) {
                    let to = match rep {
                        Rep::One | Rep::Opt => i + 1,
                        Rep::Star => i,
                    };
                    if next[to].is_none() {
                        let mut t = t;
                        if to != i {
                            mark(&mut t, to, pos + 1);
                        }
                        next[to] = Some(t);
                    }
                }
            }
            // A new attempt may start here only while no earlier attempt is alive (leftmost).
            if !self.start && (best.is_none() || !captures) && next[0].is_none() {
                next[0] = fresh(pos + 1);
            }
            close(&mut next, pos + 1);
            current = next;
            if let Some(found) = accept(&current, pos + 1 == line.len()) {
                if !captures {
                    return Some(found);
                }
                best = Some(found);
            }
            if captures && best.is_some() && current.iter().all(Option::is_none) {
                break;
            }
        }
        best
    }
}

impl Pattern {
    pub fn new(pattern: &str, regex: bool, ignore_case: bool) -> Result<Self, String> {
        if pattern.is_empty() {
            return Err("empty pattern".into());
        }
        if pattern.len() > MAX_PATTERN {
            return Err(format!("pattern longer than {MAX_PATTERN} bytes"));
        }
        let matcher = if regex {
            Matcher::Regex(
                pattern
                    .split('|')
                    .map(|b| parse_branch(b, ignore_case))
                    .collect::<Result<_, _>>()
                    .map_err(|e| format!("pattern {pattern:?}: {e}"))?,
            )
        } else if ignore_case {
            Matcher::Substring(pattern.to_lowercase())
        } else {
            Matcher::Substring(pattern.to_owned())
        };
        Ok(Self {
            matcher,
            ignore_case,
        })
    }

    pub fn is_match(&self, line: &str) -> bool {
        match &self.matcher {
            Matcher::Substring(needle) if self.ignore_case => line.to_lowercase().contains(needle),
            Matcher::Substring(needle) => line.contains(needle.as_str()),
            Matcher::Regex(branches) => {
                let chars: Vec<char> = if self.ignore_case {
                    line.chars()
                        .map(|c| c.to_lowercase().next().unwrap_or(c))
                        .collect()
                } else {
                    line.chars().collect()
                };
                branches.iter().any(|b| b.run(&chars, false).is_some())
            }
        }
    }

    /// For a regex with a `( )` group: `Some(Some(group text))` on a match, `Some(None)` on
    /// a match without a group, `None` without a match. Substring patterns never capture.
    pub fn captures(&self, line: &str) -> Option<Option<String>> {
        let Matcher::Regex(branches) = &self.matcher else {
            return self.is_match(line).then_some(None);
        };
        let original: Vec<char> = line.chars().collect();
        let chars: Vec<char> = if self.ignore_case {
            original
                .iter()
                .map(|c| c.to_lowercase().next().unwrap_or(*c))
                .collect()
        } else {
            original.clone()
        };
        for b in branches {
            if let Some(span) = b.run(&chars, true) {
                return Some(span.map(|(a, z)| original[a..z].iter().collect()));
            }
        }
        None
    }
}

// ------------------------------------------------------------------------------------------
// Search.

fn phase_name(phase: &Phase) -> String {
    phase.as_db().unwrap_or_else(|_| format!("{phase:?}"))
}

/// Does `owner` match the filter (exact, or the agent part before `@`)?
pub fn owner_matches(owner: &str, wanted: &str) -> bool {
    owner == wanted
        || owner
            .split_once('@')
            .is_some_and(|(agent, _)| agent == wanted)
}

/// Does the job's [submitted, finished-or-now] interval overlap [since, until]?
fn in_window(job: &Job, since: Option<i64>, until: Option<i64>, now: i64) -> bool {
    let start = job.submitted_at;
    let end = job
        .execution
        .as_ref()
        .and_then(|e| e.finished_at)
        .unwrap_or(if job.phase.terminal() { start } else { now });
    since.is_none_or(|s| end >= s) && until.is_none_or(|u| start <= u)
}

fn cut(line: &str) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line.to_owned();
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

/// Where a job ran, for the result: (machine, remote job id). Forwarded jobs name their peer.
pub type MachineOf<'a> = dyn Fn(&Job) -> (String, Option<i64>) + 'a;

/// The jobs passing the filters, newest first (before the `max_jobs` bound).
pub fn select<'a>(jobs: &'a [Job], q: &Query, now: i64) -> Vec<&'a Job> {
    let ids: HashSet<i64> = q.job_ids.iter().copied().collect();
    let excluded: HashSet<i64> = q.exclude_job_ids.iter().copied().collect();
    let mut out: Vec<&Job> = jobs
        .iter()
        .filter(|j| ids.is_empty() || ids.contains(&j.id))
        .filter(|j| !excluded.contains(&j.id))
        .filter(|j| {
            q.owner
                .as_deref()
                .is_none_or(|o| owner_matches(&j.spec.owner, o))
        })
        .filter(|j| q.phases.is_empty() || q.phases.contains(&phase_name(&j.phase)))
        .filter(|j| in_window(j, q.since, q.until, now))
        .collect();
    out.sort_by_key(|j| std::cmp::Reverse(j.id));
    out
}

/// Searches the logs of `jobs` under `state_dir` (`jobs/<id>/job.log`), newest first.
/// `machine_of` names where each job ran; `keep` filters on it (e.g. a machine filter).
pub fn search(
    state_dir: &Path,
    jobs: &[Job],
    q: &Query,
    machine: &str,
    now: i64,
    machine_of: &MachineOf,
    keep: &dyn Fn(&str) -> bool,
) -> Result<Report, String> {
    let q = q.clone().clamped();
    let pattern = Pattern::new(&q.pattern, q.regex, q.ignore_case)?;
    let mut report = Report {
        machine: machine.to_owned(),
        ..Default::default()
    };
    let candidates: Vec<(&Job, String, Option<i64>)> = select(jobs, &q, now)
        .into_iter()
        .filter_map(|j| {
            let (m, remote) = machine_of(j);
            keep(&m).then_some((j, m, remote))
        })
        .collect();
    report.jobs_matching_filters = candidates.len();
    let mut output = 0usize;
    let stop = |report: &mut Report, why: &str| {
        report.truncated = true;
        report
            .truncated_reason
            .get_or_insert_with(|| why.to_owned());
    };
    'jobs: for (job, job_machine, remote) in candidates {
        if report.jobs_scanned >= q.max_jobs {
            stop(&mut report, "max_jobs");
            break;
        }
        if report.bytes_scanned >= q.max_bytes {
            stop(&mut report, "max_bytes");
            break;
        }
        let path = state_dir
            .join("jobs")
            .join(job.id.to_string())
            .join("job.log");
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report.jobs_scanned += 1;
                continue;
            }
            Err(e) => {
                if report.errors.len() < 20 {
                    report.errors.push(format!("job {}: {e}", job.id));
                }
                continue;
            }
        };
        report.jobs_scanned += 1;
        let budget = q.max_bytes - report.bytes_scanned;
        let mut reader = BufReader::new(file.take(budget));
        let mut before: VecDeque<String> = VecDeque::new();
        // Matches still collecting `after` context: (index in report.matches, lines left).
        let mut open: Vec<(usize, usize)> = vec![];
        let mut number = 0u64;
        let mut raw = Vec::new();
        let phase = phase_name(&job.phase);
        loop {
            raw.clear();
            let read = match (&mut reader)
                .take(MAX_SCAN_LINE as u64)
                .read_until(b'\n', &mut raw)
            {
                Ok(n) => n,
                Err(e) => {
                    if report.errors.len() < 20 {
                        report.errors.push(format!("job {}: {e}", job.id));
                    }
                    break;
                }
            };
            if read == 0 {
                break;
            }
            report.bytes_scanned += read as u64;
            if raw.len() >= MAX_SCAN_LINE && raw.last() != Some(&b'\n') {
                // Skip the rest of an over-long line (it still counts as one line).
                let mut rest = Vec::new();
                match reader.read_until(b'\n', &mut rest) {
                    Ok(n) => report.bytes_scanned += n as u64,
                    Err(_) => break,
                }
            }
            number += 1;
            let text = String::from_utf8_lossy(&raw);
            let line = text.trim_end_matches(['\n', '\r']);
            for (index, left) in open.iter_mut() {
                let shown = cut(line);
                output += shown.len();
                report.matches[*index].after.push(shown);
                *left -= 1;
            }
            open.retain(|(_, left)| *left > 0);
            if pattern.is_match(line) {
                if report.matches.len() >= q.max_matches {
                    stop(&mut report, "max_matches");
                    break 'jobs;
                }
                let shown = cut(line);
                let size = shown.len() + before.iter().map(String::len).sum::<usize>() + 64;
                if output + size > q.max_output_bytes {
                    stop(&mut report, "max_output_bytes");
                    break 'jobs;
                }
                output += size;
                report.matches.push(Match {
                    job_id: job.id,
                    owner: job.spec.owner.clone(),
                    machine: job_machine.clone(),
                    phase: phase.clone(),
                    line_number: number,
                    line: shown,
                    before: before.iter().cloned().collect(),
                    after: vec![],
                    remote_job_id: remote,
                });
                if q.context > 0 {
                    open.push((report.matches.len() - 1, q.context));
                }
            }
            if q.context > 0 {
                before.push_back(cut(line));
                if before.len() > q.context {
                    before.pop_front();
                }
            }
            if report.bytes_scanned >= q.max_bytes {
                stop(&mut report, "max_bytes");
                break 'jobs;
            }
        }
    }
    Ok(report)
}

/// Where a job of THIS machine's ledger ran: a job forwarded to a peer (`tasks peer-forward`
/// argv) names the peer and its remote job id (link record `<state>/peer/links/<id>.json`, or
/// the ledger's `execution.peer` once linked); every other job ran on `hostname`.
pub fn local_machine_of(
    state_dir: &Path,
    hostname: &str,
) -> impl Fn(&Job) -> (String, Option<i64>) + use<> {
    let (state_dir, hostname) = (state_dir.to_path_buf(), hostname.to_owned());
    move |job: &Job| {
        let argv = crate::metrics::history::argv(job);
        let Some(Ok(args)) = crate::peer_link::ForwardArgs::from_recipe_argv(&argv) else {
            return (hostname.clone(), None);
        };
        let link: serde_json::Value = std::fs::read(
            state_dir
                .join("peer/links")
                .join(format!("{}.json", job.id)),
        )
        .ok()
        .filter(|b| b.len() <= 64 * 1024)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
        let remote = link["remote_job_id"].as_i64().or_else(|| {
            job.execution
                .as_ref()
                .and_then(|e| e.peer.as_ref())
                .and_then(|p| p.peer_job_id)
        });
        (args.peer, remote)
    }
}

/// Remote job ids, per peer name, of this ledger's forwarded jobs: their logs are mirrored
/// here, so a peer search skips them (`Query::exclude_job_ids`).
pub fn mirrored_remote_ids(
    jobs: &[Job],
    machine_of: &MachineOf,
) -> std::collections::HashMap<String, Vec<i64>> {
    let mut out: std::collections::HashMap<String, Vec<i64>> = Default::default();
    for job in jobs {
        let argv = crate::metrics::history::argv(job);
        if !crate::peer_link::is_forward_argv(&argv) {
            continue;
        }
        if let (peer, Some(remote)) = machine_of(job) {
            out.entry(peer).or_default().push(remote);
        }
    }
    out
}

#[cfg(test)]
#[path = "log_search_tests.rs"]
mod tests;
