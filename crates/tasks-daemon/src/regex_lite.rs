//! A small regex subset for log search (R-114), so the daemon needs no regex dependency.
//!
//! Supported: literals, `.`, classes `[abc]` `[a-z]` `[^x]`, escapes `\d \w \s` (and `\D \W
//! \S`, `\.` and other escaped punctuation), the quantifiers `* + ?` on one atom, anchors `^`
//! and `$` at the ends of an alternative, and top-level alternation `a|b`. Not supported:
//! groups, `{n,m}`, back-references, lazy quantifiers. A line is searched, not the whole
//! file; matching has a step budget, so a pathological pattern fails to match instead of
//! stalling the daemon.

use thiserror::Error;

/// Longest accepted pattern, in characters.
pub const MAX_PATTERN_CHARS: usize = 256;
/// Matching steps allowed per line.
const STEP_BUDGET: usize = 200_000;

/// A pattern that cannot be used.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("bad pattern: {0}")]
pub struct PatternError(pub String);

#[derive(Debug, Clone, PartialEq)]
enum Item {
    Ch(char),
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

#[derive(Debug, Clone, PartialEq)]
enum Atom {
    Any,
    Set { items: Vec<Item>, negated: bool },
}

#[derive(Debug, Clone, PartialEq)]
struct Piece {
    atom: Atom,
    min: usize,
    max: usize,
}

#[derive(Debug, Clone, PartialEq)]
struct Seq {
    start: bool,
    end: bool,
    pieces: Vec<Piece>,
}

/// A compiled pattern.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    alts: Vec<Seq>,
}

fn item_matches(item: &Item, c: char) -> bool {
    match item {
        Item::Ch(x) => *x == c,
        Item::Range(a, b) => *a <= c && c <= *b,
        Item::Digit(neg) => c.is_ascii_digit() != *neg,
        Item::Word(neg) => (c.is_alphanumeric() || c == '_') != *neg,
        Item::Space(neg) => c.is_whitespace() != *neg,
    }
}

fn atom_matches(atom: &Atom, c: char) -> bool {
    match atom {
        Atom::Any => true,
        Atom::Set { items, negated } => items.iter().any(|i| item_matches(i, c)) != *negated,
    }
}

fn escape_item(c: char) -> Item {
    match c {
        'd' => Item::Digit(false),
        'D' => Item::Digit(true),
        'w' => Item::Word(false),
        'W' => Item::Word(true),
        's' => Item::Space(false),
        'S' => Item::Space(true),
        'n' => Item::Ch('\n'),
        't' => Item::Ch('\t'),
        other => Item::Ch(other),
    }
}

impl Pattern {
    /// Compiles `text`.
    pub fn new(text: &str) -> Result<Pattern, PatternError> {
        if text.is_empty() {
            return Err(PatternError("empty pattern".into()));
        }
        if text.chars().count() > MAX_PATTERN_CHARS {
            return Err(PatternError(format!(
                "longer than {MAX_PATTERN_CHARS} characters"
            )));
        }
        let mut alts = Vec::new();
        for part in split_alternatives(text) {
            alts.push(parse_seq(&part)?);
        }
        Ok(Pattern { alts })
    }

    /// True when the pattern matches anywhere in `line`.
    pub fn is_match(&self, line: &str) -> bool {
        let chars: Vec<char> = line.chars().collect();
        self.alts.iter().any(|seq| {
            let mut budget = STEP_BUDGET;
            let starts: Vec<usize> = if seq.start {
                vec![0]
            } else {
                (0..=chars.len()).collect()
            };
            starts
                .into_iter()
                .any(|s| matches_at(seq, 0, &chars, s, &mut budget))
        })
    }
}

fn split_alternatives(text: &str) -> Vec<Vec<char>> {
    let mut out = vec![Vec::new()];
    let mut escaped = false;
    let mut in_class = false;
    for c in text.chars() {
        if escaped {
            escaped = false;
            if let Some(cur) = out.last_mut() {
                cur.push(c);
            }
            continue;
        }
        match c {
            '\\' => {
                escaped = true;
                if let Some(cur) = out.last_mut() {
                    cur.push(c);
                }
            }
            '[' => {
                in_class = true;
                if let Some(cur) = out.last_mut() {
                    cur.push(c);
                }
            }
            ']' => {
                in_class = false;
                if let Some(cur) = out.last_mut() {
                    cur.push(c);
                }
            }
            '|' if !in_class => out.push(Vec::new()),
            _ => {
                if let Some(cur) = out.last_mut() {
                    cur.push(c);
                }
            }
        }
    }
    out
}

fn parse_seq(chars: &[char]) -> Result<Seq, PatternError> {
    let mut i = 0;
    let mut start = false;
    let mut end = false;
    if chars.first() == Some(&'^') {
        start = true;
        i = 1;
    }
    let mut pieces: Vec<Piece> = Vec::new();
    while let Some(&c) = chars.get(i) {
        i += 1;
        let atom = match c {
            '$' if i == chars.len() => {
                end = true;
                break;
            }
            '.' => Atom::Any,
            '\\' => {
                let Some(&e) = chars.get(i) else {
                    return Err(PatternError("trailing backslash".into()));
                };
                i += 1;
                Atom::Set {
                    items: vec![escape_item(e)],
                    negated: false,
                }
            }
            '[' => {
                let (atom, next) = parse_class(chars, i)?;
                i = next;
                atom
            }
            '*' | '+' | '?' => {
                return Err(PatternError(format!("nothing to repeat before `{c}`")));
            }
            '(' | ')' | '{' | '}' => {
                return Err(PatternError(format!(
                    "`{c}` is not supported (groups and counted repeats are not in the subset)"
                )));
            }
            other => Atom::Set {
                items: vec![Item::Ch(other)],
                negated: false,
            },
        };
        let (min, max) = match chars.get(i) {
            Some('*') => (0, usize::MAX),
            Some('+') => (1, usize::MAX),
            Some('?') => (0, 1),
            _ => (1, 1),
        };
        if (min, max) != (1, 1) {
            i += 1;
        }
        pieces.push(Piece { atom, min, max });
    }
    Ok(Seq { start, end, pieces })
}

fn parse_class(chars: &[char], mut i: usize) -> Result<(Atom, usize), PatternError> {
    let mut negated = false;
    if chars.get(i) == Some(&'^') {
        negated = true;
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    loop {
        let Some(&c) = chars.get(i) else {
            return Err(PatternError("unclosed `[`".into()));
        };
        i += 1;
        if c == ']' && !first {
            break;
        }
        first = false;
        let lo = if c == '\\' {
            let Some(&e) = chars.get(i) else {
                return Err(PatternError("trailing backslash".into()));
            };
            i += 1;
            if matches!(e, 'd' | 'D' | 'w' | 'W' | 's' | 'S') {
                items.push(escape_item(e));
                continue;
            }
            match escape_item(e) {
                Item::Ch(x) => x,
                _ => e,
            }
        } else {
            c
        };
        if chars.get(i) == Some(&'-') && chars.get(i + 1).is_some_and(|n| *n != ']') {
            let hi = chars.get(i + 1).copied().unwrap_or(lo);
            if hi < lo {
                return Err(PatternError(format!("range {lo}-{hi} is backwards")));
            }
            items.push(Item::Range(lo, hi));
            i += 2;
        } else {
            items.push(Item::Ch(lo));
        }
    }
    Ok((Atom::Set { items, negated }, i))
}

fn matches_at(seq: &Seq, pi: usize, chars: &[char], pos: usize, budget: &mut usize) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    let Some(piece) = seq.pieces.get(pi) else {
        return !seq.end || pos == chars.len();
    };
    let mut reach = 0;
    while reach < piece.max
        && chars
            .get(pos + reach)
            .is_some_and(|c| atom_matches(&piece.atom, *c))
    {
        reach += 1;
    }
    if reach < piece.min {
        return false;
    }
    (piece.min..=reach)
        .rev()
        .any(|n| matches_at(seq, pi + 1, chars, pos + n, budget))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, line: &str) -> bool {
        Pattern::new(p).unwrap().is_match(line)
    }

    // R-114
    #[test]
    fn r114_the_documented_subset_matches() {
        assert!(m("error", "an error here"));
        assert!(m("^error", "error: x"));
        assert!(!m("^error", " error: x"));
        assert!(m("done$", "all done"));
        assert!(!m("done$", "done already"));
        assert!(m("^$", ""));
        assert!(m("test .* ok", "test lifecycle::x ... ok"));
        assert!(m(r"exit \d+", "process exit 137 now"));
        assert!(!m(r"exit \d+", "process exit now"));
        assert!(m("colou?r", "color"));
        assert!(m("colou?r", "colour"));
        assert!(m("ab+c", "xabbbc"));
        assert!(!m("ab+c", "xac"));
        assert!(m("[0-9]+ passed", "12 passed"));
        assert!(m("[^a-z]x", "Ax"));
        assert!(!m("[^a-z]x", "ax"));
        assert!(m("warning|error", "got an error"));
        assert!(m("warning|error", "warning!"));
        assert!(!m("warning|error", "fine"));
        assert!(m(r"a\.b", "a.b"));
        assert!(!m(r"a\.b", "axb"));
        assert!(m(r"\S+@\S+", "mail me@x.org please"));
        assert!(m(r"[\d.]+s", "took 12.5s"));
    }

    #[test]
    fn r114_unsupported_and_bad_patterns_are_refused_not_guessed() {
        for bad in ["", "*x", "a(b)", "a{2}", "[abc", "a\\", "[z-a]", "+"] {
            assert!(Pattern::new(bad).is_err(), "{bad:?}");
        }
        assert!(Pattern::new(&"a".repeat(MAX_PATTERN_CHARS + 1)).is_err());
    }

    #[test]
    fn a_pathological_pattern_gives_up_instead_of_stalling() {
        let p = Pattern::new("a*a*a*a*a*a*a*a*b").unwrap();
        let line = "a".repeat(200);
        let t = std::time::Instant::now();
        assert!(!p.is_match(&line));
        assert!(t.elapsed() < std::time::Duration::from_secs(2));
    }
}
