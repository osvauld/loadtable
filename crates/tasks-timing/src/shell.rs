//! Minimal shell-script splitting for `sh -c "a && b; c"` style wrappers.

/// Splits a script into commands (each a word list) on `;`, `&&` and newlines.
///
/// Returns `None` when the script uses anything else (pipes, redirects, `||`, `&`, subshells,
/// substitutions, comments), in which case the caller keeps the whole script verbatim.
pub(crate) fn split_script(script: &str) -> Option<Vec<Vec<String>>> {
    let mut cmds: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        x => word.push(x),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => word.push(chars.next()?),
                        '$' if chars.peek() == Some(&'(') => return None,
                        '`' => return None,
                        x => word.push(x),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next()?);
            }
            ' ' | '\t' => {
                if in_word {
                    cur.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            ';' | '\n' | '&' => {
                if c == '&' && chars.next_if_eq(&'&').is_none() {
                    return None;
                }
                if in_word {
                    cur.push(std::mem::take(&mut word));
                    in_word = false;
                }
                if !cur.is_empty() {
                    cmds.push(std::mem::take(&mut cur));
                }
            }
            '|' | '>' | '<' | '(' | ')' | '{' | '}' | '`' | '#' if !in_word || c != '#' => {
                return None;
            }
            '$' if chars.peek() == Some(&'(') => return None,
            x => {
                in_word = true;
                word.push(x);
            }
        }
    }
    if in_word {
        cur.push(word);
    }
    if !cur.is_empty() {
        cmds.push(cur);
    }
    Some(cmds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<Vec<String>> {
        split_script(s).unwrap()
    }

    #[test]
    fn splits_chains_and_quotes() {
        assert_eq!(w("a b").len(), 1);
        assert_eq!(w("a; b && c\nd").len(), 4);
        assert_eq!(w("echo 'x y' \"z w\""), vec![vec!["echo", "x y", "z w"]]);
        assert_eq!(w("a\\ b c"), vec![vec!["a b", "c"]]);
        assert_eq!(w("a;;b;").len(), 2);
        assert!(w("").is_empty());
    }

    #[test]
    fn unsupported_syntax_is_none() {
        for s in [
            "a | b",
            "a > f",
            "a || b",
            "a &",
            "(a)",
            "a $(b)",
            "a `b`",
            "echo \"$(b)\"",
            "'open",
            "a #c",
        ] {
            assert!(split_script(s).is_none(), "{s}");
        }
    }

    #[test]
    fn hash_inside_a_word_is_literal() {
        assert_eq!(w("a b#c"), vec![vec!["a", "b#c"]]);
    }
}
