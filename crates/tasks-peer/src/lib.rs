//! Peer forwarding for `tasks`: the wire protocol, checkout sync (git over the peer link), the origin-side
//! forwarding state machine, the receiving side and output fetch.
//!
//! The core is synchronous and clock-injected; it is tested entirely over the
//! in-memory [`fake`] transport. The real iroh transport is behind the optional
//! `iroh` feature.

pub mod fake;
pub mod forward;
pub mod ids;
pub mod locks;
pub mod outputs;
pub mod proto;
pub mod receiver;
pub mod sync;
pub mod transport;
pub mod trust;

#[cfg(feature = "iroh")]
pub mod iroh;
#[cfg(feature = "iroh")]
pub mod iroh_blobs_shelf;

pub use forward::{
    Forward, ForwardIo, ForwardJob, ForwardPolicy, ForwardResult, ForwardState, LogSink,
};
pub use ids::{CheckoutId, GitId, Hash, IdError, PeerId, RemoteJobId, SubmitKey};
pub use locks::{Acquire, CheckoutLocks, CopyTree, LockMode, LockOwner};
pub use proto::{Body, JobEvent, Refusal};
pub use receiver::{Allowlist, JobRunner, Receiver, StatusSource, SubmitPolicy};
pub use transport::{Connection, EventStream, Handler, PeerError, StreamPoll, Transport};
