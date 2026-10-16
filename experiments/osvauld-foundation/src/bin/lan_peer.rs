//! `lan-peer`: experimental two-process LAN check (handover gate 4).
//!
//! SYNTHETIC ONLY. Both sides recover one account from a PUBLIC BIP39 test vector, so the shared
//! DID proves nothing about real ownership and the sealed key at rest protects nothing. Each
//! installation still generates its own random iroh transport key. No relays, no address
//! lookup, no portmapper, no unspecified bind, no command execution.
//!
//! Phase 2 adds content-addressed source transfer (see `source.rs`): `manifest` hashes an
//! explicit file list (BLAKE3 = iroh-blobs raw hash), `push-source` sends only the blobs the
//! server's store lacks, and `serve --store --checkouts` imports them and materializes an
//! immutable checkout. No execution of anything received.
//!
//! ```text
//! lan-peer init        --data-dir DIR
//! lan-peer serve       --bind IP:PORT --data-dir DIR [--allow FILE]... [--max-connections N]
//!                      [--store DIR --checkouts DIR]
//! lan-peer connect     --peer ENDPOINT_ID --addr IP:PORT --data-dir DIR --allow FILE [--bind IP:PORT]
//! lan-peer manifest    SOURCE_DIR --files-from0 FILE
//! lan-peer push-source --peer ENDPOINT_ID --addr IP:PORT --data-dir DIR --allow FILE
//!                      --source DIR --manifest FILE --checkout NAME [--bind IP:PORT]
//! ```
//! Exit codes: 0 ok, 2 usage/config error, 3 rejected/verification failure, 4 transport/timeout.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use iroh::{EndpointAddr, PublicKey, SecretKey};
use iroh_blobs::store::fs::FsStore;
use serde_json::json;
use task_manager_osvauld_foundation::source::{
    self, Limits, Manifest, PushMode, SOURCE_ALPN, SourceOutcome,
};
use task_manager_osvauld_foundation::{
    ALPN, AttestationFile, LanReport, Mode, Outcome, Policy, accept_one, attest_endpoint, handle,
    lan_endpoint, load_or_create_transport_key, now_secs, open_synthetic_installation, request,
    response_is_fresh,
};
use tokio::time::timeout;

const DAY: u64 = 86_400;
const ATTESTATION_TTL: u64 = 7 * DAY;
const CONNECT_BUDGET: Duration = Duration::from_secs(30);
const HANDLE_BUDGET: Duration = Duration::from_secs(30);
/// Whole source session (manifest, missing blobs, materialize). Bounded, not resumable.
const SOURCE_BUDGET: Duration = Duration::from_secs(300);
const CLOSE_BUDGET: Duration = Duration::from_secs(5);
const PING: &[u8] = b"lan-peer synthetic ping; not a command";
const USAGE: &str = "usage:
  lan-peer init        --data-dir DIR
  lan-peer serve       --bind IP:PORT --data-dir DIR [--allow FILE]... [--max-connections N]
                       [--store DIR --checkouts DIR]
  lan-peer connect     --peer ENDPOINT_ID --addr IP:PORT --data-dir DIR --allow FILE [--bind IP:PORT]
  lan-peer manifest    SOURCE_DIR --files-from0 FILE
  lan-peer push-source --peer ENDPOINT_ID --addr IP:PORT --data-dir DIR --allow FILE
                       --source DIR --manifest FILE --checkout NAME [--bind IP:PORT]
SYNTHETIC test account only (public test mnemonic). No relays, discovery or 0.0.0.0 binds.";

enum Fail {
    Usage(String),
    Rejected(serde_json::Value),
    Transport(serde_json::Value),
}

impl From<String> for Fail {
    fn from(e: String) -> Self {
        Fail::Usage(e)
    }
}

struct Args(Vec<String>);

impl Args {
    fn one(&self, flag: &str) -> Result<Option<String>, String> {
        let mut found = None;
        for pair in self.0.windows(2) {
            if pair[0] == flag {
                if found.is_some() {
                    return Err(format!("{flag} given more than once"));
                }
                found = Some(pair[1].clone());
            }
        }
        Ok(found)
    }

    fn required(&self, flag: &str) -> Result<String, String> {
        self.one(flag)?.ok_or_else(|| format!("missing {flag}"))
    }

    fn many(&self, flag: &str) -> Vec<String> {
        self.0
            .windows(2)
            .filter(|p| p[0] == flag)
            .map(|p| p[1].clone())
            .collect()
    }

    fn check_known(&self, known: &[&str]) -> Result<(), String> {
        let mut i = 0;
        while i < self.0.len() {
            if !known.contains(&self.0[i].as_str()) {
                return Err(format!("unknown argument {}", self.0[i]));
            }
            if i + 1 >= self.0.len() {
                return Err(format!("{} needs a value", self.0[i]));
            }
            i += 2;
        }
        Ok(())
    }
}

fn parse_addr(flag: &str, value: &str) -> Result<SocketAddr, String> {
    value
        .parse()
        .map_err(|e| format!("{flag} {value}: expected IP:PORT ({e})"))
}

/// Bind addresses must name one interface; 0.0.0.0 / :: are refused before anything opens.
fn parse_bind(value: &str) -> Result<SocketAddr, String> {
    let addr = parse_addr("--bind", value)?;
    if addr.ip().is_unspecified() {
        return Err(format!(
            "--bind {value}: unspecified address refused; name the LAN interface IP"
        ));
    }
    Ok(addr)
}

struct Installation {
    did: String,
    key: SecretKey,
    attestation: AttestationFile,
    attestation_path: PathBuf,
}

/// Opens or creates the synthetic installation in `dir`, reusing its attestation while it stays
/// valid for at least another day; otherwise the vault signer issues a fresh one.
fn installation(dir: &Path) -> Result<Installation, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (vault, did) = open_synthetic_installation(dir)?;
    let key = load_or_create_transport_key(&vault)?;
    let endpoint = key.public();
    let attestation_path = dir.join("attestation.json");
    let now = now_secs();
    let reusable = AttestationFile::read(&attestation_path)
        .ok()
        .filter(|file| {
            file.endpoint_id == endpoint.to_string()
                && Policy::new(&did, now + DAY).attested_endpoint(&file.attestation) == Ok(endpoint)
        });
    let attestation = match reusable {
        Some(file) => file,
        None => {
            let token = vault
                .with_signer(|s| attest_endpoint(s, &endpoint, now, now + ATTESTATION_TTL))
                .ok_or("vault locked")?
                .map_err(|e| e.to_string())?;
            let file = AttestationFile {
                synthetic: true,
                account_did: did.clone(),
                endpoint_id: endpoint.to_string(),
                attestation: token,
            };
            file.write(&attestation_path)?;
            file
        }
    };
    Ok(Installation {
        did,
        key,
        attestation,
        attestation_path,
    })
}

fn identity_json(inst: &Installation) -> serde_json::Value {
    json!({
        "synthetic": true,
        "account_did": inst.did,
        "endpoint_id": inst.key.public().to_string(),
        "attestation_file": inst.attestation_path,
    })
}

fn cmd_init(args: &Args) -> Result<serde_json::Value, Fail> {
    args.check_known(&["--data-dir"])?;
    let inst = installation(Path::new(&args.required("--data-dir")?))?;
    let mut out = identity_json(&inst);
    out["event"] = json!("initialized");
    Ok(out)
}

fn emit(value: &serde_json::Value) {
    println!("{value}");
}

async fn cmd_serve(args: &Args) -> Result<serde_json::Value, Fail> {
    args.check_known(&[
        "--bind",
        "--data-dir",
        "--allow",
        "--max-connections",
        "--store",
        "--checkouts",
    ])?;
    let bind = parse_bind(&args.required("--bind")?)?;
    let max = match args.one("--max-connections")? {
        Some(n) => Some(
            n.parse::<usize>()
                .map_err(|e| format!("--max-connections {n}: {e}"))?,
        ),
        None => None,
    };
    let inst = installation(Path::new(&args.required("--data-dir")?))?;
    let mut policy = Policy::new(&inst.did, now_secs());
    let mut allowed = Vec::new();
    for path in args.many("--allow") {
        let file = AttestationFile::read(Path::new(&path))?;
        let endpoint = policy
            .approve_attestation(&file.attestation)
            .map_err(|r| format!("--allow {path}: {r:?}"))?;
        if file.endpoint_id != endpoint.to_string() {
            return Err(Fail::Usage(format!(
                "--allow {path}: endpoint_id does not match the attestation"
            )));
        }
        allowed.push(endpoint.to_string());
    }

    // Source transfer is opt-in: both a caller-given store dir and a checkout root.
    let source = match (args.one("--store")?, args.one("--checkouts")?) {
        (None, None) => None,
        (Some(store), Some(checkouts)) => {
            let checkouts = PathBuf::from(checkouts);
            std::fs::create_dir_all(&checkouts)
                .map_err(|e| format!("--checkouts {}: {e}", checkouts.display()))?;
            std::fs::create_dir_all(&store).map_err(|e| format!("--store {store}: {e}"))?;
            let store = FsStore::load(&store)
                .await
                .map_err(|e| format!("--store {store}: {e}"))?;
            Some((store, checkouts))
        }
        _ => return Err(Fail::Usage("--store and --checkouts go together".into())),
    };
    let mut alpns = vec![ALPN.to_vec()];
    if source.is_some() {
        alpns.push(SOURCE_ALPN.to_vec());
    }

    let ep = lan_endpoint(inst.key.clone(), bind, alpns).await?;
    let lan = LanReport::of(&ep)?;
    if !lan.is_relay_and_lookup_free() {
        return Err(Fail::Usage(format!("endpoint is not LAN-only: {lan:?}")));
    }
    let mut ready = identity_json(&inst);
    ready["event"] = json!("listening");
    ready["addr"] = json!(lan.bound_sockets.first());
    ready["alpn"] = json!(String::from_utf8_lossy(ALPN));
    ready["allowed_endpoints"] = json!(allowed);
    ready["source_transfer"] = json!(source.is_some());
    ready["lan"] = json!(lan);
    emit(&ready);

    let handled = AtomicUsize::new(0);
    let mut seen = 0usize;
    while max.is_none_or(|m| seen < m) {
        let conn = accept_one(&ep).await;
        seen += 1;
        let conn = match conn {
            Ok(conn) => conn,
            Err(error) => {
                emit(&json!({"event": "connection_failed", "error": error}));
                continue;
            }
        };
        let remote = conn.remote_id().to_string();
        policy.now_s = now_secs();
        if conn.alpn() == SOURCE_ALPN
            && let Some((store, checkouts)) = &source
        {
            let limits = Limits::default();
            let session = source::handle_source(&ep, conn, &policy, store, checkouts, &limits);
            let event = match timeout(SOURCE_BUDGET, session).await {
                Ok(Ok(SourceOutcome::Materialized {
                    checkout,
                    files,
                    blobs_requested,
                    blobs_received,
                    bytes_received,
                })) => json!({
                    "event": "source_materialized",
                    "remote": remote,
                    "checkout": checkout,
                    "files": files,
                    "blobs_requested": blobs_requested,
                    "blobs_received": blobs_received,
                    "bytes_received": bytes_received,
                }),
                Ok(Ok(SourceOutcome::Rejected(reason))) => {
                    json!({"event": "rejected", "alpn": "source", "remote": remote, "reason": reason})
                }
                Ok(Err(error)) => {
                    json!({"event": "error", "alpn": "source", "remote": remote, "error": error})
                }
                Err(_) => json!({"event": "timeout", "alpn": "source", "remote": remote}),
            };
            emit(&event);
            continue;
        }
        let event = match timeout(HANDLE_BUDGET, handle(&ep, conn, &policy, &handled)).await {
            Ok(Ok(Outcome::Handled)) => json!({"event": "handled", "remote": remote}),
            Ok(Ok(Outcome::Rejected(r))) => {
                json!({"event": "rejected", "remote": remote, "reason": format!("{r:?}")})
            }
            Ok(Err(error)) => json!({"event": "error", "remote": remote, "error": error}),
            Err(_) => json!({"event": "timeout", "remote": remote}),
        };
        emit(&event);
    }
    let _ = timeout(CLOSE_BUDGET, ep.close()).await;
    if let Some((store, _)) = source {
        let _ = timeout(CLOSE_BUDGET, store.shutdown()).await;
    }
    Ok(json!({"event": "stopped", "connections": seen}))
}

/// Shared requester setup: peer id, address, explicit bind, installation, and the requester-side
/// check that `--allow` attests exactly `--peer` for our account (before dialing).
struct Dial {
    peer: PublicKey,
    addr: SocketAddr,
    bind: SocketAddr,
    inst: Installation,
    base: serde_json::Value,
}

fn dial_setup(args: &Args) -> Result<Dial, Fail> {
    let peer_text = args.required("--peer")?;
    let peer: PublicKey = peer_text
        .parse()
        .map_err(|e| format!("--peer {peer_text}: {e}"))?;
    let addr = parse_addr("--addr", &args.required("--addr")?)?;
    let bind = match args.one("--bind")? {
        Some(b) => parse_bind(&b)?,
        None if addr.ip().is_loopback() && addr.is_ipv4() => (Ipv4Addr::LOCALHOST, 0).into(),
        None if addr.ip().is_loopback() => (Ipv6Addr::LOCALHOST, 0).into(),
        None => {
            return Err(Fail::Usage(
                "--bind LOCAL_LAN_IP:0 is required for a non-loopback --addr".into(),
            ));
        }
    };
    let allow = args.required("--allow")?;
    let inst = installation(Path::new(&args.required("--data-dir")?))?;

    // Requester-side allowlist: the peer must be attested by the expected account AND be the
    // exact endpoint we dial. iroh's TLS then proves the remote holds that endpoint key.
    let mut policy = Policy::new(&inst.did, now_secs());
    let file = AttestationFile::read(Path::new(&allow))?;
    let base = json!({"peer": peer.to_string(), "addr": addr, "local_endpoint": inst.key.public().to_string()});
    let attested = policy.approve_attestation(&file.attestation).map_err(|r| {
        let mut v = base.clone();
        v["status"] = json!("rejected");
        v["stage"] = json!("peer_attestation");
        v["reason"] = json!(format!("{r:?}"));
        Fail::Rejected(v)
    })?;
    if attested != peer {
        let mut v = base.clone();
        v["status"] = json!("rejected");
        v["stage"] = json!("peer_attestation");
        v["reason"] = json!("EndpointMismatch: --allow file attests a different endpoint");
        return Err(Fail::Rejected(v));
    }
    Ok(Dial {
        peer,
        addr,
        bind,
        inst,
        base,
    })
}

async fn cmd_connect(args: &Args) -> Result<serde_json::Value, Fail> {
    args.check_known(&["--peer", "--addr", "--data-dir", "--allow", "--bind"])?;
    let Dial {
        peer,
        addr,
        bind,
        inst,
        base,
    } = dial_setup(args)?;

    let ep = lan_endpoint(inst.key.clone(), bind, vec![]).await?;
    let lan = LanReport::of(&ep)?;
    let to = EndpointAddr::new(peer).with_ip_addr(addr);
    let attempt = timeout(
        CONNECT_BUDGET,
        request(
            &ep,
            to,
            Some(inst.attestation.attestation.clone()),
            PING,
            Mode::Honest,
        ),
    )
    .await;
    let mut out = base;
    out["lan"] = json!(lan);
    let result = match attempt {
        Err(_) => {
            out["status"] = json!("error");
            out["stage"] = json!("timeout");
            Err(Fail::Transport(out))
        }
        Ok(Err(error)) if error.starts_with("rejected:") => {
            out["status"] = json!("rejected");
            out["stage"] = json!("receiver");
            out["reason"] = json!(error.trim_start_matches("rejected: "));
            Err(Fail::Rejected(out))
        }
        Ok(Err(error)) => {
            out["status"] = json!("error");
            out["stage"] = json!("transport");
            out["error"] = json!(error);
            Err(Fail::Transport(out))
        }
        Ok(Ok(exchange)) => {
            let fresh = response_is_fresh(
                &exchange.response,
                &exchange.challenge,
                &peer,
                &ep.id(),
                PING,
            );
            out["challenge_bytes"] = json!(exchange.challenge.len());
            if fresh {
                out["status"] = json!("ok");
                out["response_fresh"] = json!(true);
                Ok(out)
            } else {
                out["status"] = json!("rejected");
                out["stage"] = json!("response");
                out["reason"] = json!("response not bound to this session");
                Err(Fail::Rejected(out))
            }
        }
    };
    let _ = timeout(CLOSE_BUDGET, ep.close()).await;
    result
}

/// `manifest SOURCE_DIR --files-from0 FILE`: prints the manifest (one JSON line) with BLAKE3
/// iroh-blobs hashes. Structural path safety only; selection policy is the Python capture's.
fn cmd_manifest(args: &[String]) -> Result<serde_json::Value, Fail> {
    let (root, rest) = args
        .split_first()
        .ok_or_else(|| "manifest needs SOURCE_DIR".to_string())?;
    let rest = Args(rest.to_vec());
    rest.check_known(&["--files-from0"])?;
    let list = rest.required("--files-from0")?;
    let bytes = std::fs::read(&list).map_err(|e| format!("--files-from0 {list}: {e}"))?;
    let paths = source::parse_files0(&bytes).map_err(|e| e.to_string())?;
    let manifest = source::manifest_from_files(Path::new(root), &paths, &Limits::default())
        .map_err(|e| e.to_string())?;
    serde_json::to_value(manifest).map_err(|e| Fail::Usage(e.to_string()))
}

async fn cmd_push_source(args: &Args) -> Result<serde_json::Value, Fail> {
    args.check_known(&[
        "--peer",
        "--addr",
        "--data-dir",
        "--allow",
        "--bind",
        "--source",
        "--manifest",
        "--checkout",
    ])?;
    let source_dir = PathBuf::from(args.required("--source")?);
    let manifest_path = args.required("--manifest")?;
    let checkout = args.required("--checkout")?;
    let manifest: Manifest = serde_json::from_slice(
        &std::fs::read(&manifest_path).map_err(|e| format!("--manifest {manifest_path}: {e}"))?,
    )
    .map_err(|e| format!("--manifest {manifest_path}: {e}"))?;
    let Dial {
        peer,
        addr,
        bind,
        inst,
        base,
    } = dial_setup(args)?;

    let ep = lan_endpoint(inst.key.clone(), bind, vec![]).await?;
    let to = EndpointAddr::new(peer).with_ip_addr(addr);
    let attempt = timeout(
        SOURCE_BUDGET,
        source::push_source(
            &ep,
            to,
            Some(inst.attestation.attestation.clone()),
            &source_dir,
            &manifest,
            &checkout,
            PushMode::Honest,
        ),
    )
    .await;
    let mut out = base;
    out["checkout_name"] = json!(checkout);
    let result = match attempt {
        Err(_) => {
            out["status"] = json!("error");
            out["stage"] = json!("timeout");
            Err(Fail::Transport(out))
        }
        Ok(Err(error)) if error.starts_with("rejected:") => {
            out["status"] = json!("rejected");
            out["stage"] = json!("receiver");
            out["reason"] = json!(error.trim_start_matches("rejected: "));
            Err(Fail::Rejected(out))
        }
        Ok(Err(error)) if error.starts_with("source:") => {
            out["status"] = json!("error");
            out["stage"] = json!("source");
            out["error"] = json!(error);
            Err(Fail::Rejected(out))
        }
        Ok(Err(error)) => {
            out["status"] = json!("error");
            out["stage"] = json!("transport");
            out["error"] = json!(error);
            Err(Fail::Transport(out))
        }
        Ok(Ok(report)) => {
            out["blobs_missing"] = json!(report.missing);
            out["blobs_sent"] = json!(report.blobs_sent);
            out["bytes_sent"] = json!(report.bytes_sent);
            out["files"] = json!(manifest.files.len());
            out["receiver"] = serde_json::from_slice(&report.response_body).unwrap_or_default();
            if report.response_fresh {
                out["status"] = json!("ok");
                out["response_fresh"] = json!(true);
                Ok(out)
            } else {
                out["status"] = json!("rejected");
                out["stage"] = json!("response");
                out["reason"] = json!("response not bound to this session");
                Err(Fail::Rejected(out))
            }
        }
    };
    let _ = timeout(CLOSE_BUDGET, ep.close()).await;
    result
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "--help" || argv[0] == "-h" {
        eprintln!("{USAGE}");
        return ExitCode::from(if argv.is_empty() { 2 } else { 0 });
    }
    let command = argv.remove(0);
    if command == "manifest" {
        return finish(cmd_manifest(&argv));
    }
    let args = Args(argv);
    let result = match command.as_str() {
        "init" => cmd_init(&args),
        "push-source" => cmd_push_source(&args).await,
        "serve" => cmd_serve(&args).await,
        "connect" => cmd_connect(&args).await,
        other => Err(Fail::Usage(format!("unknown command {other}"))),
    };
    finish(result)
}

fn finish(result: Result<serde_json::Value, Fail>) -> ExitCode {
    match result {
        Ok(value) => {
            emit(&value);
            ExitCode::SUCCESS
        }
        Err(Fail::Usage(error)) => {
            emit(&json!({"status": "error", "stage": "usage", "error": error}));
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
        Err(Fail::Rejected(value)) => {
            emit(&value);
            ExitCode::from(3)
        }
        Err(Fail::Transport(value)) => {
            emit(&value);
            ExitCode::from(4)
        }
    }
}
