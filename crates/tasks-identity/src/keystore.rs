//! The keystore file: one versioned JSON document in the daemon's state dir.
//!
//! Public parts (DID and the three public keys) are in the clear, so `identity show` and a
//! tamper check need no secret. The only secret is the 32 bytes of entropy behind the backup
//! phrase, stored as hex in a file created mode 0600 and refused on load if group or other can
//! read it. Every key is derived from that entropy, so the public parts are re-derived on load
//! and must match what the file says.

use crate::error::IdentityError;
use crate::hex;
use crate::identity::{Identity, generate};
use crate::signer::Signer;
use bip39::Mnemonic;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// The keystore format this build reads and writes.
pub const KEYSTORE_VERSION: u32 = 1;
/// The file name inside the state dir.
pub const KEYSTORE_FILE: &str = "identity.json";

#[derive(Serialize, Deserialize)]
struct Public {
    did: String,
    signing: String,
    encryption: String,
    device: String,
}

#[derive(Serialize, Deserialize)]
struct Secret {
    entropy: String,
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    public: Public,
    secret: Secret,
}

/// An opened keystore: the identity plus the entropy behind its backup phrase.
pub struct Keystore {
    path: PathBuf,
    identity: Identity,
    entropy: Zeroizing<[u8; 32]>,
    created: bool,
}

impl std::fmt::Debug for Keystore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Keystore({}, {})",
            self.path.display(),
            self.identity.did()
        )
    }
}

fn io(path: &Path, e: impl std::fmt::Display) -> IdentityError {
    IdentityError::Io {
        path: path.display().to_string(),
        detail: e.to_string(),
    }
}

fn corrupt(path: &Path, detail: &str) -> IdentityError {
    IdentityError::Corrupt {
        path: path.display().to_string(),
        detail: detail.to_string(),
    }
}

impl Keystore {
    /// The keystore path inside a state dir.
    pub fn path_in(state_dir: &Path) -> PathBuf {
        state_dir.join(KEYSTORE_FILE)
    }

    /// Opens the keystore in `state_dir`, creating a fresh identity on first use.
    pub fn open_or_create(state_dir: &Path) -> Result<Keystore, IdentityError> {
        let path = Keystore::path_in(state_dir);
        match Keystore::load(&path) {
            Ok(k) => Ok(k),
            Err(IdentityError::Io { .. }) if !path.exists() => {
                let (identity, entropy) = generate()?;
                Keystore::write_new(&path, identity, entropy)
            }
            Err(e) => Err(e),
        }
    }

    /// Reads and checks an existing keystore.
    pub fn load(path: &Path) -> Result<Keystore, IdentityError> {
        let meta = std::fs::metadata(path).map_err(|e| io(path, e))?;
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(IdentityError::LooseMode {
                path: path.display().to_string(),
                mode,
            });
        }
        let text = Zeroizing::new(std::fs::read_to_string(path).map_err(|e| io(path, e))?);
        let file: File =
            serde_json::from_str(&text).map_err(|_| corrupt(path, "not a keystore document"))?;
        if file.version > KEYSTORE_VERSION {
            return Err(IdentityError::NewerVersion {
                path: path.display().to_string(),
                found: file.version,
                supported: KEYSTORE_VERSION,
            });
        }
        let entropy = Zeroizing::new(
            hex::decode::<32>(&file.secret.entropy).ok_or_else(|| corrupt(path, "bad secret"))?,
        );
        let identity = Identity::from_entropy(&entropy).map_err(|_| corrupt(path, "bad secret"))?;
        let derived = public_of(&identity);
        if derived.did != file.public.did
            || derived.signing != file.public.signing
            || derived.encryption != file.public.encryption
            || derived.device != file.public.device
        {
            return Err(corrupt(path, "public keys do not match the secret"));
        }
        Ok(Keystore {
            path: path.to_path_buf(),
            identity,
            entropy,
            created: false,
        })
    }

    fn write_new(
        path: &Path,
        identity: Identity,
        entropy: Zeroizing<[u8; 32]>,
    ) -> Result<Keystore, IdentityError> {
        let file = File {
            version: KEYSTORE_VERSION,
            public: public_of(&identity),
            secret: Secret {
                entropy: hex::encode(&*entropy),
            },
        };
        let text = Zeroizing::new(serde_json::to_string_pretty(&file).map_err(|e| io(path, e))?);
        let tmp = path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| io(&tmp, e))?;
        f.write_all(text.as_bytes())
            .and_then(|()| f.write_all(b"\n"))
            .and_then(|()| f.sync_all())
            .map_err(|e| io(&tmp, e))?;
        // hard_link fails if another process created the keystore meanwhile, so two daemons
        // starting at once end up with one identity.
        let linked = std::fs::hard_link(&tmp, path);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => Ok(Keystore {
                path: path.to_path_buf(),
                identity,
                entropy,
                created: true,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Keystore::load(path),
            Err(e) => Err(io(path, e)),
        }
    }

    /// The identity.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// True when this open created the keystore.
    pub fn created(&self) -> bool {
        self.created
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The backup phrase (24 words). A secret: print it only to a person at a terminal.
    pub fn mnemonic(&self) -> Result<Zeroizing<String>, IdentityError> {
        let m = Mnemonic::from_entropy(&*self.entropy).map_err(|_| IdentityError::BadPhrase)?;
        Ok(Zeroizing::new(m.to_string()))
    }
}

fn public_of(id: &Identity) -> Public {
    Public {
        did: id.did().to_string(),
        signing: hex::encode(&id.signing_public_key()),
        encryption: hex::encode(&id.encryption_public_key()),
        device: hex::encode(&id.device_public_key()),
    }
}

/// What can be shown without any secret: read from the file's clear parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicInfo {
    /// The DID.
    pub did: String,
    /// The device public key, which is also the node id (lowercase hex).
    pub node_id: String,
}

/// Reads the public parts of a keystore without touching its secret.
pub fn read_public(state_dir: &Path) -> Result<PublicInfo, IdentityError> {
    let path = Keystore::path_in(state_dir);
    let text = std::fs::read_to_string(&path).map_err(|e| io(&path, e))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| corrupt(&path, "not a keystore document"))?;
    let field = |k: &str| {
        v.get("public")
            .and_then(|p| p.get(k))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| corrupt(&path, "missing public part"))
    };
    Ok(PublicInfo {
        did: field("did")?,
        node_id: field("device")?,
    })
}
