//! Admission error type.

use tasks_core::SlotKind;
use tasks_platform::PlatformError;
use thiserror::Error;

/// Errors from the admission layer. Waiting is never an error.
#[derive(Debug, Error)]
pub enum AdmissionError {
    /// A platform reading or lock operation failed; admission refuses (fail closed).
    #[error("platform error: {0}")]
    Platform(#[from] PlatformError),
    /// The short lane only exists for compile and run jobs.
    #[error("the short lane does not exist for {0:?} slots")]
    NoShortLane(SlotKind),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_cause() {
        let e = AdmissionError::NoShortLane(SlotKind::Device);
        assert!(e.to_string().contains("Device"));
        let p = AdmissionError::from(PlatformError::Parse("bad".into()));
        assert!(p.to_string().contains("bad"));
    }
}
