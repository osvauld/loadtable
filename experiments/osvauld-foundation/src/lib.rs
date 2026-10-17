//! Experimental LAN identity/transport helpers (handover gates 3–4) and phase-2 content-addressed
//! source transfer ([`source`]). NOT a worker: no command execution, no persistence of
//! revocations/allowlists. Reuses parent `identity`/`vault`/`courier` APIs unchanged and upstream
//! iroh 1.3 configured without `presets::N0`, relays or address lookup.

pub mod prune;
pub mod source;
pub mod sync;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use courier::CourierError;
use courier::token::{self, KeyBinding, Token};
use identity::Signer;
use iroh::endpoint::{
    Connection, NetReportConfig, PortmapperConfig, RecvStream, SendStream, VarInt, presets,
};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMode, SecretKey, Signature, Watcher};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use storage::Store;
use tokio::time::timeout;
use vault::Vault;

/// Task-specific protocol; never the parent's `osvauld/voice/0`.
pub const ALPN: &[u8] = b"task-manager/worker/0";
/// Vault entry name of the installation's random iroh secret key.
pub const TRANSPORT_ENTRY: &str = "task-manager/transport/v0/secret-key";
/// Distinguishes an endpoint binding from courier's device-key bindings inside `KeyBinding`.
pub const ENDPOINT_BINDING_PREFIX: &str = "task-manager/endpoint/v0:";
const REQUEST_DOMAIN: &[u8] = b"task-manager/worker/request/v0\0";
const RESPONSE_DOMAIN: &[u8] = b"task-manager/worker/response/v0\0";
const MAX_FRAME: usize = 64 * 1024;
/// How long a receiver waits for the requester to close after the final frame.
const CLOSE_GRACE: Duration = Duration::from_secs(10);

/// SYNTHETIC, PUBLICLY KNOWN BIP39 test vector. Anyone can derive this account's keys, so the
/// sealed vault entries of an installation created from it protect nothing. It exists only so
/// two test machines share one DID without a real account-linking flow. Never use real data.
pub const SYNTHETIC_TEST_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon \
     abandon abandon abandon abandon abandon about";
/// Synthetic passphrase for the synthetic test account's local keystore.
pub const SYNTHETIC_PASSPHRASE: &str = "synthetic-lan-peer-test-passphrase";

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Installation: parent vault holds the account; a random transport key lives in a sealed entry.

/// Loads this installation's transport key, or generates and seals a new random one. Never
/// derived from the mnemonic: the parent's derived device key is identical on every install.
pub fn load_or_create_transport_key(vault: &Vault) -> Result<SecretKey, String> {
    if let Some(bytes) = vault
        .get_entry(TRANSPORT_ENTRY)
        .map_err(|e| e.to_string())?
    {
        let bytes: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| "stored transport key is not 32 bytes".to_string())?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    vault
        .put_entry(TRANSPORT_ENTRY, &key.to_bytes())
        .map_err(|e| e.to_string())?;
    Ok(key)
}

fn account_file(dir: &Path, did: &str) -> Result<PathBuf, String> {
    let multibase = did.strip_prefix("did:key:").ok_or("not a did:key")?;
    Ok(dir.join(format!("{multibase}.redb")))
}

/// STAND-IN for a missing parent API: `vault` has signup/login but no import-from-mnemonic.
/// This reproduces the vault's private on-disk layout (`<multibase>.redb`, `identity/keystore`)
/// using only public `identity`/`storage` calls, then logs in through the real vault. A real
/// adapter needs a reviewed parent restore API instead of depending on this layout.
pub fn restore_installation(
    dir: &Path,
    phrase: &str,
    passphrase: &str,
) -> Result<(Vault, String), String> {
    let account = identity::recover(phrase).map_err(|e| e.to_string())?;
    let did = account.did().to_owned();
    let keystore = identity::seal(&account, passphrase).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    {
        let store = Store::open(account_file(dir, &did)?).map_err(|e| e.to_string())?;
        store
            .put("identity/keystore", &keystore.to_bytes())
            .map_err(|e| e.to_string())?;
        store
            .put("identity/label", b"synthetic restored install")
            .map_err(|e| e.to_string())?;
    }
    let mut vault = Vault::open(dir.to_owned()).map_err(|e| e.to_string())?;
    vault.login(&did, passphrase).map_err(|e| e.to_string())?;
    Ok((vault, did))
}

/// Opens (or on first use creates) the SYNTHETIC test account installation under `dir/vault`.
pub fn open_synthetic_installation(dir: &Path) -> Result<(Vault, String), String> {
    let vault_dir = dir.join("vault");
    let did = identity::recover(SYNTHETIC_TEST_MNEMONIC)
        .map_err(|e| e.to_string())?
        .did()
        .to_owned();
    if account_file(&vault_dir, &did)?.exists() {
        let mut vault = Vault::open(vault_dir).map_err(|e| e.to_string())?;
        vault
            .login(&did, SYNTHETIC_PASSPHRASE)
            .map_err(|e| e.to_string())?;
        return Ok((vault, did));
    }
    restore_installation(&vault_dir, SYNTHETIC_TEST_MNEMONIC, SYNTHETIC_PASSPHRASE)
}

pub fn device_key(vault: &Vault) -> Option<[u8; 32]> {
    vault.with_signer(|s| s.device_public_key())
}

// ---------------------------------------------------------------------------------------------
// Account attestation: the parent's claimant-rooted courier attestation, bound to an endpoint id.

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn endpoint_binding(endpoint: &PublicKey) -> String {
    format!("{ENDPOINT_BINDING_PREFIX}{}", endpoint.to_z32())
}

/// The account (via any parent `Signer`, e.g. the vault's) states "this endpoint is one of my
/// installations". `aud` is the account itself: the statement is shown to the account's own
/// installations. Uses parent `token::attest` unchanged — role `relationship`, non-delegable,
/// no platform capability.
pub fn attest_endpoint(
    account: &(impl Signer + ?Sized),
    endpoint: &PublicKey,
    iat: u64,
    exp: u64,
) -> Result<Token, CourierError> {
    token::attest(
        account,
        account.did(),
        KeyBinding {
            encryption: hex(&account.encryption_public_key()),
            device: endpoint_binding(endpoint),
        },
        iat,
        exp,
    )
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reject {
    Unattested,
    NotSingleLink,
    Attestation(CourierError),
    EndpointMismatch,
    NotAllowlisted,
    BadRequest,
}

/// Local authorization state. `allow` is a separate explicit approval: account membership (a
/// valid attestation) never implies permission. In-memory only in this experiment.
pub struct Policy {
    pub account_did: String,
    pub now_s: u64,
    pub revoked: HashSet<[u8; 32]>,
    pub allow: HashSet<[u8; 32]>,
}

impl Policy {
    pub fn new(account_did: &str, now_s: u64) -> Self {
        Self {
            account_did: account_did.to_owned(),
            now_s,
            revoked: HashSet::new(),
            allow: HashSet::new(),
        }
    }

    pub fn approve(&mut self, endpoint: &PublicKey) {
        self.allow.insert(*endpoint.as_bytes());
    }

    /// Verifies an attestation for this policy's account and returns the endpoint it binds,
    /// WITHOUT consulting the allowlist.
    pub fn attested_endpoint(&self, attestation: &Token) -> Result<PublicKey, Reject> {
        let raw = attestation.claims().map_err(Reject::Attestation)?;
        if raw.prf.is_some() || raw.delegable {
            return Err(Reject::NotSingleLink);
        }
        let claims = token::verify_chain(
            attestation,
            &self.account_did,
            &self.account_did,
            self.now_s,
            &self.revoked,
        )
        .map_err(Reject::Attestation)?;
        let binds = claims.binds.ok_or(Reject::Unattested)?;
        let z32 = binds
            .device
            .strip_prefix(ENDPOINT_BINDING_PREFIX)
            .ok_or(Reject::EndpointMismatch)?;
        PublicKey::from_z32(z32).map_err(|_| Reject::EndpointMismatch)
    }

    /// Explicit out-of-band approval of an attested endpoint (e.g. an exchanged file).
    pub fn approve_attestation(&mut self, attestation: &Token) -> Result<PublicKey, Reject> {
        let endpoint = self.attested_endpoint(attestation)?;
        self.approve(&endpoint);
        Ok(endpoint)
    }

    /// Runs on the TLS-authenticated remote endpoint id before any request is read.
    pub fn admit(&self, remote: &PublicKey, attestation: Option<&Token>) -> Result<(), Reject> {
        let attestation = attestation.ok_or(Reject::Unattested)?;
        if self.attested_endpoint(attestation)? != *remote {
            return Err(Reject::EndpointMismatch);
        }
        if !self.allow.contains(remote.as_bytes()) {
            return Err(Reject::NotAllowlisted);
        }
        Ok(())
    }
}

/// What a peer hands out of band: synthetic account DID, endpoint id and its attestation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationFile {
    /// Always true in this experiment: the account comes from a public test mnemonic.
    pub synthetic: bool,
    pub account_did: String,
    pub endpoint_id: String,
    pub attestation: Token,
}

impl AttestationFile {
    pub fn read(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
    }
}

// ---------------------------------------------------------------------------------------------
// Freshness without shared clocks: receiver challenge, fixed-width signed tuple, no timestamps.

pub fn request_message(
    challenge: &[u8],
    requester: &PublicKey,
    receiver: &PublicKey,
    body: &[u8],
) -> Vec<u8> {
    let mut m = REQUEST_DOMAIN.to_vec();
    m.extend_from_slice(challenge);
    m.extend_from_slice(requester.as_bytes());
    m.extend_from_slice(receiver.as_bytes());
    m.extend(Sha256::digest(body));
    m
}

pub fn response_message(
    challenge: &[u8],
    receiver: &PublicKey,
    requester: &PublicKey,
    request_body: &[u8],
    response_body: &[u8],
) -> Vec<u8> {
    let mut m = RESPONSE_DOMAIN.to_vec();
    m.extend_from_slice(challenge);
    m.extend_from_slice(receiver.as_bytes());
    m.extend_from_slice(requester.as_bytes());
    m.extend(Sha256::digest(request_body));
    m.extend(Sha256::digest(response_body));
    m
}

pub fn verify_sig(key: &PublicKey, message: &[u8], sig: &[u8]) -> bool {
    let Ok(sig) = <[u8; 64]>::try_from(sig) else {
        return false;
    };
    key.verify(message, &Signature::from_bytes(&sig)).is_ok()
}

/// Length-prefixed JSON frames. `Missing` and `Blob` are used only on [`source::SOURCE_ALPN`]:
/// `Missing` lists the lowercase-hex iroh-blobs hashes the receiver's store lacks, in the exact
/// order it expects them; `Blob` heads exactly `size` raw bytes that follow on the stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Frame {
    Hello { attestation: Option<Token> },
    Challenge { nonce: Vec<u8> },
    Request { body: Vec<u8>, sig: Vec<u8> },
    Response { body: Vec<u8>, sig: Vec<u8> },
    Rejected { reason: String },
    Missing { hashes: Vec<String> },
    Blob { hash: String, size: u64 },
}

pub async fn write_frame(send: &mut SendStream, frame: &Frame) -> Result<(), String> {
    let bytes = serde_json::to_vec(frame).map_err(|e| e.to_string())?;
    send.write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .map_err(|e| e.to_string())?;
    send.write_all(&bytes).await.map_err(|e| e.to_string())
}

pub async fn read_frame(recv: &mut RecvStream) -> Result<Frame, String> {
    read_frame_limited(recv, MAX_FRAME).await
}

/// [`read_frame`] with an explicit bound (the source request carries a whole manifest).
pub async fn read_frame_limited(recv: &mut RecvStream, max: usize) -> Result<Frame, String> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.map_err(|e| e.to_string())?;
    let len = u32::from_be_bytes(len) as usize;
    if len > max {
        return Err("frame too large".into());
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&buf).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------
// Endpoints: no presets::N0. Minimal = crypto provider only; then explicitly no relays, no
// address lookup, no portmapper, minimal net report, and exactly ONE caller-chosen socket.

/// Refuses unspecified (0.0.0.0 / ::) binds: the caller must name the interface address.
pub async fn lan_endpoint(
    secret: SecretKey,
    bind: SocketAddr,
    alpns: Vec<Vec<u8>>,
) -> Result<Endpoint, String> {
    if bind.ip().is_unspecified() {
        return Err(format!(
            "refusing unspecified bind {bind}; name the LAN interface address"
        ));
    }
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .clear_address_lookup()
        .portmapper_config(PortmapperConfig::Disabled)
        .net_report_config(NetReportConfig::minimal())
        .clear_ip_transports()
        .bind_addr(bind)
        .map_err(|e| e.to_string())?
        .secret_key(secret)
        .alpns(alpns)
        .bind()
        .await
        .map_err(|e| e.to_string())
}

/// Observable LAN-only configuration of a bound endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct LanReport {
    pub address_lookup_services: usize,
    pub relay_urls: usize,
    pub home_relays: usize,
    pub bound_sockets: Vec<SocketAddr>,
    pub advertised_ip_addrs: Vec<SocketAddr>,
}

impl LanReport {
    pub fn of(ep: &Endpoint) -> Result<Self, String> {
        let addr = ep.addr();
        Ok(Self {
            address_lookup_services: ep.address_lookup().map_err(|e| e.to_string())?.len(),
            relay_urls: addr.relay_urls().count(),
            home_relays: ep.home_relay_status().get().len(),
            bound_sockets: ep.bound_sockets(),
            advertised_ip_addrs: addr.ip_addrs().copied().collect(),
        })
    }

    pub fn is_relay_and_lookup_free(&self) -> bool {
        self.address_lookup_services == 0 && self.relay_urls == 0 && self.home_relays == 0
    }
}

pub fn direct_addr(ep: &Endpoint) -> Result<EndpointAddr, String> {
    let socket = *ep
        .bound_sockets()
        .first()
        .ok_or("endpoint has no bound socket")?;
    Ok(EndpointAddr::new(ep.id()).with_ip_addr(socket))
}

#[derive(Debug)]
pub enum Outcome {
    Handled,
    Rejected(Reject),
}

pub async fn accept_one(ep: &Endpoint) -> Result<Connection, String> {
    let incoming = ep.accept().await.ok_or("endpoint closed")?;
    incoming
        .accept()
        .map_err(|e| e.to_string())?
        .await
        .map_err(|e| e.to_string())
}

/// Receiver: authenticate+authorize the remote endpoint, then challenge, then handle once.
pub async fn handle(
    ep: &Endpoint,
    conn: Connection,
    policy: &Policy,
    handled: &AtomicUsize,
) -> Result<Outcome, String> {
    if conn.alpn() != ALPN {
        return Err("unexpected ALPN".into());
    }
    let remote = conn.remote_id();
    let (mut send, mut recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
    let outcome = async {
        let Frame::Hello { attestation } = read_frame(&mut recv).await? else {
            return Err("expected hello".to_string());
        };
        if let Err(reject) = policy.admit(&remote, attestation.as_ref()) {
            return Ok(Outcome::Rejected(reject));
        }
        let mut nonce = vec![0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        write_frame(
            &mut send,
            &Frame::Challenge {
                nonce: nonce.clone(),
            },
        )
        .await?;
        let Frame::Request { body, sig } = read_frame(&mut recv).await? else {
            return Err("expected request".to_string());
        };
        if !verify_sig(
            &remote,
            &request_message(&nonce, &remote, &ep.id(), &body),
            &sig,
        ) {
            return Ok(Outcome::Rejected(Reject::BadRequest));
        }
        // Stand-in for "handling a request": no execution exists in this experiment.
        handled.fetch_add(1, Ordering::SeqCst);
        let response = format!("acknowledged {} bytes", body.len()).into_bytes();
        let sig = ep
            .secret_key()
            .sign(&response_message(
                &nonce,
                &ep.id(),
                &remote,
                &body,
                &response,
            ))
            .to_bytes()
            .to_vec();
        write_frame(
            &mut send,
            &Frame::Response {
                body: response,
                sig,
            },
        )
        .await?;
        Ok(Outcome::Handled)
    }
    .await?;
    if let Outcome::Rejected(reject) = &outcome {
        let reason = format!("{reject:?}");
        write_frame(&mut send, &Frame::Rejected { reason }).await?;
    }
    send.finish().map_err(|e| e.to_string())?;
    let _ = timeout(CLOSE_GRACE, conn.closed()).await;
    Ok(outcome)
}

pub async fn serve_one(
    ep: &Endpoint,
    policy: &Policy,
    handled: &AtomicUsize,
) -> Result<Outcome, String> {
    let conn = accept_one(ep).await?;
    handle(ep, conn, policy, handled).await
}

pub enum Mode {
    Honest,
    SignWrongChallenge,
    Replay(Frame),
}

#[derive(Debug, Clone)]
pub struct Exchange {
    pub challenge: Vec<u8>,
    pub request: Frame,
    pub response: Frame,
}

/// Requester. Returns the full exchange, or the receiver's rejection reason (`rejected: ...`).
pub async fn request(
    ep: &Endpoint,
    to: EndpointAddr,
    attestation: Option<Token>,
    body: &[u8],
    mode: Mode,
) -> Result<Exchange, String> {
    let receiver = to.id;
    let conn = ep.connect(to, ALPN).await.map_err(|e| e.to_string())?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    write_frame(&mut send, &Frame::Hello { attestation }).await?;
    let result = async {
        let challenge = match read_frame(&mut recv).await? {
            Frame::Challenge { nonce } => nonce,
            Frame::Rejected { reason } => return Err(format!("rejected: {reason}")),
            other => return Err(format!("unexpected {other:?}")),
        };
        let wrong_challenge = matches!(mode, Mode::SignWrongChallenge);
        let request = match mode {
            Mode::Replay(old) => old,
            Mode::Honest | Mode::SignWrongChallenge => {
                let mut signed = challenge.clone();
                if wrong_challenge {
                    signed[0] ^= 0xff;
                }
                let msg = request_message(&signed, &ep.id(), &receiver, body);
                Frame::Request {
                    body: body.to_vec(),
                    sig: ep.secret_key().sign(&msg).to_bytes().to_vec(),
                }
            }
        };
        write_frame(&mut send, &request).await?;
        match read_frame(&mut recv).await? {
            response @ Frame::Response { .. } => Ok(Exchange {
                challenge,
                request,
                response,
            }),
            Frame::Rejected { reason } => Err(format!("rejected: {reason}")),
            other => Err(format!("unexpected {other:?}")),
        }
    }
    .await;
    conn.close(VarInt::from_u32(0), b"done");
    result
}

/// Requester-side check: the response must be bound to THIS session's challenge and request.
pub fn response_is_fresh(
    response: &Frame,
    challenge: &[u8],
    receiver: &PublicKey,
    requester: &PublicKey,
    request_body: &[u8],
) -> bool {
    let Frame::Response { body, sig } = response else {
        return false;
    };
    verify_sig(
        receiver,
        &response_message(challenge, receiver, requester, request_body, body),
        sig,
    )
}
