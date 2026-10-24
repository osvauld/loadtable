//! Retrying the tests that failed under load, alone (R-716..R-718).
//!
//! A `cargo test` job with `retry_alone` that fails with libtest's `failures:` list gets ONE
//! follow-up job: the same command with `-- --exact NAME... --test-threads=1`, marked `isolate`
//! so nothing else runs beside it. The original keeps its failed exit; its one-line outcome says
//! what the retry found. The parsing and the wording are pure functions; the queueing is the
//! scheduler's.

use crate::scheduler::{Scheduler, lock};
use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use tasks_core::{JobId, Phase, StepHints};
use tasks_ledger::NewJob;

/// More failures than this are a real breakage, not load: no retry.
pub(crate) const MAX_RETRY_TESTS: usize = 50;

/// How many names an outcome line spells out before it says "and N more".
const SHOWN_NAMES: usize = 5;

/// libtest flags that mean the same on a retry and are kept; every other word after `--`
/// (filters, `--skip X`, `--test-threads N`) is dropped because the exact names replace them.
const KEPT_FLAGS: [&str; 7] = [
    "--nocapture",
    "--no-capture",
    "--show-output",
    "--ignored",
    "--include-ignored",
    "--quiet",
    "-q",
];

/// Removes ANSI colour escapes (`ESC [ ... letter`), which cargo adds when it thinks it is on a
/// terminal.
fn plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The names of the tests that failed, in the order libtest reported them, without repeats:
/// the names under a `failures:` heading, and every `test NAME ... FAILED` line.
///
/// libtest prints `failures:` twice per test binary: first followed by a blank line and the
/// `---- NAME stdout ----` captures, then followed directly by the indented names. Only the
/// second shape is a list.
pub(crate) fn failed_tests(log: &str) -> Vec<String> {
    let lines: Vec<String> = log.lines().map(plain).collect();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut add = |name: &str| {
        let name = name.trim();
        if !name.is_empty() && seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
    };
    let mut i = 0;
    while let Some(line) = lines.get(i) {
        i += 1;
        if line.trim_end() == "failures:" {
            while let Some(n) = lines.get(i)
                && n.starts_with("    ")
                && !n.trim().is_empty()
            {
                add(n);
                i += 1;
            }
        } else if let Some(rest) = line.strip_prefix("test ")
            && let Some(name) = rest.trim_end().strip_suffix(" ... FAILED")
        {
            add(name);
        }
    }
    out
}

/// The argv of the follow-up job: `argv` as submitted with its libtest part replaced by
/// `--exact NAME... --test-threads=1`. `None` when this is not a `cargo test` that can be
/// retried (not cargo, `--no-run`, nothing failed, or too many failed).
pub(crate) fn retry_argv(argv: &[String], failed: &[String]) -> Option<Vec<String>> {
    if failed.is_empty() || failed.len() > MAX_RETRY_TESTS {
        return None;
    }
    let program = argv.first()?.rsplit('/').next()?;
    if program != "cargo" {
        return None;
    }
    // `cargo test` or `cargo +toolchain test`
    let sub = argv
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('+'))
        .map(String::as_str);
    if sub != Some("test") {
        return None;
    }
    let (head, tail) = match argv.iter().position(|a| a == "--") {
        Some(i) => (argv.get(..i)?, argv.get(i + 1..)?),
        None => (argv, &[][..]),
    };
    if head.iter().any(|a| a == "--no-run") {
        return None;
    }
    let mut out = head.to_vec();
    out.push("--".to_string());
    out.extend(
        tail.iter()
            .filter(|a| KEPT_FLAGS.contains(&a.as_str()))
            .cloned(),
    );
    out.push("--exact".to_string());
    out.extend(failed.iter().cloned());
    out.push("--test-threads=1".to_string());
    Some(out)
}

/// The test names a follow-up argv asks for: the words between `--exact` and `--test-threads=1`.
pub(crate) fn retried_names(argv: &[String]) -> Vec<String> {
    argv.iter()
        .skip_while(|a| *a != "--exact")
        .skip(1)
        .take_while(|a| !a.starts_with("--test-threads"))
        .cloned()
        .collect()
}

fn list(names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(SHOWN_NAMES).map(String::as_str).collect();
    let more = names.len().saturating_sub(SHOWN_NAMES);
    if more == 0 {
        shown.join(", ")
    } else {
        format!("{} and {more} more", shown.join(", "))
    }
}

fn tests(n: usize) -> String {
    if n == 1 {
        "1 test".to_string()
    } else {
        format!("{n} tests")
    }
}

/// The outcome line while the retry is queued or running.
pub(crate) fn retrying_text(original: &str, failed: &[String], retry: JobId) -> String {
    format!(
        "{original}; {} failed, retrying alone as job #{}: {}",
        tests(failed.len()),
        retry.0,
        list(failed)
    )
}

/// The outcome line once the retry ended. `asked` are the tests it ran; `failed_again` the
/// failures its own log lists; `passed` says whether the job passed.
pub(crate) fn verdict_text(
    asked: &[String],
    failed_again: &[String],
    passed: bool,
    retry: JobId,
    ended: &str,
) -> String {
    if passed {
        return format!(
            "{} failed under load and passed alone: {} (retry: job #{})",
            tests(asked.len()),
            list(asked),
            retry.0
        );
    }
    if failed_again.is_empty() {
        // the retry itself broke (build error, cancelled, timed out): nothing can be said
        // about the tests
        return format!(
            "{} failed; the retry alone (job #{}) {ended}",
            tests(asked.len()),
            retry.0
        );
    }
    let passed_alone: Vec<String> = asked
        .iter()
        .filter(|a| !failed_again.contains(a))
        .cloned()
        .collect();
    let again = format!("failed again alone: {}", list(failed_again));
    if passed_alone.is_empty() {
        format!("{again} (retry: job #{})", retry.0)
    } else {
        format!(
            "{} failed under load and passed alone: {}; {again} (retry: job #{})",
            tests(passed_alone.len()),
            list(&passed_alone),
            retry.0
        )
    }
}

/// How much of the end of a log is searched for the failures list.
const LOG_TAIL_BYTES: u64 = 8 << 20;

/// The last [`LOG_TAIL_BYTES`] of a file as text; empty when it cannot be read.
fn log_tail(path: &Path) -> String {
    let read = || -> std::io::Result<Vec<u8>> {
        let mut f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    };
    read()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

impl Scheduler {
    /// Called once a job has ended. A `retry_alone` `cargo test` job that failed on its own (not
    /// cancelled, timed out or killed by us), that was not sent here by a peer (the origin
    /// retries) and is not itself a retry, gets one follow-up job for its failed tests.
    pub(crate) fn retry_alone_after(&self, id: JobId) {
        let Some(Some(rec)) = self.guard("get", self.ledger.get(id)) else {
            return;
        };
        let wanted = rec.spec.retry_alone || rec.hints.retry_alone;
        let failed_alone = matches!(rec.phase, Phase::Failed { exit: Some(c) } if c != 0)
            && rec.signal.is_none()
            && rec.timeout.is_none()
            && !rec.cancel_requested();
        if !wanted || !failed_alone || rec.origin.is_some() || rec.hints.retry_of.is_some() {
            return;
        }
        let failed = failed_tests(&log_tail(&self.cfg.log_path(id)));
        if failed.is_empty() {
            return;
        }
        let original = rec.summary.clone().unwrap_or_else(|| "failed".to_string());
        let Some(argv) = retry_argv(&rec.spec.argv, &failed) else {
            if failed.len() > MAX_RETRY_TESTS {
                let text = format!(
                    "{original}; {} failed, too many to retry alone",
                    tests(failed.len())
                );
                let _ = self.guard("set_summary", self.ledger.set_summary(id, &text));
            }
            return;
        };
        let mut spec = rec.spec.clone();
        spec.argv = argv;
        spec.isolate = true;
        spec.light = false;
        // one retry at most
        spec.retry_alone = false;
        spec.after = Vec::new();
        spec.fetch = Vec::new();
        if let Err(e) = spec.validate() {
            self.note_error(&format!("retry of job {} alone: {e}", id.0));
            return;
        }
        let hints = StepHints {
            isolate: true,
            retry_of: Some(id.0),
            ..StepHints::default()
        };
        let job = NewJob {
            spec,
            hints,
            step: None,
            origin: None,
        };
        let Some(retry) = self.guard("submit retry", self.ledger.submit_job(&job)) else {
            return;
        };
        lock(&self.state).retries.insert(retry, id);
        let text = retrying_text(&original, &failed, retry);
        let _ = self.guard("set_summary", self.ledger.set_summary(id, &text));
        self.wake.notify();
    }

    /// Writes what each finished retry found onto the job it retried.
    pub(crate) fn settle_retries(&self) {
        let pending: Vec<(JobId, JobId)> = lock(&self.state)
            .retries
            .iter()
            .map(|(r, o)| (*r, *o))
            .collect();
        for (retry, original) in pending {
            let rec = match self.guard("get", self.ledger.get(retry)) {
                Some(Some(r)) => r,
                // pruned: nothing more to learn
                Some(None) => {
                    lock(&self.state).retries.remove(&retry);
                    continue;
                }
                None => continue,
            };
            if !rec.phase.is_terminal() {
                continue;
            }
            let asked = retried_names(&rec.spec.argv);
            let again = failed_tests(&log_tail(&self.cfg.log_path(retry)));
            let ended = match rec.phase {
                Phase::Cancelled => "was cancelled",
                Phase::TimedOut { .. } => "timed out",
                Phase::Interrupted => "was interrupted",
                Phase::NotStarted { .. } => "did not start",
                _ => "failed without naming a test",
            };
            let text = verdict_text(&asked, &again, rec.phase == Phase::Passed, retry, ended);
            let _ = self.guard("set_summary", self.ledger.set_summary(original, &text));
            lock(&self.state).retries.remove(&retry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    // a real libtest run: two failures, one of them with captured output
    const LOG: &str = "\
[tasks] cache: sccache /x
running 3 tests
test glare::quiet ... ok
test glare::glare_leaves_exactly_one_call ... FAILED
test shards::a_live_push_from_a_clock_30_hours_ahead_is_refused ... FAILED

failures:

---- glare::glare_leaves_exactly_one_call stdout ----
thread 'glare::glare_leaves_exactly_one_call' panicked at tests/calls.rs:88:9:
    assertion `left == right` failed

---- shards::a_live_push_from_a_clock_30_hours_ahead_is_refused stdout ----
thread panicked at tests/chat_shards.rs:12:5:


failures:
    glare::glare_leaves_exactly_one_call
    shards::a_live_push_from_a_clock_30_hours_ahead_is_refused

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 41.20s

error: test failed, to rerun pass `-p p2pcore --test calls`
";

    // R-716
    #[test]
    fn r716_the_failures_list_and_the_failed_lines_give_each_name_once_in_order() {
        assert_eq!(
            failed_tests(LOG),
            s(&[
                "glare::glare_leaves_exactly_one_call",
                "shards::a_live_push_from_a_clock_30_hours_ahead_is_refused"
            ])
        );
    }

    // R-716
    #[test]
    fn r716_a_log_without_failures_or_with_only_a_build_error_gives_nothing() {
        assert!(failed_tests("").is_empty());
        assert!(
            failed_tests("error[E0425]: cannot find value\nerror: could not compile\n").is_empty()
        );
        // the summary line and the heading of the captures are not names
        assert!(
            failed_tests(
                "test result: FAILED. 0 passed; 1 failed\nfailures:\n\n---- a stdout ----\n"
            )
            .is_empty()
        );
    }

    // R-716
    #[test]
    fn r716_colours_doc_tests_and_a_failure_in_the_middle_of_captured_output_are_handled() {
        let log = "test \u{1b}[0m\u{1b}[31mx ... FAILED\u{1b}[0m\n\
                   test src/lib.rs - total (line 12) ... FAILED\n\
                   failures:\n    src/lib.rs - total (line 12)\n    other\n\ntest result: FAILED.\n";
        assert_eq!(
            failed_tests(log),
            s(&["x", "src/lib.rs - total (line 12)", "other"])
        );
    }

    // R-716
    #[test]
    fn r716_a_same_named_test_in_two_binaries_is_retried_once() {
        let log = "failures:\n    t::a\n\ntest result: FAILED.\nfailures:\n    t::a\n    t::b\n";
        assert_eq!(failed_tests(log), s(&["t::a", "t::b"]));
    }

    // R-716
    #[test]
    fn r716_the_retry_keeps_the_target_selection_and_runs_the_names_exactly_and_serially() {
        let argv = s(&["cargo", "test", "-p", "p2pcore", "--test", "calls"]);
        let got = retry_argv(&argv, &s(&["glare", "other"])).unwrap();
        assert_eq!(
            got,
            s(&[
                "cargo",
                "test",
                "-p",
                "p2pcore",
                "--test",
                "calls",
                "--",
                "--exact",
                "glare",
                "other",
                "--test-threads=1"
            ])
        );
        assert_eq!(retried_names(&got), s(&["glare", "other"]));
    }

    // R-716
    #[test]
    fn r716_libtest_words_after_the_separator_are_replaced_but_output_flags_are_kept() {
        let argv = s(&[
            "cargo",
            "+nightly",
            "test",
            "--lib",
            "--",
            "--nocapture",
            "--test-threads=8",
            "--skip",
            "slow",
            "glare",
        ]);
        let got = retry_argv(&argv, &s(&["glare"])).unwrap();
        assert_eq!(
            got,
            s(&[
                "cargo",
                "+nightly",
                "test",
                "--lib",
                "--",
                "--nocapture",
                "--exact",
                "glare",
                "--test-threads=1"
            ])
        );
    }

    // R-716
    #[test]
    fn r716_only_a_cargo_test_that_ran_and_failed_a_few_tests_is_retried() {
        let ok = s(&["a"]);
        assert!(retry_argv(&s(&["cargo", "build"]), &ok).is_none());
        assert!(retry_argv(&s(&["make", "test"]), &ok).is_none());
        assert!(retry_argv(&s(&["cargo", "test", "--no-run"]), &ok).is_none());
        assert!(retry_argv(&s(&["cargo", "test"]), &[]).is_none());
        let many: Vec<String> = (0..=MAX_RETRY_TESTS).map(|i| format!("t{i}")).collect();
        assert!(retry_argv(&s(&["cargo", "test"]), &many).is_none());
        assert!(retry_argv(&s(&["/usr/bin/cargo", "test"]), &ok).is_some());
    }

    // R-717
    #[test]
    fn r717_the_outcome_says_what_passed_alone_and_what_failed_again() {
        let asked = s(&["a", "b"]);
        assert_eq!(
            verdict_text(&asked, &[], true, JobId(9), "passed"),
            "2 tests failed under load and passed alone: a, b (retry: job #9)"
        );
        assert_eq!(
            verdict_text(&s(&["a"]), &s(&["a"]), false, JobId(9), "failed"),
            "failed again alone: a (retry: job #9)"
        );
        assert_eq!(
            verdict_text(&asked, &s(&["b"]), false, JobId(9), "failed"),
            "1 test failed under load and passed alone: a; failed again alone: b (retry: job #9)"
        );
        assert_eq!(
            verdict_text(&asked, &[], false, JobId(9), "was cancelled"),
            "2 tests failed; the retry alone (job #9) was cancelled"
        );
    }

    // R-717
    #[test]
    fn r717_a_long_list_is_cut_after_five_names() {
        let names: Vec<String> = (1..=8).map(|i| format!("t{i}")).collect();
        assert_eq!(list(&names), "t1, t2, t3, t4, t5 and 3 more");
        assert_eq!(
            retrying_text("exit code 101", &names[..2], JobId(4)),
            "exit code 101; 2 tests failed, retrying alone as job #4: t1, t2"
        );
    }
}
