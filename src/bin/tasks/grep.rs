//! `tasks logs JOB_ID --grep PATTERN`: matching lines of a job's retained log (the whole
//! `job.log` file, not the head-and-tail view), read straight from the state directory.
use crate::follow::job_log;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use tinline_task_manager::log_search::{MAX_CONTEXT, Pattern};
use tinline_task_manager::{Result, state_dir};

const USAGE: &str =
    "usage: tasks logs JOB_ID --grep PATTERN [--context N] [--fixed] [--ignore-case]
  Prints the lines of the job's retained log matching PATTERN as LINE:TEXT, with N lines of
  context (default 0) as LINE-TEXT and `--` between separate groups. PATTERN is a regular
  expression subset (literals, `.`, classes, `* + ?`, `^ $`, `|`; no counted repetition or
  back-references); --fixed makes it a plain substring. Exit status 1 when nothing matches.";

pub fn run(args: &[String]) -> Result<()> {
    let mut id = None;
    let (mut pattern, mut context) = (None, 0usize);
    let (mut fixed, mut ignore_case) = (false, false);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--grep" => pattern = Some(rest.next().ok_or(USAGE)?.clone()),
            "--context" => context = rest.next().ok_or(USAGE)?.parse().map_err(|_| USAGE)?,
            "--fixed" => fixed = true,
            "--ignore-case" => ignore_case = true,
            _ if id.is_none() && !arg.starts_with("--") => id = Some(arg.parse::<i64>()?),
            _ => return Err(USAGE.into()),
        }
    }
    let (Some(id), Some(pattern)) = (id, pattern) else {
        return Err(USAGE.into());
    };
    if context > MAX_CONTEXT {
        return Err(format!("--context is at most {MAX_CONTEXT}").into());
    }
    let pattern = Pattern::new(&pattern, !fixed, ignore_case)?;
    let file = match std::fs::File::open(job_log(&state_dir(), id)) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("job {id} has no log").into());
        }
        Err(e) => return Err(e.into()),
    };
    // A closed stdout (`| head`) ends the search quietly instead of panicking.
    let mut stdout = std::io::stdout().lock();
    let found = match grep(BufReader::new(file), &pattern, context, &mut stdout) {
        Ok(found) => found,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if found == 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Writes the matching lines of `log` with `context` lines around them; returns the match count.
fn grep(
    log: impl BufRead,
    pattern: &Pattern,
    context: usize,
    out: &mut impl Write,
) -> std::io::Result<usize> {
    let mut before: VecDeque<(usize, String)> = VecDeque::new();
    // Number of the last line written, and the context lines still owed after a match.
    let (mut written, mut owed, mut found) = (0usize, 0usize, 0usize);
    for (index, raw) in log.split(b'\n').enumerate() {
        let number = index + 1;
        let text = String::from_utf8_lossy(&raw?)
            .trim_end_matches('\r')
            .to_owned();
        if pattern.is_match(&text) {
            found += 1;
            let first = before.front().map_or(number, |(n, _)| *n);
            if context > 0 && written > 0 && first > written + 1 {
                writeln!(out, "--")?;
            }
            for (n, line) in before.drain(..) {
                writeln!(out, "{n}-{line}")?;
            }
            writeln!(out, "{number}:{text}")?;
            written = number;
            owed = context;
        } else if owed > 0 {
            writeln!(out, "{number}-{text}")?;
            written = number;
            owed -= 1;
        } else if context > 0 {
            before.push_back((number, text));
            if before.len() > context {
                before.pop_front();
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_grep(log: &str, pattern: &str, regex: bool, context: usize) -> (String, usize) {
        let pattern = Pattern::new(pattern, regex, false).unwrap();
        let mut out = Vec::new();
        let found = grep(log.as_bytes(), &pattern, context, &mut out).unwrap();
        (String::from_utf8(out).unwrap(), found)
    }

    const LOG: &str = "a\nb\nerror: one\nc\nd\ne\nf\nerror: two\nerror: three\ng\n";

    #[test]
    fn prints_matches_with_line_numbers() {
        let (out, found) = run_grep(LOG, "error: ", false, 0);
        assert_eq!(out, "3:error: one\n8:error: two\n9:error: three\n");
        assert_eq!(found, 3);
        let (out, found) = run_grep(LOG, "^error: t", true, 0);
        assert_eq!(out, "8:error: two\n9:error: three\n");
        assert_eq!(found, 2);
        assert_eq!(run_grep(LOG, "absent", false, 2), (String::new(), 0));
    }

    #[test]
    fn context_marks_lines_and_separates_groups() {
        let (out, _) = run_grep(LOG, "error: one", false, 1);
        assert_eq!(out, "2-b\n3:error: one\n4-c\n");
        // Touching windows merge; distant ones are split by `--`.
        let (out, found) = run_grep(LOG, "error: ", false, 2);
        assert_eq!(found, 3);
        assert_eq!(
            out,
            "1-a\n2-b\n3:error: one\n4-c\n5-d\n6-e\n7-f\n8:error: two\n9:error: three\n10-g\n"
        );
        let (out, _) = run_grep(LOG, "error: ", false, 1);
        assert_eq!(
            out,
            "2-b\n3:error: one\n4-c\n--\n7-f\n8:error: two\n9:error: three\n10-g\n"
        );
    }

    #[test]
    fn rejects_bad_arguments() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(run(&args(&["7"])).is_err());
        assert!(run(&args(&["7", "--grep"])).is_err());
        assert!(run(&args(&["7", "--grep", "x", "--context", "many"])).is_err());
        assert!(run(&args(&["7", "--grep", "x", "--context", "11"])).is_err());
        assert!(run(&args(&["7", "--grep", "a{2}"])).is_err());
    }
}
