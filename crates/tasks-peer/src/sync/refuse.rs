//! Why a snapshot refuses a file. Every refusal names the file and the reason; nothing is
//! skipped silently.

use std::fmt;

/// The reason a file stops a sync.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefuseReason {
    /// A Git LFS pointer: the real content is not in the working tree.
    #[error("is a Git LFS pointer (the content lives outside the repository)")]
    LfsPointer,
    /// A submodule entry.
    #[error("is a submodule (submodules are not synced)")]
    Submodule,
    /// A directory that holds its own git repository.
    #[error("is a nested git repository (not synced)")]
    NestedRepository,
    /// Over the per-file byte cap.
    #[error("is {size} bytes, over the {limit} byte limit")]
    TooLarge {
        /// Size.
        size: u64,
        /// Limit.
        limit: u64,
    },
    /// The file name says key material or credentials.
    #[error("looks like a secret by name ({0})")]
    SecretName(&'static str),
    /// The content has a key or token pattern.
    #[error("looks like a secret by content ({0})")]
    SecretContent(&'static str),
    /// A socket, fifo or device.
    #[error("is not a regular file or symlink")]
    NotRegular,
    /// Could not be read.
    #[error("could not be read: {0}")]
    Unreadable(String),
    /// The path is not safe to send.
    #[error("has an unsafe path: {0}")]
    BadPath(String),
    /// An allowlisted extra that is a symlink.
    #[error("is allowlisted as an extra but is a symlink")]
    ExtraSymlink,
}

impl RefuseReason {
    /// A stable short code for logs and audit rows.
    pub fn code(&self) -> &'static str {
        match self {
            RefuseReason::LfsPointer => "lfs_pointer",
            RefuseReason::Submodule => "submodule",
            RefuseReason::NestedRepository => "nested_repository",
            RefuseReason::TooLarge { .. } => "too_large",
            RefuseReason::SecretName(_) => "secret_name",
            RefuseReason::SecretContent(_) => "secret_content",
            RefuseReason::NotRegular => "not_regular",
            RefuseReason::Unreadable(_) => "unreadable",
            RefuseReason::BadPath(_) => "bad_path",
            RefuseReason::ExtraSymlink => "extra_symlink",
        }
    }
}

/// One refused file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// Path relative to the checkout.
    pub path: String,
    /// Why.
    pub reason: RefuseReason,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.path, self.reason)
    }
}

/// Every file that stopped one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusals(pub Vec<Refused>);

impl Refusals {
    /// True when some refusal has this reason code.
    pub fn has(&self, code: &str) -> bool {
        self.0.iter().any(|r| r.reason.code() == code)
    }
}

impl fmt::Display for Refusals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const SHOWN: usize = 8;
        write!(f, "{} file(s) refused: ", self.0.len())?;
        for (i, r) in self.0.iter().take(SHOWN).enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{r}")?;
        }
        if self.0.len() > SHOWN {
            write!(f, "; and {} more", self.0.len() - SHOWN)?;
        }
        Ok(())
    }
}
