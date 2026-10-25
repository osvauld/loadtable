//! The real transport over iroh (feature `iroh`): one endpoint keyed by the machine's device key,
//! iroh's public relays and address discovery by default, one ALPN, admission by grant before
//! any request is read, one bidirectional stream per request, and a long-lived stream per event
//! subscription.
//!
//! Frames on a stream are `u32` big-endian length + the JSON of [`crate::proto`]. Fetched outputs
//! travel on a second ALPN, iroh-blobs ([`crate::iroh_blobs_shelf`], R-795).
//! The blocking [`Transport`] methods use a tokio runtime handle and must be
//! called from outside that runtime's worker threads (the daemon's job threads).

use crate::ids::{PeerId, RemoteJobId};
use crate::proto::{self, ALPN, Body, MAX_CONTROL_BYTES, MAX_FRAME_BYTES, Refusal};
use crate::transport::{
    Closer, Connection, DialHints, EventStream, Handler, LinkInfo, PathKind, PeerError, StreamPoll,
    Transport, Upload,
};
use crate::trust::{AddrSource, AddressBook, SelfAddr, SelfReach};
use iroh::endpoint::{NetReportConfig, PortmapperConfig, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMode, RelayUrl, SecretKey, Watcher};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::{Receiver as StdReceiver, TryRecvError, channel};
use std::time::Duration;
use tokio::runtime::Handle;

/// Idle time after which the serving side sends a heartbeat on a subscription.
pub const HEARTBEAT: Duration = Duration::from_secs(15);
/// A raw upload that makes no progress this long is abandoned by the receiving side.
pub const UPLOAD_IDLE: Duration = Duration::from_secs(120);
/// A connection that has not presented a valid grant this long after its last stream is closed.
pub const PRE_ADMISSION: Duration = Duration::from_secs(10);

/// Why the endpoint could not be built.
#[derive(Debug, thiserror::Error)]
pub enum IrohError {
    /// The endpoint could not be built.
    #[error("iroh endpoint: {0}")]
    Endpoint(String),
    /// A relay URL does not parse.
    #[error("relay url {url:?}: {detail}")]
    Relay {
        /// The text.
        url: String,
        /// What was wrong.
        detail: String,
    },
}

/// Which relays the endpoint uses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RelayChoice {
    /// iroh's public default relays, with DNS/pkarr address discovery (design D2).
    #[default]
    Default,
    /// A self-hosted relay at this URL, with the same discovery.
    Custom(String),
    /// No relay and no discovery: direct addresses only (LAN, or a test in one process).
    Disabled,
}

/// How to build the endpoint.
#[derive(Debug, Clone, Default)]
pub struct EndpointOptions {
    /// Relays.
    pub relay: RelayChoice,
    /// A fixed UDP socket to bind (never unspecified when set); `None` lets iroh choose.
    pub bind: Option<SocketAddr>,
}

/// Builds the endpoint. The endpoint key is the machine's device key.
pub async fn endpoint(secret: SecretKey, opts: &EndpointOptions) -> Result<Endpoint, IrohError> {
    let err = |e: &dyn std::fmt::Display| IrohError::Endpoint(e.to_string());
    let mut b = match &opts.relay {
        RelayChoice::Default => Endpoint::builder(presets::N0),
        RelayChoice::Custom(url) => {
            let u: RelayUrl =
                url.parse()
                    .map_err(|e: iroh::RelayUrlParseError| IrohError::Relay {
                        url: url.clone(),
                        detail: e.to_string(),
                    })?;
            Endpoint::builder(presets::N0).relay_mode(RelayMode::custom([u]))
        }
        RelayChoice::Disabled => Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .clear_address_lookup()
            .portmapper_config(PortmapperConfig::Disabled)
            .net_report_config(NetReportConfig::minimal()),
    };
    if let Some(bind) = opts.bind {
        b = b
            .clear_ip_transports()
            .bind_addr(bind)
            .map_err(|e| err(&e))?;
    }
    b.secret_key(secret)
        .alpns(vec![ALPN.to_vec(), iroh_blobs::ALPN.to_vec()])
        .bind()
        .await
        .map_err(|e| err(&e))
}

fn endpoint_addr(peer: &PeerId, hints: &DialHints) -> Result<EndpointAddr, PeerError> {
    let offline = |detail: String| PeerError::Offline {
        peer: peer.to_string(),
        detail,
    };
    let bytes = tasks_identity::hex::decode::<32>(peer.as_str())
        .ok_or_else(|| offline("not a node id".to_string()))?;
    let key = PublicKey::from_bytes(&bytes).map_err(|e| offline(e.to_string()))?;
    let mut addr = EndpointAddr::new(key);
    for a in &hints.direct_addrs {
        if let Ok(sa) = a.parse::<SocketAddr>() {
            addr = addr.with_ip_addr(sa);
        }
    }
    if let Some(Ok(u)) = hints.relay.as_ref().map(|r| r.parse::<RelayUrl>()) {
        addr = addr.with_relay_url(u);
    }
    Ok(addr)
}

/// This endpoint's reachability, as it goes into tickets.
pub struct EndpointAddrs(pub Endpoint);

impl AddrSource for EndpointAddrs {
    fn current(&self) -> SelfAddr {
        let a = self.0.addr();
        SelfAddr {
            relay: a.relay_urls().next().map(ToString::to_string),
            direct_addrs: a.ip_addrs().map(ToString::to_string).collect(),
        }
    }

    fn reach(&self) -> SelfReach {
        let a = self.current();
        let mut watch = self.0.home_relay_status();
        let homes = watch.get();
        // the relay in the address is the one chosen; its status says whether it is up
        let home = homes
            .iter()
            .find(|r| Some(r.url().to_string()) == a.relay)
            .or_else(|| homes.first());
        SelfReach {
            relay: a.relay.or_else(|| home.map(|r| r.url().to_string())),
            relay_connected: home.map(|r| r.is_connected()),
            relay_error: home
                .filter(|r| !r.is_connected())
                .and_then(|r| r.last_error().map(ToString::to_string)),
            direct_addrs: a.direct_addrs,
        }
    }
}

/// The relay and direct addresses a set of open paths reach the peer at.
fn hints_of_paths(paths: &iroh::endpoint::PathList<'_>) -> DialHints {
    let mut hints = DialHints::default();
    for p in paths.iter() {
        match p.remote_addr() {
            iroh::TransportAddr::Relay(u) => hints.relay = Some(u.to_string()),
            iroh::TransportAddr::Ip(sa) => hints.direct_addrs.push(sa.to_string()),
            _ => {}
        }
    }
    hints
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
    read_frame_limited(recv, MAX_FRAME_BYTES).await
}

/// Reads one frame of at most `max` bytes; the length is checked before anything is allocated.
async fn read_frame_limited(recv: &mut RecvStream, max: usize) -> Result<Body, PeerError> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len)
        .await
        .map_err(|e| PeerError::LinkLost(e.to_string()))?;
    let len = usize::try_from(u32::from_be_bytes(len)).unwrap_or(usize::MAX);
    if len > max {
        return Err(PeerError::Refused(Refusal::TooLarge {
            size: len,
            limit: max,
        }));
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| PeerError::LinkLost(e.to_string()))?;
    proto::decode(&buf).map_err(PeerError::Refused)
}

/// The client half: dials paired peers at the addresses the address book remembers.
#[derive(Clone)]
pub struct IrohTransport {
    endpoint: Endpoint,
    rt: Handle,
    book: Arc<dyn AddressBook>,
}

impl IrohTransport {
    /// A transport that dials the peers `book` knows.
    pub fn new(endpoint: Endpoint, rt: Handle, book: Arc<dyn AddressBook>) -> Self {
        IrohTransport { endpoint, rt, book }
    }

    fn dial(
        &self,
        peer: &PeerId,
        hints: &DialHints,
        timeout: Duration,
    ) -> Result<Box<dyn Connection>, PeerError> {
        let offline = |detail: String| PeerError::Offline {
            peer: peer.to_string(),
            detail,
        };
        let addr = endpoint_addr(peer, hints)?;
        let ep = self.endpoint.clone();
        // iroh checks the TLS identity of the answering endpoint against the key in `addr`, so a
        // machine that does not hold the ticket's device key cannot answer.
        let conn = self
            .rt
            .block_on(async move { tokio::time::timeout(timeout, ep.connect(addr, ALPN)).await })
            .map_err(|_| offline(format!("connect timed out after {timeout:?}")))?
            .map_err(|e| offline(e.to_string()))?;
        Ok(Box::new(IrohConnection {
            conn,
            endpoint: self.endpoint.clone(),
            rt: self.rt.clone(),
        }))
    }
}

impl Transport for IrohTransport {
    fn connect(&self, peer: &PeerId, timeout: Duration) -> Result<Box<dyn Connection>, PeerError> {
        let hints = self.book.hints(peer).ok_or_else(|| PeerError::Offline {
            peer: peer.to_string(),
            detail: "not a paired machine".to_string(),
        })?;
        self.dial(peer, &hints, timeout)
    }

    fn connect_hinted(
        &self,
        peer: &PeerId,
        hints: &DialHints,
        timeout: Duration,
    ) -> Result<Box<dyn Connection>, PeerError> {
        self.dial(peer, hints, timeout)
    }
}

struct IrohConnection {
    conn: iroh::endpoint::Connection,
    /// For the blobs connection to the same peer.
    endpoint: Endpoint,
    rt: Handle,
}

/// Why a blob stream failed, as a link error.
fn blob_err(what: &str, e: impl std::fmt::Display) -> PeerError {
    PeerError::LinkLost(format!("blob stream: {what}: {e}"))
}

/// Streams `hash` on `conn` (blobs ALPN) into `sink`, verifying every piece against the BLAKE3
/// tree; a stall of [`UPLOAD_IDLE`] ends it.
async fn stream_blob(
    conn: iroh::endpoint::Connection,
    hash: iroh_blobs::Hash,
    size: u64,
    sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
) -> Result<(), PeerError> {
    use bao_tree::io::BaoContentItem;
    use iroh_blobs::get::fsm;
    let idle = |what: &'static str| move |_| blob_err(what, "stalled");
    let request = iroh_blobs::protocol::GetRequest::blob(hash);
    let connected = fsm::start(conn, request, Default::default())
        .next()
        .await
        .map_err(|e| blob_err("request", e))?;
    let fsm::ConnectedNext::StartRoot(start) =
        connected.next().await.map_err(|e| blob_err("start", e))?
    else {
        return Err(blob_err("start", "not a single blob"));
    };
    let (mut content, announced) = tokio::time::timeout(UPLOAD_IDLE, start.next().next())
        .await
        .map_err(idle("header"))?
        .map_err(|e| blob_err("header", e))?;
    if announced != size {
        return Err(blob_err(
            "header",
            format!("{announced} bytes, the listing said {size}"),
        ));
    }
    let mut at = 0u64;
    let end = loop {
        match tokio::time::timeout(UPLOAD_IDLE, content.next())
            .await
            .map_err(idle("content"))?
        {
            fsm::BlobContentNext::More((next, item)) => {
                content = next;
                match item.map_err(|e| blob_err("verification", e))? {
                    BaoContentItem::Parent(_) => {}
                    BaoContentItem::Leaf(leaf) => {
                        if leaf.offset != at {
                            return Err(blob_err("content", "pieces out of order"));
                        }
                        sink(&leaf.data).map_err(|e| blob_err("writing", e))?;
                        at += leaf.data.len() as u64;
                    }
                }
            }
            fsm::BlobContentNext::Done(end) => break end,
        }
    };
    if let fsm::EndBlobNext::Closing(closing) = end.next() {
        let _ = closing.next().await;
    }
    Ok(())
}

impl Connection for IrohConnection {
    fn link_info(&self) -> LinkInfo {
        let paths = self.conn.paths();
        match paths.iter().find(|p| p.is_selected()) {
            Some(p) => LinkInfo {
                path: if p.is_relay() {
                    PathKind::Relay
                } else {
                    PathKind::Direct
                },
                rtt: Some(p.rtt()),
            },
            None => LinkInfo::UNKNOWN,
        }
    }

    fn remote_hints(&self) -> Option<DialHints> {
        Some(hints_of_paths(&self.conn.paths()))
    }

    fn fetch_blob(
        &mut self,
        blob: &str,
        size: u64,
        sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
    ) -> Result<(), PeerError> {
        let hash: iroh_blobs::Hash = blob
            .parse()
            .map_err(|e| PeerError::Protocol(format!("blob hash {blob:?}: {e}")))?;
        let (ep, remote) = (self.endpoint.clone(), self.conn.remote_id());
        // a connection of its own on the blobs ALPN, over the paths iroh already has to the peer
        let conn = self
            .rt
            .block_on(async move {
                tokio::time::timeout(UPLOAD_IDLE, ep.connect(remote, iroh_blobs::ALPN)).await
            })
            .map_err(|_| blob_err("connect", "timed out"))?
            .map_err(|e| blob_err("connect", e))?;
        let streamed = self
            .rt
            .block_on(stream_blob(conn.clone(), hash, size, sink));
        conn.close(0u32.into(), b"done");
        streamed
    }

    fn open_upload(&mut self, head: Body) -> Result<Box<dyn Upload>, PeerError> {
        let conn = self.conn.clone();
        let (send, recv) = self.rt.block_on(async move {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| PeerError::LinkLost(e.to_string()))?;
            write_frame(&mut send, &head).await?;
            // the receiver accepts or refuses the head before a byte of data is sent; an older
            // build refuses an unknown message here
            match read_frame(&mut recv).await? {
                Body::Ok => Ok((send, recv)),
                Body::Refuse(r) => Err(PeerError::Refused(r)),
                _ => Err(PeerError::Protocol("unexpected answer to an upload".into())),
            }
        })?;
        Ok(Box::new(IrohUpload {
            send: Some(send),
            recv,
            rt: self.rt.clone(),
        }))
    }

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

struct IrohUpload {
    /// Taken when the upload ends; still there on drop means it was abandoned.
    send: Option<SendStream>,
    recv: RecvStream,
    rt: Handle,
}

impl Drop for IrohUpload {
    fn drop(&mut self) {
        if let Some(mut s) = self.send.take() {
            // an abandoned upload must not look like a complete (shorter) one
            let _ = s.reset(0u32.into());
        }
    }
}

impl Upload for IrohUpload {
    fn write(&mut self, data: &[u8]) -> Result<(), PeerError> {
        let Some(send) = self.send.as_mut() else {
            return Err(PeerError::LinkLost("the upload already ended".into()));
        };
        self.rt.block_on(async move {
            send.write_all(data)
                .await
                .map_err(|e| PeerError::LinkLost(e.to_string()))
        })
    }

    fn finish(mut self: Box<Self>) -> Result<Body, PeerError> {
        let Some(mut send) = self.send.take() else {
            return Err(PeerError::LinkLost("the upload already ended".into()));
        };
        let rt = self.rt.clone();
        let recv = &mut self.recv;
        let reply = rt.block_on(async move {
            send.finish()
                .map_err(|e| PeerError::LinkLost(e.to_string()))?;
            read_frame(recv).await
        })?;
        match reply {
            Body::Refuse(r) => Err(PeerError::Refused(r)),
            other => Ok(other),
        }
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

/// Serves `handler` until the endpoint closes. The handler is the admission gate
/// ([`crate::trust::TrustedHandler`]): iroh proves the dialer's node id, the handler decides on
/// the grant in the first frame. A connection that stays unadmitted past [`PRE_ADMISSION`] is
/// closed, and until it is admitted only small control frames are read.
///
/// Connections on the blobs ALPN go to `blobs`, which admits only what fetch listings granted
/// ([`crate::iroh_blobs_shelf`]); without it they are closed.
pub async fn serve(
    endpoint: Endpoint,
    handler: Arc<dyn Handler>,
    blobs: Option<iroh_blobs::BlobsProtocol>,
) {
    use iroh::protocol::ProtocolHandler;
    while let Some(incoming) = endpoint.accept().await {
        let handler = Arc::clone(&handler);
        let blobs = blobs.clone();
        tokio::spawn(async move {
            let Ok(accepting) = incoming.accept() else {
                return;
            };
            let Ok(conn) = accepting.await else {
                return;
            };
            if conn.alpn() == iroh_blobs::ALPN {
                match blobs {
                    Some(b) => {
                        let _ = b.accept(conn).await;
                    }
                    None => conn.close(4u32.into(), b"no blob store"),
                }
                return;
            }
            let Ok(peer) = PeerId::new(&conn.remote_id().to_string()) else {
                conn.close(1u32.into(), b"bad identity");
                return;
            };
            let closer: Closer = {
                let conn = conn.clone();
                Arc::new(move |why: &str| {
                    let short: String = why.chars().take(64).collect();
                    conn.close(3u32.into(), short.as_bytes());
                })
            };
            let id = handler.connection_opened(&peer, closer);
            loop {
                let admitted = handler.is_admitted(&peer);
                let next = if admitted {
                    conn.accept_bi().await.ok()
                } else {
                    match tokio::time::timeout(PRE_ADMISSION, conn.accept_bi()).await {
                        Ok(r) => r.ok(),
                        Err(_) => {
                            conn.close(2u32.into(), b"no grant presented");
                            None
                        }
                    }
                };
                let Some((send, recv)) = next else { break };
                let (handler, peer) = (Arc::clone(&handler), peer.clone());
                tokio::spawn(async move {
                    let _ = serve_stream(handler, peer, send, recv, admitted).await;
                });
            }
            handler.connection_closed(&peer, id, "peer left");
        });
    }
}

async fn serve_stream(
    handler: Arc<dyn Handler>,
    peer: PeerId,
    mut send: SendStream,
    mut recv: RecvStream,
    admitted: bool,
) -> Result<(), PeerError> {
    let limit = if admitted {
        MAX_FRAME_BYTES
    } else {
        MAX_CONTROL_BYTES
    };
    let msg = match read_frame_limited(&mut recv, limit).await {
        Ok(m) => m,
        Err(PeerError::Refused(r)) => {
            write_frame(&mut send, &Body::Refuse(r)).await?;
            let _ = send.finish();
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    if matches!(msg, Body::SyncPackStream { .. }) {
        return serve_upload(handler, peer, send, recv, msg).await;
    }
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

/// Bridges the blocking [`std::io::Read`] a handler consumes to a QUIC receive stream. Runs on a
/// blocking thread; each read waits at most [`UPLOAD_IDLE`].
struct StreamReader {
    recv: RecvStream,
    rt: Handle,
}

impl std::io::Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let recv = &mut self.recv;
        let got = self
            .rt
            .block_on(async move { tokio::time::timeout(UPLOAD_IDLE, recv.read(buf)).await });
        match got {
            Ok(Ok(n)) => Ok(n.unwrap_or(0)),
            Ok(Err(e)) => Err(std::io::Error::other(e.to_string())),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the upload stalled",
            )),
        }
    }
}

/// A raw upload: the head was read; accept or refuse it, then hand the rest of the stream to the
/// handler and answer with its reply.
async fn serve_upload(
    handler: Arc<dyn Handler>,
    peer: PeerId,
    mut send: SendStream,
    recv: RecvStream,
    head: Body,
) -> Result<(), PeerError> {
    let (h, p, hd) = (Arc::clone(&handler), peer.clone(), head.clone());
    let accepted = tokio::task::spawn_blocking(move || h.accept_upload(&p, &hd))
        .await
        .map_err(|e| PeerError::Protocol(e.to_string()))?;
    if let Err(r) = accepted {
        write_frame(&mut send, &Body::Refuse(r)).await?;
        let _ = send.finish();
        return Ok(());
    }
    write_frame(&mut send, &Body::Ok).await?;
    let rt = Handle::current();
    let reply = tokio::task::spawn_blocking(move || {
        let mut reader = StreamReader { recv, rt };
        handler.upload(&peer, head, &mut reader)
    })
    .await
    .map_err(|e| PeerError::Protocol(e.to_string()))?;
    write_frame(&mut send, &reply).await?;
    let _ = send.finish();
    Ok(())
}
