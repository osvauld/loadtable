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
    /// Target labels (`test:lifecycle`) with any sample under `recipe` on any machine, sorted
    /// and without duplicates. Lets a policy notice new and removed targets.
    fn targets(&self, recipe: &str) -> Result<Vec<String>, TimingError>;
    /// The peak memory in bytes that the latest runs of the whole-job series `key` measured,
    /// newest first, at most [`RECENT_PEAKS`]; `None` for a run that measured none. A store
    /// that keeps no memory figures returns an empty list: unknown.
    fn recent_peaks(&self, _key: &SampleKey) -> Result<Vec<Option<u64>>, TimingError> {
        Ok(Vec::new())
    }
    /// Every whole-job sample on `machine` whose recipe runs `tool` (its first word), with its
    /// recipe: what a command's relatives took when it has no runs of its own. A store that
    /// cannot list them returns an empty list.
    fn machine_samples(
        &self,
        _machine: &str,
        _tool: &str,
    ) -> Result<Vec<(String, Sample)>, TimingError> {
        Ok(Vec::new())
    }
}

/// Most entries [`History::recent_peaks`] returns.
pub const RECENT_PEAKS: usize = 3;

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

    fn targets(&self, recipe: &str) -> Result<Vec<String>, TimingError> {
        if self.fail {
            return Err(TimingError::History("store unavailable".into()));
        }
        let set: std::collections::BTreeSet<&String> = self
            .map
            .keys()
            .filter(|k| k.recipe == recipe)
            .filter_map(|k| k.target.as_ref())
            .collect();
        Ok(set.into_iter().cloned().collect())
    }

    fn machine_samples(
        &self,
        machine: &str,
        tool: &str,
    ) -> Result<Vec<(String, Sample)>, TimingError> {
        if self.fail {
            return Err(TimingError::History("store unavailable".into()));
        }
        Ok(self
            .map
            .iter()
            .filter(|(k, _)| {
                k.machine == machine && k.target.is_none() && crate::family::tool(&k.recipe) == tool
            })
            .flat_map(|(k, v)| v.iter().map(|s| (k.recipe.clone(), *s)))
            .collect())
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
    fn targets_lists_each_label_once_for_the_recipe() {
        let mut h = MemHistory::new();
        let s = Sample {
            secs: 1.0,
            cache: Cache::Warm,
            outcome: Outcome::Passed,
            at_ms: 5,
        };
        for (recipe, target, machine) in [
            ("r", Some("test:b"), "a"),
            ("r", Some("test:a"), "a"),
            ("r", Some("test:a"), "b"),
            ("r", None, "a"),
            ("other", Some("test:c"), "a"),
        ] {
            let key = SampleKey {
                recipe: recipe.into(),
                target: target.map(str::to_string),
                machine: machine.into(),
            };
            h.add(key, s);
        }
        assert_eq!(h.targets("r").unwrap(), ["test:a", "test:b"]);
        assert!(h.targets("none").unwrap().is_empty());
        assert!(MemHistory::failing().targets("r").is_err());
    }

    #[test]
    fn failing_history_errors() {
        assert!(matches!(
            MemHistory::failing().samples(&key("a")),
            Err(TimingError::History(_))
        ));
    }
}
