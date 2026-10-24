//! The transport abstraction: request/response plus a stream of events.
//!
//! Everything is synchronous and poll-driven so the state machines can be
//! driven by a manual clock. The iroh adapter (feature `iroh`) wraps a tokio
//! runtime behind these traits; the in-memory [`crate::fake`] network backs
//! every test.

use crate::ids::{PeerId, RemoteJobId};
use crate::proto::{Body, JobEvent, Refusal};
use std::time::Duration;

/// Why a transport call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PeerError {
    /// Could not reach the peer; the request was not delivered.
    #[error("peer {peer} is offline: {detail}")]
    Offline {
        /// The peer.
        peer: String,
        /// Detail.
        detail: String,
    },
    /// The link died after the request may have been delivered.
    #[error("link to the peer was lost: {0}")]
    LinkLost(String),
    /// The request took too long.
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    /// The peer refused with a name.
    #[error("{0}")]
    Refused(Refusal),
    /// The peer answered with something that makes no sense here.
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl PeerError {
    /// True when the request certainly never reached the peer.
    pub fn not_delivered(&self) -> bool {
        matches!(self, PeerError::Offline { .. })
    }
}

/// Where to try reaching a peer: from a ticket or from what the peer last reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DialHints {
    /// The peer's relay.
    pub relay: Option<String>,
    /// Direct socket addresses, tried first.
    pub direct_addrs: Vec<String>,
}

/// How a link currently carries traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Not known (the fake network, or no path yet).
    Unknown,
    /// A direct UDP path (same LAN or hole-punched).
    Direct,
    /// Through a relay.
    Relay,
}

impl PathKind {
    /// The name shown in status.
    pub fn name(self) -> &'static str {
        match self {
            PathKind::Unknown => "unknown",
            PathKind::Direct => "direct",
            PathKind::Relay => "relay",
        }
    }
}

/// What a connection knows about its own path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkInfo {
    /// Direct or relayed.
    pub path: PathKind,
    /// Round-trip time of the selected path.
    pub rtt: Option<Duration>,
}

impl LinkInfo {
    /// Nothing known.
    pub const UNKNOWN: LinkInfo = LinkInfo {
        path: PathKind::Unknown,
        rtt: None,
    };
}

/// Shuts a connection from the serving side, with a reason for the log.
pub type Closer = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// What a poll of an event stream found.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamPoll {
    /// An event.
    Event(JobEvent),
    /// Nothing right now.
    Idle,
    /// The stream ended cleanly (after `Finished`).
    Closed,
}

/// A stream of job events. Dropping it detaches; it never cancels the job.
pub trait EventStream: Send {
    /// Non-blocking poll. `Err` means the link broke.
    fn poll(&mut self) -> Result<StreamPoll, PeerError>;
}

/// A raw byte stream to the peer, opened with [`Connection::open_upload`]. Bytes are delivered in
/// order without a round trip each; the only wait is at the end. Dropping it without
/// [`Upload::finish`] abandons the upload.
pub trait Upload: Send {
    /// Appends bytes to the stream. May block while the link applies back-pressure.
    fn write(&mut self, data: &[u8]) -> Result<(), PeerError>;
    /// Ends the stream and returns the peer's answer. A [`Body::Refuse`] becomes
    /// `Err(Refused)`.
    fn finish(self: Box<Self>) -> Result<Body, PeerError>;
}

/// A live link to one peer.
pub trait Connection: Send {
    /// Opens a raw byte stream introduced by `head` (see [`Body::SyncPackStream`]). The peer
    /// accepts or refuses `head` before any byte is sent; a peer that does not know uploads
    /// refuses, which is how a sender finds out to fall back to chunked requests. The default
    /// refuses.
    fn open_upload(&mut self, head: Body) -> Result<Box<dyn Upload>, PeerError> {
        let _ = head;
        Err(PeerError::Refused(Refusal::Malformed {
            detail: "this link has no raw streams".to_string(),
        }))
    }
    /// One request, one reply. A [`Body::Refuse`] reply becomes `Err(Refused)`.
    fn request(&mut self, msg: Body) -> Result<Body, PeerError>;
    /// Attaches to a job's events from log offset `from_offset`.
    fn subscribe(
        &mut self,
        remote: RemoteJobId,
        from_offset: u64,
    ) -> Result<Box<dyn EventStream>, PeerError>;
    /// How this link carries traffic now.
    fn link_info(&self) -> LinkInfo {
        LinkInfo::UNKNOWN
    }
    /// Where the peer is reachable right now, as this live connection sees it: its relay and the
    /// direct addresses of its open paths. `None` when the transport cannot tell.
    fn remote_hints(&self) -> Option<DialHints> {
        None
    }
}

/// Opens links to peers.
pub trait Transport: Send + Sync {
    /// Connects, failing within `timeout` (an offline peer fails fast, R-7.6).
    fn connect(&self, peer: &PeerId, timeout: Duration) -> Result<Box<dyn Connection>, PeerError>;
    /// Connects to a peer that is not paired yet, using the addresses of a ticket. The default
    /// ignores the hints.
    fn connect_hinted(
        &self,
        peer: &PeerId,
        hints: &DialHints,
        timeout: Duration,
    ) -> Result<Box<dyn Connection>, PeerError> {
        let _ = hints;
        self.connect(peer, timeout)
    }
}

/// The serving side: what a daemon implements (see [`crate::receiver::Receiver`]).
pub trait Handler: Send + Sync {
    /// Answers one request from the authenticated peer `from`.
    fn handle(&self, from: &PeerId, msg: Body) -> Body;
    /// Opens an event source for `from`, or a refusal.
    fn events(
        &self,
        from: &PeerId,
        remote: RemoteJobId,
        from_offset: u64,
    ) -> Result<Box<dyn EventStream>, Refusal>;
    /// Decides, before any byte is read, whether the upload introduced by `head` may proceed.
    /// The default refuses (an older build: the sender falls back to chunked requests).
    fn accept_upload(&self, from: &PeerId, head: &Body) -> Result<(), Refusal> {
        let _ = (from, head);
        Err(Refusal::Malformed {
            detail: "raw streams are not supported".to_string(),
        })
    }
    /// Consumes an upload accepted by [`Handler::accept_upload`] up to its end and answers it.
    fn upload(&self, from: &PeerId, head: Body, data: &mut dyn std::io::Read) -> Body {
        let _ = (from, head, data);
        Body::Refuse(Refusal::Malformed {
            detail: "raw streams are not supported".to_string(),
        })
    }
    /// A connection from `peer` opened; `closer` shuts it. Returns an id for
    /// [`Handler::connection_closed`].
    fn connection_opened(&self, peer: &PeerId, closer: Closer) -> u64 {
        let _ = (peer, closer);
        0
    }
    /// The connection `conn` from `peer` ended.
    fn connection_closed(&self, peer: &PeerId, conn: u64, reason: &str) {
        let _ = (peer, conn, reason);
    }
    /// True once `peer` has presented a grant, so the transport stops its pre-admission limits.
    fn is_admitted(&self, peer: &PeerId) -> bool {
        let _ = peer;
        true
    }
}
