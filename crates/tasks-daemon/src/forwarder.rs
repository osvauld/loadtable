//! Handing jobs to other machines. The peer crate implements [`Forwarder`]; until it does, the
//! daemon uses [`NoForwarder`] and every job runs here.

use tasks_core::MachineName;
use tasks_ledger::JobRecord;
use tasks_placement::MachineView;
use thiserror::Error;

/// Why a forward failed.
#[derive(Debug, Error)]
pub enum ForwardError {
    /// The peer cannot be reached or there is no peer support.
    #[error("{0}")]
    Unavailable(String),
    /// The peer refused the job.
    #[error("refused: {0}")]
    Refused(String),
}

impl ForwardError {
    /// True when the peer certainly received nothing, so placement may run the job locally
    /// instead (maps to the peer crate's `ForwardResult.nothing_submitted`).
    pub fn nothing_submitted(&self) -> bool {
        true
    }
}

/// A job accepted by a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardTicket {
    /// The job's id on the peer, when the peer already knows it.
    pub remote_job: Option<i64>,
    /// The peer's duplicate-submit key for the job. The ledger must persist the
    /// (peer, remote key) pair so protection survives a restart; today it stores only
    /// `remote_job` (needs a ledger change, see the report).
    pub remote_key: Option<String>,
}

/// The daemon's view of its peers.
pub trait Forwarder: Send + Sync {
    /// The other machines as placement sees them (`is_local` false), with link state, free
    /// slots and checkout sync facts. Estimates may be left empty; the daemon fills them.
    fn peers(&self) -> Vec<MachineView>;
    /// Sends the job to `peer`. On success the daemon records the forward and stops
    /// scheduling the job locally.
    fn forward(&self, job: &JobRecord, peer: &MachineName) -> Result<ForwardTicket, ForwardError>;
    /// True once a forwarded job may take a local offload slot: the per-checkout lock on the
    /// peer side is held (`tasks_peer::Forward::ready_to_start`). A waiter holding a slot
    /// before that would recreate R-230, so the daemon takes the slot only when this is true.
    /// Blocking transport calls belong behind this trait, off any async runtime's workers;
    /// the daemon itself has no tokio dependency.
    fn ready_to_start(&self, peer: &MachineName, remote_job: Option<i64>) -> bool {
        let _ = (peer, remote_job);
        true
    }
    /// Cancels the job on `peer`. A forced reroute off an unreachable peer must call this
    /// first (or confirm the job is gone), so a job never runs twice.
    fn cancel_remote(&self, peer: &MachineName, remote_job: i64) -> Result<(), ForwardError>;
}

/// No peers: everything runs locally.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoForwarder;

impl Forwarder for NoForwarder {
    fn peers(&self) -> Vec<MachineView> {
        Vec::new()
    }
    fn forward(&self, _job: &JobRecord, peer: &MachineName) -> Result<ForwardTicket, ForwardError> {
        Err(ForwardError::Unavailable(format!(
            "no peer support in this daemon, cannot reach {}",
            peer.0
        )))
    }
    fn cancel_remote(&self, peer: &MachineName, _remote_job: i64) -> Result<(), ForwardError> {
        Err(ForwardError::Unavailable(format!(
            "no peer support in this daemon, cannot reach {}",
            peer.0
        )))
    }
}
