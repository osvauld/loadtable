//! Characterizes the actual parent crates; this is not a remote-worker implementation.

use courier::proof::{self, Replay};
use identity::{generate, public_key_from_did, recover, verify};
use loro::{ExportMode, LoroDoc};
use storage::Store;
use vault::Vault;

#[test]
fn recovered_account_has_same_did_and_verifiable_signatures() {
    let (account, mnemonic) = generate();
    let recovered = recover(&mnemonic.to_string()).unwrap();
    assert_eq!(account.did(), recovered.did());
    let body = b"synthetic foundation characterization, not an execution grant";
    let key = public_key_from_did(account.did()).unwrap();
    assert!(verify(&key, body, &recovered.sign(body)));
    assert!(!verify(&key, b"modified", &recovered.sign(body)));
}

#[test]
fn another_account_cannot_sign_as_the_expected_did() {
    let (account, _) = generate();
    let (other, _) = generate();
    let key = public_key_from_did(account.did()).unwrap();
    assert!(!verify(&key, b"request", &other.sign(b"request")));
    assert!(public_key_from_did("did:key:invalid").is_none());
}

#[test]
fn parent_mnemonic_recovery_does_not_create_a_distinct_device() {
    let (account, mnemonic) = generate();
    let recovered = recover(&mnemonic.to_string()).unwrap();
    // A required characterization, NOT the desired multi-install transport behavior.
    assert_eq!(account.device_public_key(), recovered.device_public_key());
}

#[test]
fn account_encryption_round_trips_and_rejects_another_account() {
    let (account, _) = generate();
    let (other, _) = generate();
    let sealed =
        identity::encrypt_for(&account.encryption_public_key(), b"synthetic data").unwrap();
    assert_eq!(account.decrypt_sealed(&sealed).unwrap(), b"synthetic data");
    assert!(other.decrypt_sealed(&sealed).is_err());
}

#[test]
fn parent_request_proof_binds_body_caller_and_recipient() {
    let (caller, _) = generate();
    let (node, _) = generate();
    let (other, _) = generate();
    let body = b"synthetic exact request bytes";
    let p = proof::prove(&caller, node.did(), body, 1_001);
    assert!(proof::verify(&p, caller.did(), node.did(), body, 1_002).is_ok());
    assert!(proof::verify(&p, caller.did(), node.did(), b"changed", 1_002).is_err());
    assert!(proof::verify(&p, other.did(), node.did(), body, 1_002).is_err());
    assert!(proof::verify(&p, caller.did(), other.did(), body, 1_002).is_err());
}

#[test]
fn parent_request_proof_rejects_replay_restart_and_clock_skew() {
    let (caller, _) = generate();
    let (node, _) = generate();
    let body = b"bounded synthetic request";
    let p = proof::prove(&caller, node.did(), body, 1_001);
    let mut replay = Replay::new(1_000);
    proof::verify(&p, caller.did(), node.did(), body, 1_002).unwrap();
    replay.admit(&p, 1_002).unwrap();
    assert!(replay.admit(&p, 1_002).is_err());
    assert!(Replay::new(1_002).admit(&p, 1_003).is_err());
    assert!(proof::verify(&p, caller.did(), node.did(), body, 1_000).is_err());
    assert!(proof::verify(&p, caller.did(), node.did(), body, 1_002 + proof::WINDOW_MS).is_err());
}

#[test]
fn node_stores_are_independent_and_clones_share_one_handle() {
    let a = Store::open_in_memory().unwrap();
    let b = Store::open_in_memory().unwrap();
    a.put("compute/test", b"node-a").unwrap();
    b.put("compute/test", b"node-b").unwrap();
    assert_eq!(a.clone().get("compute/test").unwrap().unwrap(), b"node-a");
    assert_eq!(b.get("compute/test").unwrap().unwrap(), b"node-b");
    a.delete("compute/test").unwrap();
    assert!(a.get("compute/test").unwrap().is_none());
    assert!(b.get("compute/test").unwrap().is_some());
}

#[test]
fn node_store_reopens_without_copying_a_live_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic.redb");
    {
        let store = Store::open(&path).unwrap();
        store.put("compute/input", b"immutable test bytes").unwrap();
        store.put("unrelated", b"keep").unwrap();
    }
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        reopened.get("compute/input").unwrap().unwrap(),
        b"immutable test bytes"
    );
    assert_eq!(
        reopened.list_prefixed("compute/").unwrap(),
        ["compute/input"]
    );
}

#[test]
fn vault_entries_are_sealed_durable_and_locked_signing_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let did;
    {
        let mut vault = Vault::open(dir.path().to_owned()).unwrap();
        let (account_did, _) = vault
            .signup("synthetic compute", "test-only-passphrase")
            .unwrap();
        did = account_did;
        vault
            .put_entry("compute/test", b"synthetic private bytes")
            .unwrap();
        let raw = vault
            .store()
            .unwrap()
            .get("entry/compute/test")
            .unwrap()
            .unwrap();
        assert!(
            !raw.windows(b"synthetic private bytes".len())
                .any(|w| w == b"synthetic private bytes")
        );
        assert!(
            vault
                .with_signer(|s| s.sign(b"synthetic request"))
                .is_some()
        );
        vault.lock();
        assert!(
            vault
                .with_signer(|s| s.sign(b"synthetic request"))
                .is_none()
        );
        assert!(vault.get_entry("compute/test").is_err());
    }
    let mut reopened = Vault::open(dir.path().to_owned()).unwrap();
    reopened.login(&did, "test-only-passphrase").unwrap();
    assert_eq!(
        reopened.get_entry("compute/test").unwrap().unwrap(),
        b"synthetic private bytes"
    );
}

#[test]
fn two_distinct_loro_writers_exchange_actual_snapshots() {
    let a = LoroDoc::new();
    let b = LoroDoc::new();
    a.set_peer_id(1001).unwrap();
    b.set_peer_id(1002).unwrap();
    a.get_map("compute-view")
        .insert("node-a", "online")
        .unwrap();
    b.get_map("compute-view")
        .insert("node-b", "online")
        .unwrap();
    let a_snapshot = a.export(ExportMode::Snapshot).unwrap();
    let b_snapshot = b.export(ExportMode::Snapshot).unwrap();
    a.import(&b_snapshot).unwrap();
    b.import(&a_snapshot).unwrap();
    assert_eq!(a.get_deep_value(), b.get_deep_value());
    assert_ne!(a.peer_id(), b.peer_id());
}
