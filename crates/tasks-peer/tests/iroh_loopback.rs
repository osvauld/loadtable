//! Two real iroh endpoints in one process on loopback, relays and discovery off: pairing by
//! ticket, one request through the admission gate, the path report, and revocation closing the
//! live connection. Needs no network access.
#![cfg(feature = "iroh")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;
use tasks_identity::generate;
use tasks_ledger::{MemoryTrust, SystemClock};
use tasks_peer::iroh::{
    EndpointAddrs, EndpointOptions, IrohTransport, RelayChoice, endpoint, serve,
};
use tasks_peer::iroh_blobs_shelf::BlobServe;
use tasks_peer::outputs::{self, FetchPolicy};
use tasks_peer::proto::FetchEntry;
use tasks_peer::proto::{Body, Refusal};
use tasks_peer::transport::{PathKind, PeerError, Transport};
use tasks_peer::trust::{
    AddrSource, AddressBook, NoEvents, Role, SelfAddr, TrustNode, TrustPolicy, TrustedHandler,
    TrustedTransport,
};
use tasks_peer::{Handler, PeerId, RemoteJobId};

struct Ok_;
impl Handler for Ok_ {
    fn handle(&self, _f: &PeerId, _m: Body) -> Body {
        Body::Ok
    }
    fn events(
        &self,
        _f: &PeerId,
        remote: RemoteJobId,
        _o: u64,
    ) -> Result<Box<dyn tasks_peer::EventStream>, Refusal> {
        Err(Refusal::UnknownJob { remote })
    }
}

/// A handler that takes raw uploads: it counts the bytes and answers with the length.
struct Streams;
impl Handler for Streams {
    fn handle(&self, _f: &PeerId, _m: Body) -> Body {
        Body::Ok
    }
    fn events(
        &self,
        _f: &PeerId,
        remote: RemoteJobId,
        _o: u64,
    ) -> Result<Box<dyn tasks_peer::EventStream>, Refusal> {
        Err(Refusal::UnknownJob { remote })
    }
    fn accept_upload(&self, _f: &PeerId, head: &Body) -> Result<(), Refusal> {
        match head {
            Body::SyncPackStream { session: 13, .. } => Err(Refusal::Busy {
                checkout: tasks_peer::CheckoutId::derive("h", std::path::Path::new("/p")),
            }),
            Body::SyncPackStream { .. } => Ok(()),
            _ => Err(Refusal::Malformed {
                detail: "not an upload".into(),
            }),
        }
    }
    fn upload(&self, _f: &PeerId, _h: Body, data: &mut dyn std::io::Read) -> Body {
        let mut n = 0u64;
        let mut buf = [0u8; 8192];
        loop {
            match data.read(&mut buf) {
                Ok(0) => break,
                Ok(k) => n += k as u64,
                Err(_) => break,
            }
        }
        Body::SyncPackAck { received: n }
    }
}

struct Side {
    node: Arc<TrustNode>,
    ep: iroh::Endpoint,
    transport: TrustedTransport<IrohTransport>,
}

fn side(rt: &tokio::runtime::Runtime, name: &str) -> Side {
    side_with(rt, name, Arc::new(Ok_))
}

fn side_with(rt: &tokio::runtime::Runtime, name: &str, inner: Arc<dyn Handler>) -> Side {
    side_serving(rt, name, inner, None)
}

fn side_serving(
    rt: &tokio::runtime::Runtime,
    name: &str,
    inner: Arc<dyn Handler>,
    blobs: Option<iroh_blobs::BlobsProtocol>,
) -> Side {
    let (ident, _) = generate().unwrap();
    let ident = Arc::new(ident);
    let node = TrustNode::new(
        ident.clone(),
        name,
        Arc::new(MemoryTrust::new()),
        Arc::new(NoEvents),
        Arc::new(SystemClock),
        TrustPolicy::default(),
    )
    .unwrap();
    let opts = EndpointOptions {
        relay: RelayChoice::Disabled,
        bind: Some("127.0.0.1:0".parse().unwrap()),
    };
    let secret = iroh::SecretKey::from_bytes(&ident.device_secret());
    let ep = rt.block_on(endpoint(secret, &opts)).unwrap();
    // the endpoint key is the identity's device key
    assert_eq!(ep.id().to_string(), node.node_id());
    let book: Arc<dyn AddressBook> = node.clone();
    let raw = IrohTransport::new(ep.clone(), rt.handle().clone(), book);
    let gate: Arc<dyn Handler> = Arc::new(TrustedHandler::new(node.clone(), inner));
    rt.spawn(serve(ep.clone(), gate, blobs));
    Side {
        transport: TrustedTransport::new(raw, node.clone()),
        node,
        ep,
    }
}

// R-262
#[test]
fn r262_two_real_endpoints_pair_forward_one_request_and_a_revoke_closes_the_connection() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let worker = side(&rt, "nixos");
    let laptop = side(&rt, "kurukkan");
    let t = Duration::from_secs(10);
    let wid = PeerId::new(&worker.node.node_id()).unwrap();

    // a stranger without a grant is turned away at the first frame
    let nobody = laptop.transport.connect(&wid, t);
    assert!(matches!(
        nobody,
        Err(PeerError::Refused(Refusal::Unauthorized { .. }))
    ));

    let addrs = EndpointAddrs(worker.ep.clone()).current();
    assert!(
        !addrs.direct_addrs.is_empty(),
        "loopback addresses are known"
    );
    let ticket = worker
        .node
        .invite(&[Role::Submit], false, None, &addrs)
        .unwrap();
    laptop
        .node
        .accept(
            &ticket.to_text().unwrap(),
            &laptop.transport,
            &SelfAddr::default(),
            t,
        )
        .unwrap();
    // the ticket is single use
    let again = laptop.node.accept(
        &ticket.to_text().unwrap(),
        &laptop.transport,
        &SelfAddr::default(),
        t,
    );
    assert!(again.is_err());

    let mut c = laptop.transport.connect(&wid, t).unwrap();
    assert!(matches!(
        c.request(Body::Cancel {
            remote: RemoteJobId(1)
        }),
        Ok(Body::Ok)
    ));
    assert_eq!(c.link_info().path, PathKind::Direct);

    let report = worker.node.revoke_peer("kurukkan").unwrap();
    assert_eq!(report.revoked, 1);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        c.request(Body::Cancel {
            remote: RemoteJobId(1)
        })
        .is_err()
    );
    assert!(laptop.transport.connect(&wid, t).is_err());
    rt.block_on(async {
        worker.ep.close().await;
        laptop.ep.close().await;
    });
}

// R-367
#[test]
fn r367_a_pack_crosses_one_raw_iroh_stream_and_an_older_peer_refuses_it_at_the_head() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let t = Duration::from_secs(10);
    let pair = |worker: Side, laptop: Side| {
        let wid = PeerId::new(&worker.node.node_id()).unwrap();
        let addrs = EndpointAddrs(worker.ep.clone()).current();
        let ticket = worker
            .node
            .invite(&[Role::Submit], false, None, &addrs)
            .unwrap();
        laptop
            .node
            .accept(
                &ticket.to_text().unwrap(),
                &laptop.transport,
                &SelfAddr::default(),
                t,
            )
            .unwrap();
        (wid, worker, laptop)
    };
    let head = |session| Body::SyncPackStream {
        checkout: tasks_peer::CheckoutId::derive("h", std::path::Path::new("/p")),
        session,
    };

    // a build that knows streams: 3 MiB written in odd pieces arrive whole
    let (wid, worker, laptop) = pair(
        side_with(&rt, "nixos", Arc::new(Streams)),
        side(&rt, "kurukkan"),
    );
    let mut c = laptop.transport.connect(&wid, t).unwrap();
    let mut up = c.open_upload(head(1)).unwrap();
    let piece = vec![9u8; 100_003];
    let mut sent = 0u64;
    for _ in 0..30 {
        up.write(&piece).unwrap();
        sent += piece.len() as u64;
    }
    assert!(matches!(
        up.finish(),
        Ok(Body::SyncPackAck { received }) if received == sent
    ));
    // the worker's own refusal (a live job in the copy) comes back before any byte is sent
    assert!(matches!(
        c.open_upload(head(13)),
        Err(PeerError::Refused(Refusal::Busy { .. }))
    ));
    // an upload dropped half way does not look like a finished one and the link stays usable
    let mut half = c.open_upload(head(2)).unwrap();
    half.write(&piece).unwrap();
    drop(half);
    assert!(matches!(
        c.request(Body::Cancel {
            remote: RemoteJobId(1)
        }),
        Ok(Body::Ok)
    ));
    rt.block_on(async {
        worker.ep.close().await;
        laptop.ep.close().await;
    });

    // a build that does not: the head is refused as a malformed message, so the sender falls back
    let (wid, worker, laptop) = pair(side(&rt, "nixos2"), side(&rt, "kurukkan2"));
    let mut c = laptop.transport.connect(&wid, t).unwrap();
    assert!(matches!(
        c.open_upload(head(1)),
        Err(PeerError::Refused(Refusal::Malformed { .. }))
    ));
    rt.block_on(async {
        worker.ep.close().await;
        laptop.ep.close().await;
    });
}

/// A worker that lists files of one tree, offering them on its blob store, and counts the chunk
/// requests it serves. `bogus` lists a blob hash it never offered.
struct Outputs {
    tree: std::path::PathBuf,
    shelf: Arc<BlobServe>,
    chunks: std::sync::atomic::AtomicU64,
    bogus: bool,
}

impl Handler for Outputs {
    fn handle(&self, from: &PeerId, m: Body) -> Body {
        let policy = FetchPolicy::default();
        match m {
            Body::FetchList { paths, .. } => {
                match outputs::list_offered(&self.tree, &paths, &policy, self.shelf.as_ref(), from)
                {
                    Ok(mut entries) => {
                        if self.bogus {
                            for e in &mut entries {
                                if let FetchEntry::Found { blob, .. } = e {
                                    *blob = Some(iroh_blobs::Hash::new(b"never offered").to_hex());
                                }
                            }
                        }
                        Body::FetchListing { entries }
                    }
                    Err(r) => Body::Refuse(r),
                }
            }
            Body::FetchChunk {
                path,
                hash,
                offset,
                len,
                ..
            } => {
                self.chunks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                match outputs::read_chunk(&self.tree, &path, &hash, offset, len, &policy) {
                    Ok((data, eof)) => Body::FetchData { offset, data, eof },
                    Err(r) => Body::Refuse(r),
                }
            }
            _ => Body::Ok,
        }
    }
    fn events(
        &self,
        _f: &PeerId,
        remote: RemoteJobId,
        _o: u64,
    ) -> Result<Box<dyn tasks_peer::EventStream>, Refusal> {
        Err(Refusal::UnknownJob { remote })
    }
}

// R-795
#[test]
fn r795_an_output_comes_back_on_one_verified_blob_stream_and_only_what_was_listed_is_served() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let t = Duration::from_secs(10);
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    std::fs::create_dir_all(tree.join("target/debug")).unwrap();
    // 40 MiB and an odd tail: thousands of chunks the old way
    let big: Vec<u8> = (0..40 * 1024 * 1024 + 4099u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    std::fs::write(tree.join("target/debug/app"), &big).unwrap();
    std::fs::write(tree.join("secret.txt"), b"not listed").unwrap();
    let checkout = tasks_peer::CheckoutId::derive("h", std::path::Path::new("/p"));

    let serve_with = |name: &str, bogus: bool, store: &str| {
        let shelf = Arc::new(
            rt.block_on(BlobServe::open(&tmp.path().join(store)))
                .unwrap(),
        );
        let handler = Arc::new(Outputs {
            tree: tree.clone(),
            shelf: Arc::clone(&shelf),
            chunks: std::sync::atomic::AtomicU64::new(0),
            bogus,
        });
        let side = side_serving(&rt, name, handler.clone(), Some(shelf.protocol()));
        (side, handler)
    };
    let pair = |worker: &Side, laptop: &Side| {
        let addrs = EndpointAddrs(worker.ep.clone()).current();
        let ticket = worker
            .node
            .invite(&[Role::Submit], false, None, &addrs)
            .unwrap();
        laptop
            .node
            .accept(
                &ticket.to_text().unwrap(),
                &laptop.transport,
                &SelfAddr::default(),
                t,
            )
            .unwrap();
        PeerId::new(&worker.node.node_id()).unwrap()
    };

    let (worker, served) = serve_with("nixos", false, "blobs");
    let laptop = side(&rt, "kurukkan");
    let wid = pair(&worker, &laptop);
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    let mut c = laptop.transport.connect(&wid, t).unwrap();
    let report = outputs::fetch(
        c.as_mut(),
        &checkout,
        &["target/debug/app".to_string()],
        &dest,
        u64::MAX,
    )
    .unwrap();
    assert_eq!(report.fetched.len(), 1, "{report:?}");
    assert_eq!(report.fetched[0].blob_fallback, None, "{report:?}");
    assert_eq!(std::fs::read(dest.join("target/debug/app")).unwrap(), big);
    assert_eq!(
        served.chunks.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "no chunk was asked for"
    );

    // a hash the worker never listed for us is refused on the blob stream
    let unlisted = iroh_blobs::Hash::new(b"not listed").to_hex();
    let mut sink = |_: &[u8]| Ok(());
    assert!(c.fetch_blob(&unlisted, 10, &mut sink).is_err());
    // and a paired machine that listed nothing gets nothing, even for a listed hash
    let other = side(&rt, "mac");
    let _ = pair(&worker, &other);
    let mut oc = other.transport.connect(&wid, t).unwrap();
    let listed = iroh_blobs::Hash::new(&big).to_hex();
    assert!(oc.fetch_blob(&listed, big.len() as u64, &mut sink).is_err());

    // a blob that cannot be served falls back to chunks and says why
    let (worker2, served2) = serve_with("nixos2", true, "blobs2");
    let laptop2 = side(&rt, "kurukkan2");
    let wid2 = pair(&worker2, &laptop2);
    let dest2 = tmp.path().join("dest2");
    std::fs::create_dir_all(&dest2).unwrap();
    let mut c2 = laptop2.transport.connect(&wid2, t).unwrap();
    let report = outputs::fetch(
        c2.as_mut(),
        &checkout,
        &["target/debug/app".to_string()],
        &dest2,
        u64::MAX,
    )
    .unwrap();
    assert_eq!(report.fetched.len(), 1, "{report:?}");
    assert!(report.fetched[0].blob_fallback.is_some(), "{report:?}");
    assert_eq!(std::fs::read(dest2.join("target/debug/app")).unwrap(), big);
    assert!(served2.chunks.load(std::sync::atomic::Ordering::Relaxed) > 0);

    rt.block_on(async {
        for s in [&worker, &laptop, &other, &worker2, &laptop2] {
            s.ep.close().await;
        }
    });
}
