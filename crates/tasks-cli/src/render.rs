//! Rendering: pure functions from API values to text. Tested against fixed inputs.

use std::fmt::Write;
use std::path::Path;
use tasks_core::Phase;
use tasks_daemon::api::{
    EstimateView, GrepGroup, JobGrep, JobView, Source, StatusSnapshot, WaiterView,
};
use tasks_timing::dur::{fmt_eta, fmt_precise};

/// Longest `NAME=value` word shown whole; longer values are abbreviated for display only.
const ENV_VALUE_MAX: usize = 40;
/// Longest command text in a table row.
const TABLE_COMMAND: usize = 60;
/// Longest command text in the submit line.
const SUBMIT_COMMAND: usize = 160;
/// Widest cwd column.
const TABLE_CWD: usize = 36;

/// Keeps the first `max` characters, ending with an ellipsis when something was cut.
pub fn truncate_end(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Keeps the last `max` characters, starting with an ellipsis when something was cut.
pub fn truncate_start(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let skip = n - max.saturating_sub(1);
    let mut out = String::from("…");
    out.extend(s.chars().skip(skip));
    out
}

/// One argument, quoted so that arguments stay distinguishable.
pub fn quote_arg(a: &str) -> String {
    let plain = !a.is_empty()
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if plain {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', "'\\''"))
    }
}

/// `NAME=value` with a long value cut for display. Anything else is returned unchanged.
pub fn abbreviate_arg(a: &str) -> String {
    let Some((name, value)) = a.split_once('=') else {
        return a.to_string();
    };
    let is_env = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    let n = value.chars().count();
    if !is_env || n <= ENV_VALUE_MAX {
        return a.to_string();
    }
    let head: String = value.chars().take(ENV_VALUE_MAX - 12).collect();
    format!("{name}={head}…({n} chars)")
}

/// The command as shown: env values abbreviated, arguments quoted. Display only.
pub fn command_display(argv: &[String]) -> String {
    argv.iter()
        .map(|a| quote_arg(&abbreviate_arg(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The first client line; scripts parse the `tasks: submitted job N;` prefix (R-183).
pub fn submit_line(id: i64, cwd: &Path, argv: &[String]) -> String {
    format!(
        "tasks: submitted job {id}; cwd {}; argv {}; Ctrl-C detaches, tasks2 cancel {id} cancels",
        cwd.display(),
        truncate_end(&command_display(argv), SUBMIT_COMMAND)
    )
}

/// The detach message (R-104).
pub fn still_live(id: i64) -> String {
    format!(
        "tasks: job {id} is still live; query with `tasks2 logs {id}` / `tasks2 list`; do not resubmit (`tasks2 cancel {id}` cancels)"
    )
}

/// The phase tag used in filters and JSON.
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

/// The peer's phase to show: a job that has ended here never shows an unfinished remote phase
/// (rows written before the final phase was stored still carry the last one mirrored).
fn shown_remote_phase(j: &JobView) -> Option<&Phase> {
    j.remote_phase
        .as_ref()
        .filter(|rp| !j.phase.is_terminal() || rp.is_terminal())
}

/// Phase plus where a forward runs and its remote job (R-186).
pub fn phase_label(j: &JobView) -> String {
    // a forwarded job the peer holds back on its own gates is waiting, not running
    let mut s = if j.remote_wait.is_some() {
        "waiting".to_string()
    } else {
        phase_tag(&j.phase).to_string()
    };
    if j.phase == Phase::Cancelled
        && let Some(why) = j.cancel_reason.as_ref().filter(|w| !w.is_empty())
    {
        let _ = write!(s, " ({})", truncate_end(why, 30));
    }
    if let Some((peer, remote)) = &j.forwarded_to {
        let _ = write!(s, " → {peer}");
        if let Some(r) = remote {
            let _ = write!(s, " #{r}");
        }
        // what the peer says about the job, beside the local phase (R-015, R-186)
        let mut notes = Vec::new();
        if let Some(rp) = shown_remote_phase(j) {
            notes.push(format!("remote {}", phase_tag(rp)));
        }
        if !j.phase.is_terminal()
            && let Some(t) = j.summary.as_ref().filter(|t| !t.is_empty())
        {
            notes.push(truncate_end(t, 30));
        }
        if !notes.is_empty() {
            let _ = write!(s, " ({})", notes.join(", "));
        }
    }
    s
}

/// The exit column: only `passed` shows 0 (R-009).
pub fn exit_label(j: &JobView) -> String {
    match (&j.phase, j.exit) {
        (Phase::Passed, _) => "0".to_string(),
        (_, Some(c)) if c != 0 => c.to_string(),
        _ => "-".to_string(),
    }
}

/// The exit code `tasks2 run` returns for a finished job: the job's own code when it has one.
/// A job that passed but could not bring back a file it was asked to fetch exits 1, as the
/// earlier release did (R-504); the command's own exit code is never changed.
pub fn job_exit_code(j: &JobView) -> i32 {
    match &j.phase {
        Phase::Passed if tasks_core::any_failed(&j.fetch) => 1,
        Phase::Passed => 0,
        Phase::Failed { .. } | Phase::TimedOut { .. } => {
            let own = j.exit.filter(|c| *c != 0);
            let own = own.or(match &j.phase {
                Phase::Failed { exit } | Phase::TimedOut { exit } => exit.filter(|c| *c != 0),
                _ => None,
            });
            match (own, &j.phase) {
                (Some(c), _) => c & 0xff,
                (None, Phase::TimedOut { .. }) => 124,
                (None, _) => 1,
            }
        }
        Phase::Cancelled | Phase::Interrupted | Phase::NotStarted { .. } => 125,
        Phase::Queued | Phase::Waiting(_) | Phase::Running => 1,
    }
}

/// The last stdout line of `tasks2 run`: `job N <word> (exit X) owner=W [on HOST]`, so that
/// `| tail -1` recovers the id and the result (REQUIREMENTS 1.1). `HOST` is the peer a forwarded
/// job ran on. A timeout or a job that never started reads as `failed`, with the exit code
/// `tasks2 run` itself returns.
pub fn summary_line(j: &JobView) -> String {
    let word = match &j.phase {
        Phase::Passed => "passed",
        Phase::Cancelled => "cancelled",
        Phase::Interrupted => "interrupted",
        _ => "failed",
    };
    let mut s = format!(
        "job {} {word} (exit {}) owner={}",
        j.id.0,
        job_exit_code(j),
        j.owner
    );
    if let Some((peer, _)) = &j.forwarded_to {
        s.push_str(&format!(" on {peer}"));
    }
    s
}

fn ms_to_secs(ms: i64) -> f64 {
    ms.max(0) as f64 / 1000.0
}

/// Age since submission, or to the finish when done.
pub fn age(j: &JobView, now_ms: i64) -> String {
    let end = j.finished_at_ms.unwrap_or(now_ms);
    fmt_precise(ms_to_secs(end - j.submitted_at_ms))
}

fn run_time(j: &JobView, now_ms: i64) -> String {
    match j.started_at_ms.filter(|_| !is_waiting(j)) {
        None => "-".to_string(),
        Some(s) => {
            let end = j.finished_at_ms.unwrap_or(now_ms);
            let text = fmt_precise(ms_to_secs(end - s));
            if j.finished_at_ms.is_some() {
                text
            } else {
                format!("{text}+")
            }
        }
    }
}

fn is_waiting(j: &JobView) -> bool {
    matches!(j.phase, Phase::Queued | Phase::Waiting(_)) || j.remote_wait.is_some()
}

/// Options of the list table.
#[derive(Debug, Clone, Copy, Default)]
pub struct TableOptions {
    /// One short line per job: no cwd column, no notes, no legend.
    pub brief: bool,
}

/// Home-relative path for tables.
pub fn home_relative(p: &Path, home: Option<&str>) -> String {
    let s = p.display().to_string();
    match home.and_then(|h| s.strip_prefix(h)) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => s,
    }
}

/// The notes under a queued job: what it waits for, position and start estimate (R-129, R-186).
pub fn wait_note(j: &JobView, waiter: Option<&WaiterView>) -> Option<String> {
    if !is_waiting(j) {
        return None;
    }
    let reason = waiter
        .map(|w| w.wait_text.as_str())
        .or(j.remote_wait.as_deref())
        .or(j.wait_text.as_deref())
        .filter(|t| !t.is_empty());
    let mut parts = Vec::new();
    if let Some(r) = reason {
        parts.push(format!("waits for: {r}"));
    }
    if let Some(w) = waiter {
        if w.on_peer.is_none() {
            parts.push(format!("position {}", w.position));
        }
        if !w.class_note.is_empty() {
            parts.push(w.class_note.clone());
        } else if !w.class.is_empty() && w.class != "normal" {
            parts.push(format!("class {}", w.class));
        }
        if let Some(e) = w.start_estimate_secs {
            parts.push(format!(
                "starts in ~{}{}",
                fmt_eta(e),
                if w.rough { " (rough, no history)" } else { "" }
            ));
        }
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// The job table: one line per job, notes only for queued jobs (R-185, R-186).
pub fn job_table(
    jobs: &[JobView],
    now_ms: i64,
    home: Option<&str>,
    waiters: &[WaiterView],
    opts: TableOptions,
) -> String {
    if jobs.is_empty() {
        return "(no matching jobs)\n".to_string();
    }
    let width = jobs
        .iter()
        .map(|j| phase_label(j).chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 44);
    let mut out = String::new();
    if opts.brief {
        let _ = writeln!(
            out,
            "{:>6} {:<width$} {:<4} {:<20} {:>8} COMMAND",
            "ID", "PHASE", "EXIT", "OWNER", "AGE"
        );
    } else {
        let _ = writeln!(
            out,
            "{:>6} {:<width$} {:<4} {:<20} {:>8} {:>8} {:<TABLE_CWD$} COMMAND",
            "ID", "PHASE", "EXIT", "OWNER", "AGE", "RUN", "CWD"
        );
    }
    for j in jobs {
        let owner = truncate_end(&j.owner, 20);
        let cmd = truncate_end(&command_display(&j.argv), TABLE_COMMAND);
        if opts.brief {
            let _ = writeln!(
                out,
                "{:>6} {:<width$} {:<4} {:<20} {:>8} {cmd}",
                j.id.0,
                phase_label(j),
                exit_label(j),
                owner,
                age(j, now_ms)
            );
        } else {
            let cwd = truncate_start(&home_relative(&j.cwd, home), TABLE_CWD);
            let _ = writeln!(
                out,
                "{:>6} {:<width$} {:<4} {:<20} {:>8} {:>8} {:<TABLE_CWD$} {cmd}",
                j.id.0,
                phase_label(j),
                exit_label(j),
                owner,
                age(j, now_ms),
                run_time(j, now_ms),
                cwd
            );
            let waiter = waiters.iter().find(|w| w.id == j.id);
            if let Some(n) = wait_note(j, waiter) {
                let _ = writeln!(out, "{:>6}   {n}", "");
            }
        }
    }
    if !opts.brief {
        out.push_str(TABLE_LEGEND);
        out.push('\n');
    }
    out
}

/// The line under the table.
pub const TABLE_LEGEND: &str = "tasks: AGE = since submission (to finish when done); RUN = run time so far (+ = still running). Env values over 40 chars are abbreviated here only; tasks2 list --json has the full argv.";

/// One job in full.
pub fn job_detail(j: &JobView, now_ms: i64, waiter: Option<&WaiterView>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "job {}: {}", j.id.0, phase_label(j));
    let _ = writeln!(out, "  owner:     {}", j.owner);
    let _ = writeln!(out, "  cwd:       {}", j.cwd.display());
    let _ = writeln!(out, "  command:   {}", command_display(&j.argv));
    let _ = writeln!(out, "  placement: {}, kind {}", j.placement, j.kind);
    let _ = writeln!(out, "  exit:      {}", exit_label(j));
    if let Some(s) = &j.summary {
        let _ = writeln!(out, "  summary:   {s}");
    }
    let _ = writeln!(out, "  age:       {}", age(j, now_ms));
    let _ = writeln!(out, "  run:       {}", run_time(j, now_ms));
    if let Some(m) = &j.machine {
        let _ = writeln!(out, "  machine:   {m}");
    }
    if let Some((peer, remote)) = &j.forwarded_to {
        let _ = writeln!(
            out,
            "  forwarded: to {peer}{}",
            remote.map_or(String::new(), |r| format!(" as job #{r}"))
        );
    }
    if !j.after.is_empty() {
        let ids: Vec<String> = j.after.iter().map(|d| d.0.to_string()).collect();
        let _ = writeln!(out, "  after:     {}", ids.join(", "));
    }
    if let Some((plan, step)) = &j.plan {
        let _ = writeln!(out, "  plan:      {plan} step {step}");
    }
    if let Some(w) = &j.cancel_requested_by {
        match &j.cancel_reason {
            Some(why) => {
                let _ = writeln!(out, "  cancel requested by {w}: {why}");
            }
            None => {
                let _ = writeln!(out, "  cancel requested by {w}");
            }
        }
    }
    if let Some(t) = j.timeout_secs {
        let _ = writeln!(out, "  timeout:   {}", fmt_precise(t as f64));
    }
    if j.priority != tasks_core::Priority::Normal {
        match &j.priority_reason {
            Some(r) => {
                let _ = writeln!(out, "  priority:  {} (reason: {r})", j.priority);
            }
            None => {
                let _ = writeln!(out, "  priority:  {}", j.priority);
            }
        }
    }
    if let Some(rp) = shown_remote_phase(j) {
        let _ = writeln!(out, "  remote:    {}", phase_tag(rp));
    }
    if let Some(n) = wait_note(j, waiter) {
        let _ = writeln!(out, "  {n}");
    }
    if let Some(f) = &j.figures {
        let _ = writeln!(out, "  cost:      {}", f.summary());
    }
    for (i, r) in j.fetch.iter().enumerate() {
        let label = if i == 0 { "fetch:     " } else { "           " };
        let _ = writeln!(out, "  {label}{}", r.line());
    }
    if j.fetch.is_empty() && !j.fetch_paths.is_empty() {
        let _ = writeln!(out, "  fetch:     requested {}", j.fetch_paths.join(", "));
    }
    if let Some(p) = &j.log_path {
        let _ = writeln!(out, "  log:       {}", p.display());
    }
    out
}

/// Grep output: `N:text` for matches, `N-text` for context, `--` between groups (R-112).
pub fn grep_groups(groups: &[GrepGroup]) -> String {
    let mut out = String::new();
    for (i, g) in groups.iter().enumerate() {
        if i > 0 {
            out.push_str("--\n");
        }
        for l in &g.lines {
            let sep = if l.matched { ':' } else { '-' };
            let _ = writeln!(out, "{}{sep}{}", l.n, l.text);
        }
    }
    out
}

/// Cross-job search output, newest job first, naming each job.
pub fn log_search(results: &[JobGrep], matches: usize, truncated: bool) -> String {
    let mut out = String::new();
    for r in results {
        let _ = writeln!(out, "== job {} ({}, {})", r.id.0, r.owner, r.phase);
        out.push_str(&grep_groups(&r.groups));
    }
    if results.is_empty() {
        out.push_str("(no matches)\n");
    }
    let _ = writeln!(
        out,
        "tasks: {matches} matching line(s) in {} job(s){}",
        results.len(),
        if truncated {
            "; the search hit a bound, narrow it (--phase, --since, --id) or raise --max-matches"
        } else {
            ""
        }
    );
    out
}

/// An estimate.
pub fn estimate(e: &EstimateView) -> String {
    format!(
        "estimate: median {}, p90 {}, {} sample(s)\n  {}\n  recipe: {}\n",
        fmt_precise(e.median_secs),
        fmt_precise(e.p90_secs),
        e.samples,
        e.text,
        e.recipe
    )
}

/// A running job's current resource use and its busiest program groups.
pub fn job_metrics(m: &tasks_core::metrics::JobMetricsView) -> String {
    use tasks_core::metrics::rates;
    let mut out = String::new();
    let last_two = match m.samples.as_slice() {
        [.., a, b] => Some((a, b)),
        _ => None,
    };
    let r = last_two.map(|(a, b)| rates(a, b)).unwrap_or_default();
    let na = || "n/a".to_string();
    let cores = r.cores.map_or_else(na, |c| format!("{c:.1} cores"));
    let mem = m
        .samples
        .last()
        .and_then(|s| s.mem_bytes)
        .map_or_else(na, fmt_bytes);
    let usual = m
        .usual_peak_mem_bytes
        .map(|b| format!(" (usual peak {})", fmt_bytes(b)))
        .unwrap_or_default();
    let rate = |b: Option<f64>| b.map_or_else(na, |v| format!("{}/s", fmt_bytes(v as u64)));
    let stall = r.io_stall_pct.map_or_else(na, |p| format!("{p:.0}%"));
    let quality = m.quality.map_or("unknown", |q| q.label());
    out.push_str(&format!(
        "cpu {cores}, memory {mem}{usual}\ndisk read {}, write {}, io stall {stall}\n({} samples, {quality})\n",
        rate(r.read_bps),
        rate(r.write_bps),
        m.samples.len()
    ));
    if let Some(n) = &m.note {
        out.push_str(&format!("note: {n}\n"));
    }
    for g in m.procs.iter().take(5) {
        let cpu = g.cpu_cores.map_or_else(na, |c| format!("{c:.1} cores"));
        let name = if g.others {
            g.name.clone()
        } else {
            format!("{} x{}", g.name, g.count)
        };
        out.push_str(&format!("  {name}: {cpu}, {}\n", fmt_bytes(g.rss_bytes)));
    }
    out
}

/// Bytes as GiB or MiB.
pub fn fmt_bytes(b: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    let f = b as f64;
    if f >= GIB {
        format!("{:.1} GiB", f / GIB)
    } else {
        format!("{:.0} MiB", f / MIB)
    }
}

/// Any size from bytes to TiB with one decimal (`0 B`, `4.0 KiB`, `1.0 GiB`): swap figures and
/// rates are often small.
fn fmt_size(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS.get(u).copied().unwrap_or("B"))
    }
}

fn pct(v: Option<f32>) -> String {
    v.map_or_else(|| "?".to_string(), |p| format!("{p:.1}%"))
}

fn source_name(s: Source) -> &'static str {
    match s {
        Source::Default => "default",
        Source::File => "file",
        Source::Env => "env",
    }
}

fn slots_section(out: &mut String, s: &StatusSnapshot) {
    out.push_str("slots:\n");
    for g in &s.slots {
        let _ = writeln!(out, "  {:<8} {}/{} free", g.kind, g.free, g.total);
        for b in &g.busy {
            let who = match (b.job, b.external, &b.holder) {
                (Some(j), _, _) => format!("job {}", j.0),
                (None, true, Some(h)) => format!("external work, not a ledger job: {h}"),
                (None, true, None) => "external work, not a ledger job".to_string(),
                (None, false, _) => "held, holder unknown".to_string(),
            };
            let _ = writeln!(out, "    slot {}: {who}", b.index);
        }
    }
}

fn queue_state(out: &mut String, s: &StatusSnapshot) {
    let d = &s.daemon;
    if d.draining {
        match (&d.drained_by, d.drained_at_ms) {
            // R-189: who stopped it, how long ago and how to resume, not "worker not running"
            (Some(who), Some(at)) => {
                let ago = fmt_precise(((d.now_ms - at).max(0) / 1000) as f64);
                let _ = writeln!(
                    out,
                    "queue: DRAINING: stopped by {who}, {ago} ago, resume with `tasks2 resume` (no new job starts until then or the next daemon start)"
                );
            }
            _ => out.push_str(
                "queue: DRAINING: no new job starts until the daemon restarts; `tasks2 resume` resumes it\n",
            ),
        }
    }
    if !d.execution_enabled {
        out.push_str(
            "queue: STOPPED: queued jobs do not start; `tasks2 execution start` lets them run\n",
        );
    }
}

/// `tasks2 status`.
pub fn status(s: &StatusSnapshot) -> String {
    let d = &s.daemon;
    let mut out = format!(
        "daemon: tasks2d {} pid {} on {} (up {})\n",
        d.version,
        d.pid,
        d.machine,
        fmt_precise(d.uptime_secs as f64)
    );
    queue_state(&mut out, s);
    slots_section(&mut out, s);
    if s.slots.iter().all(|g| g.busy.is_empty()) && s.running.is_empty() && s.waiting.is_empty() {
        out.push_str("no live jobs\n");
    }
    if !s.running.is_empty() {
        out.push_str("running:\n");
        for r in &s.running {
            let slot = r
                .slot
                .as_ref()
                .map_or(String::new(), |(k, i)| format!(" [{k} {i}]"));
            let progress = if r.total_targets > 0 {
                format!(" {}/{} targets", r.done_targets, r.total_targets)
            } else {
                String::new()
            };
            let eta = r
                .eta_secs
                .map_or(String::new(), |e| format!(", ~{} left", fmt_eta(e)));
            let _ = writeln!(
                out,
                "  job {} {}{slot} {} for {}{progress}{eta}: {}",
                r.id.0,
                r.owner,
                r.current.as_deref().unwrap_or(""),
                fmt_precise(r.elapsed_secs),
                truncate_end(&r.command, TABLE_COMMAND)
            );
            if let Some(f) = &r.figures {
                let _ = writeln!(out, "    cost: {}", f.summary());
            }
        }
    }
    if !s.waiting.is_empty() {
        let _ = writeln!(out, "waiting ({}):", s.waiting.len() + s.waiting_more);
        for w in &s.waiting {
            let est = w.start_estimate_secs.map_or(String::new(), |e| {
                format!(
                    "; starts in ~{}{}",
                    fmt_eta(e),
                    if w.rough { " (rough)" } else { "" }
                )
            });
            let place = match &w.on_peer {
                Some(p) => format!("on {p}"),
                None => format!("#{}", w.position),
            };
            let class = if w.class.is_empty() || w.class == "normal" {
                String::new()
            } else {
                format!(" [{}]", w.class)
            };
            let _ = writeln!(
                out,
                "  {place} job {} {}{class}: {} ({}{est})",
                w.id.0,
                w.owner,
                truncate_end(&w.command, TABLE_COMMAND),
                w.wait_text
            );
            if !w.class_note.is_empty() {
                let _ = writeln!(out, "      {}", w.class_note);
            }
        }
        if s.waiting_more > 0 {
            let _ = writeln!(out, "  ... and {} more", s.waiting_more);
        }
    }
    let p = &s.pressure;
    let _ = writeln!(
        out,
        "pressure: memory {} (some {}, full {}), cpu {}, io {}",
        p.memory,
        pct(p.memory_some),
        pct(p.memory_full),
        pct(p.cpu),
        pct(p.io)
    );
    if let Some(l) = load_line(p) {
        let _ = writeln!(out, "{l}");
    }
    if let Some(b) = &p.compile_blocker {
        let _ = writeln!(out, "a new compile job would wait: {b}");
    }
    if let Some(w) = &p.warning {
        let _ = writeln!(out, "warning: {w}");
    }
    peers_section(&mut out, &s.peers);
    for a in &s.alerts {
        let _ = writeln!(out, "alert [{}] {}", a.level, a.message);
    }
    // an older daemon sends no attribution line
    if !s.attribution.is_empty() {
        let _ = writeln!(out, "{}", s.attribution);
    }
    if let Some(t) = &s.transfer {
        let wait = t
            .waiting
            .as_deref()
            .map_or(String::new(), |w| format!("; waiting: {w}"));
        let _ = writeln!(
            out,
            "transfers: {} active, {} run(s); daemon read {}, wrote {}{wait}",
            t.active,
            t.runs,
            fmt_bytes(t.inproc_read_bytes),
            fmt_bytes(t.inproc_write_bytes)
        );
        if let Some(f) = &t.figures {
            let _ = writeln!(out, "  transfer cgroup: {}", f.summary());
        }
    }
    if !s.trips.is_empty() {
        out.push_str("pressure trips (newest first; `tasks2 stalls` lists more):\n");
        for t in &s.trips {
            let _ = writeln!(
                out,
                "  {} {}",
                tasks_daemon::logs::format_utc(t.at_ms),
                truncate_end(&t.summary, 150)
            );
        }
    }
    out
}

/// A peer's memory pressure: the PSI percentage where it has one, else its native level
/// (macOS), else `?` (R-523).
fn peer_memory_text(pct: Option<u8>, level: Option<&str>) -> String {
    match (pct, level) {
        (Some(p), _) => format!("{p}%"),
        (None, Some(l)) => l.to_string(),
        (None, None) => "?".to_string(),
    }
}

/// `cpu busy 23%, swap 1.0 GiB of 8.0 GiB used (in 0 B/s, out 0 B/s)`; `None` when the machine
/// reported none of it (an older daemon).
fn load_line(p: &tasks_daemon::api::PressureView) -> Option<String> {
    let cpu = p.cpu_busy_pct.map(|c| format!("cpu busy {c:.0}%"));
    let swap = swap_text(p.swap_total, p.swap_used, p.swap_in_per_s, p.swap_out_per_s);
    match (cpu, swap) {
        (None, None) => None,
        (Some(c), None) => Some(c),
        (None, Some(s)) => Some(s),
        (Some(c), Some(s)) => Some(format!("{c}, {s}")),
    }
}

/// Swap size, use and rates in words; `None` when swap was not reported.
fn swap_text(
    total: Option<u64>,
    used: Option<u64>,
    in_s: Option<u64>,
    out_s: Option<u64>,
) -> Option<String> {
    let total = total?;
    if total == 0 {
        return Some("no swap".to_string());
    }
    let mut s = format!(
        "swap {} of {} used",
        fmt_size(used.unwrap_or(0)),
        fmt_size(total)
    );
    if in_s.is_some() || out_s.is_some() {
        let _ = write!(
            s,
            " (in {}/s, out {}/s)",
            fmt_size(in_s.unwrap_or(0)),
            fmt_size(out_s.unwrap_or(0))
        );
    }
    Some(s)
}

/// One block per peer: name, state, path, the two latencies, roles, and what it last reported.
fn peers_section(out: &mut String, peers: &[tasks_daemon::api::PeerView]) {
    if peers.is_empty() {
        return;
    }
    out.push_str("peers:\n");
    for p in peers {
        let ms = |v: Option<u64>| v.map_or("-".to_string(), |r| format!("{r} ms"));
        let seen = p.last_seen_ago_secs.map_or(String::new(), |s| {
            format!(", last seen {} ago", fmt_precise(s as f64))
        });
        let _ = writeln!(
            out,
            "  {} {} ({}; path RTT {}, reply time {}{seen})",
            p.name,
            p.state,
            if p.path.is_empty() {
                "unknown"
            } else {
                &p.path
            },
            ms(p.rtt_ms),
            ms(p.reply_ms)
        );
        if !p.detail.is_empty() {
            let _ = writeln!(out, "    {}", p.detail);
        }
        if !p.roles_out.is_empty() || !p.roles_in.is_empty() {
            let list = |v: &[String]| {
                if v.is_empty() {
                    "none".to_string()
                } else {
                    v.join(", ")
                }
            };
            let _ = writeln!(
                out,
                "    roles: we may {}; it may {}",
                list(&p.roles_out),
                list(&p.roles_in)
            );
        }
        if let Some(m) = &p.machine {
            let slots = m
                .slots
                .iter()
                .map(|x| format!("{} {}/{}", x.kind, x.used, x.total))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                out,
                "    {} cores, {}; slots {slots}; {} waiting, {} running",
                m.cores,
                fmt_size(m.memory_bytes),
                m.waiting,
                m.running
            );
            let pc = |v: Option<u8>| v.map_or("?".to_string(), |x| format!("{x}%"));
            let mut load = format!(
                "    pressure: cpu {}, memory {}, io {}",
                pc(m.cpu_pct),
                peer_memory_text(m.memory_pct, m.memory_level.as_deref()),
                pc(m.io_pct)
            );
            if let Some(c) = m.cpu_busy_pct {
                let _ = write!(load, "; cpu busy {c}%");
            }
            if let Some(sw) =
                swap_text(m.swap_total, m.swap_used, m.swap_in_per_s, m.swap_out_per_s)
            {
                let _ = write!(load, "; {sw}");
            }
            if !m.pressure_summary.is_empty() {
                let _ = write!(load, " ({})", m.pressure_summary);
            }
            let _ = writeln!(out, "{load}");
        }
    }
}

/// `tasks2 admission`: the lock directory, thresholds, slot holders and the source of every
/// policy value. `client_lock_dir` is where this client would look; a disagreement is flagged
/// (R-045, R-231).
pub fn admission(s: &StatusSnapshot, client_lock_dir: Option<&Path>) -> String {
    let d = &s.daemon;
    let mut out = format!("lock dir: {} (the daemon's)\n", d.lock_dir.display());
    if let Some(c) = client_lock_dir.filter(|c| *c != d.lock_dir) {
        let _ = writeln!(
            out,
            "warning: this shell would resolve the lock dir to {}, not the daemon's; only the daemon's counts, set TASKS_LOCK_DIR for the daemon rather than the client",
            c.display()
        );
    }
    let _ = writeln!(out, "state dir: {}", d.state_dir.display());
    let _ = writeln!(
        out,
        "machine policy: {} ({})",
        d.policy_file.display(),
        if d.policy_file_loaded {
            "loaded"
        } else {
            "not found or not valid: defaults apply"
        }
    );
    for p in &d.policy_problems {
        let _ = writeln!(out, "  policy problem: {p}");
    }
    queue_state(&mut out, s);
    slots_section(&mut out, s);
    let p = &s.pressure;
    match &p.compile_blocker {
        Some(b) => {
            let _ = writeln!(out, "compile admission: blocked: {b}");
        }
        None => out.push_str("compile admission: open\n"),
    }
    let _ = writeln!(
        out,
        "disk: cache {} free, state {} free; memory available {}",
        p.cache_free.map_or("?".to_string(), fmt_bytes),
        p.state_free.map_or("?".to_string(), fmt_bytes),
        p.available_memory.map_or("?".to_string(), fmt_bytes)
    );
    if let Some(w) = &p.warning {
        let _ = writeln!(out, "warning: {w}");
    }
    out.push_str("policy values:\n");
    let key_w = s.settings.iter().map(|v| v.key.len()).max().unwrap_or(0);
    for v in &s.settings {
        let _ = writeln!(
            out,
            "  {:<key_w$} = {} ({})",
            v.key,
            v.value,
            source_name(v.source)
        );
    }
    out
}

/// One row of `stats`.
#[derive(Debug, Clone, PartialEq)]
pub struct StatRow {
    /// The recipe.
    pub recipe: String,
    /// The machine the samples come from; `None` when rows come from finished jobs.
    pub machine: Option<String>,
    /// The test target of a per-target row (`test:lifecycle`); `None` for the whole job.
    pub target: Option<String>,
    /// Finished runs.
    pub runs: usize,
    /// Passed runs.
    pub passed: usize,
    /// Median wall seconds of runs that started.
    pub median_secs: f64,
    /// 90th percentile wall seconds.
    pub p90_secs: f64,
}

fn row_label(r: &StatRow) -> String {
    let mut s = truncate_end(&r.recipe, 90);
    if let Some(m) = &r.machine {
        let _ = write!(s, "  @{m}");
    }
    if let Some(t) = &r.target {
        let _ = write!(s, "  > {t}");
    }
    s
}

/// `tasks2 stats` table.
pub fn stats(rows: &[StatRow]) -> String {
    if rows.is_empty() {
        return "(no timing samples)\n".to_string();
    }
    let mut out = format!(
        "{:>5} {:>6} {:>8} {:>8}  RECIPE\n",
        "RUNS", "PASSED", "MEDIAN", "P90"
    );
    for r in rows {
        let _ = writeln!(
            out,
            "{:>5} {:>6} {:>8} {:>8}  {}",
            r.runs,
            r.passed,
            fmt_precise(r.median_secs),
            fmt_precise(r.p90_secs),
            row_label(r)
        );
    }
    out
}

#[cfg(test)]
mod tests;
