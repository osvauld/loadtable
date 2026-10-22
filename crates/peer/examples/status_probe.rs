//! Transport-only diagnostic using the production peer endpoint/status protocol.
//! No daemon, jobs, source/artifact service, child commands or default config.
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use task_manager_peer::config::{self, Approve};
use task_manager_peer::iroh::{Endpoint, PublicKey};
use task_manager_peer::server::{self, Services};
use task_manager_peer::status::{self, Sample, StatusEvent, StatusHub};
use task_manager_peer::{Allowlist, LanReport, PeerEntry, Role, lan_endpoint, peer_addr};

const USAGE: &str = "status_probe init NEW_KEY_FILE\nstatus_probe serve KEY_FILE IP:PORT SECONDS [ALLOWED_ENDPOINT_ID ...]\nstatus_probe watch KEY_FILE LOCAL_IP:0 REMOTE_ENDPOINT_ID REMOTE_IP:PORT [SNAPSHOTS]\nLAN only; status only; no jobs. New disposable identity, never the daemon's key/config.";

fn bounded(value: &str, max: u64) -> Result<u64, String> {
    let n: u64 = value
        .parse()
        .map_err(|_| "expected an integer".to_owned())?;
    if !(1..=max).contains(&n) {
        return Err(format!("expected 1..={max}"));
    }
    Ok(n)
}
fn services(ids: Vec<PublicKey>) -> Arc<Services> {
    let allow = Allowlist {
        peers: ids
            .into_iter()
            .enumerate()
            .map(|(i, endpoint)| PeerEntry {
                name: format!("probe-{i}"),
                endpoint,
                addr: None,
                role: Role::Observer,
                approve: Approve::Manual,
            })
            .collect(),
    };
    let source = Arc::new(|| {
        Sample::Snapshot(Arc::new(
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "probe": { "kind": "transport-only", "execution_enabled": false,
                    "os": std::env::consts::OS, "arch": std::env::consts::ARCH }
            }))
            .expect("fixed diagnostic object"),
        ))
    });
    Arc::new(Services {
        allow,
        status: Some(StatusHub::new(source, Duration::from_secs(1))),
        jobs: None,
        source: None,
    })
}
async fn watch(
    ep: &Endpoint,
    to: task_manager_peer::iroh::EndpointAddr,
    count: u64,
) -> Result<(), String> {
    let mut seen = 0;
    let mut sample_error = None;
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        status::watch_status(ep, to, Duration::from_secs(20), |event| match event {
            StatusEvent::Snapshot { seq, value } => {
                println!(
                    "{}",
                    serde_json::json!({"event":"snapshot", "seq":seq, "snapshot":value})
                );
                seen += 1;
                seen < count
            }
            StatusEvent::Error { error, .. } => {
                sample_error = Some(error);
                false
            }
        }),
    )
    .await
    .map_err(|_| "status probe timed out".to_owned())?;
    result.map_err(|e| e.to_string())?;
    if let Some(error) = sample_error {
        return Err(format!("remote status error: {error}"));
    }
    if seen != count {
        return Err(format!("expected {count} snapshots, received {seen}"));
    }
    Ok(())
}

async fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("init") if args.len() == 2 => {
            let key = config::create_secret_key(Path::new(&args[1]))?;
            println!("{}", key.public());
            Ok(())
        }
        Some("serve") if args.len() >= 4 => {
            let bind: SocketAddr = args[2].parse().map_err(|e| format!("bind: {e}"))?;
            let seconds = bounded(&args[3], 3600)?;
            let ids = args[4..]
                .iter()
                .map(|s| s.parse::<PublicKey>().map_err(|e| e.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            let count = ids.len();
            let key = config::load_secret_key(Path::new(&args[1]))?;
            let ep = lan_endpoint(key, bind, vec![status::STATUS_ALPN.to_vec()]).await?;
            let network = LanReport::of(&ep)?;
            if !network.is_relay_and_lookup_free() {
                ep.close().await;
                return Err("unexpected relay/lookup configuration".into());
            }
            println!(
                "{}",
                serde_json::json!({ "event":"ready", "endpoint_id":ep.id().to_string(),
                "network":network, "allowlisted_peers":count, "capabilities":["status"],
                "execution_enabled":false, "duration_s":seconds })
            );
            let server = tokio::spawn(server::serve(
                ep.clone(),
                services(ids),
                Arc::new(|line| eprintln!("{line}")),
            ));
            let result = tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(seconds)) => Ok(()),
                signal = tokio::signal::ctrl_c() => signal.map_err(|e| e.to_string()),
            };
            ep.close().await;
            server.await.map_err(|e| e.to_string())?;
            result
        }
        Some("watch") if (5..=6).contains(&args.len()) => {
            let bind: SocketAddr = args[2].parse().map_err(|e| format!("bind: {e}"))?;
            let id: PublicKey = args[3].parse().map_err(|e| format!("remote id: {e}"))?;
            let remote: SocketAddr = args[4]
                .parse()
                .map_err(|e| format!("remote address: {e}"))?;
            let count = match args.get(5) {
                Some(n) => bounded(n, 10)?,
                None => 1,
            };
            let key = config::load_secret_key(Path::new(&args[1]))?;
            let ep = lan_endpoint(key, bind, vec![]).await?;
            eprintln!(
                "local endpoint {} -> authenticated remote {id} at {remote}; status only",
                ep.id()
            );
            let result = watch(&ep, peer_addr(id, remote), count).await;
            ep.close().await;
            result
        }
        _ => Err(USAGE.into()),
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    if let Err(error) = run(&std::env::args().skip(1).collect::<Vec<_>>()).await {
        eprintln!("status_probe: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use task_manager_peer::iroh::SecretKey;
    use task_manager_peer::{ClientError, direct_addr};

    #[test]
    fn limits_reject_zero_and_unbounded_runs() {
        assert!(bounded("0", 3600).is_err());
        assert!(bounded("3601", 3600).is_err());
        assert!(bounded("-1", 10).is_err());
        assert_eq!(bounded("3", 10).unwrap(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_status_round_trip_and_unknown_identity_refusal() {
        let client = lan_endpoint(
            SecretKey::generate(),
            "127.0.0.1:0".parse().unwrap(),
            vec![],
        )
        .await
        .unwrap();
        let server = lan_endpoint(
            SecretKey::generate(),
            "127.0.0.1:0".parse().unwrap(),
            vec![status::STATUS_ALPN.to_vec()],
        )
        .await
        .unwrap();
        let to = direct_addr(&server).unwrap();
        assert!(LanReport::of(&server).unwrap().is_relay_and_lookup_free());
        let svc = services(vec![client.id()]);
        assert!(svc.jobs.is_none() && svc.source.is_none());
        assert!(
            !svc.allow.peers[0]
                .role
                .permits(task_manager_peer::Capability::Jobs)
        );
        let task = tokio::spawn(server::serve(server.clone(), svc, Arc::new(|_| {})));
        watch(&client, to.clone(), 1).await.unwrap();
        let unknown = lan_endpoint(
            SecretKey::generate(),
            "127.0.0.1:0".parse().unwrap(),
            vec![],
        )
        .await
        .unwrap();
        let result = status::watch_status(&unknown, to, Duration::from_secs(5), |_| false).await;
        assert!(matches!(result, Err(ClientError::Refused(_))), "{result:?}");
        unknown.close().await;
        client.close().await;
        server.close().await;
        task.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn job_source_and_artifact_alpns_are_not_negotiable() {
        let server = lan_endpoint(
            SecretKey::generate(),
            "127.0.0.1:0".parse().unwrap(),
            vec![status::STATUS_ALPN.to_vec()],
        )
        .await
        .unwrap();
        let client = lan_endpoint(
            SecretKey::generate(),
            "127.0.0.1:0".parse().unwrap(),
            vec![],
        )
        .await
        .unwrap();
        let task = tokio::spawn(server::serve(
            server.clone(),
            services(vec![client.id()]),
            Arc::new(|_| {}),
        ));
        for alpn in [
            task_manager_peer::jobs::JOBS_ALPN,
            task_manager_peer::source::SOURCE_ALPN,
            task_manager_peer::artifact::ARTIFACT_ALPN,
        ] {
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                client.connect(direct_addr(&server).unwrap(), alpn),
            )
            .await;
            assert!(
                matches!(result, Ok(Err(_))),
                "disabled ALPN must fail negotiation"
            );
        }
        client.close().await;
        server.close().await;
        task.await.unwrap();
    }
}
