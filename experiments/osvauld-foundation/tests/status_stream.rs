//! Read-only status stream (`task-manager/status/0`): in-process loopback endpoints, synthetic
//! installations and a stub status command that prints a fixture snapshot. Nothing here runs the
//! installed `tasks`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iroh::{Endpoint, SecretKey};
use task_manager_osvauld_foundation::status::{
    self, MAX_SNAPSHOT_BYTES, STATUS_ALPN, Sample, StatusConfig, StatusEvent, StatusHub,
    StatusOutcome, WatchOptions,
};
use task_manager_osvauld_foundation::{
    AttestationFile, Policy, attest_endpoint, direct_addr, lan_endpoint,
    load_or_create_transport_key, now_secs, open_synthetic_installation,
};
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(60);

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/status-snapshot.json")
}

/// argv of a stub status command: counts its runs in `counter`, then prints the fixture.
fn stub_argv(counter: &Path) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        format!(
            "echo run >> '{}' && cat '{}'",
            counter.display(),
            fixture().display()
        ),
    ]
}

fn runs(counter: &Path) -> usize {
    std::fs::read_to_string(counter)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

struct Peer {
    key: SecretKey,
    attestation: AttestationFile,
    did: String,
    _dir: tempfile::TempDir,
}

fn peer() -> Peer {
    let dir = tempfile::tempdir().unwrap();
    let (vault, did) = open_synthetic_installation(dir.path()).unwrap();
    let key = load_or_create_transport_key(&vault).unwrap();
    let now = now_secs();
    let token = vault
        .with_signer(|s| attest_endpoint(s, &key.public(), now, now + 3600))
        .unwrap()
        .unwrap();
    Peer {
        attestation: AttestationFile {
            synthetic: true,
            account_did: did.clone(),
            endpoint_id: key.public().to_string(),
            attestation: token,
        },
        key,
        did,
        _dir: dir,
    }
}

async fn endpoint(key: &SecretKey, alpns: Vec<Vec<u8>>) -> Endpoint {
    lan_endpoint(key.clone(), "127.0.0.1:0".parse().unwrap(), alpns)
        .await
        .unwrap()
}

/// A status server: accepts forever, each stream in its own task; outcomes go to the channel.
struct Server {
    ep: Endpoint,
    hub: Arc<StatusHub>,
    outcomes: tokio::sync::mpsc::UnboundedReceiver<Result<StatusOutcome, String>>,
    _peer: Peer,
}

async fn server(config: StatusConfig, allowed: &[&Peer]) -> Server {
    let me = peer();
    let mut policy = Policy::new(&me.did, now_secs());
    for p in allowed {
        policy
            .approve_attestation(&p.attestation.attestation)
            .unwrap();
    }
    let ep = endpoint(&me.key, vec![STATUS_ALPN.to_vec()]).await;
    let hub = StatusHub::new(config);
    let (tx, outcomes) = tokio::sync::mpsc::unbounded_channel();
    let (accept_ep, accept_hub) = (ep.clone(), Arc::clone(&hub));
    let policy = Arc::new(policy);
    tokio::spawn(async move {
        while let Some(incoming) = accept_ep.accept().await {
            let Ok(connecting) = incoming.accept() else {
                continue;
            };
            let Ok(conn) = connecting.await else {
                continue;
            };
            let (ep, hub, policy, tx) = (
                accept_ep.clone(),
                Arc::clone(&accept_hub),
                Arc::clone(&policy),
                tx.clone(),
            );
            tokio::spawn(async move {
                let _ = tx.send(status::handle_status(&ep, conn, &policy, &hub).await);
            });
        }
    });
    Server {
        ep,
        hub,
        outcomes,
        _peer: me,
    }
}

async fn collect(
    client: &Peer,
    server: &Server,
    n: usize,
    options: WatchOptions,
) -> Result<Vec<StatusEvent>, String> {
    let ep = endpoint(&client.key, vec![]).await;
    let mut events = Vec::new();
    let result = timeout(
        STEP,
        status::watch_status(
            &ep,
            direct_addr(&server.ep).unwrap(),
            Some(client.attestation.attestation.clone()),
            options,
            |event| {
                events.push(event);
                events.len() < n
            },
        ),
    )
    .await
    .expect("watch finished in time");
    ep.close().await;
    result.map(|_| events)
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + STEP;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn config(argv: Vec<String>, max_bytes: usize) -> StatusConfig {
    StatusConfig::new(argv, Duration::from_secs(1), max_bytes).unwrap()
}

#[tokio::test]
async fn authorized_peer_gets_compact_snapshots_and_sampling_stops_without_subscribers() {
    let work = tempfile::tempdir().unwrap();
    let counter = work.path().join("runs");
    let client = peer();
    let srv = server(config(stub_argv(&counter), MAX_SNAPSHOT_BYTES), &[&client]).await;
    assert_eq!(runs(&counter), 0, "nothing runs before a subscriber");
    assert!(!srv.hub.sampler_running());

    let started = Instant::now();
    let events = collect(&client, &srv, 2, WatchOptions::default())
        .await
        .unwrap();
    // Two samples are at least one interval apart: the rate is bounded by the server.
    assert!(started.elapsed() >= Duration::from_millis(900));
    let expected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture()).unwrap()).unwrap();
    let mut seqs = Vec::new();
    for event in events {
        let StatusEvent::Snapshot { seq, value } = event else {
            panic!("expected snapshots, got {event:?}");
        };
        assert_eq!(value, expected);
        seqs.push(seq);
    }
    assert!(seqs[0] < seqs[1], "{seqs:?}");

    // The client left: the sampler stops at its next tick and the command is not run again.
    wait_until("sampler stop", || !srv.hub.sampler_running()).await;
    assert_eq!(srv.hub.subscribers(), 0);
    let taken = srv.hub.samples_taken();
    assert_eq!(runs(&counter) as u64, taken);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        srv.hub.samples_taken(),
        taken,
        "no sampling without subscribers"
    );
    assert_eq!(runs(&counter) as u64, taken);
}

#[tokio::test]
async fn two_subscribers_share_one_sampler() {
    let work = tempfile::tempdir().unwrap();
    let counter = work.path().join("runs");
    let (a, b) = (peer(), peer());
    let srv = server(config(stub_argv(&counter), MAX_SNAPSHOT_BYTES), &[&a, &b]).await;
    let (ra, rb) = tokio::join!(
        collect(&a, &srv, 3, WatchOptions::default()),
        collect(&b, &srv, 3, WatchOptions::default())
    );
    assert_eq!(ra.unwrap().len(), 3);
    assert_eq!(rb.unwrap().len(), 3);
    wait_until("sampler stop", || !srv.hub.sampler_running()).await;
    // Shared: well under the 6 runs two independent samplers would need.
    assert!(srv.hub.samples_taken() <= 5, "{}", srv.hub.samples_taken());
}

#[tokio::test]
async fn unauthorized_peer_is_refused_before_anything_runs() {
    let work = tempfile::tempdir().unwrap();
    let counter = work.path().join("runs");
    let (client, stranger) = (peer(), peer());
    let mut srv = server(config(stub_argv(&counter), MAX_SNAPSHOT_BYTES), &[&client]).await;

    let err = collect(&stranger, &srv, 1, WatchOptions::default())
        .await
        .expect_err("stranger refused");
    assert!(err.starts_with("rejected: NotAllowlisted"), "{err}");
    let outcome = timeout(STEP, srv.outcomes.recv()).await.unwrap().unwrap();
    assert!(
        matches!(&outcome, Ok(StatusOutcome::Rejected(r)) if r == "NotAllowlisted"),
        "{outcome:?}"
    );

    // No attestation at all.
    let ep = endpoint(&stranger.key, vec![]).await;
    let err = status::watch_status(
        &ep,
        direct_addr(&srv.ep).unwrap(),
        None,
        WatchOptions::default(),
        |_| false,
    )
    .await
    .expect_err("unattested refused");
    assert!(err.starts_with("rejected: Unattested"), "{err}");
    ep.close().await;

    // An attestation of ANOTHER endpoint (the allowlisted client's) presented by the stranger.
    let ep = endpoint(&stranger.key, vec![]).await;
    let err = status::watch_status(
        &ep,
        direct_addr(&srv.ep).unwrap(),
        Some(client.attestation.attestation.clone()),
        WatchOptions::default(),
        |_| false,
    )
    .await
    .expect_err("borrowed attestation refused");
    assert!(err.starts_with("rejected: EndpointMismatch"), "{err}");
    ep.close().await;

    assert_eq!(srv.hub.samples_taken(), 0);
    assert_eq!(
        runs(&counter),
        0,
        "the status command never ran for a refused peer"
    );
}

#[tokio::test]
async fn size_limits_apply_on_both_sides() {
    let work = tempfile::tempdir().unwrap();
    let counter = work.path().join("runs");
    let client = peer();
    let fixture_len = std::fs::read(fixture()).unwrap().len();

    // Server bound smaller than the snapshot: the client gets a status error, not data.
    let srv = server(config(stub_argv(&counter), 64), &[&client]).await;
    let events = collect(&client, &srv, 1, WatchOptions::default())
        .await
        .unwrap();
    let StatusEvent::Error { error, .. } = &events[0] else {
        panic!("expected a status error, got {:?}", events[0]);
    };
    assert!(error.contains("too large"), "{error}");

    // Client bound smaller than what the server sends: the client refuses before reading.
    let srv = server(config(stub_argv(&counter), MAX_SNAPSHOT_BYTES), &[&client]).await;
    let options = WatchOptions {
        max_bytes: 100,
        ..WatchOptions::default()
    };
    let err = collect(&client, &srv, 1, options)
        .await
        .expect_err("client size bound");
    assert!(err.starts_with("verify: snapshot too large"), "{err}");
    assert!(fixture_len > 100);
}

#[tokio::test]
async fn failing_or_invalid_command_is_reported_as_status_error() {
    let client = peer();
    let failing = vec![
        "sh".into(),
        "-c".into(),
        "echo 'tasks: unknown command status' >&2; exit 2".into(),
    ];
    let srv = server(config(failing, MAX_SNAPSHOT_BYTES), &[&client]).await;
    let events = collect(&client, &srv, 2, WatchOptions::default())
        .await
        .unwrap();
    for event in &events {
        let StatusEvent::Error { error, .. } = event else {
            panic!("{event:?}");
        };
        assert!(error.contains("unknown command status"), "{error}");
    }

    let not_json = vec!["echo".into(), "hello".into()];
    let srv = server(config(not_json, MAX_SNAPSHOT_BYTES), &[&client]).await;
    let events = collect(&client, &srv, 1, WatchOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(&events[0], StatusEvent::Error { error, .. } if error.contains("not JSON")),
        "{events:?}"
    );
}

#[test]
fn command_timeout_kills_the_process_group() {
    let mut cfg = config(vec!["sleep".into(), "30".into()], MAX_SNAPSHOT_BYTES);
    cfg.command_timeout = Duration::from_millis(300);
    let started = Instant::now();
    let sample = status::run_status_command(&cfg);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        matches!(&sample, Sample::Error(e) if e.contains("timed out")),
        "{sample:?}"
    );
}

#[test]
fn config_and_command_parsing() {
    assert_eq!(
        status::parse_status_cmd("~/.local/bin/tasks status --json", Some("/home/u")).unwrap(),
        ["/home/u/.local/bin/tasks", "status", "--json"]
    );
    // No shell: metacharacters are plain words.
    assert_eq!(
        status::parse_status_cmd("tasks status;rm -rf ~x", Some("/h")).unwrap(),
        ["tasks", "status;rm", "-rf", "~x"]
    );
    assert!(status::parse_status_cmd("  ", Some("/h")).is_err());
    assert!(status::parse_status_cmd("~/x", None).is_err());
    let argv = vec!["true".to_owned()];
    assert!(StatusConfig::new(argv.clone(), Duration::from_millis(999), 10).is_err());
    assert!(StatusConfig::new(argv.clone(), Duration::from_secs(1), 0).is_err());
    assert!(
        StatusConfig::new(argv.clone(), Duration::from_secs(1), MAX_SNAPSHOT_BYTES + 1).is_err()
    );
    assert!(StatusConfig::new(vec![], Duration::from_secs(1), 10).is_err());
    assert!(StatusConfig::new(argv, Duration::from_secs(1), MAX_SNAPSHOT_BYTES).is_ok());

    assert!(status::validate_snapshot(b"[1]", 100).is_err());
    assert!(status::validate_snapshot(br#"{"version":"1"}"#, 100).is_err());
    assert_eq!(
        status::validate_snapshot(b"{ \"version\" : 1 }\n", 100).unwrap(),
        br#"{"version":1}"#
    );
}

#[test]
fn markers_and_live_annotation_follow_the_contract() {
    let offline = status::marker("offline", "w", "iroh", "peer", "boom", Some(2.0), 5.0);
    assert_eq!(offline["version"], 1);
    assert_eq!(offline["machine"]["id"], "w");
    assert_eq!(offline["machine"]["online"], false);
    assert_eq!(offline["jobs"], serde_json::json!([]));
    assert_eq!(offline["stream"]["marker"], "offline");
    assert_eq!(offline["stream"]["retry_in_s"], 2.0);
    let err = status::marker("status_error", "w", "iroh", "peer", "boom", None, 5.0);
    assert_eq!(err["machine"]["online"], true);

    let live = status::annotate_live(
        serde_json::json!({"version": 1, "machine": {"id": "w", "online": false}}),
        "iroh",
        "peer",
        3,
        9.0,
    );
    assert_eq!(live["machine"]["online"], true);
    assert_eq!(live["machine"]["id"], "w");
    assert_eq!(live["stream"]["seq"], 3);
    assert!(live["stream"].get("marker").is_none());
}
