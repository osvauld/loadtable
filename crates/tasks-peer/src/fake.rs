//! An in-memory transport. Every message goes through `proto::encode` and
//! `proto::decode`, so size limits and wire shapes are exercised too.

use crate::ids::{PeerId, RemoteJobId};
use crate::proto::{self, Body, Refusal};
use crate::transport::{
    Closer, Connection, DialHints, EventStream, Handler, LinkInfo, PeerError, StreamPoll,
    Transport, Upload,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

#[derive(Default)]
struct NetState {
    handlers: BTreeMap<PeerId, Arc<dyn Handler>>,
    down: BTreeSet<PeerId>,
    /// Streams opened from now on break after this many events (once).
    break_streams_after: Option<usize>,
    /// Streams opened from now on deliver nothing (a silent peer).
    stall_streams: bool,
    /// Requests of this kind fail with `LinkLost` after the handler ran.
    drop_replies: BTreeSet<&'static str>,
    log: Vec<(PeerId, String)>,
    cut_epoch: u64,
    link: Option<LinkInfo>,
    /// What every connection reports as the peer's current addresses.
    remote_hints: Option<DialHints>,
    /// Real time every round trip takes (a request and its reply; an upload's open and end).
    latency: Duration,
    /// Round trips taken so far (requests, upload openings, upload endings).
    round_trips: u64,
}

/// A set of fake peers that can be taken down, stalled and cut.
#[derive(Clone, Default)]
pub struct FakeNet {
    inner: Arc<Mutex<NetState>>,
}

fn lock(m: &Mutex<NetState>) -> MutexGuard<'_, NetState> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Short name of a request body, for logs and fault injection.
pub fn kind(b: &Body) -> &'static str {
    match b {
        Body::Hello(_) => "hello",
        Body::StatusRequest => "status_request",
        Body::Submit(_) => "submit",
        Body::Cancel { .. } => "cancel",
        Body::SyncOffer { .. } => "sync_offer",
        Body::SyncPackChunk { .. } => "sync_pack_chunk",
        Body::SyncPackStream { .. } => "sync_pack_stream",
        Body::SyncApply(_) => "sync_apply",
        Body::FetchList { .. } => "fetch_list",
        Body::FetchChunk { .. } => "fetch_chunk",
        Body::JobMetricsRequest { .. } => "job_metrics",
        _ => "other",
    }
}

impl FakeNet {
    /// An empty network.
    pub fn new() -> FakeNet {
        FakeNet::default()
    }
    /// Registers (or replaces, as after a restart) the daemon behind `peer`.
    pub fn serve(&self, peer: &PeerId, handler: Arc<dyn Handler>) {
        lock(&self.inner).handlers.insert(peer.clone(), handler);
    }
    /// Takes a peer off the network (or back).
    pub fn set_down(&self, peer: &PeerId, down: bool) {
        let mut s = lock(&self.inner);
        if down {
            s.down.insert(peer.clone());
        } else {
            s.down.remove(peer);
        }
    }
    /// Cuts every stream that is open now.
    pub fn cut_streams(&self) {
        lock(&self.inner).cut_epoch += 1;
    }
    /// Streams opened from now on break after `n` events.
    pub fn break_streams_after(&self, n: Option<usize>) {
        lock(&self.inner).break_streams_after = n;
    }
    /// Makes streams deliver nothing at all.
    pub fn stall_streams(&self, on: bool) {
        let mut s = lock(&self.inner);
        s.stall_streams = on;
    }
    /// Delivers requests of `kind` but loses the reply.
    pub fn drop_replies_of(&self, kind: &'static str, on: bool) {
        let mut s = lock(&self.inner);
        if on {
            s.drop_replies.insert(kind);
        } else {
            s.drop_replies.remove(kind);
        }
    }
    /// What every connection reports as its path and round-trip time.
    pub fn set_link_info(&self, info: Option<LinkInfo>) {
        lock(&self.inner).link = info;
    }
    /// What every connection reports as the peer's relay and direct addresses.
    pub fn set_remote_hints(&self, hints: Option<DialHints>) {
        lock(&self.inner).remote_hints = hints;
    }
    /// Makes every round trip take `latency` of real time: a request, an upload's opening
    /// handshake and its closing answer. Bytes written to an open upload cost nothing, which is
    /// what a stream is for.
    pub fn set_latency(&self, latency: Duration) {
        lock(&self.inner).latency = latency;
    }
    /// How many round trips have been taken: one per request, and two per upload (its opening
    /// handshake and its closing answer), however many bytes the upload carried.
    pub fn round_trips(&self) -> u64 {
        lock(&self.inner).round_trips
    }
    /// The requests seen so far as `(to, kind)`.
    pub fn requests(&self) -> Vec<(PeerId, String)> {
        lock(&self.inner).log.clone()
    }
    /// A transport for the daemon named `me`.
    pub fn transport(&self, me: &PeerId) -> FakeTransport {
        FakeTransport {
            me: me.clone(),
            net: self.clone(),
        }
    }
}

/// The client half of the fake network.
pub struct FakeTransport {
    me: PeerId,
    net: FakeNet,
}

impl Transport for FakeTransport {
    fn connect(&self, peer: &PeerId, _timeout: Duration) -> Result<Box<dyn Connection>, PeerError> {
        let s = lock(&self.net.inner);
        if s.down.contains(peer) || !s.handlers.contains_key(peer) {
            return Err(PeerError::Offline {
                peer: peer.to_string(),
                detail: "no route".to_string(),
            });
        }
        let handler = s.handlers.get(peer).cloned();
        drop(s);
        // The serving side learns who connected (the proven identity is `me`) and gets a way to
        // shut the connection, as the real transport gives it.
        let closed: Arc<Mutex<Option<String>>> = Arc::default();
        let id = handler.as_ref().map_or(0, |h| {
            let flag = closed.clone();
            let closer: Closer = Arc::new(move |why: &str| {
                let mut g = flag.lock().unwrap_or_else(|e| e.into_inner());
                g.get_or_insert_with(|| why.to_string());
            });
            h.connection_opened(&self.me, closer)
        });
        Ok(Box::new(FakeConnection {
            me: self.me.clone(),
            to: peer.clone(),
            net: self.net.clone(),
            closed,
            id,
        }))
    }

    fn connect_hinted(
        &self,
        peer: &PeerId,
        _hints: &DialHints,
        timeout: Duration,
    ) -> Result<Box<dyn Connection>, PeerError> {
        self.connect(peer, timeout)
    }
}

struct FakeConnection {
    me: PeerId,
    to: PeerId,
    net: FakeNet,
    closed: Arc<Mutex<Option<String>>>,
    id: u64,
}

impl Drop for FakeConnection {
    fn drop(&mut self) {
        let handler = lock(&self.net.inner).handlers.get(&self.to).cloned();
        if let Some(h) = handler {
            h.connection_closed(&self.me, self.id, "peer left");
        }
    }
}

impl FakeConnection {
    fn handler(&self) -> Result<Arc<dyn Handler>, PeerError> {
        if let Some(why) = self
            .closed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Err(PeerError::LinkLost(format!("closed by the peer: {why}")));
        }
        let s = lock(&self.net.inner);
        if s.down.contains(&self.to) {
            return Err(PeerError::LinkLost("peer went away".to_string()));
        }
        s.handlers
            .get(&self.to)
            .cloned()
            .ok_or_else(|| PeerError::LinkLost("peer went away".to_string()))
    }
}

fn wire(b: &Body) -> Result<Body, PeerError> {
    let bytes = proto::encode(b).map_err(PeerError::Refused)?;
    proto::decode(&bytes).map_err(PeerError::Refused)
}

impl Connection for FakeConnection {
    fn open_upload(&mut self, head: Body) -> Result<Box<dyn Upload>, PeerError> {
        let handler = self.handler()?;
        let head = wire(&head)?;
        let k = kind(&head);
        let latency = {
            let mut s = lock(&self.net.inner);
            s.log.push((self.to.clone(), k.to_string()));
            s.round_trips += 1;
            s.latency
        };
        std::thread::sleep(latency);
        handler
            .accept_upload(&self.me, &head)
            .map_err(PeerError::Refused)?;
        Ok(Box::new(FakeUpload {
            me: self.me.clone(),
            handler,
            net: self.net.clone(),
            head,
            buf: Vec::new(),
        }))
    }

    fn request(&mut self, msg: Body) -> Result<Body, PeerError> {
        let handler = self.handler()?;
        let msg = wire(&msg)?;
        let k = kind(&msg);
        let latency = {
            let mut s = lock(&self.net.inner);
            s.log.push((self.to.clone(), k.to_string()));
            s.round_trips += 1;
            s.latency
        };
        std::thread::sleep(latency);
        let reply = wire(&handler.handle(&self.me, msg))?;
        if lock(&self.net.inner).drop_replies.contains(k) {
            return Err(PeerError::LinkLost("reply lost".to_string()));
        }
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
        let handler = self.handler()?;
        {
            let mut s = lock(&self.net.inner);
            s.log.push((self.to.clone(), "subscribe".to_string()));
        }
        let inner = handler
            .events(&self.me, remote, from_offset)
            .map_err(PeerError::Refused)?;
        let s = lock(&self.net.inner);
        Ok(Box::new(FakeStream {
            inner,
            net: self.net.clone(),
            to: self.to.clone(),
            cut_epoch: s.cut_epoch,
            left: s.break_streams_after,
            closed: self.closed.clone(),
        }))
    }

    fn link_info(&self) -> LinkInfo {
        lock(&self.net.inner).link.unwrap_or(LinkInfo::UNKNOWN)
    }

    fn remote_hints(&self) -> Option<DialHints> {
        lock(&self.net.inner).remote_hints.clone()
    }
}

struct FakeUpload {
    me: PeerId,
    handler: Arc<dyn Handler>,
    net: FakeNet,
    head: Body,
    buf: Vec<u8>,
}

impl Upload for FakeUpload {
    fn write(&mut self, data: &[u8]) -> Result<(), PeerError> {
        self.buf.extend_from_slice(data);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Body, PeerError> {
        let this = *self;
        let latency = {
            let mut s = lock(&this.net.inner);
            s.round_trips += 1;
            s.latency
        };
        std::thread::sleep(latency);
        let mut data = std::io::Cursor::new(this.buf);
        match wire(&this.handler.upload(&this.me, this.head, &mut data))? {
            Body::Refuse(r) => Err(PeerError::Refused(r)),
            other => Ok(other),
        }
    }
}

struct FakeStream {
    inner: Box<dyn EventStream>,
    net: FakeNet,
    to: PeerId,
    cut_epoch: u64,
    left: Option<usize>,
    closed: Arc<Mutex<Option<String>>>,
}

impl EventStream for FakeStream {
    fn poll(&mut self) -> Result<StreamPoll, PeerError> {
        if let Some(why) = self
            .closed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Err(PeerError::LinkLost(format!("closed by the peer: {why}")));
        }
        {
            let s = lock(&self.net.inner);
            if s.down.contains(&self.to) {
                return Err(PeerError::LinkLost("peer went away".to_string()));
            }
            if s.cut_epoch != self.cut_epoch {
                return Err(PeerError::LinkLost("stream cut".to_string()));
            }
            if s.stall_streams {
                return Ok(StreamPoll::Idle);
            }
        }
        if self.left == Some(0) {
            return Err(PeerError::LinkLost("stream cut".to_string()));
        }
        let p = self.inner.poll()?;
        if let (StreamPoll::Event(_), Some(n)) = (&p, self.left.as_mut()) {
            *n -= 1;
        }
        // Events pass through the codec too.
        if let StreamPoll::Event(e) = p {
            return match wire(&Body::Event(e))? {
                Body::Event(e) => Ok(StreamPoll::Event(e)),
                _ => Err(PeerError::Protocol("event changed shape".to_string())),
            };
        }
        Ok(p)
    }
}

/// A refusal helper for handlers in tests.
pub fn refuse(r: Refusal) -> Body {
    Body::Refuse(r)
}

// ---------------------------------------------------------------------------
// Fake daemon parts, so a daemon's wiring tests and ours share one fake.

use crate::ids::SubmitKey;
use crate::proto::{
    CancelState, PeerStatus, PressureSummary, QueueSummary, WireEnd, WireOutcome, WireSample,
};
use crate::receiver::{FinishedJob, JobRunner, JobView, StatusSource};
use tasks_core::{Arch, JobSpec, Machine, MachineName, Os, Phase};

struct FakeJob {
    peer: PeerId,
    key: SubmitKey,
    spec: JobSpec,
    phase: Phase,
    detail: String,
    log: Vec<u8>,
    finished: Option<FinishedJob>,
    metrics: Option<tasks_core::metrics::JobMetricsView>,
}

/// A runner whose jobs are driven by the test.
#[derive(Default)]
pub struct FakeRunner {
    jobs: Mutex<Vec<FakeJob>>,
}

impl FakeRunner {
    /// An empty runner.
    pub fn new() -> FakeRunner {
        FakeRunner::default()
    }
    fn jobs(&self) -> MutexGuard<'_, Vec<FakeJob>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn with<R>(&self, id: RemoteJobId, f: impl FnOnce(&mut FakeJob) -> R) -> Option<R> {
        let idx = usize::try_from(id.0).ok()?.checked_sub(1)?;
        self.jobs().get_mut(idx).map(f)
    }
    /// Sets a job's phase and detail.
    pub fn set_phase(&self, id: RemoteJobId, phase: Phase, detail: &str) {
        self.with(id, |j| {
            j.phase = phase;
            j.detail = detail.to_string();
        });
    }
    /// Appends log bytes.
    pub fn append_log(&self, id: RemoteJobId, bytes: &[u8]) {
        self.with(id, |j| j.log.extend_from_slice(bytes));
    }
    /// Ends a job with an exit code and samples.
    pub fn finish(&self, id: RemoteJobId, exit: i32, samples: Vec<WireSample>) {
        self.with(id, |j| {
            j.phase = if exit == 0 {
                Phase::Passed
            } else {
                Phase::Failed { exit: Some(exit) }
            };
            j.finished = Some(FinishedJob {
                outcome: WireOutcome {
                    end: WireEnd::Exited,
                    exit_code: Some(exit),
                    signal: None,
                    wall_ms: 1000,
                    rss_bytes: None,
                    cpu_s: None,
                    description: format!("command exit {exit}"),
                },
                samples,
            });
        });
    }
    /// Sets what the job answers to a metrics request.
    pub fn set_metrics(&self, id: RemoteJobId, view: tasks_core::metrics::JobMetricsView) {
        self.with(id, |j| j.metrics = Some(view));
    }
    /// Specs the runner was given, in order, with their keys.
    pub fn submitted(&self) -> Vec<(SubmitKey, JobSpec)> {
        self.jobs()
            .iter()
            .map(|j| (j.key.clone(), j.spec.clone()))
            .collect()
    }
    /// True when a cancel was applied to the job.
    pub fn is_cancelled(&self, id: RemoteJobId) -> bool {
        self.with(id, |j| j.phase == Phase::Cancelled)
            .unwrap_or(false)
    }
}

impl JobRunner for FakeRunner {
    fn submit(
        &self,
        peer: &PeerId,
        key: &SubmitKey,
        spec: JobSpec,
    ) -> Result<(RemoteJobId, bool), String> {
        let mut jobs = self.jobs();
        if let Some(i) = jobs.iter().position(|j| &j.peer == peer && &j.key == key) {
            return Ok((RemoteJobId(i as u64 + 1), true));
        }
        jobs.push(FakeJob {
            peer: peer.clone(),
            key: key.clone(),
            spec,
            phase: Phase::Queued,
            detail: String::new(),
            log: Vec::new(),
            finished: None,
            metrics: None,
        });
        Ok((RemoteJobId(jobs.len() as u64), false))
    }
    fn view(&self, peer: &PeerId, id: RemoteJobId) -> Option<JobView> {
        let peer = peer.clone();
        self.with(id, |j| {
            (j.peer == peer).then(|| JobView {
                phase: j.phase.clone(),
                detail: j.detail.clone(),
                log_len: j.log.len() as u64,
                finished: j.finished.clone(),
            })
        })
        .flatten()
    }
    fn read_log(&self, peer: &PeerId, id: RemoteJobId, offset: u64, max: usize) -> Vec<u8> {
        let peer = peer.clone();
        self.with(id, |j| {
            if j.peer != peer {
                return Vec::new();
            }
            let from = usize::try_from(offset).unwrap_or(usize::MAX);
            j.log
                .get(from..)
                .unwrap_or(&[])
                .iter()
                .take(max)
                .copied()
                .collect()
        })
        .unwrap_or_default()
    }
    fn metrics(
        &self,
        peer: &PeerId,
        id: RemoteJobId,
    ) -> Option<tasks_core::metrics::JobMetricsView> {
        let peer = peer.clone();
        self.with(id, |j| {
            (j.peer == peer).then(|| j.metrics.clone()).flatten()
        })
        .flatten()
    }
    fn checkout_busy(&self, peer: &PeerId, tree: &std::path::Path) -> bool {
        self.jobs()
            .iter()
            .any(|j| &j.peer == peer && !j.phase.is_terminal() && j.spec.cwd.starts_with(tree))
    }
    fn cancel(&self, peer: &PeerId, id: RemoteJobId) -> Option<CancelState> {
        let peer = peer.clone();
        self.with(id, |j| {
            if j.peer != peer {
                return None;
            }
            if j.phase.is_terminal() {
                return Some(CancelState::AlreadyFinished);
            }
            j.phase = Phase::Cancelled;
            j.finished = Some(FinishedJob {
                outcome: WireOutcome {
                    end: WireEnd::Cancelled,
                    exit_code: Some(143),
                    signal: Some(15),
                    wall_ms: 10,
                    rss_bytes: None,
                    cpu_s: None,
                    description: "cancelled command killed by signal 15".to_string(),
                },
                samples: Vec::new(),
            });
            Some(CancelState::Requested)
        })
        .flatten()
    }
}

/// A fixed status.
pub struct FakeStatus(pub PeerStatus);

impl FakeStatus {
    /// A plausible NixOS-like worker status.
    pub fn nixos() -> FakeStatus {
        FakeStatus(PeerStatus {
            machine: Machine {
                name: MachineName("nixos".into()),
                os: Os::Linux,
                arch: Arch::X86_64,
                cores: 16,
                memory_bytes: 32 << 30,
                tools: Default::default(),
                devices: Default::default(),
            },
            slots: Vec::new(),
            queue: QueueSummary::default(),
            pressure: PressureSummary::default(),
            jobs: Vec::new(),
            cores_in_use_milli: None,
            power: None,
        })
    }
}

impl StatusSource for FakeStatus {
    fn status(&self) -> PeerStatus {
        self.0.clone()
    }
}
