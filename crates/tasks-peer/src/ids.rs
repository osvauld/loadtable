//! Validated identifiers that cross the wire.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::Path;

/// Why an identifier was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The text is empty.
    #[error("{what} is empty")]
    Empty {
        /// Which identifier.
        what: &'static str,
    },
    /// The text is longer than the limit.
    #[error("{what} is longer than {limit} bytes")]
    TooLong {
        /// Which identifier.
        what: &'static str,
        /// The limit.
        limit: usize,
    },
    /// The text has a character outside the allowed set.
    #[error("{what} has a character that is not allowed: {ch:?}")]
    BadChar {
        /// Which identifier.
        what: &'static str,
        /// The character.
        ch: char,
    },
    /// The text is not hex of the right length.
    #[error("{what} must be {len} lowercase hex characters")]
    BadHex {
        /// Which identifier.
        what: &'static str,
        /// Expected length.
        len: usize,
    },
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(
            D.get(usize::from(b >> 4)).copied().unwrap_or(b'0'),
        ));
        s.push(char::from(
            D.get(usize::from(b & 15)).copied().unwrap_or(b'0'),
        ));
    }
    s
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    b.chunks(2)
        .map(|p| {
            let hi = nib(*p.first()?)?;
            let lo = nib(*p.get(1)?)?;
            Some(hi << 4 | lo)
        })
        .collect()
}

/// Serde helper: byte vectors as lowercase hex strings.
pub(crate) mod hexbytes {
    use serde::{Deserialize, Deserializer, Serializer};
    pub(crate) fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::hex(v))
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let t = String::deserialize(d)?;
        super::unhex(&t).ok_or_else(|| serde::de::Error::custom("data is not lowercase hex"))
    }
}

fn plain(what: &'static str, s: &str, limit: usize, extra: &[char]) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty { what });
    }
    if s.len() > limit {
        return Err(IdError::TooLong { what, limit });
    }
    match s
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || extra.contains(c)))
    {
        Some(ch) => Err(IdError::BadChar { what, ch }),
        None => Ok(()),
    }
}

/// The authenticated identity of a peer (an iroh node id, or a test name).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PeerId(String);

impl PeerId {
    /// Validates a peer id: at most 128 bytes of `[A-Za-z0-9._-]`. The text is
    /// also used as a directory name, so nothing else is accepted.
    pub fn new(s: &str) -> Result<PeerId, IdError> {
        plain("peer id", s, 128, &['.', '_', '-'])?;
        if s.chars().all(|c| c == '.') {
            return Err(IdError::BadChar {
                what: "peer id",
                ch: '.',
            });
        }
        Ok(PeerId(s.to_string()))
    }
    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for PeerId {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        PeerId::new(&s)
    }
}
impl From<PeerId> for String {
    fn from(p: PeerId) -> String {
        p.0
    }
}
impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The peer's own id for a job it accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RemoteJobId(pub u64);

impl fmt::Display for RemoteJobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "remote #{}", self.0)
    }
}

/// Idempotency key of a submission: bounded, plain text (R-152).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SubmitKey(String);

impl SubmitKey {
    /// At most 128 bytes of `[A-Za-z0-9._:-]`.
    pub fn new(s: &str) -> Result<SubmitKey, IdError> {
        plain("submit key", s, 128, &['.', '_', ':', '-'])?;
        Ok(SubmitKey(s.to_string()))
    }
    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for SubmitKey {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        SubmitKey::new(&s)
    }
}
impl From<SubmitKey> for String {
    fn from(p: SubmitKey) -> String {
        p.0
    }
}

/// One checkout on one origin host: 32 hex characters derived from the host
/// name and the checkout path, so one checkout is always one remote copy (R-160).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CheckoutId(String);

impl CheckoutId {
    /// Derives the id from the origin host name and the checkout root.
    pub fn derive(origin_host: &str, root: &Path) -> CheckoutId {
        let mut h = Sha256::new();
        h.update(origin_host.as_bytes());
        h.update([0]);
        h.update(root.to_string_lossy().as_bytes());
        let d = h.finalize();
        CheckoutId(hex(d.get(..16).unwrap_or(&[])))
    }
    /// Parses 32 lowercase hex characters.
    pub fn parse(s: &str) -> Result<CheckoutId, IdError> {
        if s.len() == 32 && unhex(s).is_some() {
            Ok(CheckoutId(s.to_string()))
        } else {
            Err(IdError::BadHex {
                what: "checkout id",
                len: 32,
            })
        }
    }
    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for CheckoutId {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        CheckoutId::parse(&s)
    }
}
impl From<CheckoutId> for String {
    fn from(p: CheckoutId) -> String {
        p.0
    }
}
impl fmt::Display for CheckoutId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A SHA-256 content hash.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Hash([u8; 32]);

impl Hash {
    /// Hashes a byte slice.
    pub fn of(data: &[u8]) -> Hash {
        let mut h = Sha256::new();
        h.update(data);
        Hash::from_digest(h)
    }
    pub(crate) fn from_digest(h: Sha256) -> Hash {
        let d = h.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&d);
        Hash(out)
    }
    /// Parses 64 lowercase hex characters.
    pub fn parse(s: &str) -> Result<Hash, IdError> {
        let bad = IdError::BadHex {
            what: "hash",
            len: 64,
        };
        let v = unhex(s).filter(|v| v.len() == 32).ok_or(bad)?;
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        Ok(Hash(out))
    }
    /// The 64-character hex text.
    pub fn to_hex(self) -> String {
        hex(&self.0)
    }
}
impl TryFrom<String> for Hash {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        Hash::parse(&s)
    }
}
impl From<Hash> for String {
    fn from(h: Hash) -> String {
        h.to_hex()
    }
}
impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex().get(..12).unwrap_or(""))
    }
}
impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r152_keys_are_bounded_and_plain() {
        assert!(SubmitKey::new("host:job-12.a").is_ok());
        assert!(SubmitKey::new("").is_err());
        assert!(SubmitKey::new(&"a".repeat(129)).is_err());
        assert!(SubmitKey::new("a b").is_err());
        assert!(SubmitKey::new("../x").is_err());
    }

    #[test]
    fn peer_ids_are_safe_directory_names() {
        assert!(PeerId::new("abc123").is_ok());
        assert!(PeerId::new("..").is_err());
        assert!(PeerId::new("a/b").is_err());
        assert!(PeerId::new("").is_err());
    }

    #[test]
    fn r160_checkout_id_depends_on_host_and_path() {
        let a = CheckoutId::derive("laptop", Path::new("/w/a"));
        assert_eq!(a, CheckoutId::derive("laptop", Path::new("/w/a")));
        assert_ne!(a, CheckoutId::derive("mac", Path::new("/w/a")));
        assert_ne!(a, CheckoutId::derive("laptop", Path::new("/w/b")));
        assert!(CheckoutId::parse(a.as_str()).is_ok());
        assert!(CheckoutId::parse("../../etc").is_err());
    }

    #[test]
    fn hash_round_trips_and_refuses_junk() {
        let h = Hash::of(b"abc");
        assert_eq!(Hash::parse(&h.to_hex()), Ok(h));
        assert!(Hash::parse("zz").is_err());
        assert!(Hash::parse(&"A".repeat(64)).is_err());
    }
}
