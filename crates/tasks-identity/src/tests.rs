use crate::keystore::KEYSTORE_FILE;
use crate::*;
use std::os::unix::fs::PermissionsExt;

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

// R-232
#[test]
fn r232_the_three_keys_are_distinct_and_the_derivation_is_deterministic() {
    let (a, entropy) = generate().unwrap();
    let b = Identity::from_entropy(&entropy).unwrap();
    assert_eq!(a.did(), b.did());
    assert_eq!(a.device_public_key(), b.device_public_key());
    assert_ne!(a.signing_public_key(), a.device_public_key());
    assert!(a.encryption_secret_is_distinct());
    let (c, _) = generate().unwrap();
    assert_ne!(a.did(), c.did());
}

// R-232
#[test]
fn r232_signatures_verify_only_for_their_message_and_key() {
    let (a, _) = generate().unwrap();
    let (b, _) = generate().unwrap();
    let sig = a.sign(b"m");
    assert!(verify(&a.signing_public_key(), b"m", &sig));
    assert!(!verify(&a.signing_public_key(), b"n", &sig));
    assert!(!verify(&b.signing_public_key(), b"m", &sig));
    let dsig = a.sign_with_device(b"m");
    assert!(verify(&a.device_public_key(), b"m", &dsig));
    assert!(!verify(&a.signing_public_key(), b"m", &dsig));
}

// R-233
#[test]
fn r233_the_keystore_is_created_once_mode_0600_and_reopens_to_the_same_identity() {
    let d = dir();
    let first = Keystore::open_or_create(d.path()).unwrap();
    assert!(first.created());
    let mode = std::fs::metadata(first.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let second = Keystore::open_or_create(d.path()).unwrap();
    assert!(!second.created());
    assert_eq!(first.identity().did(), second.identity().did());
    let info = read_public(d.path()).unwrap();
    assert_eq!(info.did, first.identity().did());
    assert_eq!(
        info.node_id,
        hex::encode(&first.identity().device_public_key())
    );
}

// R-233
#[test]
fn r233_a_keystore_other_users_can_read_is_refused() {
    let d = dir();
    Keystore::open_or_create(d.path()).unwrap();
    let p = Keystore::path_in(d.path());
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        Keystore::load(&p),
        Err(IdentityError::LooseMode { mode: 0o644, .. })
    ));
}

// R-233
#[test]
fn r233_edited_public_parts_a_newer_version_and_junk_are_refused() {
    let d = dir();
    let ks = Keystore::open_or_create(d.path()).unwrap();
    let p = Keystore::path_in(d.path());
    let good = std::fs::read_to_string(&p).unwrap();
    let write = |t: &str| {
        std::fs::write(&p, t).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    let did = ks.identity().did().to_string();
    write(&good.replace(&did, "did:key:z6MkOther"));
    assert!(matches!(
        Keystore::load(&p),
        Err(IdentityError::Corrupt { .. })
    ));
    write(&good.replace("\"version\": 1", "\"version\": 9"));
    assert!(matches!(
        Keystore::load(&p),
        Err(IdentityError::NewerVersion { found: 9, .. })
    ));
    write("not json");
    assert!(matches!(
        Keystore::load(&p),
        Err(IdentityError::Corrupt { .. })
    ));
    assert_eq!(p.file_name().unwrap(), KEYSTORE_FILE);
}

// R-233
#[test]
fn r233_secrets_never_appear_in_debug_or_errors() {
    let d = dir();
    let ks = Keystore::open_or_create(d.path()).unwrap();
    let text = std::fs::read_to_string(ks.path()).unwrap();
    let entropy = serde_json::from_str::<serde_json::Value>(&text).unwrap()["secret"]["entropy"]
        .as_str()
        .unwrap()
        .to_string();
    let shown = format!("{ks:?} {:?}", ks.identity());
    assert!(!shown.contains(&entropy));
    let phrase = ks.mnemonic().unwrap();
    assert!(!shown.contains(phrase.as_str()));
    let p = Keystore::path_in(d.path());
    std::fs::write(&p, text.replace(&entropy, &"0".repeat(63))).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    let err = Keystore::load(&p).unwrap_err().to_string();
    assert!(!err.contains(&entropy));
}

// R-234
#[test]
fn r234_the_backup_phrase_recovers_the_same_identity() {
    let d = dir();
    let ks = Keystore::open_or_create(d.path()).unwrap();
    let phrase = ks.mnemonic().unwrap();
    assert_eq!(phrase.split(' ').count(), 24);
    let back = Identity::recover(&phrase).unwrap();
    assert_eq!(back.did(), ks.identity().did());
    assert_eq!(back.device_public_key(), ks.identity().device_public_key());
}

// R-234
#[test]
fn r234_random_bytes_differ() {
    assert_ne!(random_bytes::<16>().unwrap(), random_bytes::<16>().unwrap());
}
