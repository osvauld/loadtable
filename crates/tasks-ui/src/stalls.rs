//! The Pressure events tab's data: the `pressure.trip` audit rows as a table. Pure, so the table
//! is tested on fixed rows; the window only draws it.

use crate::link::DaemonLink;
use tasks_core::human_bytes;
use tasks_daemon::api::{AuditRowView, Request, Response, TripDetails};
use tasks_daemon::logs::format_utc;

/// Rows the tab asks for.
pub const ROWS: usize = 200;

/// One trip: the audit row's time and sequence, and its typed details.
#[derive(Debug, Clone, PartialEq)]
pub struct StallRow {
    /// Audit sequence number.
    pub seq: i64,
    /// When, ms since the epoch.
    pub at_ms: i64,
    /// What the row recorded.
    pub trip: TripDetails,
}

/// The rows the daemon answered with.
#[derive(Debug, Clone, PartialEq)]
pub struct StallData {
    /// Newest first.
    pub rows: Vec<StallRow>,
    /// True when the row limit cut the list.
    pub truncated: bool,
    /// Rows whose details could not be read (a newer daemon, say).
    pub unreadable: usize,
}

/// The audit request for trips.
pub fn request() -> Request {
    Request::Audit {
        since_ms: None,
        until_ms: None,
        peer: None,
        job: None,
        action: Some(tasks_daemon::api::PRESSURE_TRIP_ACTION.to_string()),
        limit: Some(ROWS),
    }
}

/// Turns audit rows into trips.
pub fn rows_from(rows: Vec<AuditRowView>, truncated: bool) -> StallData {
    let mut out = Vec::new();
    let mut unreadable = 0;
    for r in rows {
        match serde_json::from_value::<TripDetails>(r.details) {
            Ok(trip) => out.push(StallRow {
                seq: r.seq,
                at_ms: r.at_ms,
                trip,
            }),
            Err(_) => unreadable += 1,
        }
    }
    StallData {
        rows: out,
        truncated,
        unreadable,
    }
}

/// Asks the daemon for the trips.
pub fn fetch(link: &dyn DaemonLink) -> Result<StallData, String> {
    match link.call(&request()) {
        Ok(Response::Audit { rows, truncated }) => Ok(rows_from(rows, truncated)),
        Ok(other) => Err(format!(
            "unexpected reply to audit: {}",
            crate::feed::kind_of(&other)
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// The table's column titles.
pub const COLUMNS: [&str; 6] = [
    "Time (UTC)",
    "Resource",
    "Source",
    "Reason",
    "Busiest outside jobs",
    "Jobs",
];

/// One row's cells, in [`COLUMNS`] order.
pub fn cells(r: &StallRow) -> [String; 6] {
    let t = &r.trip;
    let busiest = match (t.top_io.first(), t.top_rss.first()) {
        (Some(p), _) if p.io_bytes > 0 => {
            format!(
                "{} (pid {}) moved {}",
                p.name,
                p.pid,
                human_bytes(p.io_bytes)
            )
        }
        (_, Some(p)) => format!(
            "{} (pid {}) holds {}",
            p.name,
            p.pid,
            human_bytes(p.rss_bytes)
        ),
        _ => "-".to_string(),
    };
    let jobs = if t.jobs.is_empty() {
        "none running".to_string()
    } else {
        t.jobs
            .iter()
            .map(|j| format!("#{}: {}", j.job, j.figures.summary()))
            .collect::<Vec<_>>()
            .join("; ")
    };
    let mut reason = t.reason.clone();
    if t.suppressed > 0 {
        reason = format!("{reason} (+{} suppressed)", t.suppressed);
    }
    [
        format_utc(r.at_ms),
        t.resource.clone(),
        t.source.clone(),
        reason,
        busiest,
        jobs,
    ]
}

/// The count line under the table.
pub fn summary(d: &StallData) -> String {
    let mut s = if d.rows.is_empty() {
        "No pressure trips recorded.".to_string()
    } else if d.truncated {
        format!("Showing the newest {} trips.", d.rows.len())
    } else {
        format!("{} trip(s), newest first.", d.rows.len())
    };
    if d.unreadable > 0 {
        s.push_str(&format!(" {} row(s) could not be read.", d.unreadable));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{FakeLink, LinkError};
    use tasks_core::{Attribution, JobFigures};
    use tasks_daemon::api::{TripJob, TripProc};

    fn trip() -> TripDetails {
        let mut f = JobFigures::empty(Attribution::Cgroup);
        f.io_write_bytes = Some(2 << 30);
        TripDetails {
            resource: "io".into(),
            source: "psi".into(),
            reason: "I/O pressure 22% >= 10%".into(),
            suppressed: 3,
            jobs: vec![TripJob { job: 7, figures: f }],
            top_io: vec![TripProc {
                pid: 99,
                name: "firefox".into(),
                io_bytes: 25 << 30,
                rss_bytes: 1 << 30,
            }],
            ..TripDetails::default()
        }
    }

    fn audit_row(seq: i64, details: serde_json::Value) -> AuditRowView {
        AuditRowView {
            seq,
            at_ms: 1_792_239_733_000,
            actor: "did:key:zA".into(),
            actor_name: None,
            owner: String::new(),
            action: "pressure.trip".into(),
            job: None,
            peer: None,
            peer_name: None,
            target: Some("io".into()),
            details,
            prev_hash: "0".into(),
            hash: "1".into(),
        }
    }

    // R-336
    #[test]
    fn r336_the_tab_asks_for_trips_and_shows_who_was_busiest() {
        assert!(matches!(
            request(),
            Request::Audit { action: Some(a), .. } if a == "pressure.trip"
        ));
        let link = FakeLink::from_fn(|_| {
            Ok(Response::Audit {
                rows: vec![
                    audit_row(2, serde_json::to_value(trip()).unwrap()),
                    audit_row(1, serde_json::json!("not a trip")),
                ],
                truncated: false,
            })
        });
        let d = fetch(&link).unwrap();
        assert_eq!(d.rows.len(), 1);
        assert_eq!(d.unreadable, 1);
        let c = cells(&d.rows[0]);
        assert_eq!(c[1], "io");
        assert!(c[3].contains("(+3 suppressed)"), "{}", c[3]);
        assert_eq!(c[4], "firefox (pid 99) moved 25 GiB");
        assert!(c[5].starts_with("#7: wrote 2 GiB"), "{}", c[5]);
        assert!(summary(&d).contains("1 row(s) could not be read"));
    }

    // R-336
    #[test]
    fn r336_failures_and_empty_lists_are_said_in_words() {
        let down = FakeLink::from_fn(|_| Err(LinkError("socket gone".into())));
        assert_eq!(fetch(&down), Err("socket gone".to_string()));
        let odd = FakeLink::from_fn(|_| Ok(Response::Ack));
        assert!(fetch(&odd).unwrap_err().contains("unexpected reply"));
        let none = StallData {
            rows: vec![],
            truncated: false,
            unreadable: 0,
        };
        assert_eq!(summary(&none), "No pressure trips recorded.");
    }
}
