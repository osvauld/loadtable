//! Stored job records and list filters.

use tasks_core::{JobId, JobSpec, MachineName, Phase};

/// A job as stored in the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    /// Job id.
    pub id: JobId,
    /// The submitted spec.
    pub spec: JobSpec,
    /// Current phase.
    pub phase: Phase,
    /// Submission time (ms since epoch).
    pub submitted_at_ms: i64,
    /// Start time, once running.
    pub started_at_ms: Option<i64>,
    /// Finish time, once terminal.
    pub finished_at_ms: Option<i64>,
    /// Machine that ran or runs it.
    pub machine: Option<MachineName>,
}

/// Criteria for `Store::list`; all set fields must match.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Only this owner.
    pub owner: Option<String>,
    /// Only non-terminal jobs.
    pub active_only: bool,
    /// Maximum rows returned.
    pub limit: Option<usize>,
}

impl ListFilter {
    /// True when the record passes the owner and activity criteria.
    pub fn matches(&self, r: &JobRecord) -> bool {
        self.owner
            .as_deref()
            .is_none_or(|o| r.spec.owner.as_str() == o)
            && !(self.active_only && r.phase.is_terminal())
    }
}

/// Current time in ms since the Unix epoch (0 if the clock is before it).
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Timestamps to set when entering `next`: (start, finish).
pub(crate) fn stamps(next: &Phase) -> (bool, bool) {
    (matches!(next, Phase::Running), next.is_terminal())
}
