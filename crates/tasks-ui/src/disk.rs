//! The Disk tab's data: the daemon's `gc` report as machine cards and one row per checkout,
//! and the request that frees the rows the user ticked (R-840). Pure, so every text is tested
//! on a fixed report; the window only draws it and keeps the ticks.

use crate::link::DaemonLink;
use std::collections::BTreeSet;
use tasks_core::human_bytes;
use tasks_daemon::api::{GcRow, GcState, GcView, Request, Response};

/// The report alone: nothing deleted.
pub fn report_request() -> Request {
    Request::Gc {
        delete: Vec::new(),
        checkouts: Vec::new(),
        idle_days: None,
    }
}

/// Frees the checkouts named, as the report names them. The daemon still keeps any a job
/// uses.
pub fn free_request(checkouts: &BTreeSet<String>) -> Request {
    Request::Gc {
        delete: Vec::new(),
        checkouts: checkouts.iter().cloned().collect(),
        idle_days: None,
    }
}

/// Sends `req` and returns the report.
pub fn fetch(link: &dyn DaemonLink, req: &Request) -> Result<GcView, String> {
    match link.call(req) {
        Ok(Response::Gc(v)) => Ok(*v),
        Ok(other) => Err(format!(
            "unexpected reply to gc: {}",
            crate::feed::kind_of(&other)
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// One machine's card at the top of the tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskCard {
    /// Machine name.
    pub name: String,
    /// `build cache 134.5 GiB · 98.5 GiB can be freed`, or why it could not report.
    pub line: String,
    /// True when the machine could not report.
    pub failed: bool,
}

/// The cards, this machine first as the daemon sends them.
pub fn cards(v: &GcView) -> Vec<DiskCard> {
    v.machines
        .iter()
        .map(|m| match &m.error {
            Some(e) => DiskCard {
                name: m.name.clone(),
                line: format!("could not report: {e}"),
                failed: true,
            },
            None => DiskCard {
                name: m.name.clone(),
                line: format!(
                    "build cache {} · {} can be freed",
                    human_bytes(m.held_bytes),
                    human_bytes(m.reclaimable_bytes)
                ),
                failed: false,
            },
        })
        .collect()
}

/// The machines that reported, one size column each.
pub fn machine_columns(v: &GcView) -> Vec<String> {
    v.machines
        .iter()
        .filter(|m| m.error.is_none())
        .map(|m| m.name.clone())
        .collect()
}

/// The table's column titles: the tick, the fixed columns, one per machine, then what was
/// freed when something was.
pub fn columns(v: &GcView) -> Vec<String> {
    let mut c: Vec<String> = ["", "State", "Checkout", "Branch", "Last job"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    c.extend(machine_columns(v));
    if v.deleted {
        c.push("Freed".into());
    }
    c
}

/// How long ago, in the largest whole unit.
pub fn ago(ms: Option<i64>, now_ms: i64) -> String {
    let Some(ms) = ms else {
        return "-".into();
    };
    let s = (now_ms - ms).max(0) / 1000;
    match s {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// One row's cells after the tick, in [`columns`] order.
pub fn cells(v: &GcView, r: &GcRow, now_ms: i64) -> Vec<String> {
    let branch = match (&r.branch, &r.merged_into) {
        (Some(b), Some(m)) => format!("{b} (in {m})"),
        (Some(b), None) => b.clone(),
        (None, _) => "-".into(),
    };
    let mut c = vec![
        r.state.name().replace('_', " "),
        r.checkout.clone(),
        branch,
        ago(r.last_used_ms, now_ms),
    ];
    for m in machine_columns(v) {
        c.push(match r.held.iter().find(|h| h.machine == m) {
            Some(h) => human_bytes(h.bytes),
            None => "-".into(),
        });
    }
    if v.deleted {
        c.push(match r.freed_bytes {
            0 => "-".into(),
            b => human_bytes(b),
        });
    }
    c
}

/// What a state means, for the State cell's tooltip.
pub fn state_help(s: GcState) -> &'static str {
    match s {
        GcState::InUse => "A job of it is queued or running: it cannot be freed now.",
        GcState::Active => "Used recently. Freeing it means its next build starts cold.",
        GcState::Idle => "No job for a while. Freeing it means its next build starts cold.",
        GcState::Merged => "Its branch is merged and nothing tracked is changed.",
        GcState::Gone => "Its working tree no longer exists.",
        GcState::Old => "Output of the older cache layout that no working tree links to.",
    }
}

/// True when the row can be ticked: anything a job does not use.
pub fn selectable(r: &GcRow) -> bool {
    r.state != GcState::InUse
}

/// The rows `Select merged, gone and old` ticks.
pub fn reclaimable(v: &GcView) -> BTreeSet<String> {
    v.rows
        .iter()
        .filter(|r| r.state.reclaimable() && r.freed_bytes == 0)
        .map(|r| r.checkout.clone())
        .collect()
}

/// Drops ticks of rows that are gone from the report or a job now uses.
pub fn prune(v: &GcView, ticked: &mut BTreeSet<String>) {
    ticked.retain(|c| {
        v.rows
            .iter()
            .any(|r| &r.checkout == c && selectable(r) && r.freed_bytes == 0)
    });
}

/// Bytes the ticked rows hold, over every machine.
pub fn ticked_bytes(v: &GcView, ticked: &BTreeSet<String>) -> u64 {
    v.rows
        .iter()
        .filter(|r| ticked.contains(&r.checkout))
        .flat_map(|r| &r.held)
        .map(|h| h.bytes)
        .sum()
}

/// The Free button's label: `Free 12.3 GiB from 3 checkouts`.
pub fn free_label(v: &GcView, ticked: &BTreeSet<String>) -> String {
    let n = ticked.len();
    format!(
        "Free {} from {n} checkout{}",
        human_bytes(ticked_bytes(v, ticked)),
        if n == 1 { "" } else { "s" }
    )
}

/// The confirmation text: what goes, what stays, and that a ticked active or idle row builds
/// cold next time.
pub fn confirm_text(v: &GcView, ticked: &BTreeSet<String>) -> String {
    let mut s = format!(
        "{}?\n\nOnly build output and the peers' synced copies are deleted. Working trees stay, \
         and a checkout a job starts using meanwhile is kept.",
        free_label(v, ticked)
    );
    let warm: Vec<&str> = v
        .rows
        .iter()
        .filter(|r| ticked.contains(&r.checkout))
        .filter(|r| matches!(r.state, GcState::Active | GcState::Idle))
        .map(|r| r.checkout.as_str())
        .collect();
    if !warm.is_empty() {
        s.push_str(&format!(
            "\n\nStill in use, their next build starts cold: {}.",
            warm.join(", ")
        ));
    }
    s
}

/// The line under the cards: after a free what it did, else how much can go.
pub fn summary(v: &GcView) -> String {
    if v.deleted {
        let freed: u64 = v.rows.iter().map(|r| r.freed_bytes).sum();
        let n = v.rows.iter().filter(|r| r.freed_bytes > 0).count();
        let kept = v.rows.iter().filter(|r| r.kept.is_some()).count();
        let mut s = format!(
            "Freed {} from {n} checkout{}.",
            human_bytes(freed),
            if n == 1 { "" } else { "s" }
        );
        if kept > 0 {
            s.push_str(&format!(" {kept} kept something; hover its row for why."));
        }
        return s;
    }
    if v.rows.is_empty() {
        return "No checkout keeps build output.".into();
    }
    let free: u64 = v.machines.iter().map(|m| m.reclaimable_bytes).sum();
    if free == 0 {
        format!(
            "{} checkout(s). Nothing merged, gone or old to free.",
            v.rows.len()
        )
    } else {
        format!(
            "{} checkout(s). {} is merged, gone or old and can be freed.",
            v.rows.len(),
            human_bytes(free)
        )
    }
}

/// The shared caches, one line each: `compiler cache (sccache): 30 GiB in … (note)`.
pub fn shared_lines(v: &GcView) -> Vec<String> {
    v.shared
        .iter()
        .map(|s| {
            format!(
                "{}: {} in {} ({})",
                s.name,
                human_bytes(s.bytes),
                s.path,
                s.note
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{FakeLink, LinkError};
    use std::sync::{Arc, Mutex};
    use tasks_daemon::api::{GcHeld, GcMachine, GcShared};

    const NOW: i64 = 10 * 86_400_000;
    const GIB: u64 = 1 << 30;

    fn held(machine: &str, bytes: u64) -> GcHeld {
        GcHeld {
            machine: machine.into(),
            bytes,
        }
    }

    fn row(checkout: &str, state: GcState, held: Vec<GcHeld>) -> GcRow {
        GcRow {
            checkout: checkout.into(),
            branch: Some("feature".into()),
            state,
            merged_into: (state == GcState::Merged).then(|| "dev".into()),
            last_used_ms: Some(NOW - 2 * 86_400_000),
            held,
            freed_bytes: 0,
            kept: None,
        }
    }

    fn view() -> GcView {
        GcView {
            idle_days: 14,
            machines: vec![
                GcMachine {
                    name: "laptop".into(),
                    held_bytes: 12 * GIB,
                    reclaimable_bytes: 4 * GIB,
                    error: None,
                },
                GcMachine {
                    name: "mac".into(),
                    held_bytes: 0,
                    reclaimable_bytes: 0,
                    error: Some("mac runs an older loadtabled".into()),
                },
            ],
            rows: vec![
                row(
                    "(unknown checkout 4ca7e0dc2b4d)",
                    GcState::Gone,
                    vec![held("laptop", GIB)],
                ),
                row("~/wt/done", GcState::Merged, vec![held("laptop", 3 * GIB)]),
                row("~/main", GcState::Active, vec![held("laptop", 6 * GIB)]),
                row("~/wt/busy", GcState::InUse, vec![held("laptop", 2 * GIB)]),
            ],
            shared: vec![GcShared {
                name: "compiler cache (sccache)".into(),
                path: "/c/sccache".into(),
                bytes: 30 * GIB,
                note: "sccache keeps itself under 30G".into(),
            }],
            notes: vec![],
            deleted: false,
        }
    }

    // R-840
    #[test]
    fn r840_the_disk_tab_shows_a_card_per_machine_and_a_row_per_checkout() {
        let v = view();
        let c = cards(&v);
        assert_eq!(c[0].line, "build cache 12 GiB · 4 GiB can be freed");
        assert!(!c[0].failed);
        assert_eq!(c[1].line, "could not report: mac runs an older loadtabled");
        assert!(c[1].failed);
        // a machine that did not report gets no column
        assert_eq!(
            columns(&v),
            vec!["", "State", "Checkout", "Branch", "Last job", "laptop"]
        );
        assert_eq!(
            cells(&v, &v.rows[1], NOW),
            vec!["merged", "~/wt/done", "feature (in dev)", "2d ago", "3 GiB"]
        );
        assert_eq!(cells(&v, &v.rows[3], NOW)[0], "in use");
        assert_eq!(
            summary(&v),
            "4 checkout(s). 4 GiB is merged, gone or old and can be freed."
        );
        assert_eq!(
            shared_lines(&v),
            vec!["compiler cache (sccache): 30 GiB in /c/sccache (sccache keeps itself under 30G)"]
        );
    }

    // R-840
    #[test]
    fn r840_ticking_frees_only_the_named_rows_and_never_offers_one_in_use() {
        let v = view();
        assert!(!selectable(&v.rows[3]), "in use cannot be ticked");
        let mut t = reclaimable(&v);
        assert_eq!(
            t.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["(unknown checkout 4ca7e0dc2b4d)", "~/wt/done"]
        );
        assert_eq!(free_label(&v, &t), "Free 4 GiB from 2 checkouts");
        assert!(!confirm_text(&v, &t).contains("starts cold"));
        t.insert("~/main".into());
        t.insert("~/wt/busy".into());
        t.insert("~/vanished".into());
        prune(&v, &mut t);
        assert_eq!(t.len(), 3, "busy and vanished rows are dropped: {t:?}");
        assert!(confirm_text(&v, &t).contains("their next build starts cold: ~/main."));

        let asked = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&asked);
        let link = FakeLink::from_fn(move |req| {
            *seen.lock().unwrap() = Some(req.clone());
            let mut done = view();
            done.deleted = true;
            done.rows[1].freed_bytes = 3 * GIB;
            done.rows[2].kept = Some("nixos: unreachable".into());
            Ok(Response::Gc(Box::new(done)))
        });
        let after = fetch(&link, &free_request(&t)).unwrap();
        assert!(matches!(
            asked.lock().unwrap().clone(),
            Some(Request::Gc { delete, checkouts, .. })
                if delete.is_empty() && checkouts.len() == 3 && checkouts.contains(&"~/main".to_string())
        ));
        assert_eq!(
            summary(&after),
            "Freed 3 GiB from 1 checkout. 1 kept something; hover its row for why."
        );
        assert_eq!(columns(&after).last().map(String::as_str), Some("Freed"));
        let mut t2 = t.clone();
        prune(&after, &mut t2);
        assert!(!t2.contains("~/wt/done"), "a freed row is no longer ticked");
    }

    // R-840
    #[test]
    fn r840_a_failed_report_is_said_in_words() {
        let down = FakeLink::from_fn(|_| Err(LinkError("socket gone".into())));
        assert_eq!(
            fetch(&down, &report_request()),
            Err("socket gone".to_string())
        );
        let odd = FakeLink::from_fn(|_| Ok(Response::Ack));
        assert!(
            fetch(&odd, &report_request())
                .unwrap_err()
                .contains("unexpected reply to gc")
        );
        assert_eq!(ago(None, NOW), "-");
        assert_eq!(ago(Some(NOW - 30_000), NOW), "just now");
        assert_eq!(ago(Some(NOW - 3 * 3_600_000), NOW), "3h ago");
    }
}
