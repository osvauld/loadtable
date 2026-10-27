//! `loadtable audit`: the audit log, and `audit verify` / `audit checkpoint`.
//!
//! Rows are printed oldest first (the latest N, like `tail`), as JSON lines with `--json`.
//! Times are UTC. A broken chain is a normal answer, not a failure of the command: `verify`
//! prints where and why and exits 1.

use crate::args::{AuditArgs, AuditCommand, StallsArgs};
use crate::commands::Ctx;
use crate::error::{CliError, EXIT_FAILURE, EXIT_OK};
use crate::trust::short_did;
use tasks_daemon::api::{AuditRowView, AuditVerifyView, Request, Response, TripDetails};
use tasks_daemon::logs::{format_utc, parse_time, parse_utc};

/// Rows shown when `--limit` is not given.
const DEFAULT_LIMIT: usize = 100;

/// A time given as a duration back from now (`2h`, `3d`), unix seconds, or a UTC date.
pub fn parse_when(text: &str, now_ms: i64) -> Result<i64, CliError> {
    if text.contains('-')
        && let Some(ms) = parse_utc(text)
    {
        return Ok(ms);
    }
    parse_time(text, now_ms).map_err(|_| {
        CliError::Usage(format!(
            "{text:?} is not a time; write a duration such as 2h or 3d, unix seconds, or a UTC \
             date such as 2026-10-22 or 2026-10-22T14:30"
        ))
    })
}

/// `loadtable audit …`.
pub fn audit(ctx: &mut Ctx<'_>, a: &AuditArgs) -> Result<i32, CliError> {
    match &a.sub {
        Some(AuditCommand::Verify { json }) => verify(ctx, *json),
        Some(AuditCommand::Checkpoint) => checkpoint(ctx),
        None => rows(ctx, a),
    }
}

fn rows(ctx: &mut Ctx<'_>, a: &AuditArgs) -> Result<i32, CliError> {
    let now = (ctx.now_ms)();
    let since_ms = a.since.as_deref().map(|t| parse_when(t, now)).transpose()?;
    let until_ms = a.until.as_deref().map(|t| parse_when(t, now)).transpose()?;
    let limit = a.limit.unwrap_or(DEFAULT_LIMIT);
    let Response::Audit {
        mut rows,
        truncated,
    } = ctx.client.call(&Request::Audit {
        since_ms,
        until_ms,
        peer: a.peer.clone(),
        job: a.job,
        action: a.action_filter.clone(),
        limit: Some(limit),
    })?
    else {
        return Err(CliError::Failed("unexpected reply to audit".into()));
    };
    // the daemon sends the newest first; read it oldest first
    rows.reverse();
    if a.json {
        for r in &rows {
            let line = serde_json::to_string(r)
                .map_err(|e| CliError::Failed(format!("cannot render JSON: {e}")))?;
            writeln!(ctx.out, "{line}")?;
        }
        return Ok(EXIT_OK);
    }
    if rows.is_empty() {
        writeln!(ctx.out, "no audit rows match")?;
        return Ok(EXIT_OK);
    }
    write!(ctx.out, "{}", table(&rows))?;
    if truncated {
        writeln!(
            ctx.err,
            "showing the latest {} rows; narrow with --since, --job, --peer or --action, or raise --limit",
            rows.len()
        )?;
    }
    Ok(EXIT_OK)
}

/// Who a DID is, for a table cell: the paired name, else a short DID.
fn who(name: Option<&String>, did: &str) -> String {
    name.cloned().unwrap_or_else(|| short_did(did))
}

/// The details as `key=value` pairs, bounded.
fn detail_text(v: &serde_json::Value) -> String {
    let Some(o) = v.as_object() else {
        return String::new();
    };
    let text = o
        .iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => format!("{k}={s}"),
            other => format!("{k}={other}"),
        })
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() > 100 {
        let head: String = text.chars().take(97).collect();
        format!("{head}...")
    } else {
        text
    }
}

/// The rows as an aligned table.
pub fn table(rows: &[AuditRowView]) -> String {
    let head = [
        "SEQ",
        "TIME (UTC)",
        "ACTION",
        "ACTOR",
        "OWNER",
        "JOB",
        "PEER",
        "DETAILS",
    ];
    let cells: Vec<[String; 8]> = rows
        .iter()
        .map(|r| {
            let mut details = detail_text(&r.details);
            if let Some(t) = &r.target
                && !t.is_empty()
            {
                details = format!("target={t} {details}").trim_end().to_string();
            }
            [
                r.seq.to_string(),
                format_utc(r.at_ms),
                r.action.clone(),
                who(r.actor_name.as_ref(), &r.actor),
                r.owner.clone(),
                r.job.map_or_else(String::new, |j| j.to_string()),
                r.peer
                    .as_deref()
                    .map_or_else(String::new, |p| who(r.peer_name.as_ref(), p)),
                details,
            ]
        })
        .collect();
    let mut widths = head.map(str::len);
    for row in &cells {
        for (w, c) in widths.iter_mut().zip(row.iter()).take(7) {
            *w = (*w).max(c.chars().count());
        }
    }
    let line = |cols: &[String]| {
        cols.iter()
            .zip(widths.iter())
            .map(|(c, w)| format!("{c:<w$}", w = *w))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let mut out = line(&head.map(str::to_string));
    out.push('\n');
    for row in &cells {
        out.push_str(&line(row));
        out.push('\n');
    }
    out
}

/// Trips shown when `--limit` is not given.
const DEFAULT_STALLS: usize = 20;

/// `loadtable stalls`: the `pressure.trip` audit rows, oldest first, each with the busiest
/// outside process and the running jobs' costs. Reads the same rows as
/// `loadtable audit --action pressure.trip`.
pub fn stalls(ctx: &mut Ctx<'_>, a: &StallsArgs) -> Result<i32, CliError> {
    let now = (ctx.now_ms)();
    let since_ms = a.since.as_deref().map(|t| parse_when(t, now)).transpose()?;
    let until_ms = a.until.as_deref().map(|t| parse_when(t, now)).transpose()?;
    let Response::Audit {
        mut rows,
        truncated,
    } = ctx.client.call(&Request::Audit {
        since_ms,
        until_ms,
        peer: None,
        job: None,
        action: Some(tasks_daemon::api::PRESSURE_TRIP_ACTION.to_string()),
        limit: Some(a.limit.unwrap_or(DEFAULT_STALLS)),
    })?
    else {
        return Err(CliError::Failed("unexpected reply to audit".into()));
    };
    rows.reverse();
    if a.json {
        for r in &rows {
            let line = serde_json::to_string(r)
                .map_err(|e| CliError::Failed(format!("cannot render JSON: {e}")))?;
            writeln!(ctx.out, "{line}")?;
        }
        return Ok(EXIT_OK);
    }
    if rows.is_empty() {
        writeln!(ctx.out, "no pressure trips recorded")?;
        return Ok(EXIT_OK);
    }
    write!(ctx.out, "{}", stalls_text(&rows))?;
    if truncated {
        writeln!(
            ctx.err,
            "showing the latest {} trips; narrow with --since or raise --limit",
            rows.len()
        )?;
    }
    Ok(EXIT_OK)
}

/// The trips as text, oldest first: one header line each, then the evidence.
pub fn stalls_text(rows: &[AuditRowView]) -> String {
    let mut out = String::new();
    for r in rows {
        let Ok(d) = serde_json::from_value::<TripDetails>(r.details.clone()) else {
            out.push_str(&format!(
                "{} (row {}: details not readable by this client)\n",
                format_utc(r.at_ms),
                r.seq
            ));
            continue;
        };
        let more = if d.suppressed > 0 {
            format!(" (+{} more suppressed since the last)", d.suppressed)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "{} {} {}: {}{more}\n",
            format_utc(r.at_ms),
            d.resource,
            d.source,
            d.reason
        ));
        let m = &d.machine;
        let pct = |v: Option<f32>| v.map_or("-".to_string(), |x| format!("{x:.0}%"));
        out.push_str(&format!(
            "  machine: io {} cpu {} memory {} (some {}, full {}), {} available\n",
            pct(m.io_pct),
            pct(m.cpu_pct),
            m.mem_level,
            pct(m.mem_some_pct),
            pct(m.mem_full_pct),
            m.available_bytes
                .map_or("-".to_string(), tasks_core::human_bytes)
        ));
        if d.jobs.is_empty() {
            out.push_str("  jobs: none running\n");
        }
        for j in &d.jobs {
            out.push_str(&format!("  job {}: {}\n", j.job, j.figures.summary()));
        }
        if let Some(t) = &d.transfer {
            out.push_str(&format!(
                "  transfer: {} active; daemon read {} wrote {}{}\n",
                t.active,
                tasks_core::human_bytes(t.inproc_read_bytes),
                tasks_core::human_bytes(t.inproc_write_bytes),
                t.figures
                    .as_ref()
                    .map_or(String::new(), |f| format!("; cgroup {}", f.summary()))
            ));
        }
        let window = if d.window_ms == 0 {
            "first sample, no window".to_string()
        } else {
            format!("last {} s", d.window_ms / 1000)
        };
        if !d.top_io.is_empty() {
            out.push_str(&format!("  outside jobs, by disk bytes ({window}):\n"));
            for p in &d.top_io {
                out.push_str(&format!(
                    "    {:<20} pid {:<8} {}\n",
                    p.name,
                    p.pid,
                    tasks_core::human_bytes(p.io_bytes)
                ));
            }
        }
        if !d.top_rss.is_empty() {
            out.push_str("  outside jobs, by memory:\n");
            for p in d.top_rss.iter().take(5) {
                out.push_str(&format!(
                    "    {:<20} pid {:<8} {}\n",
                    p.name,
                    p.pid,
                    tasks_core::human_bytes(p.rss_bytes)
                ));
            }
        }
        if d.unreadable > 0 {
            out.push_str(&format!(
                "  ({} process(es) of other users could not be read)\n",
                d.unreadable
            ));
        }
    }
    out
}

fn verify(ctx: &mut Ctx<'_>, json: bool) -> Result<i32, CliError> {
    let Response::AuditVerify(v) = ctx.client.call(&Request::AuditVerify)? else {
        return Err(CliError::Failed("unexpected reply to audit verify".into()));
    };
    if json {
        let line = serde_json::to_string(&v)
            .map_err(|e| CliError::Failed(format!("cannot render JSON: {e}")))?;
        writeln!(ctx.out, "{line}")?;
    } else {
        write!(ctx.out, "{}", verify_text(&v))?;
    }
    Ok(if v.ok { EXIT_OK } else { EXIT_FAILURE })
}

/// The human text of a verify result.
pub fn verify_text(v: &AuditVerifyView) -> String {
    let span = match (v.first_seq, v.last_seq) {
        (Some(a), Some(b)) => format!("rows {a}..{b}"),
        _ => "no rows".to_string(),
    };
    let mut out = String::new();
    match &v.problem {
        None => out.push_str(&format!("audit log OK: {} row(s), {span}\n", v.rows)),
        Some(p) => out.push_str(&format!(
            "audit log BROKEN at row {}: {} ({})\n",
            p.seq, p.message, p.kind
        )),
    }
    out.push_str(&match v.signed_through {
        Some(s) => format!(
            "{} signed checkpoint(s); rows up to {s} are covered by a signature\n",
            v.checkpoints
        ),
        None => "no signed checkpoint yet: the chain alone protects these rows\n".to_string(),
    });
    if let Some(p) = v.pruned_before {
        out.push_str(&format!("rows before {p} were removed by retention\n"));
    }
    out
}

fn checkpoint(ctx: &mut Ctx<'_>) -> Result<i32, CliError> {
    let Response::AuditCheckpoint { seq } = ctx.client.call(&Request::AuditCheckpoint)? else {
        return Err(CliError::Failed(
            "unexpected reply to audit checkpoint".into(),
        ));
    };
    match seq {
        Some(s) => writeln!(ctx.out, "signed a checkpoint at row {s}")?,
        None => writeln!(ctx.out, "the audit log is empty; nothing to sign")?,
    }
    Ok(EXIT_OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeClient;
    use crate::testutil::{Harness, NOW};
    use std::sync::Arc;
    use tasks_daemon::api::AuditProblemView;

    fn row(seq: i64, action: &str, job: Option<i64>) -> AuditRowView {
        AuditRowView {
            seq,
            at_ms: 1_792_239_733_000 + seq * 1000,
            actor: "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK".into(),
            actor_name: Some("kurukkan".into()),
            owner: "agent-1".into(),
            action: action.into(),
            job,
            peer: Some("did:key:z6MknncVmedphgDSbQoYLyNaFRFdB2WtyjxcFdKV6zTfcYvs".into()),
            peer_name: Some("nixos".into()),
            target: None,
            details: serde_json::json!({"by": "agent-1", "reason": "wrong branch"}),
            prev_hash: "00".into(),
            hash: "11".into(),
        }
    }

    fn verify_view(problem: Option<AuditProblemView>) -> AuditVerifyView {
        AuditVerifyView {
            ok: problem.is_none(),
            rows: 12,
            first_seq: Some(3),
            last_seq: Some(14),
            checkpoints: 2,
            signed_through: Some(10),
            pruned_before: Some(3),
            problem,
        }
    }

    // R-283
    #[test]
    fn r283_audit_sends_the_filters_and_prints_oldest_first() {
        let client = Arc::new(FakeClient::from_fn(|_| {
            Ok(Response::Audit {
                rows: vec![
                    row(2, "job.cancelled", Some(9)),
                    row(1, "job.started", Some(9)),
                ],
                truncated: false,
            })
        }));
        let h = Harness::default();
        let r = h.run(
            &client,
            &[
                "audit", "--since", "2h", "--peer", "nixos", "--job", "9", "--action", "job",
                "--limit", "5",
            ],
        );
        assert_eq!(r.code, 0, "{}", r.err);
        match &client.requests()[0] {
            Request::Audit {
                since_ms,
                until_ms,
                peer,
                job,
                action,
                limit,
            } => {
                assert_eq!(*since_ms, Some(NOW - 2 * 3_600_000));
                assert_eq!(*until_ms, None);
                assert_eq!(peer.as_deref(), Some("nixos"));
                assert_eq!(
                    (*job, action.as_deref(), *limit),
                    (Some(9), Some("job"), Some(5))
                );
            }
            other => panic!("{other:?}"),
        }
        let lines: Vec<&str> = r.out.lines().collect();
        assert_eq!(lines.len(), 3, "{}", r.out);
        assert!(lines[0].starts_with("SEQ"), "{}", lines[0]);
        // the daemon answered newest first; the table reads oldest first
        assert!(
            lines[1].contains("job.started") && lines[2].contains("job.cancelled"),
            "{}",
            r.out
        );
        assert!(lines[2].contains("2026-10-17 12:22:15"), "{}", lines[2]);
        assert!(
            lines[2].contains("kurukkan")
                && lines[2].contains("nixos")
                && lines[2].contains("agent-1")
        );
        assert!(
            lines[2].contains("by=agent-1 reason=wrong branch"),
            "{}",
            lines[2]
        );
    }

    fn trip_row(seq: i64) -> AuditRowView {
        use tasks_daemon::api::{TripJob, TripProc};
        let mut f = tasks_core::JobFigures::empty(tasks_core::Attribution::Cgroup);
        f.io_write_bytes = Some(2 << 30);
        let d = TripDetails {
            resource: "io".into(),
            source: "gate".into(),
            reason: "I/O stalls: pressure 52% > 40% for compile".into(),
            suppressed: 4,
            jobs: vec![TripJob {
                job: 12,
                figures: f,
            }],
            window_ms: 5000,
            top_io: vec![TripProc {
                pid: 4242,
                name: "firefox".into(),
                io_bytes: 25 << 30,
                rss_bytes: 1 << 30,
            }],
            ..TripDetails::default()
        };
        let mut r = row(seq, "pressure.trip", None);
        r.details = serde_json::to_value(d).unwrap();
        r
    }

    // R-336
    #[test]
    fn r336_stalls_reads_the_trip_rows_and_names_the_busiest_process() {
        let client = Arc::new(FakeClient::from_fn(|_| {
            Ok(Response::Audit {
                rows: vec![trip_row(2), trip_row(1)],
                truncated: false,
            })
        }));
        let r = Harness::default().run(&client, &["stalls", "--since", "1d"]);
        assert_eq!(r.code, 0, "{}", r.err);
        match &client.requests()[0] {
            Request::Audit {
                action,
                since_ms,
                limit,
                ..
            } => {
                assert_eq!(action.as_deref(), Some("pressure.trip"));
                assert_eq!(*since_ms, Some(NOW - 86_400_000));
                assert_eq!(*limit, Some(20));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            r.out.contains("io gate: I/O stalls: pressure 52% > 40%"),
            "{}",
            r.out
        );
        assert!(
            r.out.contains("(+4 more suppressed since the last)"),
            "{}",
            r.out
        );
        assert!(
            r.out.contains("job 12: wrote 2 GiB (exact cgroup)"),
            "{}",
            r.out
        );
        assert!(
            r.out.contains("firefox") && r.out.contains("pid 4242") && r.out.contains("25 GiB"),
            "{}",
            r.out
        );
        assert!(r.out.contains("last 5 s"), "{}", r.out);
    }

    // R-336
    #[test]
    fn r336_stalls_json_is_one_audit_row_per_line_and_an_empty_log_is_said() {
        let client = Arc::new(FakeClient::from_fn(|_| {
            Ok(Response::Audit {
                rows: vec![trip_row(2), trip_row(1)],
                truncated: false,
            })
        }));
        let r = Harness::default().run(&client, &["stalls", "--json", "--limit", "3"]);
        let parsed: Vec<AuditRowView> = r
            .out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed.iter().map(|p| p.seq).collect::<Vec<_>>(), [1, 2]);
        let empty = Arc::new(FakeClient::from_fn(|_| {
            Ok(Response::Audit {
                rows: vec![],
                truncated: false,
            })
        }));
        assert_eq!(
            Harness::default().run(&empty, &["stalls"]).out,
            "no pressure trips recorded\n"
        );
    }

    // R-283
    #[test]
    fn r283_audit_json_is_one_row_per_line_and_defaults_to_a_hundred_rows() {
        let client = Arc::new(FakeClient::from_fn(|_| {
            Ok(Response::Audit {
                rows: vec![row(2, "job.ended", Some(1)), row(1, "job.started", Some(1))],
                truncated: true,
            })
        }));
        let r = Harness::default().run(&client, &["audit", "--json"]);
        assert_eq!(r.code, 0);
        let parsed: Vec<AuditRowView> = r
            .out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed.iter().map(|p| p.seq).collect::<Vec<_>>(), [1, 2]);
        assert!(matches!(
            &client.requests()[0],
            Request::Audit {
                limit: Some(100),
                since_ms: None,
                ..
            }
        ));
        // the human form says it was cut
        let r = Harness::default().run(&client, &["audit"]);
        assert!(r.err.contains("showing the latest 2 rows"), "{}", r.err);
    }

    // R-283
    #[test]
    fn r283_an_empty_log_and_a_bad_time_are_said_plainly() {
        let client = Arc::new(FakeClient::from_fn(|_| {
            Ok(Response::Audit {
                rows: vec![],
                truncated: false,
            })
        }));
        let h = Harness::default();
        assert_eq!(h.run(&client, &["audit"]).out, "no audit rows match\n");
        let r = h.run(&client, &["audit", "--since", "soonish"]);
        assert_eq!(r.code, 2);
        assert!(r.err.contains("not a time"), "{}", r.err);
        assert_eq!(client.requests().len(), 1, "a bad time asks nobody");
    }

    // R-283
    #[test]
    fn r283_times_are_durations_unix_seconds_or_utc_dates() {
        assert_eq!(parse_when("90m", NOW).unwrap(), NOW - 5_400_000);
        assert_eq!(parse_when("1792239733", NOW).unwrap(), 1_792_239_733_000);
        assert_eq!(parse_when("2026-10-17", NOW).unwrap(), 1_792_195_200_000);
        assert_eq!(
            parse_when("2026-10-17T12:22:13", NOW).unwrap(),
            1_792_239_733_000
        );
        assert!(parse_when("2026-13-17", NOW).is_err());
        assert!(parse_when("", NOW).is_err());
    }

    // R-283
    #[test]
    fn r283_verify_prints_ok_or_the_first_break_and_exits_accordingly() {
        let good = Arc::new(FakeClient::from_fn(|_| {
            Ok(Response::AuditVerify(verify_view(None)))
        }));
        let r = Harness::default().run(&good, &["audit", "verify"]);
        assert_eq!(r.code, 0);
        assert!(
            r.out.contains("audit log OK: 12 row(s), rows 3..14"),
            "{}",
            r.out
        );
        assert!(
            r.out.contains("2 signed checkpoint(s)") && r.out.contains("up to 10"),
            "{}",
            r.out
        );
        assert!(
            r.out.contains("rows before 3 were removed by retention"),
            "{}",
            r.out
        );
        assert!(matches!(good.requests()[0], Request::AuditVerify));

        let bad = Arc::new(FakeClient::from_fn(|_| {
            Ok(Response::AuditVerify(verify_view(Some(AuditProblemView {
                seq: 7,
                kind: "edited".into(),
                message: "row 7 no longer matches its hash: it was edited".into(),
            }))))
        }));
        let r = Harness::default().run(&bad, &["audit", "verify"]);
        assert_eq!(r.code, 1);
        assert!(
            r.out.contains("audit log BROKEN at row 7") && r.out.contains("(edited)"),
            "{}",
            r.out
        );
        let r = Harness::default().run(&bad, &["audit", "verify", "--json"]);
        assert_eq!(r.code, 1);
        let v: AuditVerifyView = serde_json::from_str(r.out.trim()).unwrap();
        assert_eq!(v.problem.unwrap().seq, 7);
    }

    // R-283
    #[test]
    fn r283_verify_with_no_signature_says_the_chain_alone_protects_the_rows() {
        let mut v = verify_view(None);
        v.signed_through = None;
        v.checkpoints = 0;
        v.pruned_before = None;
        let text = verify_text(&v);
        assert!(text.contains("no signed checkpoint yet"), "{text}");
        assert!(!text.contains("retention"));
    }

    // R-283
    #[test]
    fn r283_checkpoint_on_demand_reports_its_row() {
        let client = Arc::new(FakeClient::from_fn(|_| {
            Ok(Response::AuditCheckpoint { seq: Some(41) })
        }));
        let r = Harness::default().run(&client, &["audit", "checkpoint"]);
        assert_eq!(
            (r.code, r.out.as_str()),
            (0, "signed a checkpoint at row 41\n")
        );
        assert!(matches!(client.requests()[0], Request::AuditCheckpoint));
        let empty = Arc::new(FakeClient::from_fn(|_| {
            Ok(Response::AuditCheckpoint { seq: None })
        }));
        assert!(
            Harness::default()
                .run(&empty, &["audit", "checkpoint"])
                .out
                .contains("empty")
        );
    }

    // R-283
    #[test]
    fn r283_a_daemon_that_keeps_no_log_is_reported_not_hidden() {
        let client = Arc::new(FakeClient::from_fn(|_| {
            Err(crate::testutil::api_err(
                tasks_daemon::api::ErrorCode::Refused,
                "this daemon keeps no audit log",
            ))
        }));
        let r = Harness::default().run(&client, &["audit"]);
        assert_eq!(r.code, 2);
        assert!(r.err.contains("keeps no audit log"), "{}", r.err);
    }
}
