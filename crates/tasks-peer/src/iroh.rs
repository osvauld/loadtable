//! The real transport over iroh (feature `iroh`): LAN-only endpoint, one ALPN,
//! admission by node id before any stream is read, one bidirectional stream per
//! request, and a long-lived stream per event subscription.
//!
//! Frames on a stream are `u32` big-endian length + the JSON of [`crate::proto`].
//! The blocking [`Transport`] methods use a tokio runtime handle and must be
//! called from outside that runtime's worker threads (the daemon's job threads).

use crate::ids::{PeerId, RemoteJobId};
use crate::proto::{self, ALPN, Body, MAX_FRAME_BYTES, Refusal};
use crate::receiver::Allowlist;
use crate::transport::{Connection, EventStream, Handler, PeerError, StreamPoll, Transport};
use iroh::endpoint::{NetReportConfig, PortmapperConfig, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, RelayMode, SecretKey};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::{Receiver as StdReceiver, TryRecvError, channel};
use std::time::Duration;
use tokio::runtime::Handle;

/// Idle time after which the serving side sends a heartbeat on a subscription.
pub const HEARTBEAT: Duration = Duration::from_secs(15);

/// Why the endpoint could not be built.
#[derive(Debug, thiserror::Error)]
pub enum IrohError {
    /// Binding to the unspecified address is refused.
    #[error("refusing unspecified bind {0}; name the LAN interface address")]
    UnspecifiedBind(SocketAddr),
    /// The endpoint could not be built.
    #[error("iroh endpoint: {0}")]
    Endpoint(String),
}

/// Builds a LAN-only endpoint: no relays, no address lookup, no port mapping,
/// exactly one caller-chosen socket.
pub async fn lan_endpoint(secret: SecretKey, bind: SocketAddr) -> Result<Endpoint, IrohError> {
    if bind.ip().is_unspecified() {
        return Err(IrohError::UnspecifiedBind(bind));
    }
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .clear_address_lookup()
        .portmapper_config(PortmapperConfig::Disabled)
        .net_report_config(NetReportConfig::minimal())
        .clear_ip_transports()
        .bind_addr(bind)
        .map_err(|e| IrohError::Endpoint(e.to_string()))?
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .map_err(|e| IrohError::Endpoint(e.to_string()))
}

async fn write_frame(send: &mut SendStream, body: &Body) -> Result<(), PeerError> {
    let bytes = proto::encode(body).map_err(PeerError::Refused)?;
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(&bytes);
    send.write_all(&out)
        .await
        .map_err(|e| PeerError::LinkLost(e.to_string()))
}

async fn read_frame(recv: &mut RecvStream) -> Result<Body, PeerError> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len)
        .await
        .map_err(|e| PeerError::LinkLost(e.to_string()))?;
    let len = usize::try_from(u32::from_be_bytes(len)).unwrap_or(usize::MAX);
    if len > MAX_FRAME_BYTES {
        return Err(PeerError::Refused(Refusal::TooLarge {
            size: len,
            limit: MAX_FRAME_BYTES,
        }));
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| PeerError::LinkLost(e.to_string()))?;
    proto::decode(&buf).map_err(PeerError::Refused)
}

/// The client half: knows where its allowlisted peers are.
pub struct IrohTransport {
    endpoint: Endpoint,
    rt: Handle,
    peers: BTreeMap<PeerId, EndpointAddr>,
}

impl IrohTransport {
    /// A transport that can reach `peers`.
    pub fn new(endpoint: Endpoint, rt: Handle, peers: BTreeMap<PeerId, EndpointAddr>) -> Self {
        IrohTransport {
            endpoint,
            rt,
            peers,
        }
    }
}

impl Transport for IrohTransport {
    fn connect(&self, peer: &PeerId, timeout: Duration) -> Result<Box<dyn Connection>, PeerError> {
        let offline = |detail: String| PeerError::Offline {
            peer: peer.to_string(),
            detail,
        };
        let addr = self
            .peers
            .get(peer)
            .cloned()
            .ok_or_else(|| offline("no address configured".to_string()))?;
        let ep = self.endpoint.clone();
        let conn = self
            .rt
            .block_on(async move { tokio::time::timeout(timeout, ep.connect(addr, ALPN)).await })
            .map_err(|_| offline(format!("connect timed out after {timeout:?}")))?
            .map_err(|e| offline(e.to_string()))?;
        Ok(Box::new(IrohConnection {
            conn,
            rt: self.rt.clone(),
        }))
    }
}

struct IrohConnection {
    conn: iroh::endpoint::Connection,
    rt: Handle,
}

impl Connection for IrohConnection {
    fn request(&mut self, msg: Body) -> Result<Body, PeerError> {
        let conn = self.conn.clone();
        let reply = self.rt.block_on(async move {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| PeerError::LinkLost(e.to_string()))?;
            write_frame(&mut send, &msg).await?;
            let _ = send.finish();
            read_frame(&mut recv).await
        })?;
        match reply {
            Body::Refuse(r) => Err(PeerError::Refused(r)),
            other => Ok(other),
        }
    }

    fn subscribe(
        &mut self,
        remote: RemoteJobId,
        from_offset: u64,
    ) -> Result<Box<dyn EventStream>, PeerError> {
        let conn = self.conn.clone();
        let (tx, rx) = channel::<Result<StreamPoll, PeerError>>();
        let (ready_tx, ready_rx) = channel::<Result<(), PeerError>>();
        self.rt.spawn(async move {
            let opened = async {
                let (mut send, recv) = conn
                    .open_bi()
                    .await
                    .map_err(|e| PeerError::LinkLost(e.to_string()))?;
                write_frame(
                    &mut send,
                    &Body::Subscribe {
                        remote,
                        from_offset,
                    },
                )
                .await?;
                let _ = send.finish();
                Ok::<_, PeerError>(recv)
            }
            .await;
            let mut recv = match opened {
                Ok(r) => {
                    let _ = ready_tx.send(Ok(()));
                    r
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            loop {
                match read_frame(&mut recv).await {
                    Ok(Body::Event(ev)) => {
                        if tx.send(Ok(StreamPoll::Event(ev))).is_err() {
                            return;
                        }
                    }
                    Ok(Body::Ok) => {
                        let _ = tx.send(Ok(StreamPoll::Closed));
                        return;
                    }
                    Ok(Body::Refuse(r)) => {
                        let _ = tx.send(Err(PeerError::Refused(r)));
                        return;
                    }
                    Ok(_) => {
                        let _ = tx.send(Err(PeerError::Protocol("unexpected message".into())));
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                }
            }
        });
        // The first answer decides whether the subscription exists at all.
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(PeerError::LinkLost("subscription task ended".into())),
        }
        Ok(Box::new(IrohEvents { rx }))
    }
}

struct IrohEvents {
    rx: StdReceiver<Result<StreamPoll, PeerError>>,
}

impl EventStream for IrohEvents {
    fn poll(&mut self) -> Result<StreamPoll, PeerError> {
        match self.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => Ok(StreamPoll::Idle),
            Err(TryRecvError::Disconnected) => {
                Err(PeerError::LinkLost("event stream task ended".into()))
            }
        }
    }
}

/// Serves `handler` until the endpoint closes. A connection whose
/// TLS-proven node id is not on `allow` is closed before any stream is read.
pub async fn serve(endpoint: Endpoint, handler: Arc<dyn Handler>, allow: Allowlist) {
    while let Some(incoming) = endpoint.accept().await {
        let handler = Arc::clone(&handler);
        let allow = allow.clone();
        tokio::spawn(async move {
            let Ok(accepting) = incoming.accept() else {
                return;
            };
            let Ok(conn) = accepting.await else {
                return;
            };
            let Ok(peer) = PeerId::new(&conn.remote_id().to_string()) else {
                conn.close(1u32.into(), b"bad identity");
                return;
            };
            if !allow.admits(&peer) {
                conn.close(2u32.into(), b"not allowed");
                return;
            }
            while let Ok((send, recv)) = conn.accept_bi().await {
                let (handler, peer) = (Arc::clone(&handler), peer.clone());
                tokio::spawn(async move {
                    let _ = serve_stream(handler, peer, send, recv).await;
                });
            }
        });
    }
}

async fn serve_stream(
    handler: Arc<dyn Handler>,
    peer: PeerId,
    mut send: SendStream,
    mut recv: RecvStream,
) -> Result<(), PeerError> {
    let msg = match read_frame(&mut recv).await {
        Ok(m) => m,
        Err(PeerError::Refused(r)) => {
            write_frame(&mut send, &Body::Refuse(r)).await?;
            let _ = send.finish();
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    if let Body::Subscribe {
        remote,
        from_offset,
    } = msg
    {
        let h = Arc::clone(&handler);
        let p = peer.clone();
        let opened = tokio::task::spawn_blocking(move || h.events(&p, remote, from_offset))
            .await
            .map_err(|e| PeerError::Protocol(e.to_string()))?;
        let mut stream = match opened {
            Ok(s) => s,
            Err(r) => {
                write_frame(&mut send, &Body::Refuse(r)).await?;
                let _ = send.finish();
                return Ok(());
            }
        };
        let mut idle = Duration::ZERO;
        loop {
            let (s, polled) = tokio::task::spawn_blocking(move || {
                let r = stream.poll();
                (stream, r)
            })
            .await
            .map_err(|e| PeerError::Protocol(e.to_string()))?;
            stream = s;
            match polled {
                Ok(StreamPoll::Event(ev)) => {
                    idle = Duration::ZERO;
                    write_frame(&mut send, &Body::Event(ev)).await?;
                }
                Ok(StreamPoll::Idle) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    idle += Duration::from_millis(50);
                    if idle >= HEARTBEAT {
                        idle = Duration::ZERO;
                        write_frame(&mut send, &Body::Event(proto::JobEvent::Heartbeat)).await?;
                    }
                }
                Ok(StreamPoll::Closed) => {
                    write_frame(&mut send, &Body::Ok).await?;
                    let _ = send.finish();
                    return Ok(());
                }
                Err(PeerError::Refused(r)) => {
                    write_frame(&mut send, &Body::Refuse(r)).await?;
                    let _ = send.finish();
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
    }
    let reply = tokio::task::spawn_blocking(move || handler.handle(&peer, msg))
        .await
        .map_err(|e| PeerError::Protocol(e.to_string()))?;
    write_frame(&mut send, &reply).await?;
    let _ = send.finish();
    Ok(())
}
