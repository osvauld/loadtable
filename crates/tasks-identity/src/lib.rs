//! Machine identity for `tasks`: a BIP39 seed, three derived keys, a DID and a keystore file.
//!
//! This is our own copy of the osvauld identity contract (design decision D1): the same HKDF
//! contexts and the same `did:key` encoding as osvauld2 and p2p_chat, with their golden vectors
//! pinned in `tests/golden_identity.rs` so the copies cannot drift unnoticed.
//!
//! Secrets are zeroized on drop, written mode 0600, and never appear in `Debug` output or error
//! messages.

mod did;
mod error;
pub mod hex;
mod identity;
pub mod keystore;
mod signer;

pub use did::{did_from_public_key, public_key_from_did};
pub use error::IdentityError;
pub use identity::{Identity, generate, random_bytes, verify};
pub use keystore::{Keystore, PublicInfo, read_public};
pub use signer::Signer;

#[cfg(test)]
mod tests;
