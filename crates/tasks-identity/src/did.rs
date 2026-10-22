//! `did:key:z` + base58btc(multicodec ed25519 || public key): self-authenticating, no lookup.

const MULTICODEC_ED25519: [u8; 2] = [0xed, 0x01];
const DID_PREFIX: &str = "did:key:z";

/// The DID of an Ed25519 signing public key.
pub fn did_from_public_key(signing_public: &[u8; 32]) -> String {
    let mut bytes = Vec::with_capacity(34);
    bytes.extend_from_slice(&MULTICODEC_ED25519);
    bytes.extend_from_slice(signing_public);
    format!("{DID_PREFIX}{}", bs58::encode(bytes).into_string())
}

/// The signing public key a DID names, or `None` when it is not a valid ed25519 `did:key`.
pub fn public_key_from_did(did: &str) -> Option<[u8; 32]> {
    let encoded = did.strip_prefix(DID_PREFIX)?;
    let bytes = bs58::decode(encoded).into_vec().ok()?;
    if bytes.len() != 34 || bytes.get(..2)? != MULTICODEC_ED25519 {
        return None;
    }
    bytes.get(2..)?.try_into().ok()
}
