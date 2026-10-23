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
    rt.spawn(serve(ep.clone(), gate));
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
