//! Bounded splitting of a byte stream into lines.

use crate::TimedLine;
use std::time::Duration;

/// Bounds for [`LineSplitter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitLimits {
    /// Longest line emitted in one piece; longer lines are split (and the
    /// pieces after the first are flagged `continued`). Also the memory bound.
    pub max_line_bytes: usize,
}

/// Turns appended bytes into complete lines. Only newlines end a line, so a
/// partial line waits for its newline (R-111); memory never exceeds
/// `max_line_bytes`; invalid UTF-8 is replaced, never an error.
#[derive(Debug)]
pub struct LineSplitter {
    limits: SplitLimits,
    buf: Vec<u8>,
    continued: bool,
}

impl LineSplitter {
    /// An empty splitter. A zero limit is treated as 1 byte.
    pub fn new(limits: SplitLimits) -> LineSplitter {
        LineSplitter {
            limits: SplitLimits {
                max_line_bytes: limits.max_line_bytes.max(1),
            },
            buf: Vec::new(),
            continued: false,
        }
    }

    /// Feeds bytes read at `offset`; complete lines are appended to `out`.
    pub fn feed(&mut self, mut bytes: &[u8], offset: Duration, out: &mut Vec<TimedLine>) {
        while !bytes.is_empty() {
            match bytes.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    let (line, rest) = bytes.split_at(i);
                    self.push_bytes(line, offset, out);
                    self.emit_all(offset, out, false);
                    self.continued = false;
                    bytes = rest.get(1..).unwrap_or(&[]);
                }
                None => {
                    self.push_bytes(bytes, offset, out);
                    bytes = &[];
                }
            }
        }
    }

    /// Emits the unterminated tail (job over), if any.
    pub fn finish(&mut self, offset: Duration, out: &mut Vec<TimedLine>) {
        if !self.buf.is_empty() {
            self.emit_all(offset, out, false);
        }
        self.continued = false;
    }

    /// Appends to the buffer, splitting out full pieces whenever it overflows.
    fn push_bytes(&mut self, mut data: &[u8], offset: Duration, out: &mut Vec<TimedLine>) {
        let max = self.limits.max_line_bytes;
        while !data.is_empty() {
            let room = max.saturating_sub(self.buf.len());
            if room == 0 {
                self.emit_piece(offset, out);
                continue;
            }
            let take = room.min(data.len());
            let (head, rest) = data.split_at(take);
            self.buf.extend_from_slice(head);
            data = rest;
        }
    }

    /// Emits a full buffer as one piece, holding back an incomplete trailing
    /// multi-byte sequence for the next piece.
    fn emit_piece(&mut self, offset: Duration, out: &mut Vec<TimedLine>) {
        let hold = incomplete_tail(&self.buf);
        let tail = self.buf.split_off(self.buf.len() - hold);
        self.emit_all(offset, out, true);
        self.buf = tail;
    }

    /// Emits the whole buffer as a line, clearing it.
    fn emit_all(&mut self, offset: Duration, out: &mut Vec<TimedLine>, more: bool) {
        let mut bytes = std::mem::take(&mut self.buf);
        if !more && bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        out.push(TimedLine {
            offset,
            text: String::from_utf8_lossy(&bytes).into_owned(),
            continued: self.continued,
        });
        self.continued = more;
    }
}

/// Length of an incomplete UTF-8 sequence at the end of `buf` (0 to 3 bytes),
/// never the whole buffer.
fn incomplete_tail(buf: &[u8]) -> usize {
    for back in 1..=3usize.min(buf.len().saturating_sub(1)) {
        let Some(&b) = buf.get(buf.len() - back) else {
            return 0;
        };
        if b & 0xC0 == 0x80 {
            continue; // continuation byte, keep looking for the lead
        }
        let need = match b {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return 0,
        };
        return if need > back { back } else { 0 };
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(max: usize, chunks: &[&[u8]]) -> Vec<TimedLine> {
        let mut s = LineSplitter::new(SplitLimits {
            max_line_bytes: max,
        });
        let mut out = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            s.feed(c, Duration::from_millis(i as u64), &mut out);
        }
        s.finish(Duration::from_secs(9), &mut out);
        out
    }

    fn texts(v: &[TimedLine]) -> Vec<&str> {
        v.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn r111_lines_wait_for_newlines_and_arrive_in_order() {
        let mut s = LineSplitter::new(SplitLimits {
            max_line_bytes: 100,
        });
        let mut out = Vec::new();
        s.feed(b"ab", Duration::ZERO, &mut out);
        assert!(out.is_empty());
        s.feed(b"c\nde", Duration::from_millis(5), &mut out);
        assert_eq!(texts(&out), ["abc"]);
        assert_eq!(out[0].offset, Duration::from_millis(5));
        s.feed(b"f\n\n", Duration::from_millis(6), &mut out);
        assert_eq!(texts(&out), ["abc", "def", ""]);
    }

    #[test]
    fn partial_line_is_flushed_at_finish_only() {
        let out = split(100, &[b"one\ntwo"]);
        assert_eq!(texts(&out), ["one", "two"]);
        assert_eq!(out[1].offset, Duration::from_secs(9));
    }

    #[test]
    fn crlf_is_trimmed() {
        assert_eq!(texts(&split(100, &[b"a\r\nb\r\n"])), ["a", "b"]);
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        let out = split(100, &[b"a\xff\xfeb\n"]);
        assert_eq!(out[0].text, "a\u{fffd}\u{fffd}b");
    }

    #[test]
    fn very_long_line_is_split_with_bounded_memory_and_flagged() {
        let long = vec![b'x'; 25];
        let out = split(10, &[&long, b"\nok\n"]);
        let lens: Vec<usize> = out.iter().map(|l| l.text.len()).collect();
        assert_eq!(lens, [10, 10, 5, 2]);
        let flags: Vec<bool> = out.iter().map(|l| l.continued).collect();
        assert_eq!(flags, [false, true, true, false]);
    }

    #[test]
    fn split_never_cuts_a_multibyte_character() {
        let s = "ééééé"; // 2 bytes each
        let out = split(5, &[s.as_bytes(), b"\n"]);
        assert!(out.iter().all(|l| !l.text.contains('\u{fffd}')));
        let joined: String = out.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(joined, s);
    }
}
