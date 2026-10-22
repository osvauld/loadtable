//! Peer forwarding for `tasks`: the wire protocol, checkout sync, the origin-side
//! forwarding state machine, the receiving side and output fetch.
//!
//! The core is synchronous and clock-injected; it is tested entirely over the
//! in-memory [`fake`] transport. The real iroh transport is behind the optional
//! `iroh` feature.

pub mod fake;
pub mod forward;
pub mod ids;
pub mod locks;
pub mod manifest;
pub mod outputs;
pub mod proto;
pub mod receiver;
pub mod store;
pub mod sync;
pub mod transport;

#[cfg(feature = "iroh")]
pub mod iroh;

pub use forward::{
    Forward, ForwardIo, ForwardJob, ForwardPolicy, ForwardResult, ForwardState, LogSink,
};
pub use ids::{CheckoutId, Hash, IdError, PeerId, RemoteJobId, SubmitKey};
pub use locks::{Acquire, CheckoutLocks, LockOwner};
pub use proto::{Body, JobEvent, Refusal};
pub use receiver::{Allowlist, JobRunner, Receiver, StatusSource, SubmitPolicy};
pub use transport::{Connection, EventStream, Handler, PeerError, StreamPoll, Transport};
