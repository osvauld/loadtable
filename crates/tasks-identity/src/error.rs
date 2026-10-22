//! Errors of the identity crate. None of them ever carries key material.

/// Why an identity operation failed.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The recovery phrase is not a valid BIP39 phrase.
    #[error("not a valid recovery phrase")]
    BadPhrase,
    /// The operating system has no random source.
    #[error("the operating system random source is unavailable")]
    NoRandomness,
    /// The keystore file could not be read or written.
    #[error("keystore {path}: {detail}")]
    Io {
        /// The file.
        path: String,
        /// What went wrong.
        detail: String,
    },
    /// The keystore file is not a keystore this build understands.
    #[error("keystore {path}: {detail}")]
    Corrupt {
        /// The file.
        path: String,
        /// What was wrong.
        detail: String,
    },
    /// The keystore was written by a newer build.
    #[error("keystore {path} is version {found}; this build reads version {supported}")]
    NewerVersion {
        /// The file.
        path: String,
        /// Version in the file.
        found: u32,
        /// Version understood.
        supported: u32,
    },
    /// The keystore file can be read by other users; refused rather than trusted.
    #[error("keystore {path} has mode {mode:o}; it must be 0600 (chmod 600 it)")]
    LooseMode {
        /// The file.
        path: String,
        /// The mode found.
        mode: u32,
    },
}
