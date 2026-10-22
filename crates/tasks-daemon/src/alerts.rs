//! Alerts with stable ids that dedupe and clear themselves (R-221).
//!
//! The scheduler states, on every evaluation, which conditions currently hold
//! ([`AlertBook::reconcile`]); an alert that is no longer stated clears, one that is stated
//! again keeps its first-seen time. Warn and critical alerts are queued once for a future
//! desktop notifier ([`AlertBook::take_notifications`]); nothing is sent from here.

use crate::api::AlertView;
use std::collections::{BTreeMap, BTreeSet};

/// How bad an alert is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertLevel {
    /// For the record.
    Info,
    /// Needs attention soon.
    Warn,
    /// Needs attention now.
    Critical,
}

impl AlertLevel {
    /// The level as shown in the API.
    pub fn name(self) -> &'static str {
        match self {
            AlertLevel::Info => "info",
            AlertLevel::Warn => "warn",
            AlertLevel::Critical => "critical",
        }
    }
}

/// A condition that currently holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    /// Stable id: the same condition always has the same id.
    pub id: String,
    /// Severity.
    pub level: AlertLevel,
    /// What is wrong.
    pub message: String,
    /// When it first appeared, ms since epoch.
    pub since_ms: i64,
}

/// The set of active alerts.
#[derive(Debug, Default)]
pub struct AlertBook {
    active: BTreeMap<String, Alert>,
    notified: BTreeSet<String>,
    pending: Vec<Alert>,
}

impl AlertBook {
    /// An empty book.
    pub fn new() -> AlertBook {
        AlertBook::default()
    }

    /// Makes the active set equal `wanted` (id, level, message): new ids appear, known ids keep
    /// their first-seen time and take the new text, missing ids clear.
    pub fn reconcile(&mut self, wanted: Vec<(String, AlertLevel, String)>, now_ms: i64) {
        let ids: BTreeSet<&str> = wanted.iter().map(|(id, _, _)| id.as_str()).collect();
        self.active.retain(|id, _| ids.contains(id.as_str()));
        self.notified.retain(|id| ids.contains(id.as_str()));
        for (id, level, message) in wanted {
            let since_ms = self.active.get(&id).map_or(now_ms, |a| a.since_ms);
            let alert = Alert {
                id: id.clone(),
                level,
                message,
                since_ms,
            };
            if level >= AlertLevel::Warn && self.notified.insert(id.clone()) {
                self.pending.push(alert.clone());
            }
            self.active.insert(id, alert);
        }
    }

    /// The active alerts, worst first, then by id.
    pub fn active(&self) -> Vec<&Alert> {
        let mut v: Vec<&Alert> = self.active.values().collect();
        v.sort_by(|a, b| b.level.cmp(&a.level).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// The active alerts as the API shows them.
    pub fn views(&self) -> Vec<AlertView> {
        self.active()
            .into_iter()
            .map(|a| AlertView {
                id: a.id.clone(),
                level: a.level.name().to_string(),
                message: a.message.clone(),
                since_ms: a.since_ms,
            })
            .collect()
    }

    /// Warn and critical alerts that appeared since the last call, each once.
    pub fn take_notifications(&mut self) -> Vec<Alert> {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(id: &str, l: AlertLevel, m: &str) -> (String, AlertLevel, String) {
        (id.to_string(), l, m.to_string())
    }

    // R-221
    #[test]
    fn r221_dedupe_and_auto_clear_through_the_book() {
        let mut b = AlertBook::new();
        b.reconcile(vec![w("disk-cache", AlertLevel::Warn, "12 GiB free")], 100);
        b.reconcile(vec![w("disk-cache", AlertLevel::Warn, "11 GiB free")], 200);
        assert_eq!(b.active().len(), 1);
        let a = &b.active()[0];
        assert_eq!(a.since_ms, 100, "first-seen time survives");
        assert_eq!(a.message, "11 GiB free");
        b.reconcile(vec![], 300);
        assert!(b.active().is_empty(), "auto-clear");
        b.reconcile(vec![w("disk-cache", AlertLevel::Warn, "again")], 400);
        assert_eq!(b.active()[0].since_ms, 400);
    }

    // R-221
    #[test]
    fn r221_only_warn_and_critical_notify_and_only_once() {
        let mut b = AlertBook::new();
        let set = || {
            vec![
                w("a-info", AlertLevel::Info, "fyi"),
                w("b-warn", AlertLevel::Warn, "careful"),
                w("c-crit", AlertLevel::Critical, "now"),
            ]
        };
        b.reconcile(set(), 1);
        b.reconcile(set(), 2);
        let n: Vec<String> = b.take_notifications().into_iter().map(|a| a.id).collect();
        assert_eq!(n, ["b-warn", "c-crit"]);
        b.reconcile(set(), 3);
        assert!(b.take_notifications().is_empty());
        // clears, then returns: a fresh notification
        b.reconcile(vec![], 4);
        b.reconcile(set(), 5);
        assert_eq!(b.take_notifications().len(), 2);
        assert_eq!(b.views()[0].id, "c-crit", "worst first");
    }
}
