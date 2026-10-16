//! Handover gate 3 characterization: per-installation transport keys, account attestation of
//! iroh endpoints, a LAN-configured (relay-free, lookup-free) iroh 1.3 endpoint pair on loopback,
//! and challenge-based request freshness. The reusable logic lives in the experiment library
//! (`src/lib.rs`); these tests exercise it in-process. This is NOT a worker: no blob transfer,
//! no command execution, no second machine. Loopback success is not proof of LAN confinement.
//!
//! Synthetic accounts and passphrases only; nothing here touches a live vault or network.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use courier::CourierError;
use courier::token::{self, KeyBinding, Scope, Token};
use identity::Signer;
use iroh::{Endpoint, EndpointAddr, PublicKey, SecretKey};
use task_manager_osvauld_foundation::{
    self as lan, ALPN, Exchange, Mode, Outcome, Policy, Reject, TRANSPORT_ENTRY, endpoint_binding,
    hex, request, response_is_fresh, serve_one,
};
use tokio::time::timeout;
use vault::Vault;

const PASS: &str = "test-only-passphrase";
const STEP: Duration = Duration::from_secs(20);
/// Synthetic receiver wall clock (seconds) — attestation expiry is the only clock-read check.
const NOW: u64 = 1_900_000_000;
const DAY: u64 = 86_400;

// Thin test wrappers over the library: unwrap, synthetic clock, loopback-only binding.

fn load_or_create_transport_key(vault: &Vault) -> SecretKey {
    lan::load_or_create_transport_key(vault).unwrap()
}

fn restore_installation(dir: &Path, phrase: &str) -> (Vault, String) {
    lan::restore_installation(dir, phrase, PASS).unwrap()
}

fn device_key(vault: &Vault) -> [u8; 32] {
    lan::device_key(vault).expect("unlocked")
}

fn attest_endpoint(
    account: &(impl Signer + ?Sized),
    endpoint: &PublicKey,
    iat: u64,
    exp: u64,
) -> Token {
    lan::attest_endpoint(account, endpoint, iat, exp).unwrap()
}

fn policy(account_did: &str) -> Policy {
    Policy::new(account_did, NOW)
}

async fn lan_endpoint(secret: SecretKey, alpns: Vec<Vec<u8>>) -> Endpoint {
    lan::lan_endpoint(secret, "127.0.0.1:0".parse().unwrap(), alpns)
        .await
        .unwrap()
}

fn assert_lan_only(ep: &Endpoint) {
    let report = lan::LanReport::of(ep).unwrap();
    assert!(report.is_relay_and_lookup_free(), "{report:?}");
    assert!(!report.bound_sockets.is_empty());
    assert!(
        report.bound_sockets.iter().all(|s| s.ip().is_loopback()),
        "{report:?}"
    );
    assert!(
        report
            .advertised_ip_addrs
            .iter()
            .all(|s| s.ip().is_loopback()),
        "{report:?}"
    );
}

fn direct_addr(ep: &Endpoint) -> EndpointAddr {
    lan::direct_addr(ep).unwrap()
}

/// Runs one receiver accept against one requester attempt, bounded.
async fn round(
    server: &Endpoint,
    policy: &Policy,
    handled: &AtomicUsize,
    client: &Endpoint,
    attestation: Option<Token>,
    body: &[u8],
    mode: Mode,
) -> (Outcome, Result<Exchange, String>) {
    let (served, requested) = timeout(STEP, async {
        tokio::join!(
            serve_one(server, policy, handled),
            request(client, direct_addr(server), attestation, body, mode)
        )
    })
    .await
    .expect("bounded round");
    (served.expect("receiver io"), requested)
}

// =============================================================================================
// Tests

#[test]
fn installations_from_one_mnemonic_share_device_key_but_not_transport_key() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mut vault_a = Vault::open(dir_a.path().to_owned()).unwrap();
    let (did, mnemonic) = vault_a.signup("synthetic install A", PASS).unwrap();
    let (vault_b, did_b) = restore_installation(dir_b.path(), &mnemonic.to_string());

    // Same account, and the parent's derived device key is the same on both installs.
    assert_eq!(did, did_b);
    assert_eq!(device_key(&vault_a), device_key(&vault_b));

    // Transport keys are generated per installation: distinct, and not the device key.
    let key_a = load_or_create_transport_key(&vault_a);
    let key_b = load_or_create_transport_key(&vault_b);
    assert_ne!(key_a.public(), key_b.public());
    assert_ne!(key_a.public().as_bytes(), &device_key(&vault_a));
    assert_ne!(key_b.public().as_bytes(), &device_key(&vault_b));

    // Sealed at rest: the raw store value does not contain the secret bytes.
    let raw_key = format!("entry/{TRANSPORT_ENTRY}");
    let raw = vault_a.store().unwrap().get(&raw_key).unwrap().unwrap();
    let secret = key_a.to_bytes();
    assert!(!raw.windows(32).any(|w| w == secret));
    assert_eq!(
        vault_a.list_entries("task-manager/").unwrap(),
        [TRANSPORT_ENTRY]
    );

    // Characterized limitation: entries are sealed to the ACCOUNT encryption key, so any
    // installation holding the same account secrets can open another installation's copied
    // sealed bytes. Sealing protects against non-holders, not against a hostile linked device.
    let copied = "task-manager/copied-from-a";
    vault_b
        .store()
        .unwrap()
        .put(&format!("entry/{copied}"), &raw)
        .unwrap();
    assert_eq!(vault_b.get_entry(copied).unwrap().unwrap(), secret);

    // Locked: no read, no signing. Reopen + login recovers the same key, not a new one.
    vault_a.lock();
    assert!(vault_a.get_entry(TRANSPORT_ENTRY).is_err());
    assert!(vault_a.with_signer(|s| s.did().to_owned()).is_none());
    drop(raw);
    drop(vault_a);
    let mut reopened = Vault::open(dir_a.path().to_owned()).unwrap();
    assert!(
        reopened.get_entry(TRANSPORT_ENTRY).is_err(),
        "locked after open"
    );
    reopened.login(&did, PASS).unwrap();
    assert_eq!(
        load_or_create_transport_key(&reopened).public(),
        key_a.public()
    );
    assert_eq!(
        reopened.list_entries("task-manager/").unwrap(),
        [TRANSPORT_ENTRY]
    );
}

#[test]
fn endpoint_attestation_reuses_courier_claimant_rooted_token() {
    let (account, _) = identity::generate();
    let (other, _) = identity::generate();
    let endpoint = SecretKey::generate().public();
    let stranger = SecretKey::generate().public();
    let mut policy = policy(account.did());
    policy.approve(&endpoint);
    policy.approve(&stranger);

    let att = attest_endpoint(&account, &endpoint, NOW, NOW + DAY);
    assert_eq!(policy.admit(&endpoint, Some(&att)), Ok(()));

    // The attestation carries no authority anywhere in the parent's capability table.
    let claims = att.claims().unwrap();
    assert!(courier::policy::platform_capabilities(&claims.role, &Scope::Node).is_empty());
    assert!(!claims.delegable);

    // Bound to exactly one endpoint id.
    assert_eq!(
        policy.admit(&stranger, Some(&att)),
        Err(Reject::EndpointMismatch)
    );
    assert_eq!(policy.admit(&endpoint, None), Err(Reject::Unattested));

    // Another account cannot attest into this account.
    let foreign = attest_endpoint(&other, &endpoint, NOW, NOW + DAY);
    assert_eq!(
        policy.admit(&endpoint, Some(&foreign)),
        Err(Reject::Attestation(CourierError::NodeMismatch))
    );

    // A courier-style attestation of the parent device key is not an endpoint binding.
    let device_style = token::attest(
        &account,
        account.did(),
        KeyBinding {
            encryption: hex(&account.encryption_public_key()),
            device: endpoint.to_z32(),
        },
        NOW,
        NOW + DAY,
    )
    .unwrap();
    assert_eq!(
        policy.admit(&endpoint, Some(&device_style)),
        Err(Reject::EndpointMismatch)
    );

    // An attestation shown to someone else (e.g. a courier node) is the wrong holder.
    let node_bound = token::attest(
        &account,
        other.did(),
        KeyBinding {
            encryption: hex(&account.encryption_public_key()),
            device: endpoint_binding(&endpoint),
        },
        NOW,
        NOW + DAY,
    )
    .unwrap();
    assert_eq!(
        policy.admit(&endpoint, Some(&node_bound)),
        Err(Reject::Attestation(CourierError::WrongHolder))
    );

    // A delegable grant rooted at the account is not an attestation.
    let grant = token::issue_root(
        &account,
        account.did(),
        "owner",
        Scope::Node,
        true,
        NOW,
        NOW + DAY,
    )
    .unwrap();
    assert_eq!(
        policy.admit(&endpoint, Some(&grant)),
        Err(Reject::NotSingleLink)
    );

    // Expiry is checked against the receiver's clock.
    let expired = attest_endpoint(&account, &endpoint, NOW - 2 * DAY, NOW);
    assert_eq!(
        policy.admit(&endpoint, Some(&expired)),
        Err(Reject::Attestation(CourierError::Expired))
    );

    // Revocation by token id, without touching the token itself.
    policy.revoked.insert(att.id());
    assert_eq!(
        policy.admit(&endpoint, Some(&att)),
        Err(Reject::Attestation(CourierError::Revoked))
    );
    let reissued = attest_endpoint(&account, &endpoint, NOW, NOW + DAY);
    assert_eq!(policy.admit(&endpoint, Some(&reissued)), Ok(()));

    // Valid membership is still not permission: unapproved endpoints are refused.
    let unapproved = SecretKey::generate().public();
    let member = attest_endpoint(&account, &unapproved, NOW, NOW + DAY);
    assert_eq!(
        policy.admit(&unapproved, Some(&member)),
        Err(Reject::NotAllowlisted)
    );
}

#[tokio::test]
async fn lan_endpoint_has_no_relay_lookup_or_public_fallback() {
    let server = lan_endpoint(SecretKey::generate(), vec![ALPN.to_vec()]).await;
    let client = lan_endpoint(SecretKey::generate(), vec![]).await;
    assert_lan_only(&server);
    assert_lan_only(&client);
    // The library refuses an unspecified (all-interfaces) bind outright.
    let any = lan::lan_endpoint(SecretKey::generate(), "0.0.0.0:0".parse().unwrap(), vec![]).await;
    assert!(any.is_err());

    // An endpoint id without a direct address cannot be resolved: no lookup, no relay.
    let no_addr = timeout(STEP, client.connect(EndpointAddr::new(server.id()), ALPN))
        .await
        .expect("fails promptly instead of searching");
    assert!(no_addr.is_err());

    // A different protocol is refused by the receiver's ALPN.
    let handled = AtomicUsize::new(0);
    let policy = policy("did:key:unused");
    let (served, wrong) = timeout(STEP, async {
        tokio::join!(
            serve_one(&server, &policy, &handled),
            client.connect(direct_addr(&server), b"osvauld/voice/0")
        )
    })
    .await
    .expect("bounded");
    assert!(wrong.is_err());
    assert!(served.is_err());
    assert_eq!(handled.load(Ordering::SeqCst), 0);

    assert_lan_only(&server);
    server.close().await;
    client.close().await;
}

#[tokio::test]
async fn receiver_admits_only_attested_allowlisted_fresh_requests_over_loopback() {
    // Account and two installations: the worker (receiver) and a requester.
    let worker_dir = tempfile::tempdir().unwrap();
    let requester_dir = tempfile::tempdir().unwrap();
    let mut worker_vault = Vault::open(worker_dir.path().to_owned()).unwrap();
    let (did, mnemonic) = worker_vault.signup("synthetic worker", PASS).unwrap();
    let (requester_vault, _) = restore_installation(requester_dir.path(), &mnemonic.to_string());
    let (other_account, _) = identity::generate();

    let server = lan_endpoint(
        load_or_create_transport_key(&worker_vault),
        vec![ALPN.to_vec()],
    )
    .await;
    let requester = lan_endpoint(load_or_create_transport_key(&requester_vault), vec![]).await;
    assert_ne!(server.id(), requester.id());
    assert_lan_only(&server);
    assert_lan_only(&requester);

    let attest_with_vault = |vault: &Vault, ep: &PublicKey, iat: u64, exp: u64| {
        vault
            .with_signer(|s| attest_endpoint(s, ep, iat, exp))
            .expect("unlocked")
    };
    let att = attest_with_vault(&requester_vault, &requester.id(), NOW, NOW + 7 * DAY);

    let mut policy = policy(&did);
    policy.approve(&requester.id());
    let handled = AtomicUsize::new(0);
    let body = b"synthetic request bytes; never executed".as_slice();

    // Honest request is handled once; its response is bound to this session.
    let (out, first) = round(
        &server,
        &policy,
        &handled,
        &requester,
        Some(att.clone()),
        body,
        Mode::Honest,
    )
    .await;
    assert!(matches!(out, Outcome::Handled), "{out:?}");
    let first = first.unwrap();
    assert!(response_is_fresh(
        &first.response,
        &first.challenge,
        &server.id(),
        &requester.id(),
        body
    ));
    assert_eq!(handled.load(Ordering::SeqCst), 1);

    // Second session: a new challenge; the old response does not verify against it.
    let (out, second) = round(
        &server,
        &policy,
        &handled,
        &requester,
        Some(att.clone()),
        body,
        Mode::Honest,
    )
    .await;
    assert!(matches!(out, Outcome::Handled));
    let second = second.unwrap();
    assert_ne!(first.challenge, second.challenge);
    assert!(response_is_fresh(
        &second.response,
        &second.challenge,
        &server.id(),
        &requester.id(),
        body
    ));
    assert!(!response_is_fresh(
        &first.response,
        &second.challenge,
        &server.id(),
        &requester.id(),
        body
    ));
    assert!(!response_is_fresh(
        &second.response,
        &second.challenge,
        &server.id(),
        &requester.id(),
        b"changed"
    ));
    assert_eq!(handled.load(Ordering::SeqCst), 2);

    // Replaying an old signed request into a new session fails the new challenge.
    let (out, replay) = round(
        &server,
        &policy,
        &handled,
        &requester,
        Some(att.clone()),
        body,
        Mode::Replay(first.request.clone()),
    )
    .await;
    assert!(
        matches!(out, Outcome::Rejected(Reject::BadRequest)),
        "{out:?}"
    );
    assert!(replay.unwrap_err().contains("BadRequest"));

    // Signing anything other than the issued challenge is rejected.
    let (out, wrong) = round(
        &server,
        &policy,
        &handled,
        &requester,
        Some(att.clone()),
        body,
        Mode::SignWrongChallenge,
    )
    .await;
    assert!(
        matches!(out, Outcome::Rejected(Reject::BadRequest)),
        "{out:?}"
    );
    assert!(wrong.is_err());
    assert_eq!(handled.load(Ordering::SeqCst), 2);

    // Identity/authorization rejections, all before a challenge is issued.
    let cases: Vec<(&str, Option<Token>, Reject)> = vec![
        ("unattested", None, Reject::Unattested),
        (
            "wrong account DID",
            Some(attest_endpoint(
                &other_account,
                &requester.id(),
                NOW,
                NOW + DAY,
            )),
            Reject::Attestation(CourierError::NodeMismatch),
        ),
        (
            "attestation of another endpoint",
            Some(attest_with_vault(
                &requester_vault,
                &server.id(),
                NOW,
                NOW + DAY,
            )),
            Reject::EndpointMismatch,
        ),
        (
            "expired",
            Some(attest_with_vault(
                &requester_vault,
                &requester.id(),
                NOW - DAY,
                NOW,
            )),
            Reject::Attestation(CourierError::Expired),
        ),
    ];
    for (name, attestation, expected) in cases {
        let (out, client) = round(
            &server,
            &policy,
            &handled,
            &requester,
            attestation,
            body,
            Mode::Honest,
        )
        .await;
        match out {
            Outcome::Rejected(reject) => assert_eq!(reject, expected, "{name}"),
            other => panic!("{name}: {other:?}"),
        }
        assert!(client.is_err(), "{name}");
    }
    assert_eq!(handled.load(Ordering::SeqCst), 2);

    // Same account, valid attestation, but not on the receiver's local approval list.
    let unapproved = lan_endpoint(SecretKey::generate(), vec![]).await;
    let member = attest_with_vault(&requester_vault, &unapproved.id(), NOW, NOW + DAY);
    let (out, _) = round(
        &server,
        &policy,
        &handled,
        &unapproved,
        Some(member),
        body,
        Mode::Honest,
    )
    .await;
    assert!(
        matches!(out, Outcome::Rejected(Reject::NotAllowlisted)),
        "{out:?}"
    );

    // Skewed requester clock: issued "a day in the receiver's future" still verifies, because
    // freshness is the receiver's challenge; the parent's timestamp proof would refuse it.
    let skewed = attest_with_vault(&requester_vault, &requester.id(), NOW + DAY, NOW + 8 * DAY);
    let (out, ok) = round(
        &server,
        &policy,
        &handled,
        &requester,
        Some(skewed),
        body,
        Mode::Honest,
    )
    .await;
    assert!(matches!(out, Outcome::Handled), "{out:?}");
    assert!(ok.is_ok());
    let future_proof = requester_vault
        .with_signer(|s| courier::proof::prove(s, &did, body, (NOW + DAY) * 1000))
        .unwrap();
    assert_eq!(
        courier::proof::verify(&future_proof, &did, &did, body, NOW * 1000),
        Err(CourierError::StaleRequest)
    );
    assert_eq!(handled.load(Ordering::SeqCst), 3);

    // Revocation by attestation id takes effect on the next connection.
    policy.revoked.insert(att.id());
    let (out, _) = round(
        &server,
        &policy,
        &handled,
        &requester,
        Some(att),
        body,
        Mode::Honest,
    )
    .await;
    assert!(
        matches!(
            out,
            Outcome::Rejected(Reject::Attestation(CourierError::Revoked))
        ),
        "{out:?}"
    );
    assert_eq!(handled.load(Ordering::SeqCst), 3);

    // Locking the requester's vault removes its ability to attest further endpoints.
    let mut requester_vault = requester_vault;
    requester_vault.lock();
    assert!(
        requester_vault
            .with_signer(|s| s.did().to_owned())
            .is_none()
    );

    unapproved.close().await;
    requester.close().await;
    server.close().await;
}
