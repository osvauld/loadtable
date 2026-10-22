//! Streaming parser for `gradle --console=plain` output.

use crate::dur::parse_secs;
use crate::event::{Event, TaskOutcome};

/// Feeds `(timestamp_ms, line)` pairs and emits [`Event`]s.
#[derive(Debug, Default)]
pub struct GradleParser {
    current: String,
}

impl GradleParser {
    /// A parser at the start of a run.
    pub fn new() -> GradleParser {
        GradleParser::default()
    }

    /// Consumes one output line and returns the events it produced.
    pub fn feed(&mut self, _ts_ms: u64, line: &str) -> Vec<Event> {
        let t = line.trim_end();
        if let Some(rest) = t.strip_prefix("> Task ") {
            let mut it = rest.splitn(2, ' ');
            let path = it.next().unwrap_or("").to_string();
            let outcome = match it.next().map(str::trim) {
                None | Some("") => TaskOutcome::Executed,
                Some("UP-TO-DATE") => TaskOutcome::UpToDate,
                Some("FROM-CACHE") => TaskOutcome::FromCache,
                Some("FAILED") => TaskOutcome::Failed,
                Some("SKIPPED" | "NO-SOURCE") => TaskOutcome::Skipped,
                Some(_) => TaskOutcome::Executed,
            };
            self.current = path.clone();
            return vec![Event::GradleTask { path, outcome }];
        }
        for (prefix, success) in [("BUILD SUCCESSFUL", true), ("BUILD FAILED", false)] {
            if let Some(rest) = t.strip_prefix(prefix) {
                let reported_secs = rest.trim().strip_prefix("in ").and_then(parse_secs);
                return vec![Event::BuildFinished {
                    success,
                    reported_secs,
                }];
            }
        }
        // `ClassName > testName FAILED` from the test logger.
        if let Some(head) = t.strip_suffix(" FAILED")
            && !t.starts_with(' ')
            && let Some((class, test)) = head.split_once(" > ")
        {
            return vec![Event::TestFailed {
                target: self.current.clone(),
                test: format!("{class} > {test}"),
            }];
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> Vec<Event> {
        GradleParser::new().feed(0, line)
    }

    #[test]
    fn task_outcomes() {
        let cases = [
            ("> Task :app:compileDebugKotlin", TaskOutcome::Executed),
            ("> Task :x UP-TO-DATE", TaskOutcome::UpToDate),
            ("> Task :x FROM-CACHE", TaskOutcome::FromCache),
            ("> Task :x FAILED", TaskOutcome::Failed),
            ("> Task :x NO-SOURCE", TaskOutcome::Skipped),
            ("> Task :x SKIPPED", TaskOutcome::Skipped),
        ];
        for (line, outcome) in cases {
            let ev = one(line);
            assert!(
                matches!(&ev[..], [Event::GradleTask { outcome: o, .. }] if *o == outcome),
                "{line}"
            );
        }
    }

    #[test]
    fn build_result_and_test_failure() {
        assert_eq!(
            one("BUILD SUCCESSFUL in 4m 3s"),
            vec![Event::BuildFinished {
                success: true,
                reported_secs: Some(243.0)
            }]
        );
        assert_eq!(
            one("BUILD FAILED in 2s"),
            vec![Event::BuildFinished {
                success: false,
                reported_secs: Some(2.0)
            }]
        );
        let mut p = GradleParser::new();
        p.feed(0, "> Task :app:testDebugUnitTest");
        assert_eq!(
            p.feed(1, "FooTest > bar FAILED"),
            vec![Event::TestFailed {
                target: ":app:testDebugUnitTest".into(),
                test: "FooTest > bar".into()
            }]
        );
        assert!(p.feed(2, "Deprecated Gradle features were used").is_empty());
    }
}
