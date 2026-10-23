//! Identifier newtypes.

use crate::CoreError;
use serde::{Deserialize, Serialize};

/// Numeric job identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct JobId(pub i64);

/// Name of a machine in the cluster.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MachineName(pub String);

/// A tool a machine can provide, such as "cargo" or "adb".
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Tool(pub String);

/// Identifier of an attached device (for example an adb serial).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

/// Validated job owner name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Owner(String);

impl Owner {
    /// Validates and wraps an owner name (non-empty, at most 128 bytes, `[A-Za-z0-9._-]`).
    pub fn new(s: &str) -> Result<Owner, CoreError> {
        if s.is_empty() {
            return Err(CoreError::OwnerEmpty);
        }
        if s.len() > 128 {
            return Err(CoreError::OwnerTooLong);
        }
        if let Some(c) = s
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        {
            return Err(CoreError::OwnerBadChar(c));
        }
        Ok(Owner(s.to_string()))
    }

    /// Returns the owner name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_rules() {
        assert!(Owner::new("v2-skeleton_1.a").is_ok());
        assert_eq!(Owner::new(""), Err(CoreError::OwnerEmpty));
        assert_eq!(Owner::new(&"a".repeat(129)), Err(CoreError::OwnerTooLong));
        assert!(Owner::new(&"a".repeat(128)).is_ok());
        assert_eq!(Owner::new("a b"), Err(CoreError::OwnerBadChar(' ')));
        assert_eq!(Owner::new("é"), Err(CoreError::OwnerBadChar('é')));
    }
}
