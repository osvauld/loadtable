//! Duration samples and the storage trait behind them.

use crate::error::TimingError;
use std::collections::BTreeMap;

/// Whether the build state was warm when a sample was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cache {
    /// Build artifacts were present.
    Warm,
    /// A clean build.
    Cold,
    /// Not known.
    Unknown,
}

/// How a measured run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Outcome {
    /// Passed.
    Passed,
    /// Failed.
    Failed,
    /// Cancelled; kept for the record but never used in a median.
    Cancelled,
}

/// What a sample is a sample of.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SampleKey {
    /// The `RecipeKey` display string.
    pub recipe: String,
    /// A target label such as `test:lifecycle`; `None` for the whole job.
    pub target: Option<String>,
    /// Machine the run happened on.
    pub machine: String,
}

/// One measured duration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Duration in seconds.
    pub secs: f64,
    /// Build warmth.
    pub cache: Cache,
    /// How the run ended.
    pub outcome: Outcome,
    /// When the run finished, ms.
    pub at_ms: u64,
}

/// Read access to stored samples. The daemon backs this with its database.
pub trait History {
    /// All samples for `key`, in any order.
    fn samples(&self, key: &SampleKey) -> Result<Vec<Sample>, TimingError>;
}

/// In-memory history for tests.
#[derive(Debug, Default, Clone)]
pub struct MemHistory {
    map: BTreeMap<SampleKey, Vec<Sample>>,
    fail: bool,
}

impl MemHistory {
    /// An empty history.
    pub fn new() -> MemHistory {
        MemHistory::default()
    }

    /// A history whose every lookup fails, to test error paths.
    pub fn failing() -> MemHistory {
        MemHistory {
            map: BTreeMap::new(),
            fail: true,
        }
    }

    /// Adds a sample.
    pub fn add(&mut self, key: SampleKey, sample: Sample) {
        self.map.entry(key).or_default().push(sample);
    }
}

impl History for MemHistory {
    fn samples(&self, key: &SampleKey) -> Result<Vec<Sample>, TimingError> {
        if self.fail {
            return Err(TimingError::History("store unavailable".into()));
        }
        Ok(self.map.get(key).cloned().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn key(m: &str) -> SampleKey {
        SampleKey {
            recipe: "r".into(),
            target: None,
            machine: m.into(),
        }
    }

    #[test]
    fn mem_history_returns_added_samples() {
        let mut h = MemHistory::new();
        let s = Sample {
            secs: 1.0,
            cache: Cache::Warm,
            outcome: Outcome::Passed,
            at_ms: 5,
        };
        h.add(key("a"), s);
        assert_eq!(h.samples(&key("a")).unwrap(), vec![s]);
        assert!(h.samples(&key("b")).unwrap().is_empty());
    }

    #[test]
    fn failing_history_errors() {
        assert!(matches!(
            MemHistory::failing().samples(&key("a")),
            Err(TimingError::History(_))
        ));
    }
}
