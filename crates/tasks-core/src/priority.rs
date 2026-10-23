//! Queue priority classes (design §10).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Longest `--reason` text kept with a blocking job, in characters.
pub const MAX_PRIORITY_REASON: usize = 300;

/// A job's class in the queue. Declared first-served first, so the derived order is the queue
/// order: `Blocking < Normal < Background`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// An agent or person is waiting on the result now. Needs a reason; audited.
    Blocking,
    /// The default.
    #[default]
    Normal,
    /// Warm-ups, long suites, sweeps. Waits behind everything else until it ages.
    Background,
}

impl Priority {
    /// The word used on the command line, in the API and in audit rows.
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Blocking => "blocking",
            Priority::Normal => "normal",
            Priority::Background => "background",
        }
    }

    /// True for the default class, which is left out of stored specs.
    pub fn is_normal(&self) -> bool {
        *self == Priority::Normal
    }

    /// One class up the queue after waiting, or `None` where aging stops. Aging never reaches
    /// `Blocking`: that class is a person's claim with a reason, not a reward for waiting.
    pub fn aged(self) -> Option<Priority> {
        match self {
            Priority::Background => Some(Priority::Normal),
            Priority::Normal | Priority::Blocking => None,
        }
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The text is not a priority class.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown priority {0:?} (blocking, normal or background)")]
pub struct UnknownPriority(pub String);

impl FromStr for Priority {
    type Err = UnknownPriority;

    fn from_str(s: &str) -> Result<Priority, UnknownPriority> {
        match s {
            "blocking" => Ok(Priority::Blocking),
            "normal" => Ok(Priority::Normal),
            "background" => Ok(Priority::Background),
            other => Err(UnknownPriority(other.to_string())),
        }
    }
}

/// Checks a priority and its reason together: `blocking` needs a non-blank reason of at most
/// [`MAX_PRIORITY_REASON`] characters; the other classes take none.
pub fn check_reason(priority: Priority, reason: Option<&str>) -> Result<(), crate::CoreError> {
    let text = reason.map(str::trim).filter(|r| !r.is_empty());
    if priority == Priority::Blocking && text.is_none() {
        return Err(crate::CoreError::BlockingNeedsReason);
    }
    if reason.is_some_and(|r| r.chars().count() > MAX_PRIORITY_REASON) {
        return Err(crate::CoreError::PriorityReasonTooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CoreError;

    #[test]
    fn r338_the_classes_sort_blocking_then_normal_then_background() {
        let mut v = [Priority::Background, Priority::Blocking, Priority::Normal];
        v.sort();
        assert_eq!(
            v,
            [Priority::Blocking, Priority::Normal, Priority::Background]
        );
        assert_eq!(Priority::default(), Priority::Normal);
    }

    #[test]
    fn r338_words_round_trip_and_unknown_words_are_named() {
        for p in [Priority::Blocking, Priority::Normal, Priority::Background] {
            assert_eq!(p.as_str().parse::<Priority>(), Ok(p));
            assert_eq!(serde_json::to_string(&p).unwrap(), format!("\"{p}\""));
        }
        assert_eq!(
            "urgent".parse::<Priority>(),
            Err(UnknownPriority("urgent".into()))
        );
    }

    #[test]
    fn r339_blocking_needs_a_reason_the_others_do_not() {
        assert_eq!(
            check_reason(Priority::Blocking, None),
            Err(CoreError::BlockingNeedsReason)
        );
        assert_eq!(
            check_reason(Priority::Blocking, Some("   ")),
            Err(CoreError::BlockingNeedsReason)
        );
        assert_eq!(
            check_reason(Priority::Blocking, Some("release gate")),
            Ok(())
        );
        assert_eq!(check_reason(Priority::Normal, None), Ok(()));
        assert_eq!(check_reason(Priority::Background, None), Ok(()));
        let long = "x".repeat(MAX_PRIORITY_REASON + 1);
        assert_eq!(
            check_reason(Priority::Blocking, Some(&long)),
            Err(CoreError::PriorityReasonTooLong)
        );
    }

    #[test]
    fn r340_aging_lifts_background_one_step_and_never_reaches_blocking() {
        assert_eq!(Priority::Background.aged(), Some(Priority::Normal));
        assert_eq!(Priority::Normal.aged(), None);
        assert_eq!(Priority::Blocking.aged(), None);
    }
}
