//! A transfer in progress for one job (R-801): the checkout going to a peer, or an output file
//! coming back. The daemon keeps the latest one next to the job log while it moves, so `get`,
//! `status` and the UI can show a bar instead of a job that looks stuck.

use crate::human_bytes;
use serde::{Deserialize, Serialize};

/// Which way a transfer goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    /// The checkout going to the peer before the job runs there.
    Sync,
    /// An output file coming back after the job ran (`--fetch`).
    Fetch,
}

/// How far one transfer got.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferProgress {
    /// Which way.
    pub kind: TransferKind,
    /// The machine on the other end.
    pub peer: String,
    /// The file coming back (a fetch), relative to the checkout root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Bytes moved so far.
    pub done_bytes: u64,
    /// Bytes in all, when known (a fetched file; a sync pack is built as it goes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    /// Bytes per second since the transfer started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_bps: Option<f64>,
    /// How the command ended on the peer, for a fetch: `command exit 0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_result: Option<String>,
    /// When this was measured, ms since the epoch.
    pub at_ms: i64,
}

const MIB: u64 = 1024 * 1024;

impl TransferProgress {
    /// Fraction done, when the total is known.
    pub fn fraction(&self) -> Option<f64> {
        self.total_bytes
            .filter(|t| *t > 0)
            .map(|t| (self.done_bytes as f64 / t as f64).clamp(0.0, 1.0))
    }

    /// Seconds left at the rate so far, when the total is known.
    pub fn eta_secs(&self) -> Option<f64> {
        let left = self.total_bytes?.saturating_sub(self.done_bytes);
        self.rate_bps.filter(|r| *r > 0.0).map(|r| left as f64 / r)
    }

    /// `command exit 0 on nixos; fetching target/debug/app 120/362 MiB, 3.6 MiB/s, ~67s left`,
    /// or `sending the checkout to nixos: 12 MiB sent, 2.1 MiB/s`.
    pub fn line(&self) -> String {
        let rate = self
            .rate_bps
            .map(|r| format!(", {}/s", human_bytes(r as u64)))
            .unwrap_or_default();
        if self.kind == TransferKind::Sync {
            return format!(
                "sending the checkout to {}: {} sent{rate}",
                self.peer,
                human_bytes(self.done_bytes)
            );
        }
        let mut s = String::new();
        if let Some(r) = &self.remote_result {
            s.push_str(&format!("{r} on {}; ", self.peer));
        }
        s.push_str("fetching ");
        s.push_str(self.path.as_deref().unwrap_or("outputs"));
        match self.total_bytes {
            Some(t) if t >= MIB => s.push_str(&format!(
                " {}/{} MiB",
                self.done_bytes / MIB,
                t.div_ceil(MIB)
            )),
            Some(t) => s.push_str(&format!(
                " {}/{}",
                human_bytes(self.done_bytes),
                human_bytes(t)
            )),
            None => s.push_str(&format!(" {}", human_bytes(self.done_bytes))),
        }
        s.push_str(&rate);
        if let Some(e) = self.eta_secs() {
            s.push_str(&format!(", ~{}s left", e.ceil() as u64));
        }
        s
    }
}

/// Paces the reports of one transfer: at most one every `every_ms`.
#[derive(Debug, Clone)]
pub struct TransferMeter {
    started_ms: i64,
    last_ms: Option<i64>,
    every_ms: i64,
}

impl TransferMeter {
    /// A meter for a transfer starting at `now_ms`.
    pub fn new(now_ms: i64, every_ms: i64) -> TransferMeter {
        TransferMeter {
            started_ms: now_ms,
            last_ms: None,
            every_ms,
        }
    }

    /// True when a report is due at `now_ms` (the first one at once); marks it made.
    pub fn due(&mut self, now_ms: i64) -> bool {
        if self
            .last_ms
            .is_some_and(|l| now_ms.saturating_sub(l) < self.every_ms)
        {
            return false;
        }
        self.last_ms = Some(now_ms);
        true
    }

    /// Bytes per second for `done` bytes moved by `now_ms`; `None` until some time has passed.
    pub fn rate(&self, now_ms: i64, done: u64) -> Option<f64> {
        let ms = now_ms.saturating_sub(self.started_ms);
        (ms >= 200).then(|| done as f64 * 1000.0 / ms as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch(done: u64, total: u64, rate: f64) -> TransferProgress {
        TransferProgress {
            kind: TransferKind::Fetch,
            peer: "nixos".into(),
            path: Some("target/debug/p2p-desktop".into()),
            done_bytes: done,
            total_bytes: Some(total),
            rate_bps: Some(rate),
            remote_result: Some("command exit 0".into()),
            at_ms: 0,
        }
    }

    #[test]
    fn r801_a_fetch_line_names_the_remote_result_the_file_and_how_far_it_got() {
        let p = fetch(120 * MIB, 362 * MIB, 4.0 * MIB as f64);
        assert_eq!(
            p.line(),
            "command exit 0 on nixos; fetching target/debug/p2p-desktop 120/362 MiB, 4 MiB/s, ~61s left"
        );
        assert!((p.fraction().unwrap() - 120.0 / 362.0).abs() < 1e-9);
        let s = TransferProgress {
            kind: TransferKind::Sync,
            path: None,
            total_bytes: None,
            remote_result: None,
            ..fetch(12 * MIB, 0, 2.0 * MIB as f64)
        };
        assert_eq!(
            s.line(),
            "sending the checkout to nixos: 12 MiB sent, 2 MiB/s"
        );
        assert_eq!((s.fraction(), s.eta_secs()), (None, None));
        let back: TransferProgress =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn r801_the_meter_reports_at_most_once_per_interval() {
        let mut m = TransferMeter::new(0, 1000);
        assert!(m.due(0), "the first report is due at once");
        assert!(!m.due(999));
        assert!(m.due(1000));
        assert_eq!(m.rate(100, 1), None, "too early for a rate");
        assert_eq!(m.rate(2000, 4000), Some(2000.0));
    }
}
