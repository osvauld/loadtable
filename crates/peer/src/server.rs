//! The daemon's inbound accept loop: one LAN-only endpoint, four ALPNs, allowlist admission
//! (refusals are logged), bounded concurrency.

use std::sync::Arc;
use std::time::Duration;

use iroh::Endpoint;
use iroh::endpoint::VarInt;
use tokio::sync::Semaphore;

use crate::artifact::{ARTIFACT_ALPN, ArtifactOutcome, handle_artifact};
use crate::jobs::{JOBS_ALPN, JobHost, JobsOutcome, JobsTiming, handle_jobs};
use crate::source::{SOURCE_ALPN, SourceOutcome, SourceService, handle_source};
use crate::status::{STATUS_ALPN, StatusHub, StatusOutcome, handle_status};
use crate::{Allowlist, HANDSHAKE_TIMEOUT};

/// Connections handled at once; more wait in the endpoint's accept queue.
pub const MAX_CONNECTIONS: usize = 64;

pub fn alpns() -> Vec<Vec<u8>> {
    vec![
        STATUS_ALPN.to_vec(),
        JOBS_ALPN.to_vec(),
        SOURCE_ALPN.to_vec(),
        ARTIFACT_ALPN.to_vec(),
    ]
}

pub struct Services {
    pub allow: Allowlist,
    pub status: Option<Arc<StatusHub>>,
    pub jobs: Option<(Arc<dyn JobHost>, JobsTiming)>,
    pub source: Option<Arc<SourceService>>,
}

pub type Logger = Arc<dyn Fn(String) + Send + Sync>;

fn alpn_name(alpn: &[u8]) -> String {
    String::from_utf8_lossy(alpn).into_owned()
}

/// Runs until the endpoint is closed.
pub async fn serve(ep: Endpoint, services: Arc<Services>, log: Logger) {
    let limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    while let Some(incoming) = ep.accept().await {
        let Ok(permit) = Arc::clone(&limit).acquire_owned().await else {
            return;
        };
        let (ep, services, log) = (ep.clone(), Arc::clone(&services), Arc::clone(&log));
        tokio::spawn(async move {
            let _permit = permit;
            let accepting = match incoming.accept() {
                Ok(a) => a,
                Err(e) => {
                    log(format!("peer: incoming connection failed: {e}"));
                    return;
                }
            };
            let conn = match tokio::time::timeout(HANDSHAKE_TIMEOUT, accepting).await {
                Ok(Ok(conn)) => conn,
                Ok(Err(e)) => {
                    log(format!("peer: TLS handshake failed: {e}"));
                    return;
                }
                Err(_) => {
                    log("peer: TLS handshake timed out".into());
                    return;
                }
            };
            let alpn = conn.alpn().to_vec();
            let remote = conn.remote_id();
            let who = match services.allow.get(&remote) {
                Some(p) => format!("{} ({})", p.name, remote.fmt_short()),
                None => format!("unknown endpoint {remote}"),
            };
            let what = alpn_name(&alpn);
            let refused = |reason: &str| format!("peer: REFUSED {who} on {what}: {reason}");
            let report = if alpn == STATUS_ALPN {
                match &services.status {
                    Some(hub) => match handle_status(&ep, conn.clone(), &services.allow, hub).await
                    {
                        Ok(StatusOutcome::Rejected { reason, .. }) => refused(&reason),
                        Ok(StatusOutcome::Ended {
                            snapshots_sent,
                            reason,
                            ..
                        }) => format!(
                            "peer: status stream to {who} ended after {snapshots_sent} snapshots ({reason})"
                        ),
                        Err(e) => format!("peer: status stream with {who} failed: {e}"),
                    },
                    None => refused("status not served"),
                }
            } else if alpn == JOBS_ALPN {
                match &services.jobs {
                    Some((host, timing)) => {
                        match handle_jobs(
                            &ep,
                            conn.clone(),
                            &services.allow,
                            Arc::clone(host),
                            *timing,
                        )
                        .await
                        {
                            Ok(JobsOutcome::Rejected { reason, .. }) => refused(&reason),
                            Ok(JobsOutcome::Handled { what: done, .. }) => {
                                format!("peer: jobs {done} for {who}")
                            }
                            Err(e) => format!("peer: jobs request from {who} failed: {e}"),
                        }
                    }
                    None => refused("jobs not served"),
                }
            } else if alpn == SOURCE_ALPN {
                match &services.source {
                    Some(svc) => match handle_source(&ep, conn.clone(), &services.allow, svc).await
                    {
                        Ok(SourceOutcome::Rejected { reason, .. }) => refused(&reason),
                        Ok(SourceOutcome::Synced { reply, .. }) => format!(
                            "peer: synced {} for {who}: {} files, {} written, {} unchanged, {} deleted, {} blobs/{} bytes received",
                            reply.checkout.display(),
                            reply.files,
                            reply.written,
                            reply.unchanged,
                            reply.deleted,
                            reply.blobs_received,
                            reply.bytes_received
                        ),
                        Err(e) => format!("peer: source sync from {who} failed: {e}"),
                    },
                    None => refused("source sync not served"),
                }
            } else if alpn == ARTIFACT_ALPN {
                match &services.source {
                    Some(svc) => {
                        match handle_artifact(&ep, conn.clone(), &services.allow, svc).await {
                            Ok(ArtifactOutcome::Rejected { reason, .. }) => refused(&reason),
                            Ok(ArtifactOutcome::Served {
                                checkout,
                                files,
                                bytes_sent,
                                ..
                            }) => format!(
                                "peer: served {files} artifact files of {checkout} to {who} ({bytes_sent} bytes)"
                            ),
                            Err(e) => format!("peer: artifact fetch from {who} failed: {e}"),
                        }
                    }
                    None => refused("artifacts not served"),
                }
            } else {
                refused("unknown ALPN")
            };
            log(report);
            conn.close(VarInt::from_u32(0), b"done");
        });
    }
}

/// Grace for in-flight handlers when the daemon shuts the endpoint down.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
