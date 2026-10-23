//! Short plain-text and structured renderings of daemon data, and size bounds.

use serde_json::{Value, json};
use tasks_core::Phase;
use tasks_daemon::api::{GrepGroup, JobView};

/// Most bytes of log or listing text one tool result carries.
pub const MAX_TEXT_BYTES: usize = 32 * 1024;

/// The lower-case tag of a phase.
pub fn phase_tag(p: &Phase) -> &'static str {
    match p {
        Phase::Queued => "queued",
        Phase::Waiting(_) => "waiting",
        Phase::Running => "running",
        Phase::Passed => "passed",
        Phase::Failed { .. } => "failed",
        Phase::TimedOut { .. } => "timed_out",
        Phase::NotStarted { .. } => "not_started",
        Phase::Cancelled => "cancelled",
        Phase::Interrupted => "interrupted",
    }
}

/// The phase with its exit code or reason.
pub fn phase_text(j: &JobView) -> String {
    // a forwarded job the peer holds back on its own gates is waiting, not running
    let tag = if j.remote_wait.is_some() {
        "waiting"
    } else {
        phase_tag(&j.phase)
    };
    let mut s = match &j.phase {
        Phase::Failed { exit: Some(c) } | Phase::TimedOut { exit: Some(c) } => {
            format!("{tag} (exit {c})")
        }
        Phase::NotStarted { reason } => format!("{tag} ({reason})"),
        Phase::Passed => match j.exit {
            Some(c) if c != 0 => format!("{tag} (exit {c})"),
            _ => tag.to_string(),
        },
        _ => tag.to_string(),
    };
    if let Some((peer, remote)) = &j.forwarded_to {
        s.push_str(&format!(" on {peer}"));
        if let Some(r) = remote {
            s.push_str(&format!(" as #{r}"));
        }
    }
    s
}

/// A duration as `1h02m`, `3m05s` or `7s`.
pub fn dur(secs: f64) -> String {
    let s = if secs.is_finite() && secs > 0.0 {
        secs.round() as u64
    } else {
        0
    };
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, _) => format!("{h}h{m:02}m"),
    }
}

/// Elapsed seconds of a job at `now_ms`: run time, else queue time.
pub fn elapsed_secs(j: &JobView, now_ms: i64) -> f64 {
    let (from, to) = match (j.started_at_ms, j.finished_at_ms) {
        (Some(s), Some(f)) => (s, f),
        (Some(s), None) => (s, now_ms),
        (None, Some(f)) => (j.submitted_at_ms, f),
        (None, None) => (j.submitted_at_ms, now_ms),
    };
    (to - from).max(0) as f64 / 1000.0
}

/// The command shortened to `max` characters.
pub fn short_cmd(argv: &[String], max: usize) -> String {
    let s = argv.join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// One line per job: `#id phase owner age command`.
pub fn job_line(j: &JobView, now_ms: i64) -> String {
    format!(
        "#{} {} {} {} {}",
        j.id.0,
        phase_text(j),
        j.owner,
        dur(elapsed_secs(j, now_ms)),
        short_cmd(&j.argv, 80)
    )
}

/// The structured form of a job.
pub fn job_json(j: &JobView, now_ms: i64) -> Value {
    json!({
        "id": j.id.0,
        "owner": j.owner,
        "phase": phase_tag(&j.phase),
        "terminal": j.phase.is_terminal(),
        "exit": j.exit,
        "argv": j.argv,
        "cwd": j.cwd,
        "summary": j.summary,
        "wait_text": j.wait_text,
        "machine": j.machine,
        "forwarded_to": j.forwarded_to,
        "after": j.after.iter().map(|i| i.0).collect::<Vec<_>>(),
        "plan": j.plan,
        "submitted_at_ms": j.submitted_at_ms,
        "started_at_ms": j.started_at_ms,
        "finished_at_ms": j.finished_at_ms,
        "elapsed_secs": elapsed_secs(j, now_ms),
        "cancel_requested_by": j.cancel_requested_by,
        "cancel_reason": j.cancel_reason,
        "timeout_secs": j.timeout_secs,
        "figures": j.figures,
        "priority": j.priority.as_str(),
        "priority_reason": j.priority_reason,
        "remote_wait": j.remote_wait,
        "fetch": j.fetch,
        "fetch_failed": tasks_core::any_failed(&j.fetch),
    })
}

/// The multi-line description of one job.
pub fn job_detail(j: &JobView, now_ms: i64) -> String {
    let mut s = format!(
        "job {} {} owner {} {}\n",
        j.id.0,
        phase_text(j),
        j.owner,
        dur(elapsed_secs(j, now_ms))
    );
    s.push_str(&format!("command: {}\n", short_cmd(&j.argv, 400)));
    s.push_str(&format!("cwd: {}\n", j.cwd.display()));
    if let Some(m) = &j.machine {
        s.push_str(&format!("machine: {m}\n"));
    }
    if let Some(w) = &j.wait_text {
        s.push_str(&format!("waiting: {w}\n"));
    }
    if let Some(w) = &j.remote_wait {
        s.push_str(&format!("waiting: {w}\n"));
    }
    if j.priority != tasks_core::Priority::Normal {
        s.push_str(&format!("priority: {}", j.priority));
        if let Some(r) = &j.priority_reason {
            s.push_str(&format!(" (reason: {r})"));
        }
        s.push('\n');
    }
    if let Some(t) = &j.summary {
        s.push_str(&format!("summary: {t}\n"));
    }
    if let Some(who) = &j.cancel_requested_by {
        s.push_str(&format!("cancel requested by {who}\n"));
        if let Some(r) = &j.cancel_reason {
            s.push_str(&format!("cancel reason: {r}\n"));
        }
    }
    if !j.after.is_empty() {
        let ids: Vec<String> = j.after.iter().map(|i| i.0.to_string()).collect();
        s.push_str(&format!("after: {}\n", ids.join(", ")));
    }
    if let Some((plan, step)) = &j.plan {
        s.push_str(&format!("plan {plan} step {step}\n"));
    }
    if let Some(f) = &j.figures {
        s.push_str(&format!("cost: {}\n", f.summary()));
    }
    for r in &j.fetch {
        s.push_str(&format!("fetch: {}\n", r.line()));
    }
    s
}

/// The first `max` bytes of `s`, cut at a line end when one is near. True when cut.
pub fn bound_head(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let head = s.get(..end).unwrap_or("");
    match head.rfind('\n') {
        Some(nl) if nl > end / 2 => (head.get(..=nl).unwrap_or(head), true),
        _ => (head, true),
    }
}

/// The last `max` bytes of `s`, starting at a line start when one is near. True when cut.
pub fn bound_tail(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    let tail = s.get(start..).unwrap_or("");
    match tail.find('\n') {
        Some(nl) if nl < tail.len() / 2 => (tail.get(nl + 1..).unwrap_or(tail), true),
        _ => (tail, true),
    }
}

/// Grep groups as `N:text` for matches and `N-text` for context, groups split by `--`.
pub fn grep_text(groups: &[GrepGroup]) -> String {
    let mut out = String::new();
    for (i, g) in groups.iter().enumerate() {
        if i > 0 {
            out.push_str("--\n");
        }
        for l in &g.lines {
            let sep = if l.matched { ':' } else { '-' };
            out.push_str(&format!("{}{sep}{}\n", l.n, l.text));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_short() {
        assert_eq!(dur(7.2), "7s");
        assert_eq!(dur(185.0), "3m05s");
        assert_eq!(dur(3720.0), "1h02m");
        assert_eq!(dur(-4.0), "0s");
    }

    #[test]
    fn bounds_cut_at_char_and_line_edges() {
        let s = "aaaa\nbbbb\ncccc\n";
        assert_eq!(bound_head(s, 100), (s, false));
        assert_eq!(bound_head(s, 12), ("aaaa\nbbbb\n", true));
        assert_eq!(bound_tail(s, 100), (s, false));
        assert_eq!(bound_tail(s, 12), ("bbbb\ncccc\n", true));
        let u = "ééééé";
        let (h, cut) = bound_head(u, 5);
        assert!(cut && h == "éé");
        let (t, cut) = bound_tail(u, 5);
        assert!(cut && t == "éé");
    }

    #[test]
    fn short_cmd_marks_the_cut() {
        let a = vec!["cargo".to_string(), "test".to_string()];
        assert_eq!(short_cmd(&a, 20), "cargo test");
        assert_eq!(short_cmd(&a, 6), "cargo…");
    }
}
