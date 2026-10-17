//! The controller's view of every machine's health: this daemon's own `health` plus each
//! peer's, as carried in the peers' status snapshots. Used by `tasks alerts`, MCP `alerts`,
//! `cluster_status` and `submit`.
//!
//! Each machine publishes only the alerts it raised itself, so aggregation never loops: an
//! alert appears once, `reported_by` the machine whose snapshot carried it (`local` or the
//! peer's configured name).
use super::NodeHealth;
use super::alert::{self, Alert, Severity};
use super::capability::CapabilityMark;
use super::disk::{DiskStatus, ScanSummary};
use crate::metrics::Snapshot;
use crate::peer_link::PeersReport;
use crate::placement::duration;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClusterAlert {
    #[serde(flatten)]
    pub alert: Alert,
    /// `local`, or the configured name of the peer whose snapshot carried it.
    pub reported_by: String,
}

/// One machine's health as the controller sees it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NodeView {
    /// `local` or the peer's configured name.
    pub name: String,
    pub machine_id: Option<String>,
    pub online: bool,
    /// Its snapshot is old: its alerts and disks may be out of date.
    pub stale: bool,
    pub age_s: Option<f64>,
    pub checked_at: Option<f64>,
    pub disks: Vec<DiskStatus>,
    pub capabilities: Vec<CapabilityMark>,
    pub scan: Option<ScanSummary>,
    /// Capability probes of that daemon's environment.
    pub probes: Vec<super::node::ProbeResult>,
    /// Why there is no health data (older daemon, offline, unreadable).
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ClusterHealth {
    pub generated_at: f64,
    /// Active alerts, most severe first.
    pub alerts: Vec<ClusterAlert>,
    /// Recently cleared ones, newest first.
    pub recent: Vec<ClusterAlert>,
    pub nodes: Vec<NodeView>,
    pub notes: Vec<String>,
}

/// Which alerts to show.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub min_severity: Option<Severity>,
    /// A node name, machine id or peer name.
    pub node: Option<String>,
    /// Include recently cleared alerts.
    pub recent: bool,
}

fn add(out: &mut ClusterHealth, name: &str, health: &NodeHealth) {
    for a in &health.alerts {
        out.alerts.push(ClusterAlert {
            alert: a.clone(),
            reported_by: name.into(),
        });
    }
    for a in &health.recent {
        out.recent.push(ClusterAlert {
            alert: a.clone(),
            reported_by: name.into(),
        });
    }
}

fn parse_health(snapshot: &Snapshot) -> Option<&NodeHealth> {
    snapshot.health.as_ref().filter(|h| h.checked_at > 0.0)
}

/// Builds the cluster view from this machine's snapshot and the peer runtime's report.
pub fn aggregate(
    local: Result<&Snapshot, String>,
    peers: Option<&PeersReport>,
    now: f64,
) -> ClusterHealth {
    let mut out = ClusterHealth {
        generated_at: now,
        ..Default::default()
    };
    match local {
        Ok(snap) => {
            let health = parse_health(snap);
            if let Some(h) = health {
                add(&mut out, "local", h);
            }
            out.nodes.push(NodeView {
                name: "local".into(),
                machine_id: Some(snap.machine.id.clone()),
                online: true,
                stale: false,
                age_s: Some(0.0),
                checked_at: health.map(|h| h.checked_at),
                disks: health.map(|h| h.disks.clone()).unwrap_or_default(),
                capabilities: health.map(|h| h.capabilities.clone()).unwrap_or_default(),
                scan: health.and_then(|h| h.scan.clone()),
                probes: health.map(|h| h.probes.clone()).unwrap_or_default(),
                note: health.is_none().then(|| {
                    "no health data yet (the daemon predates health monitoring, or its first check has not run)".into()
                }),
            });
        }
        Err(e) => out
            .notes
            .push(format!("this machine's status is unavailable: {e}")),
    }
    for p in peers.map(|r| r.snapshots.as_slice()).unwrap_or_default() {
        let parsed = p
            .snapshot
            .as_ref()
            .map(|v| serde_json::from_value::<Snapshot>(v.clone()));
        let (snap, mut note) = match parsed {
            Some(Ok(s)) => (Some(s), None),
            Some(Err(e)) => (None, Some(format!("unreadable snapshot: {e}"))),
            None => (None, Some(format!("no snapshot ({})", p.state))),
        };
        let health = snap.as_ref().and_then(parse_health);
        if let Some(h) = health {
            add(&mut out, &p.name, h);
        } else if note.is_none() {
            note = Some("its daemon publishes no health data (older release?)".into());
        }
        out.nodes.push(NodeView {
            name: p.name.clone(),
            machine_id: snap.as_ref().map(|s| s.machine.id.clone()),
            online: p.online(),
            stale: p.stale,
            age_s: p.age_s,
            checked_at: health.map(|h| h.checked_at),
            disks: health.map(|h| h.disks.clone()).unwrap_or_default(),
            capabilities: health.map(|h| h.capabilities.clone()).unwrap_or_default(),
            scan: health.and_then(|h| h.scan.clone()),
            probes: health.map(|h| h.probes.clone()).unwrap_or_default(),
            note,
        });
    }
    let mut active: Vec<Alert> = out.alerts.iter().map(|a| a.alert.clone()).collect();
    alert::sort(&mut active);
    out.alerts.sort_by_key(|c| {
        active
            .iter()
            .position(|a| a.id == c.alert.id && a.origin == c.alert.origin)
            .unwrap_or(usize::MAX)
    });
    out.recent.sort_by(|a, b| {
        b.alert
            .cleared_at
            .unwrap_or(0.0)
            .total_cmp(&a.alert.cleared_at.unwrap_or(0.0))
    });
    out
}

impl ClusterAlert {
    fn about(&self, node: &str, nodes: &[NodeView]) -> bool {
        self.alert.node == node
            || self.reported_by == node
            || nodes
                .iter()
                .any(|n| n.name == self.reported_by && n.machine_id.as_deref() == Some(node))
    }
}

impl ClusterHealth {
    pub fn filtered(&self, f: &Filter) -> Self {
        let keep = |a: &ClusterAlert| {
            f.min_severity.is_none_or(|s| a.alert.severity >= s)
                && f.node.as_deref().is_none_or(|n| a.about(n, &self.nodes))
        };
        Self {
            generated_at: self.generated_at,
            alerts: self.alerts.iter().filter(|a| keep(a)).cloned().collect(),
            recent: if f.recent {
                self.recent.iter().filter(|a| keep(a)).cloned().collect()
            } else {
                vec![]
            },
            nodes: self
                .nodes
                .iter()
                .filter(|n| {
                    f.node
                        .as_deref()
                        .is_none_or(|want| n.name == want || n.machine_id.as_deref() == Some(want))
                })
                .cloned()
                .collect(),
            notes: self.notes.clone(),
        }
    }

    /// Active warn/critical alerts about any of `machines` (names or ids), plus capability
    /// alerts anywhere: what a submission placed there should know.
    pub fn relevant(&self, machines: &[&str]) -> Vec<&ClusterAlert> {
        self.alerts
            .iter()
            .filter(|a| a.alert.severity >= Severity::Warn)
            .filter(|a| {
                a.alert.kind == "missing_capability"
                    || machines.iter().any(|m| a.about(m, &self.nodes))
            })
            .filter(|a| a.alert.kind != "job_waiting")
            .collect()
    }

    pub fn render(&self, now: f64) -> String {
        let mut out = String::new();
        let label = |a: &ClusterAlert| {
            if a.reported_by == "local" {
                String::new()
            } else {
                format!(" [via {}]", a.reported_by)
            }
        };
        if self.alerts.is_empty() {
            out.push_str("no active alerts\n");
        } else {
            out.push_str(&format!("{} active alert(s):\n", self.alerts.len()));
        }
        for a in &self.alerts {
            out.push_str(&format!("{}{}\n", a.alert.line(now), label(a)));
            for e in &a.alert.evidence {
                out.push_str(&format!("    - {e}\n"));
            }
            if !a.alert.fix.is_empty() {
                out.push_str(&format!("    fix: {}\n", a.alert.fix));
            }
        }
        if !self.recent.is_empty() {
            out.push_str(&format!("\n{} recently cleared:\n", self.recent.len()));
            for a in &self.recent {
                out.push_str(&format!("{}{}\n", a.alert.line(now), label(a)));
            }
        }
        for n in &self.nodes {
            let id = n.machine_id.as_deref().unwrap_or("?");
            let mut head = format!("\n{} ({id})", n.name);
            if n.name != "local" {
                head.push_str(if n.online { ", online" } else { ", OFFLINE" });
                if n.stale {
                    head.push_str(", STALE");
                }
                if let Some(age) = n.age_s {
                    head.push_str(&format!(", snapshot {} old", duration(age)));
                }
            }
            if let Some(at) = n.checked_at {
                head.push_str(&format!(", checked {} ago", duration((now - at).max(0.0))));
            }
            out.push_str(&head);
            out.push_str(":\n");
            if let Some(note) = &n.note {
                out.push_str(&format!("  {note}\n"));
            }
            for d in &n.disks {
                out.push_str(&format!("  disk {}\n", d.line()));
                for c in d.top.iter().take(3) {
                    let mut line =
                        format!("      {} {}", super::procs::gib(c.bytes), c.path.display());
                    if let Some(checkout) = &c.checkout {
                        line.push_str(&format!(" (checkout {})", checkout.display()));
                    }
                    out.push_str(&line);
                    out.push('\n');
                }
            }
            for m in &n.capabilities {
                out.push_str(&format!(
                    "  capability: {} {} — {}\n",
                    m.node,
                    m.lacks(),
                    m.evidence()
                ));
            }
            if let Some(s) = &n.scan {
                out.push_str(&format!(
                    "  last disk scan {} ago: {:.1}s wall, {:.2}s CPU, {} entries{}\n",
                    duration((now - s.at).max(0.0)),
                    s.elapsed_s,
                    s.cpu_s,
                    s.entries,
                    if s.truncated {
                        " (budget reached; sizes are lower bounds)"
                    } else {
                        ""
                    }
                ));
            }
        }
        for n in &self.notes {
            out.push_str(&format!("\nnote: {n}\n"));
        }
        out
    }
}

/// Reads this daemon's status and (with a peer config) its peers' snapshots.
#[cfg(unix)]
pub fn gather(socket: &std::path::Path) -> ClusterHealth {
    use std::time::Duration;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let local = crate::request_status(socket, Duration::from_secs(60))
        .map_err(|e| e.to_string())
        .and_then(|r| {
            r.status.ok_or_else(|| {
                r.error
                    .unwrap_or_else(|| "daemon returned no status".into())
            })
        });
    let mut notes = vec![];
    let peers = if crate::placement::peer_config_present() {
        match crate::peer_link::request_peers(socket, Duration::from_secs(30)) {
            Ok(mut r) => {
                r.refresh(now);
                Some(r)
            }
            Err(e) => {
                notes.push(format!("peer machines unavailable: {e}"));
                None
            }
        }
    } else {
        None
    };
    let mut out = aggregate(local.as_ref().map_err(Clone::clone), peers.as_ref(), now);
    out.notes.extend(notes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::alert::{AlertBook, Observation};
    use crate::peer_link::{PeerSnapshot, STREAM_ONLINE};

    fn snapshot(id: &str, alerts: Vec<Observation>) -> Snapshot {
        let path = format!(
            "{}/tests/fixtures/snapshot/metrics-idle.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut s: Snapshot =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        s.machine.id = id.into();
        let mut book = AlertBook::new(id);
        book.observe("t", alerts, 100.0);
        s.health = Some(NodeHealth {
            node: id.into(),
            checked_at: 100.0,
            interval_s: 30.0,
            alerts: book.sorted_active(),
            ..Default::default()
        });
        s
    }

    #[test]
    fn local_and_peer_alerts_are_merged_sorted_and_filtered() {
        let local = snapshot(
            "laptop",
            vec![
                Observation::new("job_waiting", Severity::Warn, "laptop")
                    .subject("5")
                    .summary("waits"),
            ],
        );
        let worker = snapshot(
            "nixos",
            vec![
                Observation::new("disk_reserve", Severity::Critical, "nixos")
                    .subject("/")
                    .summary("full"),
                Observation::new("high_cpu", Severity::Info, "nixos")
                    .subject("9")
                    .summary("cpu"),
            ],
        );
        let report = PeersReport {
            snapshots: vec![PeerSnapshot {
                name: "worker".into(),
                role: "worker".into(),
                state: STREAM_ONLINE.into(),
                age_s: Some(2.0),
                snapshot: Some(serde_json::to_value(&worker).unwrap()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let c = aggregate(Ok(&local), Some(&report), 200.0);
        let ids: Vec<&str> = c.alerts.iter().map(|a| a.alert.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "disk_reserve:nixos:/",
                "job_waiting:laptop:5",
                "high_cpu:nixos:9"
            ]
        );
        assert_eq!(c.alerts[0].reported_by, "worker");
        assert_eq!(c.nodes.len(), 2);
        let warn = c.filtered(&Filter {
            min_severity: Some(Severity::Warn),
            ..Default::default()
        });
        assert_eq!(warn.alerts.len(), 2);
        // A peer's configured name, its machine id and the alert node all select it.
        for name in ["worker", "nixos"] {
            let f = c.filtered(&Filter {
                node: Some(name.into()),
                ..Default::default()
            });
            assert_eq!(f.alerts.len(), 2, "{name}");
            assert_eq!(f.nodes.len(), 1);
        }
        let relevant = c.relevant(&["worker"]);
        assert_eq!(relevant.len(), 1);
        assert_eq!(relevant[0].alert.kind, "disk_reserve");
        let text = c.render(200.0);
        assert!(text.contains("3 active alert(s):"), "{text}");
        assert!(text.contains("CRITICAL nixos: full"), "{text}");
        assert!(text.contains("[via worker]"), "{text}");
        assert!(
            text.contains("\nworker (nixos), online, snapshot 2s old"),
            "{text}"
        );
    }

    #[test]
    fn missing_health_is_explained_not_an_error() {
        let mut s = snapshot("laptop", vec![]);
        s.health = None;
        let c = aggregate(Ok(&s), None, 1.0);
        assert!(c.alerts.is_empty());
        assert!(
            c.nodes[0]
                .note
                .as_deref()
                .unwrap()
                .contains("no health data")
        );
        let c = aggregate(Err("connection refused".into()), None, 1.0);
        assert!(c.notes[0].contains("connection refused"));
        assert!(c.render(1.0).starts_with("no active alerts"));
    }
}
