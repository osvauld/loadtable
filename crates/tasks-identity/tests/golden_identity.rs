//! Golden vectors copied from p2p_chat `crates/identity/tests/golden_identity.rs` (captured there
//! before its dependency upgrades). They pin the osvauld derivation contract: if our copy drifts
//! from the other two, these fail. Do NOT regenerate. The ECIES vector is not copied because this
//! crate does not seal to the encryption key yet.

use tasks_identity::{
    Identity, Signer, did_from_public_key, hex, public_key_from_did, verify,
};

const PHRASE: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const MSG: &[u8] = b"tinline golden message";

const DID: &str = "did:key:z6MknncVmedphgDSbQoYLyNaFRFdB2WtyjxcFdKV6zTfcYvs";
const SIGNING_PUB: &str = "7bd2a7bc71ceec29eec08b8b50e6688ea7f7092920a901321b9ce55b36a65268";
const ENC_PUB: &str = "d9aa528f5ab4f3c62d656343f6d902ecb1e9e71b39f68ee2947735c51254af67";
const DEVICE_PUB: &str = "0c0618436db1e118daed795c37432dec64af063dda6a7bb3f0711b6fc4000273";
const SIG: &str = "cab272ce948bb9f654c7ce275ef27fd3e23a9627bed6c20ee362e41cc8b1fb3e39428e72369a4ecda7ccc5d647db2ead7ff713493316ecf4b08d5c735998750a";

// R-232
#[test]
fn r232_phrase_to_keys_and_did_matches_the_shared_golden_vectors() {
    let id = Identity::recover(PHRASE).unwrap();
    assert_eq!(id.did(), DID);
    assert_eq!(hex::encode(&id.signing_public_key()), SIGNING_PUB);
    assert_eq!(hex::encode(&id.encryption_public_key()), ENC_PUB);
    assert_eq!(hex::encode(&id.device_public_key()), DEVICE_PUB);
    // Ed25519 signatures are deterministic.
    assert_eq!(hex::encode(&id.sign(MSG)), SIG);
    let sig = hex::decode::<64>(SIG).unwrap();
    assert!(verify(&id.signing_public_key(), MSG, &sig));
}

// R-232
#[test]
fn r232_did_encoding_matches_the_shared_vectors() {
    assert!(DID.starts_with("did:key:z6Mk"));
    let public = hex::decode::<32>(SIGNING_PUB).unwrap();
    assert_eq!(did_from_public_key(&public), DID);
    assert_eq!(public_key_from_did(DID), Some(public));
    assert_eq!(public_key_from_did("did:key:z"), None);
    assert_eq!(public_key_from_did("did:web:example"), None);
    assert_eq!(public_key_from_did(&format!("{DID}1")), None);
}

// R-232
#[test]
fn r232_phrase_checks_are_unchanged() {
    assert!(Identity::recover("abandon abandon").is_err());
    assert!(
        Identity::recover(&PHRASE.replace("about", "abandon")).is_err(),
        "bad checksum"
    );
}
