//! One identity: three keys derived from one BIP39 seed (the osvauld contract).

use crate::did;
use crate::error::IdentityError;
use crate::signer::Signer;
use bip39::Mnemonic;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use sha2::Sha256;
use std::sync::Arc;
use zeroize::{ZeroizeOnDrop, Zeroizing};

// Domain-separated HKDF contexts: a forever-contract shared with osvauld2 and p2p_chat.
// Changing one changes every key (and DID) derived from a seed.
const CTX_SIGNING: &[u8] = b"osv/identity/signing/v1";
const CTX_ENCRYPTION: &[u8] = b"osv/identity/encryption/v1";
const CTX_DEVICE: &[u8] = b"osv/identity/device/v1";

#[derive(ZeroizeOnDrop)]
struct Inner {
    signing_secret: [u8; 32],
    encryption_secret: [u8; 32],
    device_secret: [u8; 32],
    #[zeroize(skip)]
    signing_public: [u8; 32],
    #[zeroize(skip)]
    encryption_public: [u8; 32],
    #[zeroize(skip)]
    device_public: [u8; 32],
    #[zeroize(skip)]
    did: String,
}

/// A machine identity. Cloning shares the keys; the last drop wipes them. `Debug` shows the DID
/// only.
#[derive(Clone)]
pub struct Identity {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Identity({})", self.inner.did)
    }
}

fn hkdf32(seed: &[u8; 64], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, seed);
    let mut okm = [0u8; 32];
    // 32 bytes is always a valid HKDF-SHA256 output length; on the impossible error the zero
    // key would fail every golden test rather than sign anything.
    let _ = hk.expand(info, &mut okm);
    okm
}

/// The ed25519 public key of a secret.
pub(crate) fn ed_public(secret: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(secret).verifying_key().to_bytes()
}

fn x_public(secret: &[u8; 32]) -> [u8; 32] {
    x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*secret)).to_bytes()
}

impl Identity {
    /// Derives the identity of a mnemonic (empty BIP39 passphrase).
    pub fn from_mnemonic(mnemonic: &Mnemonic) -> Identity {
        // The BIP39 seed is the root of every key; wiped once the three keys are derived.
        let seed = Zeroizing::new(mnemonic.to_seed(""));
        Identity::from_secret_keys(
            hkdf32(&seed, CTX_SIGNING),
            hkdf32(&seed, CTX_ENCRYPTION),
            hkdf32(&seed, CTX_DEVICE),
        )
    }

    /// Derives the identity of 32 bytes of entropy (the 24-word phrase they encode).
    pub fn from_entropy(entropy: &[u8; 32]) -> Result<Identity, IdentityError> {
        let m = Mnemonic::from_entropy(entropy).map_err(|_| IdentityError::BadPhrase)?;
        Ok(Identity::from_mnemonic(&m))
    }

    /// Recovers an identity from its backup phrase.
    pub fn recover(phrase: &str) -> Result<Identity, IdentityError> {
        let m = Mnemonic::parse(phrase).map_err(|_| IdentityError::BadPhrase)?;
        Ok(Identity::from_mnemonic(&m))
    }

    fn from_secret_keys(signing: [u8; 32], encryption: [u8; 32], device: [u8; 32]) -> Identity {
        let signing_public = ed_public(&signing);
        Identity {
            inner: Arc::new(Inner {
                signing_public,
                encryption_public: x_public(&encryption),
                device_public: ed_public(&device),
                did: did::did_from_public_key(&signing_public),
                signing_secret: signing,
                encryption_secret: encryption,
                device_secret: device,
            }),
        }
    }

    /// The signing public key.
    pub fn signing_public_key(&self) -> [u8; 32] {
        self.inner.signing_public
    }

    /// The device secret key, for the transport endpoint only (iroh's node key). Wiped on drop of
    /// the returned value; never log it.
    pub fn device_secret(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.inner.device_secret)
    }

    /// Signs with the device key (proof that the holder of the node key is this machine).
    pub fn sign_with_device(&self, message: &[u8]) -> [u8; 64] {
        SigningKey::from_bytes(&self.inner.device_secret).sign_bytes(message)
    }

    #[cfg(test)]
    pub(crate) fn encryption_secret_is_distinct(&self) -> bool {
        self.inner.encryption_secret != self.inner.signing_secret
            && self.inner.encryption_secret != self.inner.device_secret
    }
}

trait SignBytes {
    fn sign_bytes(&self, message: &[u8]) -> [u8; 64];
}

impl SignBytes for SigningKey {
    fn sign_bytes(&self, message: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer as _;
        self.sign(message).to_bytes()
    }
}

impl Signer for Identity {
    fn did(&self) -> &str {
        &self.inner.did
    }
    fn sign(&self, message: &[u8]) -> [u8; 64] {
        SigningKey::from_bytes(&self.inner.signing_secret).sign_bytes(message)
    }
    fn encryption_public_key(&self) -> [u8; 32] {
        self.inner.encryption_public
    }
    fn device_public_key(&self) -> [u8; 32] {
        self.inner.device_public
    }
}

/// Strict Ed25519 verification (rejects small-order keys and malleable signatures).
pub fn verify(public: &[u8; 32], message: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public) else {
        return false;
    };
    key.verify_strict(message, &Signature::from_bytes(sig))
        .is_ok()
}

/// Fresh random bytes from the operating system.
pub fn random_bytes<const N: usize>() -> Result<[u8; N], IdentityError> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|_| IdentityError::NoRandomness)?;
    Ok(b)
}

/// A new identity and the entropy behind its backup phrase.
pub fn generate() -> Result<(Identity, Zeroizing<[u8; 32]>), IdentityError> {
    let entropy = Zeroizing::new(random_bytes::<32>()?);
    Ok((Identity::from_entropy(&entropy)?, entropy))
}
