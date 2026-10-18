//! Alerts: stable ids, severities, deduplication and auto-clear.
//!
//! A check reports what it observes on every run as [`Observation`]s. The [`AlertBook`] keeps
//! one active [`Alert`] per id: a repeat updates `last_seen`, `count`, summary and evidence
//! (never a second alert); an alert of that check that is not observed again is cleared and
//! moves to a bounded `recent` list. An alert raised again shortly after it cleared (a flapping
//! condition) keeps its original `first_seen`.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

/// Cleared alerts kept for `tasks alerts --all` and MCP `alerts`.
pub const RECENT: usize = 50;
/// A condition that returns within this many seconds continues its earlier alert.
pub const REFLAP_S: f64 = 600.0;
const MAX_EVIDENCE: usize = 10;
const MAX_TEXT: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Critical => "critical",
        }
    }
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "info" => Ok(Self::Info),
            "warn" | "warning" => Ok(Self::Warn),
            "critical" | "crit" => Ok(Self::Critical),
            other => Err(format!("severity {other:?} is not info, warn or critical")),
        }
    }
}

/// One alert, active or (with `cleared_at`) recently cleared.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    /// Stable: `KIND:NODE[:SUBJECT]`. The same condition always has the same id.
    pub id: String,
    /// `disk_reserve`, `disk_low`, `disk_filling`, `target_large`, `slot-holder`, `compiler-cache`, `declared_capability`,
    /// `memory_pressure`, `peer_offline`, `peer_stale`, `job_waiting`, `idle_peer`,
    /// `missing_capability`, `high_cpu`.
    pub kind: String,
    pub severity: Severity,
    /// The machine the alert is about (hostname, or a peer's configured name).
    pub node: String,
    /// The machine whose health monitor raised it (its hostname).
    pub origin: String,
    pub summary: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Suggested fix, for a person or an agent.
    pub fix: String,
    pub first_seen: f64,
    pub last_seen: f64,
    #[serde(default)]
    pub cleared_at: Option<f64>,
    /// Checks that observed it (repeats are counted, never duplicated).
    #[serde(default)]
    pub count: u64,
    /// Worth a desktop notification on the controller.
    #[serde(default)]
    pub needs_human: bool,
    /// The check that owns it (it clears only alerts of its own check).
    #[serde(default)]
    pub check: String,
}

impl Alert {
    pub fn active(&self) -> bool {
        self.cleared_at.is_none()
    }
    /// `CRITICAL nixos: disk below reserve ... (since 12m)`.
    pub fn line(&self, now: f64) -> String {
        let since = crate::placement::duration((now - self.first_seen).max(0.0));
        let state = match self.cleared_at {
            Some(at) => format!(
                "cleared {} ago",
                crate::placement::duration((now - at).max(0.0))
            ),
            None => format!("since {since}"),
        };
        format!(
            "{:<8} {}: {} ({state}, id {})",
            self.severity.as_str().to_uppercase(),
            self.node,
            self.summary,
            self.id
        )
    }
}

/// What a check saw on one run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Observation {
    pub kind: String,
    pub severity: Option<Severity>,
    pub node: String,
    /// Distinguishes several alerts of one kind on one node (a path, a job id, a slot).
    pub subject: Option<String>,
    pub summary: String,
    pub evidence: Vec<String>,
    pub fix: String,
    pub needs_human: bool,
}

impl Observation {
    pub fn new(kind: &str, severity: Severity, node: &str) -> Self {
        Self {
            kind: kind.into(),
            severity: Some(severity),
            node: node.into(),
            ..Default::default()
        }
    }
    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }
    pub fn summary(mut self, text: impl Into<String>) -> Self {
        self.summary = text.into();
        self
    }
    pub fn evidence(mut self, line: impl Into<String>) -> Self {
        self.evidence.push(line.into());
        self
    }
    pub fn fix(mut self, text: impl Into<String>) -> Self {
        self.fix = text.into();
        self
    }
    pub fn human(mut self) -> Self {
        self.needs_human = true;
        self
    }
    pub fn id(&self) -> String {
        match &self.subject {
            Some(s) => format!("{}:{}:{s}", self.kind, self.node),
            None => format!("{}:{}", self.kind, self.node),
        }
    }
}

fn bounded(text: &str) -> String {
    if text.chars().count() <= MAX_TEXT {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_TEXT - 1).collect();
    out.push('…');
    out
}

fn bounded_evidence(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .take(MAX_EVIDENCE)
        .map(|l| bounded(l))
        .collect()
}

/// The active alerts of one node and its recently cleared ones.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AlertBook {
    pub origin: String,
    pub active: BTreeMap<String, Alert>,
    #[serde(default)]
    pub recent: VecDeque<Alert>,
}

impl AlertBook {
    pub fn new(origin: &str) -> Self {
        Self {
            origin: origin.into(),
            ..Default::default()
        }
    }

    /// Applies one run of `check`. Returns the alerts that became active in this run (for the
    /// notifier); repeats only update the existing alert. Active alerts of `check` that were
    /// not observed are cleared.
    pub fn observe(&mut self, check: &str, observed: Vec<Observation>, now: f64) -> Vec<Alert> {
        let mut raised = vec![];
        let mut seen = std::collections::HashSet::new();
        for obs in observed {
            let id = obs.id();
            if !seen.insert(id.clone()) {
                // Same condition twice in one run: merge the evidence into the first.
                if let Some(a) = self.active.get_mut(&id) {
                    for line in obs.evidence {
                        if a.evidence.len() < MAX_EVIDENCE && !a.evidence.contains(&line) {
                            a.evidence.push(bounded(&line));
                        }
                    }
                }
                continue;
            }
            let severity = obs.severity.unwrap_or(Severity::Info);
            match self.active.get_mut(&id) {
                Some(a) => {
                    a.last_seen = now;
                    a.count += 1;
                    a.severity = severity;
                    a.summary = bounded(&obs.summary);
                    a.evidence = bounded_evidence(&obs.evidence);
                    a.fix = bounded(&obs.fix);
                    a.needs_human = obs.needs_human;
                }
                None => {
                    // A flapping condition continues its earlier alert.
                    let earlier = self.recent.iter().rposition(|r| {
                        r.id == id && r.cleared_at.is_some_and(|c| now - c <= REFLAP_S)
                    });
                    let (first_seen, count) = match earlier.and_then(|i| self.recent.remove(i)) {
                        Some(old) => (old.first_seen, old.count + 1),
                        None => (now, 1),
                    };
                    let alert = Alert {
                        id: id.clone(),
                        kind: obs.kind,
                        severity,
                        node: obs.node,
                        origin: self.origin.clone(),
                        summary: bounded(&obs.summary),
                        evidence: bounded_evidence(&obs.evidence),
                        fix: bounded(&obs.fix),
                        first_seen,
                        last_seen: now,
                        cleared_at: None,
                        count,
                        needs_human: obs.needs_human,
                        check: check.into(),
                    };
                    raised.push(alert.clone());
                    self.active.insert(id, alert);
                }
            }
        }
        let gone: Vec<String> = self
            .active
            .iter()
            .filter(|(id, a)| a.check == check && !seen.contains(*id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            if let Some(mut a) = self.active.remove(&id) {
                a.cleared_at = Some(now);
                self.recent.push_back(a);
            }
        }
        while self.recent.len() > RECENT {
            self.recent.pop_front();
        }
        raised
    }

    /// Active alerts, most severe first, then newest.
    pub fn sorted_active(&self) -> Vec<Alert> {
        let mut out: Vec<Alert> = self.active.values().cloned().collect();
        sort(&mut out);
        out
    }
}

/// Most severe first, then most recently seen, then id.
pub fn sort(alerts: &mut [Alert]) {
    alerts.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.last_seen.total_cmp(&a.last_seen))
            .then(a.id.cmp(&b.id))
    });
}
