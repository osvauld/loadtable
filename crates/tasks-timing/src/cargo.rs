//! Streaming parser for `cargo build/test` output.

use crate::dur::parse_secs;
use crate::event::{Event, TargetKind};

/// Feeds `(timestamp_ms, line)` pairs and emits [`Event`]s.
#[derive(Debug, Default)]
pub struct CargoParser {
    compiling: bool,
    current: Option<String>,
    pending_panic: Option<String>,
}

/// Strips a trailing `-<hex hash>` from a test binary name.
fn strip_hash(bin: &str) -> &str {
    match bin.rsplit_once('-') {
        Some((head, h))
            if h.len() >= 3
                && h.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()) =>
        {
            head
        }
        _ => bin,
    }
}

/// Parses the text of a `Running ...` line (after `Running `).
fn parse_running(rest: &str) -> Option<(String, TargetKind)> {
    let (desc, bin) = match rest.rsplit_once('(') {
        Some((d, b)) => (d.trim(), b.trim_end_matches(')')),
        None => (rest.trim(), ""),
    };
    let bin_name = strip_hash(bin.rsplit('/').next().unwrap_or(""));
    let path = desc.strip_prefix("unittests").map_or(desc, str::trim);
    if path.starts_with('`') {
        return None;
    }
    let comps: Vec<&str> = path.split('/').collect();
    if comps.first() == Some(&"tests") {
        let file = comps.last().copied().unwrap_or("");
        let stem = file.strip_suffix(".rs").unwrap_or(file);
        let name = if stem == "main" && comps.len() > 2 {
            comps.get(comps.len() - 2).copied().unwrap_or(stem)
        } else {
            stem
        };
        return Some((name.to_string(), TargetKind::Test(name.to_string())));
    }
    let name = if bin_name.is_empty() { path } else { bin_name };
    if path.ends_with("src/lib.rs") || path == "lib.rs" {
        Some((name.to_string(), TargetKind::Lib))
    } else {
        Some((name.to_string(), TargetKind::Bin))
    }
}

/// Parses `test result: ok. 12 passed; 0 failed; 1 ignored; ...; finished in 3.21s`.
fn parse_result(rest: &str) -> (u32, u32, u32, Option<f64>) {
    let (mut p, mut f, mut i, mut secs) = (0, 0, 0, None);
    let body = rest.split_once('.').map_or(rest, |(_, b)| b);
    for part in body.split(';') {
        let part = part.trim();
        let num = part
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<u32>().ok());
        if part.ends_with("passed") {
            p = num.unwrap_or(0);
        } else if part.ends_with("failed") {
            f = num.unwrap_or(0);
        } else if part.ends_with("ignored") {
            i = num.unwrap_or(0);
        } else if let Some(t) = part.strip_prefix("finished in ") {
            secs = parse_secs(t);
        }
    }
    (p, f, i, secs)
}

impl CargoParser {
    /// A parser at the start of a run.
    pub fn new() -> CargoParser {
        CargoParser::default()
    }

    /// Consumes one output line and returns the events it produced.
    pub fn feed(&mut self, _ts_ms: u64, line: &str) -> Vec<Event> {
        let mut out = Vec::new();
        if let Some(head) = self.pending_panic.take() {
            let msg = line.trim();
            let message = if msg.is_empty() || msg.starts_with("note:") {
                head
            } else {
                msg.to_string()
            };
            out.push(Event::Panic {
                target: self.current.clone().unwrap_or_default(),
                message,
            });
        }
        let t = line.trim();
        if ["Compiling ", "Checking ", "Documenting "]
            .iter()
            .any(|p| t.starts_with(p))
        {
            if !self.compiling {
                self.compiling = true;
                out.push(Event::CompileStarted);
            }
        } else if let Some(rest) = t.strip_prefix("Finished ") {
            if !self.compiling {
                out.push(Event::CompileStarted);
            }
            self.compiling = false;
            let profile = match rest.split_once('`') {
                Some((_, r)) => r.split('`').next().unwrap_or("").to_string(),
                None => rest.split_whitespace().next().unwrap_or("").to_string(),
            };
            out.push(Event::CompileFinished { profile });
        } else if let Some(rest) = t.strip_prefix("Running ") {
            if let Some((name, kind)) = parse_running(rest) {
                self.current = Some(name.clone());
                out.push(Event::TargetStarted { name, kind });
            }
        } else if let Some(name) = t.strip_prefix("Doc-tests ") {
            let name = name.trim().to_string();
            self.current = Some(name.clone());
            out.push(Event::TargetStarted {
                name,
                kind: TargetKind::Doc,
            });
        } else if let Some(rest) = t.strip_prefix("test result:") {
            if let Some(name) = self.current.take() {
                let (passed, failed, ignored, reported_secs) = parse_result(rest);
                out.push(Event::TargetFinished {
                    name,
                    passed,
                    failed,
                    ignored,
                    reported_secs,
                });
            }
        } else if let Some(rest) = t.strip_prefix("test ") {
            if let Some(name) = rest.strip_suffix(" ... FAILED") {
                let target = self.current.clone().unwrap_or_default();
                out.push(Event::TestFailed {
                    target,
                    test: name.trim().to_string(),
                });
            }
        } else if let Some((_, rest)) = t.split_once(" panicked at ") {
            if t.starts_with("thread ") {
                match rest.strip_prefix('\'') {
                    Some(inline) => out.push(Event::Panic {
                        target: self.current.clone().unwrap_or_default(),
                        message: inline.to_string(),
                    }),
                    None => self.pending_panic = Some(rest.trim_end_matches(':').to_string()),
                }
            }
        } else if t.starts_with("error: could not compile") {
            out.push(Event::CompileFailed);
        } else if t.starts_with("error: test failed, to rerun pass")
            && let Some(name) = self.current.take()
        {
            out.push(Event::TargetFinished {
                name,
                passed: 0,
                failed: 1,
                ignored: 0,
                reported_secs: None,
            });
        }
        out
    }

    /// Flushes state at end of input (a panic header with no following line).
    pub fn finish(&mut self) -> Vec<Event> {
        match self.pending_panic.take() {
            Some(message) => {
                vec![Event::Panic {
                    target: self.current.clone().unwrap_or_default(),
                    message,
                }]
            }
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[&str]) -> Vec<Event> {
        let mut p = CargoParser::new();
        let mut ev: Vec<Event> = lines
            .iter()
            .enumerate()
            .flat_map(|(i, l)| p.feed(i as u64 * 1000, l))
            .collect();
        ev.extend(p.finish());
        ev
    }

    #[test]
    fn compile_and_finished() {
        let ev = run(&[
            "   Compiling x v1",
            "   Compiling y v1",
            "    Finished `test` profile [optimized] target(s) in 1m 02s",
        ]);
        assert_eq!(
            ev,
            vec![
                Event::CompileStarted,
                Event::CompileFinished {
                    profile: "test".into()
                }
            ]
        );
    }

    #[test]
    fn fresh_build_still_emits_compile_pair() {
        let ev = run(&["    Finished `dev` profile [unoptimized] target(s) in 0.1s"]);
        assert_eq!(ev.len(), 2);
    }

    #[test]
    fn running_variants() {
        let ev = run(&[
            "     Running unittests src/lib.rs (target/release/deps/p2pcore-abc)",
            "test result: ok. 12 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 3.21s",
            "     Running tests/lifecycle.rs (target/release/deps/lifecycle-abc123)",
            "test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 115.55s",
            "     Running unittests src/main.rs (target/debug/deps/my-tool-0123456789abcdef)",
            "   Doc-tests x",
        ]);
        assert_eq!(
            ev[0],
            Event::TargetStarted {
                name: "p2pcore".into(),
                kind: TargetKind::Lib
            }
        );
        assert_eq!(
            ev[1],
            Event::TargetFinished {
                name: "p2pcore".into(),
                passed: 12,
                failed: 0,
                ignored: 1,
                reported_secs: Some(3.21)
            }
        );
        assert_eq!(
            ev[2],
            Event::TargetStarted {
                name: "lifecycle".into(),
                kind: TargetKind::Test("lifecycle".into())
            }
        );
        assert!(matches!(&ev[3], Event::TargetFinished { failed: 2, .. }));
        assert_eq!(
            ev[4],
            Event::TargetStarted {
                name: "my-tool".into(),
                kind: TargetKind::Bin
            }
        );
        assert_eq!(
            ev[5],
            Event::TargetStarted {
                name: "x".into(),
                kind: TargetKind::Doc
            }
        );
    }

    #[test]
    fn failures_and_panics_new_and_old_format() {
        let ev = run(&[
            "     Running tests/lifecycle.rs (target/x/lifecycle-abc)",
            "test foo::bar ... FAILED",
            "thread 'foo::bar' panicked at tests/lifecycle.rs:9:5:",
            "assertion failed: x",
            "thread 'old' panicked at 'boom', src/a.rs:1:1",
            "error: test failed, to rerun pass `--test lifecycle`",
        ]);
        assert!(ev.contains(&Event::TestFailed {
            target: "lifecycle".into(),
            test: "foo::bar".into()
        }));
        assert!(ev.contains(&Event::Panic {
            target: "lifecycle".into(),
            message: "assertion failed: x".into()
        }));
        assert!(
            ev.iter()
                .any(|e| matches!(e, Event::Panic { message, .. } if message.starts_with("boom")))
        );
        assert!(matches!(
            ev.last(),
            Some(Event::TargetFinished { failed: 1, .. })
        ));
    }

    #[test]
    fn panic_header_flushed_at_end() {
        let ev = run(&["thread 't' panicked at src/a.rs:1:1:"]);
        assert_eq!(
            ev,
            vec![Event::Panic {
                target: String::new(),
                message: "src/a.rs:1:1".into()
            }]
        );
    }

    #[test]
    fn compile_error_and_noise() {
        let ev = run(&[
            "   Compiling x v1",
            "error: could not compile `x` (lib) due to 1 previous error",
            "warning: foo",
        ]);
        assert_eq!(ev, vec![Event::CompileStarted, Event::CompileFailed]);
    }

    #[test]
    fn running_backtick_command_is_not_a_target() {
        assert!(run(&["     Running `target/debug/app`"]).is_empty());
    }

    #[test]
    fn hash_stripping() {
        assert_eq!(strip_hash("p2pcore-abc"), "p2pcore");
        assert_eq!(strip_hash("group-core"), "group-core");
        assert_eq!(strip_hash("calls-ed54271c6976e1fc"), "calls");
    }
}
