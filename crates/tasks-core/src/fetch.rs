//! What became of each output file a job asked to have fetched back.

use crate::human_bytes;
use serde::{Deserialize, Serialize};

/// How one requested output ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FetchStatus {
    /// The file is in the checkout, hash verified.
    Fetched {
        /// Size in bytes.
        bytes: u64,
        /// Content hash, hex SHA-256.
        sha256: String,
        /// True when the checkout already had exactly this content and nothing was transferred.
        #[serde(default)]
        unchanged: bool,
    },
    /// The peer has no such file.
    Missing,
    /// The file is over the size limit.
    TooLarge {
        /// Its size.
        bytes: u64,
        /// The limit.
        limit: u64,
    },
    /// Refused or failed; the reason says which.
    Failed {
        /// Why.
        reason: String,
    },
}

/// One requested path and its outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchResult {
    /// The path, relative to the checkout root.
    pub path: String,
    /// What happened.
    pub status: FetchStatus,
}

impl FetchResult {
    /// True when the file is in place.
    pub fn ok(&self) -> bool {
        matches!(self.status, FetchStatus::Fetched { .. })
    }

    /// The log line for it: `fetched PATH (12.3 MiB, sha256 ...)` or why not.
    pub fn line(&self) -> String {
        let p = &self.path;
        match &self.status {
            FetchStatus::Fetched {
                bytes,
                sha256,
                unchanged,
            } => format!(
                "fetched {p} ({}, sha256 {sha256}{})",
                human_bytes(*bytes),
                if *unchanged {
                    ", already up to date"
                } else {
                    ""
                }
            ),
            FetchStatus::Missing => format!("fetch failed: {p} does not exist on the peer"),
            FetchStatus::TooLarge { bytes, limit } => format!(
                "fetch failed: {p} is {} (over the {} limit)",
                human_bytes(*bytes),
                human_bytes(*limit)
            ),
            FetchStatus::Failed { reason } => format!("fetch failed: {p}: {reason}"),
        }
    }
}

/// Whether any requested file is not in place.
pub fn any_failed(results: &[FetchResult]) -> bool {
    results.iter().any(|r| !r.ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r502_lines_name_the_path_size_hash_or_the_reason() {
        let ok = FetchResult {
            path: "a/b.apk".into(),
            status: FetchStatus::Fetched {
                bytes: 12 * 1024 * 1024 + 300_000,
                sha256: "ab".into(),
                unchanged: false,
            },
        };
        assert_eq!(ok.line(), "fetched a/b.apk (12.2 MiB, sha256 ab)");
        let miss = FetchResult {
            path: "x".into(),
            status: FetchStatus::Missing,
        };
        assert!(miss.line().contains("does not exist on the peer"));
        assert!(any_failed(&[ok.clone(), miss]));
        assert!(!any_failed(&[ok]));
    }

    #[test]
    fn r502_results_round_trip_as_json() {
        let r = FetchResult {
            path: "p".into(),
            status: FetchStatus::TooLarge { bytes: 5, limit: 2 },
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<FetchResult>(&s).unwrap(), r);
    }
}
