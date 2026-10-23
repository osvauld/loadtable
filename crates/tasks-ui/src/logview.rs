//! The log viewer's state: a bounded line buffer, tail then follow, and search. Pure; the
//! requests go out from a feed thread ([`crate::feed`]).

use std::collections::VecDeque;
use tasks_core::JobId;
use tasks_daemon::api::{GrepGroup, LogView, Request, Response};

/// Most lines kept; older ones are dropped as new ones arrive.
pub const MAX_LINES: usize = 5000;
/// Most bytes kept over all lines.
pub const MAX_BYTES: usize = 2 << 20;
/// Lines the first read asks for.
pub const TAIL_LINES: usize = 1000;
/// Bytes one follow read asks for.
pub const CHUNK_BYTES: usize = 64 * 1024;
/// Most matches a search asks for.
pub const SEARCH_MATCHES: usize = 200;

/// A bounded buffer of log lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogBuffer {
    lines: VecDeque<String>,
    bytes: usize,
    partial: String,
    /// Lines dropped because the buffer was full.
    pub evicted: usize,
}

impl LogBuffer {
    /// An empty buffer.
    pub fn new() -> LogBuffer {
        LogBuffer::default()
    }

    /// The kept lines, oldest first. A last line still being written is not in it.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }

    /// The line being written, if any.
    pub fn partial(&self) -> &str {
        &self.partial
    }

    /// Lines kept.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// True when no line is kept.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Bytes kept.
    pub fn byte_len(&self) -> usize {
        self.bytes + self.partial.len()
    }

    fn push_line(&mut self, line: String) {
        self.bytes += line.len();
        self.lines.push_back(line);
        while self.lines.len() > MAX_LINES || self.bytes > MAX_BYTES {
            match self.lines.pop_front() {
                Some(old) => {
                    self.bytes = self.bytes.saturating_sub(old.len());
                    self.evicted += 1;
                }
                None => break,
            }
        }
    }

    /// Appends text read from the log; lines complete at each `\n`.
    pub fn push_text(&mut self, text: &str) {
        let mut rest = text;
        while let Some((line, tail)) = rest.split_once('\n') {
            let mut whole = std::mem::take(&mut self.partial);
            whole.push_str(line);
            if whole.ends_with('\r') {
                whole.pop();
            }
            self.push_line(whole);
            rest = tail;
        }
        self.partial.push_str(rest);
        // one endless line must not break the byte bound
        if self.partial.len() > MAX_BYTES {
            let cut = self.partial.len() - MAX_BYTES;
            let mut at = cut;
            while !self.partial.is_char_boundary(at) {
                at += 1;
            }
            self.partial.drain(..at);
        }
    }

    /// Replaces the contents with whole lines.
    pub fn replace(&mut self, lines: Vec<String>) {
        *self = LogBuffer {
            evicted: self.evicted,
            ..LogBuffer::default()
        };
        for l in lines {
            self.push_line(l);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Ask where the log ends now, so following starts without a gap.
    Probe,
    /// Read the last lines; follow from `start` afterwards when the job is live.
    Tail { start: u64 },
    /// Read what was appended since `offset`.
    Follow { offset: u64 },
    /// Nothing more will come.
    Finished,
}

/// One open log: what to ask next and what was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSession {
    /// The job.
    pub job: JobId,
    /// The lines.
    pub buffer: LogBuffer,
    /// The last error, cleared by the next good read.
    pub error: Option<String>,
    stage: Stage,
    caught_up: bool,
}

impl LogSession {
    /// A session for a job; `live` jobs are followed after the first read.
    pub fn new(job: JobId, live: bool) -> LogSession {
        LogSession {
            job,
            buffer: LogBuffer::new(),
            error: None,
            stage: if live {
                Stage::Probe
            } else {
                Stage::Tail { start: 0 }
            },
            caught_up: true,
        }
    }

    /// True when nothing more will be read.
    pub fn is_finished(&self) -> bool {
        self.stage == Stage::Finished
    }

    /// True when the last read filled its chunk, so the next should not wait.
    pub fn wants_more_now(&self) -> bool {
        !self.caught_up
    }

    /// The next request, or `None` once finished.
    pub fn next_request(&self) -> Option<Request> {
        let view = match self.stage {
            Stage::Probe => LogView::Follow {
                // past the end: the daemon clamps to the end and returns it as the next offset
                offset: u64::MAX,
                max_bytes: Some(1),
            },
            Stage::Tail { .. } => LogView::Tail { lines: TAIL_LINES },
            Stage::Follow { offset } => LogView::Follow {
                offset,
                max_bytes: Some(CHUNK_BYTES),
            },
            Stage::Finished => return None,
        };
        Some(Request::Logs { id: self.job, view })
    }

    /// Takes the answer to [`LogSession::next_request`].
    pub fn apply(&mut self, reply: Result<Response, String>) {
        let reply = match reply {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.error = None;
        match (self.stage, reply) {
            (Stage::Probe, Response::LogChunk { next_offset, .. }) => {
                self.stage = Stage::Tail { start: next_offset };
            }
            (
                Stage::Tail { start },
                Response::LogTail {
                    lines,
                    running,
                    end_offset,
                },
            ) => {
                self.buffer.replace(lines);
                self.stage = if running {
                    // the tail's own end offset when the daemon sends it; the probed one is
                    // read at another moment, so a line written between them is lost or doubled
                    Stage::Follow {
                        offset: end_offset.unwrap_or(start),
                    }
                } else {
                    Stage::Finished
                };
            }
            (
                Stage::Follow { offset },
                Response::LogChunk {
                    data,
                    next_offset,
                    done,
                },
            ) => {
                self.caught_up = data.len() < CHUNK_BYTES;
                self.buffer.push_text(&data);
                self.stage = if done {
                    Stage::Finished
                } else {
                    Stage::Follow {
                        offset: next_offset.max(offset),
                    }
                };
            }
            (_, _) => {
                self.error = Some("the daemon answered a log request with something else".into());
            }
        }
    }
}

/// Search hits as display lines: whether the line is a match, and its text.
pub type SearchHits = Vec<(bool, String)>;

/// A copy of a session's state for the window: the lines, bounded like the buffer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogSnapshot {
    /// Complete lines, then the line being written if there is one.
    pub lines: Vec<String>,
    /// Lines dropped because the buffer was full.
    pub evicted: usize,
    /// The last read error.
    pub error: Option<String>,
    /// True when nothing more will be read.
    pub finished: bool,
}

impl LogSession {
    /// The state for the window.
    pub fn snapshot(&self) -> LogSnapshot {
        let mut lines: Vec<String> = self.buffer.lines().map(str::to_string).collect();
        if !self.buffer.partial().is_empty() {
            lines.push(self.buffer.partial().to_string());
        }
        LogSnapshot {
            lines,
            evicted: self.buffer.evicted,
            error: self.error.clone(),
            finished: self.is_finished(),
        }
    }
}

/// The request for a search of one job's log (substring, bounded).
pub fn search_request(job: JobId, pattern: &str) -> Request {
    Request::Logs {
        id: job,
        view: LogView::Grep {
            pattern: pattern.to_string(),
            context: 2,
            regex: false,
            max_matches: Some(SEARCH_MATCHES),
        },
    }
}

/// Search hits as display lines: `  12: text` for context, `> 12: text` for a match, `...`
/// between groups.
pub fn grep_lines(groups: &[GrepGroup]) -> SearchHits {
    let mut out = Vec::new();
    for (i, g) in groups.iter().enumerate() {
        if i > 0 {
            out.push((false, "...".to_string()));
        }
        for l in &g.lines {
            let mark = if l.matched { '>' } else { ' ' };
            out.push((l.matched, format!("{mark} {:>6}: {}", l.n, l.text)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_daemon::api::GrepLine;

    #[test]
    fn the_buffer_is_bounded_in_lines_and_bytes() {
        let mut b = LogBuffer::new();
        for i in 0..(MAX_LINES + 100) {
            b.push_text(&format!("line {i}\n"));
        }
        assert_eq!(b.len(), MAX_LINES);
        assert_eq!(b.evicted, 100);
        assert_eq!(b.lines().next(), Some("line 100"));
        let mut b = LogBuffer::new();
        let big = "x".repeat(100_000);
        for _ in 0..50 {
            b.push_text(&format!("{big}\n"));
        }
        assert!(b.byte_len() <= MAX_BYTES + 100_000);
        assert!(b.len() <= 21);
        // one endless line is cut too
        let mut b = LogBuffer::new();
        b.push_text(&"y".repeat(MAX_BYTES * 2));
        assert!(b.byte_len() <= MAX_BYTES);
    }

    #[test]
    fn partial_lines_join_across_chunks_and_crlf_is_trimmed() {
        let mut b = LogBuffer::new();
        b.push_text("abc");
        assert!(b.is_empty());
        b.push_text("def\r\nghi\nj");
        assert_eq!(b.lines().collect::<Vec<_>>(), ["abcdef", "ghi"]);
        assert_eq!(b.partial(), "j");
    }

    #[test]
    fn a_live_log_is_probed_tailed_then_followed_without_resending() {
        let mut s = LogSession::new(JobId(9), true);
        let id = JobId(9);
        assert_eq!(
            s.next_request(),
            Some(Request::Logs {
                id,
                view: LogView::Follow {
                    offset: u64::MAX,
                    max_bytes: Some(1)
                }
            })
        );
        s.apply(Ok(Response::LogChunk {
            data: String::new(),
            next_offset: 120,
            done: false,
        }));
        assert!(matches!(
            s.next_request(),
            Some(Request::Logs {
                view: LogView::Tail { .. },
                ..
            })
        ));
        s.apply(Ok(Response::LogTail {
            lines: vec!["a".into(), "b".into()],
            running: true,
            end_offset: None,
        }));
        assert_eq!(s.buffer.len(), 2);
        assert!(matches!(
            s.next_request(),
            Some(Request::Logs {
                view: LogView::Follow { offset: 120, .. },
                ..
            })
        ));
        s.apply(Ok(Response::LogChunk {
            data: "c\nd".into(),
            next_offset: 123,
            done: false,
        }));
        assert_eq!(s.buffer.lines().collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(matches!(
            s.next_request(),
            Some(Request::Logs {
                view: LogView::Follow { offset: 123, .. },
                ..
            })
        ));
        s.apply(Ok(Response::LogChunk {
            data: "\n".into(),
            next_offset: 124,
            done: true,
        }));
        assert!(s.is_finished());
        assert_eq!(s.next_request(), None);
        assert_eq!(s.buffer.lines().last(), Some("d"));
    }

    #[test]
    fn a_finished_log_is_tailed_once_and_errors_do_not_lose_the_buffer() {
        let mut s = LogSession::new(JobId(1), false);
        s.apply(Ok(Response::LogTail {
            lines: vec!["only".into()],
            running: false,
            end_offset: None,
        }));
        assert!(s.is_finished());
        let mut s2 = LogSession::new(JobId(1), false);
        s2.apply(Err("no reply".into()));
        assert_eq!(s2.error.as_deref(), Some("no reply"));
        assert!(!s2.is_finished(), "a failed read is retried");
    }

    #[test]
    fn search_asks_for_a_bounded_substring_and_formats_hits() {
        let r = search_request(JobId(3), "error[");
        assert_eq!(
            r,
            Request::Logs {
                id: JobId(3),
                view: LogView::Grep {
                    pattern: "error[".into(),
                    context: 2,
                    regex: false,
                    max_matches: Some(SEARCH_MATCHES)
                }
            }
        );
        let g = |n, t: &str, m| GrepLine {
            n,
            text: t.into(),
            matched: m,
        };
        let lines = grep_lines(&[
            GrepGroup {
                lines: vec![g(1, "a", false), g(2, "boom", true)],
            },
            GrepGroup {
                lines: vec![g(9, "boom", true)],
            },
        ]);
        assert_eq!(lines[1], (true, ">      2: boom".to_string()));
        assert_eq!(lines[2].1, "...");
        assert_eq!(lines.len(), 4);
    }
}
