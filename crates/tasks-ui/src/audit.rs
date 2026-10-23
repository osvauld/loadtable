//! The Audit tab's data: the filter the person typed, the request it becomes, and the rows as
//! cells of text. Pure, so the table is tested on fixed rows; the window only draws it.

use crate::link::DaemonLink;
use tasks_daemon::api::{AuditRowView, Request, Response};
use tasks_daemon::logs::format_utc;

/// Rows the tab asks for.
pub const ROWS: usize = 300;

/// Action families the filter offers; an empty string means every action.
pub const FAMILIES: [&str; 8] = ["", "job", "pair", "grant", "conn", "queue", "audit", "data"];

/// What the person typed into the filter row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilter {
    /// An action or a family of actions; empty for all.
    pub action: String,
    /// A paired machine's name or DID; empty for all.
    pub peer: String,
    /// A job id; empty for all.
    pub job: String,
}

impl AuditFilter {
    /// The request for these filters, or why the text is not usable.
    pub fn request(&self) -> Result<Request, String> {
        let job = match self.job.trim() {
            "" => None,
            t => Some(
                t.trim_start_matches('#')
                    .parse::<i64>()
                    .map_err(|_| format!("{t:?} is not a job id"))?,
            ),
        };
        let some = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
        Ok(Request::Audit {
            since_ms: None,
            until_ms: None,
            peer: some(&self.peer),
            job,
            action: some(&self.action),
            limit: Some(ROWS),
        })
    }
}

/// The rows the daemon answered with.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditData {
    /// Newest first.
    pub rows: Vec<AuditRowView>,
    /// True when the row limit cut the list.
    pub truncated: bool,
}

/// Asks the daemon for the rows matching `filter`.
pub fn fetch(link: &dyn DaemonLink, filter: &AuditFilter) -> Result<AuditData, String> {
    let req = filter.request()?;
    match link.call(&req) {
        Ok(Response::Audit { rows, truncated }) => Ok(AuditData { rows, truncated }),
        Ok(other) => Err(format!(
            "unexpected reply to audit: {}",
            crate::feed::kind_of(&other)
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// The table's column titles.
pub const COLUMNS: [&str; 7] = [
    "Time (UTC)",
    "Action",
    "Actor",
    "Owner",
    "Job",
    "Peer",
    "Details",
];

/// One row's cells, in [`COLUMNS`] order.
pub fn cells(r: &AuditRowView) -> [String; 7] {
    let who = |name: &Option<String>, did: &str| name.clone().unwrap_or_else(|| short(did));
    let mut details = match r.details.as_object() {
        Some(o) => o
            .iter()
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => format!("{k}={s}"),
                other => format!("{k}={other}"),
            })
            .collect::<Vec<_>>()
            .join(" "),
        None => String::new(),
    };
    if let Some(t) = r.target.as_deref().filter(|t| !t.is_empty()) {
        details = format!("target={t} {details}").trim_end().to_string();
    }
    [
        format_utc(r.at_ms),
        r.action.clone(),
        who(&r.actor_name, &r.actor),
        r.owner.clone(),
        r.job.map_or_else(String::new, |j| format!("#{j}")),
        r.peer
            .as_deref()
            .map_or_else(String::new, |p| who(&r.peer_name, p)),
        details,
    ]
}

/// `did:key:z6MkhaXg…WRuf`: enough to tell machines apart.
fn short(did: &str) -> String {
    let n = did.chars().count();
    if n <= 24 {
        return did.to_string();
    }
    let head: String = did.chars().take(16).collect();
    let tail: String = did.chars().skip(n - 6).collect();
    format!("{head}…{tail}")
}

/// The row count line under the table.
pub fn summary(d: &AuditData) -> String {
    if d.rows.is_empty() {
        "No audit rows match.".to_string()
    } else if d.truncated {
        format!(
            "Showing the newest {} rows; narrow the filter to see older ones.",
            d.rows.len()
        )
    } else {
        format!("{} row(s), newest first.", d.rows.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{FakeLink, LinkError};
    use serde_json::json;

    fn row(seq: i64, action: &str) -> AuditRowView {
        AuditRowView {
            seq,
            at_ms: 1_792_239_733_000,
            actor: "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK".into(),
            actor_name: None,
            owner: "claude@laptop".into(),
            action: action.into(),
            job: Some(101),
            peer: Some("did:key:zPeer".into()),
            peer_name: Some("nixos".into()),
            target: Some("/srv/co".into()),
            details: json!({"by": "claude@laptop", "reason": "wrong branch", "n": 3}),
            prev_hash: "0".into(),
            hash: "1".into(),
        }
    }

    // R-285
    #[test]
    fn r285_the_filter_becomes_the_audit_request_and_a_bad_job_id_is_said_so() {
        let f = AuditFilter {
            action: "job".into(),
            peer: " nixos ".into(),
            job: "#101".into(),
        };
        assert_eq!(
            f.request().unwrap(),
            Request::Audit {
                since_ms: None,
                until_ms: None,
                peer: Some("nixos".into()),
                job: Some(101),
                action: Some("job".into()),
                limit: Some(ROWS)
            }
        );
        let none = AuditFilter::default().request().unwrap();
        assert!(matches!(
            none,
            Request::Audit {
                peer: None,
                job: None,
                action: None,
                ..
            }
        ));
        let bad = AuditFilter {
            job: "abc".into(),
            ..AuditFilter::default()
        };
        assert!(bad.request().unwrap_err().contains("not a job id"));
    }

    // R-285
    #[test]
    fn r285_rows_become_cells_with_names_short_dids_and_details() {
        let c = cells(&row(1, "job.cancelled"));
        assert_eq!(c[0], "2026-10-17 12:22:13");
        assert_eq!(c[1], "job.cancelled");
        assert!(
            c[2].starts_with("did:key:z6Mkh") && c[2].contains('…'),
            "{}",
            c[2]
        );
        assert_eq!(
            (c[3].as_str(), c[4].as_str(), c[5].as_str()),
            ("claude@laptop", "#101", "nixos")
        );
        assert!(c[6].starts_with("target=/srv/co "), "{}", c[6]);
        assert!(
            c[6].contains("reason=wrong branch") && c[6].contains("n=3"),
            "{}",
            c[6]
        );
        let mut named = row(2, "conn.up");
        named.actor_name = Some("kurukkan".into());
        named.job = None;
        assert_eq!(cells(&named)[2], "kurukkan");
        assert_eq!(cells(&named)[4], "");
    }

    // R-285
    #[test]
    fn r285_fetch_asks_the_daemon_and_reports_failures_as_text() {
        let link = FakeLink::from_fn(|req| match req {
            Request::Audit { job: Some(7), .. } => Ok(Response::Audit {
                rows: vec![row(2, "job.ended"), row(1, "job.started")],
                truncated: true,
            }),
            Request::Audit { .. } => Err(LinkError("socket gone".into())),
            _ => Ok(Response::Ack),
        });
        let f = AuditFilter {
            job: "7".into(),
            ..AuditFilter::default()
        };
        let d = fetch(&link, &f).unwrap();
        assert_eq!(d.rows.len(), 2);
        assert!(summary(&d).contains("Showing the newest 2 rows"));
        assert_eq!(
            fetch(&link, &AuditFilter::default()),
            Err("socket gone".to_string())
        );
        let odd = FakeLink::from_fn(|_| Ok(Response::Ack));
        assert!(
            fetch(&odd, &AuditFilter::default())
                .unwrap_err()
                .contains("unexpected reply")
        );
        let empty = AuditData {
            rows: vec![],
            truncated: false,
        };
        assert_eq!(summary(&empty), "No audit rows match.");
    }
}

#[cfg(test)]
mod family_tests {
    use super::*;

    // R-371
    #[test]
    fn r371_the_filter_offers_every_action_family_including_data() {
        assert!(FAMILIES.contains(&"data"));
        // each family the audit log writes has a button
        for f in ["job", "pair", "grant", "conn", "queue", "audit", "data"] {
            assert!(FAMILIES.contains(&f), "{f}");
        }
        let req = AuditFilter {
            action: "data".into(),
            ..AuditFilter::default()
        }
        .request()
        .unwrap();
        assert!(matches!(req, Request::Audit { action: Some(a), .. } if a == "data"));
    }
}
