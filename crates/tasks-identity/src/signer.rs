//! Acting as an identity without holding its secrets.

/// Everything a caller needs to sign as an identity. Public material and signatures only, so a
/// caller can take this and leave the keys wherever they are kept.
pub trait Signer: Send + Sync {
    /// The DID.
    fn did(&self) -> &str;
    /// An Ed25519 signature by the signing key.
    fn sign(&self, message: &[u8]) -> [u8; 64];
    /// The X25519 public key.
    fn encryption_public_key(&self) -> [u8; 32];
    /// The device (transport) public key.
    fn device_public_key(&self) -> [u8; 32];
}

impl<T: Signer + ?Sized> Signer for &T {
    fn did(&self) -> &str {
        (**self).did()
    }
    fn sign(&self, message: &[u8]) -> [u8; 64] {
        (**self).sign(message)
    }
    fn encryption_public_key(&self) -> [u8; 32] {
        (**self).encryption_public_key()
    }
    fn device_public_key(&self) -> [u8; 32] {
        (**self).device_public_key()
    }
}

impl<T: Signer + ?Sized> Signer for std::sync::Arc<T> {
    fn did(&self) -> &str {
        (**self).did()
    }
    fn sign(&self, message: &[u8]) -> [u8; 64] {
        (**self).sign(message)
    }
    fn encryption_public_key(&self) -> [u8; 32] {
        (**self).encryption_public_key()
    }
    fn device_public_key(&self) -> [u8; 32] {
        (**self).device_public_key()
    }
}
