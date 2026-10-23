//! The side panel's content for one job: a phase strip with clock times and a short list of
//! facts in words. Pure; the window only paints it (R-584, R-585).

use crate::format::{self, UNKNOWN};
use crate::model::{JobRow, PhaseClass, Section};
use jiff::tz::TimeZone;
use std::path::Path;
use tasks_core::{FetchStatus, Placement, Priority};
use tasks_daemon::api::SyncView;

/// What a strip segment stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegKind {
    /// Submitted, waiting to start.
    Queued,
    /// Sending the checkout to the peer before a forwarded run.
    Sync,
    /// Executing.
    Running,
    /// Still to come, by the estimate.
    Remaining,
}

/// One block of the strip.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// What it is.
    pub kind: SegKind,
    /// Share of the strip's width, 0..=1; the shares add up to 1.
    pub share: f32,
    /// `queued 15m`.
    pub caption: String,
}

/// The phases of a job laid side by side, with the clock times under them.
#[derive(Debug, Clone, PartialEq)]
pub struct Strip {
    /// Blocks, in time order.
    pub segments: Vec<Segment>,
    /// `12:05 submitted`, `12:20 started`, `~12:23 done`: shown left to right.
    pub times: Vec<String>,
}

/// Everything the side panel shows for a job.
#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    /// `Running · 2m00s`, `Passed in 48s`.
    pub state: String,
    /// Its colour class.
    pub tone: PhaseClass,
    /// The phase strip, when the job has anything to put on it.
    pub strip: Option<Strip>,
    /// Label and value; a value may span lines.
    pub facts: Vec<(&'static str, String)>,
}

/// The narrowest share a block gets, so a 3-second phase is still visible.
const MIN_SHARE: f64 = 0.04;

/// Seconds in words for a fact: `40 s`, `15 min`, `1 h 20 min`.
pub fn span_words(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return UNKNOWN.to_string();
    }
    let s = secs.round() as u64;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", (s + 30) / 60),
        _ => match (s % 3600) / 60 {
            0 => format!("{} h", s / 3600),
            m => format!("{} h {m} min", s / 3600),
        },
    }
}

fn secs_between(from: i64, to: i64) -> f64 {
    ((to - from).max(0) as f64) / 1000.0
}

/// Shares for blocks of `secs` seconds each: proportional, but never under [`MIN_SHARE`].
fn shares(secs: &[f64]) -> Vec<f32> {
    let total: f64 = secs.iter().sum();
    if total <= 0.0 {
        let n = secs.len().max(1) as f32;
        return secs.iter().map(|_| 1.0 / n).collect();
    }
    let mut raw: Vec<f64> = secs.iter().map(|s| (s / total).max(MIN_SHARE)).collect();
    let sum: f64 = raw.iter().sum();
    for r in &mut raw {
        *r /= sum;
    }
    raw.into_iter().map(|r| r as f32).collect()
}

fn strip(row: &JobRow, now_ms: i64, tz: &TimeZone) -> Option<Strip> {
    let j = &row.job;
    let at = |ms: i64| format::clock_in(ms, now_ms, tz);
    let end = j.finished_at_ms.unwrap_or(now_ms);
    let mut kinds: Vec<(SegKind, f64, String)> = Vec::new();
    let mut times = vec![format!("{} submitted", at(j.submitted_at_ms))];
    match j.started_at_ms {
        Some(s) => {
            let queued = secs_between(j.submitted_at_ms, s);
            kinds.push((
                SegKind::Queued,
                queued,
                format!("queued {}", format::coarse(queued)),
            ));
            let ran = secs_between(s, end);
            // the sync happens at the start of the forwarded run: carve it out of it
            let synced = j
                .sync
                .map(|y| (y, (y.millis as f64 / 1000.0).min(ran)))
                .filter(|(_, secs)| *secs > 0.0);
            let ran = match synced {
                Some((y, secs)) => {
                    kinds.push((SegKind::Sync, secs, sync_words(&y)));
                    (ran - secs).max(0.0)
                }
                None => ran,
            };
            let cap = format!("running {}", format::coarse(ran));
            kinds.push((SegKind::Running, ran, cap));
            times.push(format!("{} started", at(s)));
            match (j.finished_at_ms, row.eta_secs) {
                (Some(f), _) => times.push(format!("{} done", at(f))),
                (None, Some(eta)) if row.section == Section::Running => {
                    kinds.push((SegKind::Remaining, eta, format!("~{}", format::coarse(eta))));
                    times.push(format!("~{} done", at(now_ms + (eta * 1000.0) as i64)));
                }
                _ => {}
            }
        }
        None => {
            let waited = secs_between(j.submitted_at_ms, end);
            let word = if j.phase.is_terminal() {
                "queued"
            } else {
                "waiting"
            };
            kinds.push((
                SegKind::Queued,
                waited,
                format!("{word} {}", format::coarse(waited)),
            ));
            if let Some(f) = j.finished_at_ms {
                times.push(format!("{} ended", at(f)));
            }
        }
    }
    let secs: Vec<f64> = kinds.iter().map(|k| k.1).collect();
    let segments = kinds
        .into_iter()
        .zip(shares(&secs))
        .map(|((kind, _, caption), share)| Segment {
            kind,
            share,
            caption,
        })
        .collect();
    Some(Strip { segments, times })
}

/// A sync in words: `synced 742 B in 0.4 s`, or that the peer already had the tree (R-649).
pub fn sync_words(y: &SyncView) -> String {
    let took = if y.millis < 10_000 {
        format!("{:.1} s", y.millis as f64 / 1000.0)
    } else {
        span_words(y.millis as f64 / 1000.0)
    };
    if y.already_there {
        format!("peer already had this tree, checked in {took}")
    } else {
        format!("synced {} in {took}", format::bytes(y.bytes))
    }
}

fn why_there(row: &JobRow) -> String {
    let j = &row.job;
    let why = match &j.placement {
        Placement::Here => "this machine",
        Placement::Pin => "pinned here",
        Placement::Peer(m) => return format!("{} · asked for {}", row.machine, m.0),
        Placement::Anywhere => "chosen by the scheduler",
    };
    format!("{} · {why}", row.machine)
}

fn fetch_lines(row: &JobRow) -> Option<String> {
    let j = &row.job;
    if !j.fetch.is_empty() {
        let lines: Vec<String> = j
            .fetch
            .iter()
            .map(|r| match &r.status {
                FetchStatus::Fetched { bytes, .. } => {
                    format!("{} · {}", r.path, format::bytes(*bytes))
                }
                FetchStatus::Missing => format!("{} · not on the peer", r.path),
                FetchStatus::TooLarge { bytes, limit } => format!(
                    "{} · too large ({} over {})",
                    r.path,
                    format::bytes(*bytes),
                    format::bytes(*limit)
                ),
                FetchStatus::Failed { reason } => format!("{} · {reason}", r.path),
            })
            .collect();
        return Some(lines.join("\n"));
    }
    if !j.fetch_paths.is_empty() {
        return Some(format!("when it passes: {}", j.fetch_paths.join(", ")));
    }
    None
}

/// Builds the side panel's content. `tz` and `home` are passed in so tests are exact.
pub fn detail(row: &JobRow, now_ms: i64, tz: &TimeZone, home: Option<&Path>) -> Detail {
    let j = &row.job;
    let at = |ms: i64| format::clock_in(ms, now_ms, tz);
    let (state, tone) = match row.section {
        Section::Running => (format!("Running · {}", row.age), PhaseClass::Running),
        Section::Queued => (format!("Waiting · {}", row.age), PhaseClass::Waiting),
        Section::Recent => (row.state.head.clone(), row.class),
    };
    let mut facts: Vec<(&'static str, String)> = Vec::new();
    facts.push(("Folder", format::tilde_path(&j.cwd, home)));
    facts.push(("Machine", why_there(row)));
    if let Some(p) = &row.remote_phase {
        facts.push(("On the peer", p.clone()));
    }
    if row.section == Section::Queued {
        if let Some(w) = &row.wait {
            facts.push(("Waiting for", w.clone()));
        }
        if let Some(e) = &row.start_estimate {
            facts.push(("Expected start", e.clone()));
        }
        if !row.queue_note.is_empty() {
            facts.push((
                "Queue class",
                format!("{}: {}", row.queue_class, row.queue_note),
            ));
        }
    }
    if j.priority != Priority::Normal {
        let mut p = j.priority.to_string();
        if let Some(r) = &j.priority_reason {
            p.push_str(&format!(" · {r}"));
        }
        facts.push(("Priority", p));
    }
    if let Some(s) = j.started_at_ms {
        let waited = secs_between(j.submitted_at_ms, s);
        let after = if waited < 5.0 {
            "right away".to_string()
        } else {
            format!("after {} waiting", span_words(waited))
        };
        facts.push(("Started", format!("{}, {after}", at(s))));
    } else if j.phase.is_terminal() {
        facts.push(("Started", "never".to_string()));
    }
    if let Some(f) = j.finished_at_ms {
        let ran = j
            .started_at_ms
            .map(|s| format!(", ran {}", span_words(secs_between(s, f))))
            .unwrap_or_default();
        facts.push(("Finished", format!("{}{ran}", at(f))));
    }
    if let Some(t) = j.timeout_secs {
        facts.push(("Time limit", span_words(t as f64)));
    }
    if let Some(e) = j.exit.filter(|e| *e != 0) {
        facts.push(("Exit code", e.to_string()));
    }
    if let Some(by) = &j.cancel_requested_by {
        let mut c = format!("by {}", format::agent_of(by));
        if let Some(r) = &j.cancel_reason {
            c.push_str(&format!(" · {r}"));
        }
        facts.push(("Cancel", c));
    }
    if let Some(s) = &row.summary {
        facts.push(("Outcome", s.clone()));
    }
    if let Some((plan, step)) = &j.plan {
        facts.push(("Plan", format!("#{plan} step {step}")));
    }
    if !j.after.is_empty() {
        let ids: Vec<_> = j.after.iter().map(|i| format!("#{}", i.0)).collect();
        facts.push(("After", ids.join(", ")));
    }
    if let Some(f) = &j.figures {
        facts.push(("Cost", f.summary()));
    }
    if let Some(y) = &j.sync {
        facts.push(("Sync", format!("{} ({} files)", sync_words(y), y.files)));
    }
    if let Some(f) = fetch_lines(row) {
        facts.push(("Fetch results", f));
    }
    Detail {
        state,
        tone,
        strip: strip(row, now_ms, tz),
        facts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::model;

    fn utc() -> TimeZone {
        TimeZone::UTC
    }

    fn row(id: i64) -> JobRow {
        let vm = model::build(&fixtures::busy(), 0);
        vm.running
            .iter()
            .chain(&vm.queued)
            .chain(&vm.recent)
            .find(|r| r.id == id)
            .cloned()
            .expect("fixture row")
    }

    #[test]
    fn r584_the_strip_runs_from_submission_to_the_estimated_end_with_clock_times() {
        let d = detail(&row(101), fixtures::NOW_MS, &utc(), None);
        let s = d.strip.expect("strip");
        let kinds: Vec<_> = s.segments.iter().map(|g| g.kind).collect();
        assert_eq!(
            kinds,
            vec![SegKind::Queued, SegKind::Running, SegKind::Remaining]
        );
        let total: f32 = s.segments.iter().map(|g| g.share).sum();
        assert!((total - 1.0).abs() < 1e-4, "{total}");
        assert!(s.segments.iter().all(|g| g.share >= 0.03));
        // 101 was submitted 1000 s and started 120 s before the fixture clock (12:22:13Z)
        assert_eq!(
            s.times,
            vec!["12:05 submitted", "12:20 started", "~12:23 done"]
        );
        assert_eq!(s.segments[0].caption, "queued 15m");
        assert!(s.segments[1].caption.starts_with("running 2m"));
        assert_eq!(s.segments[2].caption, "~1m");
    }

    #[test]
    fn r584_a_waiting_job_has_only_its_queue_time_and_a_finished_one_ends_with_its_clock() {
        let w = detail(&row(110), fixtures::NOW_MS, &utc(), None)
            .strip
            .expect("strip");
        assert_eq!(w.segments.len(), 1);
        assert_eq!(w.segments[0].kind, SegKind::Queued);
        assert!(w.segments[0].caption.starts_with("waiting"));
        assert_eq!(w.times.len(), 1);

        let f = detail(&row(90), fixtures::NOW_MS, &utc(), None)
            .strip
            .expect("strip");
        assert!(f.segments.iter().all(|g| g.kind != SegKind::Remaining));
        assert!(f.times.last().expect("t").ends_with("done"));
    }

    #[test]
    fn r584_tiny_phases_stay_visible() {
        let s = shares(&[900.0, 0.5, 100.0]);
        assert!(s.iter().all(|x| *x >= 0.035), "{s:?}");
        assert!((s.iter().sum::<f32>() - 1.0).abs() < 1e-4);
        assert_eq!(shares(&[0.0, 0.0]), vec![0.5, 0.5]);
    }

    #[test]
    fn r585_facts_are_words_with_the_home_folder_as_a_tilde_and_local_times() {
        let home = Path::new("/home/demo");
        let d = detail(&row(101), fixtures::NOW_MS, &utc(), Some(home));
        let get = |k: &str| {
            d.facts
                .iter()
                .find(|(l, _)| *l == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("Folder").as_deref(), Some("~/work/tasks"));
        assert_eq!(get("Machine").as_deref(), Some("laptop · this machine"));
        assert_eq!(
            get("Started").as_deref(),
            Some("12:20, after 15 min waiting")
        );
        assert_eq!(get("Time limit").as_deref(), Some("1 h"));
        assert_eq!(d.state, "Running · 2m00s");
        // nothing the old panel printed as developer text
        for (_, v) in &d.facts {
            assert!(!v.contains("Not sent by the daemon"), "{v}");
            assert!(!v.contains('Z'), "no UTC stamp: {v}");
        }
        assert!(!d.facts.iter().any(|(l, _)| *l == "Environment"));
    }

    #[test]
    fn r585_a_forwarded_job_says_where_and_why_and_a_waiting_one_what_it_waits_for() {
        let f = detail(&row(103), fixtures::NOW_MS, &utc(), None);
        let m = &f.facts.iter().find(|(l, _)| *l == "Machine").expect("m").1;
        assert!(
            m.starts_with("nixos") && m.contains("asked for nixos"),
            "{m}"
        );
        assert!(
            f.facts
                .iter()
                .any(|(l, v)| *l == "On the peer" && v == "running")
        );
        let q = detail(&row(110), fixtures::NOW_MS, &utc(), None);
        assert!(
            q.facts
                .iter()
                .any(|(l, v)| *l == "Waiting for" && v.contains("compile slot"))
        );
        assert!(q.facts.iter().any(|(l, _)| *l == "Expected start"));
        assert!(!q.facts.iter().any(|(l, _)| *l == "Started"));
    }

    #[test]
    fn r585_finished_jobs_show_the_cancel_and_the_fetch_results() {
        let c = detail(&row(92), fixtures::NOW_MS, &utc(), None);
        assert!(
            c.facts
                .iter()
                .any(|(l, v)| *l == "Cancel" && v == "by hard-errors · wrong branch")
        );
        assert!(
            c.facts
                .iter()
                .any(|(l, v)| *l == "Finished" && v.contains("ran"))
        );

        let mut d = fixtures::busy();
        let j = d.jobs.iter_mut().find(|j| j.id.0 == 90).expect("job");
        j.fetch_paths = vec!["out/app.apk".into(), "out/b.bin".into()];
        j.fetch = vec![
            tasks_core::FetchResult {
                path: "out/app.apk".into(),
                status: FetchStatus::Fetched {
                    bytes: 3 << 20,
                    sha256: "x".into(),
                    unchanged: false,
                },
            },
            tasks_core::FetchResult {
                path: "out/b.bin".into(),
                status: FetchStatus::Missing,
            },
        ];
        let vm = model::build(&d, 0);
        let r = vm.recent.iter().find(|r| r.id == 90).expect("row");
        let det = detail(r, fixtures::NOW_MS, &utc(), None);
        let f = &det
            .facts
            .iter()
            .find(|(l, _)| *l == "Fetch results")
            .expect("f")
            .1;
        assert_eq!(f, "out/app.apk · 3.0 MiB\nout/b.bin · not on the peer");
    }

    #[test]
    fn r649_a_forwarded_job_shows_its_sync_in_the_strip_and_the_facts() {
        let mut d = fixtures::busy();
        let j = d.jobs.iter_mut().find(|j| j.id.0 == 103).expect("job");
        j.sync = Some(SyncView {
            bytes: 742,
            files: 120,
            millis: 400,
            already_there: false,
        });
        let vm = model::build(&d, 0);
        let r = vm.running.iter().find(|r| r.id == 103).expect("row");
        let det = detail(r, fixtures::NOW_MS, &utc(), None);
        let strip = det.strip.expect("strip");
        let kinds: Vec<_> = strip.segments.iter().map(|g| g.kind).collect();
        assert_eq!(
            kinds[..3],
            [SegKind::Queued, SegKind::Sync, SegKind::Running]
        );
        assert_eq!(strip.segments[1].caption, "synced 742 B in 0.4 s");
        let total: f32 = strip.segments.iter().map(|g| g.share).sum();
        assert!((total - 1.0).abs() < 1e-4, "{total}");
        let fact = det.facts.iter().find(|(l, _)| *l == "Sync").expect("fact");
        assert_eq!(fact.1, "synced 742 B in 0.4 s (120 files)");
        // a job without a note has no sync segment
        let none = detail(&row(101), fixtures::NOW_MS, &utc(), None);
        assert!(
            none.strip
                .expect("strip")
                .segments
                .iter()
                .all(|g| g.kind != SegKind::Sync)
        );
    }

    #[test]
    fn r649_sync_words_cover_a_slow_sync_and_a_tree_the_peer_already_had() {
        let slow = SyncView {
            bytes: 5 << 20,
            files: 9,
            millis: 120_000,
            already_there: false,
        };
        assert_eq!(sync_words(&slow), "synced 5.0 MiB in 2 min");
        let had = SyncView {
            bytes: 0,
            files: 9,
            millis: 200,
            already_there: true,
        };
        assert_eq!(
            sync_words(&had),
            "peer already had this tree, checked in 0.2 s"
        );
    }

    #[test]
    fn r585_spans_read_in_words() {
        assert_eq!(span_words(40.0), "40 s");
        assert_eq!(span_words(900.0), "15 min");
        assert_eq!(span_words(3600.0), "1 h");
        assert_eq!(span_words(4800.0), "1 h 20 min");
        assert_eq!(span_words(f64::NAN), UNKNOWN);
    }
}
