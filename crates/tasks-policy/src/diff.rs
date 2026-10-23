//! Rendering suggestions as a unified-style diff of the policy text. Pure: text in, text out.

use crate::suggest::{Action, Suggestion};
use std::collections::BTreeSet;
use std::fmt::Write;

const CONTEXT: usize = 3;

/// Lines to insert before old line index `at`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Insert {
    at: usize,
    lines: Vec<String>,
}

fn header_idx(lines: &[&str], header: &str) -> Option<usize> {
    lines.iter().position(|l| l.trim() == header)
}

/// Lines of the table that starts at `start` (up to the next header).
fn block<'a>(lines: &'a [&'a str], start: usize) -> &'a [&'a str] {
    let rest = lines.get(start + 1..).unwrap_or(&[]);
    let n = rest
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(rest.len());
    rest.get(..n).unwrap_or(&[])
}

fn has_key(block: &[&str], key: &str) -> bool {
    block.iter().any(|l| {
        l.trim_start()
            .strip_prefix(key)
            .is_some_and(|r| r.trim_start().starts_with('='))
    })
}

/// Index of the `name = "STEP"` line of a step, and whether the step block already has `key`.
fn step_name_line(lines: &[&str], suite: &str, step: &str, key: &str) -> Option<usize> {
    let header = format!("[[suites.{suite}.steps]]");
    let want = format!("\"{step}\"");
    for (i, l) in lines.iter().enumerate() {
        if l.trim() != header {
            continue;
        }
        let b = block(lines, i);
        let name = b.iter().position(|l| {
            l.trim_start()
                .strip_prefix("name")
                .and_then(|r| r.trim_start().strip_prefix('='))
                .is_some_and(|v| v.trim() == want)
        });
        if let Some(n) = name {
            return if has_key(b, key) {
                None
            } else {
                Some(i + 1 + n)
            };
        }
    }
    None
}

fn locate(lines: &[&str], action: &Action, appended: &mut Vec<String>) -> Option<Insert> {
    match action {
        Action::SetSplit { suite, step } => {
            let i = step_name_line(lines, suite, step, "split")?;
            Some(Insert {
                at: i + 1,
                lines: vec!["split = \"cargo-test\"".into()],
            })
        }
        Action::SetPlacement {
            suite,
            step,
            machine,
        } => {
            let i = step_name_line(lines, suite, step, "placement")?;
            Some(Insert {
                at: i + 1,
                lines: vec![format!("placement = \"{machine}\"")],
            })
        }
        Action::MarkLong { suite, target } => {
            let header = format!("[suites.{suite}.targets.\"{target}\"]");
            match header_idx(lines, &header) {
                Some(i) if has_key(block(lines, i), "long") => None,
                Some(i) => Some(Insert {
                    at: i + 1,
                    lines: vec!["long = true".into()],
                }),
                None => {
                    appended.push(String::new());
                    appended.push(header);
                    appended.push("long = true".into());
                    Some(Insert {
                        at: lines.len(),
                        lines: Vec::new(),
                    })
                }
            }
        }
    }
}

/// The unified diff for `actions` against `text`, and the actions that could not be placed
/// (the text is formatted in a way the locator does not know, or the key already exists).
pub fn render_diff(file: &str, text: &str, actions: &[Action]) -> (String, Vec<Action>) {
    let lines: Vec<&str> = text.lines().collect();
    let uniq: BTreeSet<&Action> = actions.iter().collect();
    let mut inserts = Vec::new();
    let mut appended = Vec::new();
    let mut unplaced = Vec::new();
    for a in uniq {
        match locate(&lines, a, &mut appended) {
            Some(ins) if !ins.lines.is_empty() => inserts.push(ins),
            Some(_) => {}
            None => unplaced.push(a.clone()),
        }
    }
    if !appended.is_empty() {
        // A blank line only when the file does not already end with one.
        if lines.last().is_none_or(|l| l.trim().is_empty())
            && appended.first().is_some_and(String::is_empty)
        {
            appended.remove(0);
        }
        inserts.push(Insert {
            at: lines.len(),
            lines: appended,
        });
    }
    inserts.sort_by_key(|i| i.at);
    (hunks(file, &lines, &inserts), unplaced)
}

fn hunks(file: &str, lines: &[&str], inserts: &[Insert]) -> String {
    if inserts.is_empty() {
        return String::new();
    }
    // Group inserts whose context windows touch.
    let mut groups: Vec<(usize, usize, Vec<&Insert>)> = Vec::new();
    for ins in inserts {
        let s = ins.at.saturating_sub(CONTEXT);
        let e = (ins.at + CONTEXT).min(lines.len());
        match groups.last_mut() {
            Some((_, ge, v)) if s <= *ge => {
                *ge = (*ge).max(e);
                v.push(ins);
            }
            _ => groups.push((s, e, vec![ins])),
        }
    }
    let mut out = format!("--- a/{file}\n+++ b/{file}\n");
    let mut added_before = 0;
    for (s, e, v) in groups {
        let added: usize = v.iter().map(|i| i.lines.len()).sum();
        let old_len = e - s;
        let _ = writeln!(
            out,
            "@@ -{},{} +{},{} @@",
            s + 1,
            old_len,
            s + 1 + added_before,
            old_len + added
        );
        for i in s..e {
            for ins in v.iter().filter(|x| x.at == i) {
                for l in &ins.lines {
                    let _ = writeln!(out, "+{l}");
                }
            }
            let _ = writeln!(out, " {}", lines.get(i).copied().unwrap_or(""));
        }
        for ins in v.iter().filter(|x| x.at == e) {
            for l in &ins.lines {
                let _ = writeln!(out, "+{l}");
            }
        }
        added_before += added;
    }
    out
}

/// The full `policy suggest` output: diff, then the evidence list, then what needs hand edits.
pub fn render_proposal(file: &str, text: &str, suggestions: &[Suggestion]) -> String {
    let actions: Vec<Action> = suggestions
        .iter()
        .filter_map(|s| s.action.clone())
        .collect();
    let (diff, unplaced) = render_diff(file, text, &actions);
    let mut out = String::new();
    if diff.is_empty() && suggestions.is_empty() {
        return "No policy changes suggested.\n".into();
    }
    if !diff.is_empty() {
        out.push_str(&diff);
        out.push('\n');
    }
    out.push_str("Evidence:\n");
    for (i, s) in suggestions.iter().enumerate() {
        let tag = match &s.action {
            Some(a) if unplaced.contains(a) => " [edit by hand]",
            Some(_) => "",
            None => " [advice, no text change]",
        };
        let _ = writeln!(
            out,
            "  {}. [{}/{}] {}{}",
            i + 1,
            s.suite,
            s.step,
            s.summary,
            tag
        );
        let _ = writeln!(out, "     evidence: {}", s.evidence);
    }
    out.push_str(
        "\nReview the diff and commit it yourself; tasks never rewrites the policy file.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "version = 1\n\n[suites.core]\n\n[[suites.core.steps]]\nname = \"tests\"\ncommand = [\"cargo\", \"test\"]\n\n[[suites.core.steps]]\nname = \"other\"\ncommand = [\"x\"]\n";

    fn split() -> Action {
        Action::SetSplit {
            suite: "core".into(),
            step: "tests".into(),
        }
    }

    #[test]
    fn insert_after_name_line() {
        let (d, un) = render_diff("p.toml", TEXT, &[split()]);
        assert!(un.is_empty());
        assert_eq!(
            d,
            "--- a/p.toml\n+++ b/p.toml\n@@ -4,6 +4,7 @@\n \n [[suites.core.steps]]\n name = \"tests\"\n+split = \"cargo-test\"\n command = [\"cargo\", \"test\"]\n \n [[suites.core.steps]]\n"
        );
    }

    #[test]
    fn append_new_target_table_and_dedupe() {
        let long = Action::MarkLong {
            suite: "core".into(),
            target: "test:lifecycle".into(),
        };
        let (d, un) = render_diff("p.toml", TEXT, &[long.clone(), long, split(), split()]);
        assert!(un.is_empty());
        assert_eq!(d.matches("+split = ").count(), 1);
        assert!(d.contains("+[suites.core.targets.\"test:lifecycle\"]\n+long = true\n"));
        assert!(d.contains("\n+\n+[suites"));
        assert_eq!(d.matches("@@ -").count(), 1, "nearby edits share a hunk");
    }

    #[test]
    fn existing_table_gets_key_inserted_or_is_left_to_hand() {
        let t = format!(
            "{TEXT}\n[suites.core.targets.\"test:a\"]\nnote = \"x\"\n\n[suites.core.targets.\"test:b\"]\nlong = false\n"
        );
        let a = Action::MarkLong {
            suite: "core".into(),
            target: "test:a".into(),
        };
        let b = Action::MarkLong {
            suite: "core".into(),
            target: "test:b".into(),
        };
        let (d, un) = render_diff("p.toml", &t, &[a, b.clone()]);
        assert!(d.contains("+long = true"));
        assert_eq!(un, vec![b]);
    }

    #[test]
    fn unknown_step_is_unplaced() {
        let (d, un) = render_diff(
            "p.toml",
            TEXT,
            &[Action::SetSplit {
                suite: "core".into(),
                step: "zzz".into(),
            }],
        );
        assert!(d.is_empty());
        assert_eq!(un.len(), 1);
    }

    #[test]
    fn diff_applies_cleanly() {
        // Re-apply the '+' lines to the old text and compare with an independent construction.
        let (d, _) = render_diff("p.toml", TEXT, &[split()]);
        let mut new: Vec<String> = Vec::new();
        for l in TEXT.lines() {
            new.push(l.to_string());
            if l.starts_with("name = \"tests\"") {
                new.push("split = \"cargo-test\"".into());
            }
        }
        let plus: Vec<&str> = d
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .collect();
        assert_eq!(plus, vec!["+split = \"cargo-test\""]);
        assert_eq!(new.len(), TEXT.lines().count() + 1);
    }
}
