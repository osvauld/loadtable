//! Handing jobs to other machines. The [`peer_link`](crate::peer_link) adapter implements
//! [`Forwarder`] over the peer crate; [`NoForwarder`] means every job runs here.
//!
//! # Life of a forwarded job
//!
//! 1. Placement picks a peer: the scheduler calls [`Forwarder::forward`], which only *registers*
//!    the job (a place in the per-checkout queue, nothing on the network) and returns a
//!    [`ForwardTicket`] with the duplicate-submit key. The ledger records `(peer, key)` before
//!    anything is sent, so a restarted daemon resends the same key and never invents a second
//!    submission.
//! 2. The job is then an *offload-client* job: it takes a local offload slot, but only once
//!    [`Forwarder::ready_to_start`] says its checkout lock is held or would be granted (R-230).
//!    A waiter behind another job of the same checkout holds nothing.
//! 3. [`Forwarder::start`] begins the work on the adapter's own thread (lock, sync, submit,
//!    follow). [`Forwarder::poll`] is cheap and never touches the network, so the scheduler tick
//!    and its job thread never block on a peer.
//! 4. [`ForwardPoll::Done`] carries the end. `nothing_submitted` ends may fall back to local
//!    placement; anything else is final and is never run here or resubmitted.

use std::path::Path;
use tasks_core::{JobId, MachineName, Phase};
use tasks_ledger::JobRecord;
use tasks_placement::MachineView;
use tasks_timing::{Sample, SampleKey};
use thiserror::Error;

/// Why a call on the forwarder failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ForwardError {
    /// The peer cannot be reached or there is no peer support.
    #[error("{0}")]
    Unavailable(String),
    /// The peer refused the job.
    #[error("refused: {0}")]
    Refused(String),
}

/// A job registered for forwarding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardTicket {
    /// The job's id on the peer, when the peer already knows it.
    pub remote_job: Option<i64>,
    /// The peer's duplicate-submit key for the job. The ledger persists the (peer, key) pair
    /// before the first byte is sent.
    pub remote_key: Option<String>,
}

/// How far a started forward has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardProgress {
    /// One line mirroring the peer's state, for the job's wait text and log (R-3).
    pub mirror: String,
    /// The peer's id for the job, once it answered.
    pub remote_job: Option<i64>,
    /// True once the peer has accepted the job: from here it may be running there, so it can
    /// no longer be run here.
    pub accepted: bool,
    /// The peer's phase for the job, once it accepted it.
    pub remote_phase: Option<Phase>,
}

/// How a forward ended.
#[derive(Debug, Clone, PartialEq)]
pub struct ForwardEnd {
    /// The terminal phase for the local job.
    pub phase: Phase,
    /// Why, in words (also the job's summary).
    pub reason: String,
    /// The command's exit code, when the peer reported one.
    pub exit_code: Option<i32>,
    /// The signal that ended it, when the peer reported one.
    pub signal: Option<i32>,
    /// The peer's id for the job, when it was accepted.
    pub remote_job: Option<i64>,
    /// True when nothing reached the peer (`tasks_peer::ForwardResult::nothing_submitted`): only
    /// then may the job fall back to local placement.
    pub nothing_submitted: bool,
    /// Timing samples measured on the peer.
    pub samples: Vec<(SampleKey, Sample)>,
}

/// What [`Forwarder::poll`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum ForwardPoll {
    /// Still going.
    Active(ForwardProgress),
    /// Over; the adapter forgets the job once this has been returned.
    Done(ForwardEnd),
}

/// Reachability of one peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Not probed yet since the daemon started.
    Unknown,
    /// The last probe succeeded recently.
    Up,
    /// It answered once, but not recently enough to trust its numbers.
    Stale,
    /// The last probe could not connect.
    Offline,
}

impl LinkState {
    /// The state as shown in the API.
    pub fn name(self) -> &'static str {
        match self {
            LinkState::Unknown => "unknown",
            LinkState::Up => "up",
            LinkState::Stale => "stale",
            LinkState::Offline => "offline",
        }
    }
}

/// The link to one peer, for status and alerts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkReport {
    /// The peer's name.
    pub peer: String,
    /// Its transport identity.
    pub node_id: String,
    /// Reachability.
    pub state: LinkState,
    /// Seconds since it last answered, if ever.
    pub last_seen_ago_secs: Option<u64>,
    /// The last probe's error, or empty.
    pub detail: String,
    /// What the peer last reported about itself.
    pub status: Option<tasks_peer::proto::PeerStatus>,
}

/// The daemon's view of its peers.
pub trait Forwarder: Send + Sync {
    /// The other machines as placement sees them (`is_local` false), with link state, free
    /// slots and checkout sync facts. Estimates may be left empty; the daemon fills them.
    fn peers(&self) -> Vec<MachineView>;
    /// Reachability of every configured peer.
    fn links(&self) -> Vec<LinkReport> {
        Vec::new()
    }
    /// Registers `job` for `peer`: takes its place in the per-checkout queue. No network. Calling
    /// it again for a registered job returns the same ticket; a job whose ledger row already
    /// holds a key reuses it (see [`Forwarder::registered`]).
    fn forward(&self, job: &JobRecord, peer: &MachineName) -> Result<ForwardTicket, ForwardError>;
    /// True when the adapter knows the job (false after a daemon restart, until the scheduler
    /// registers it again with its persisted key).
    fn registered(&self, job: JobId) -> bool {
        let _ = job;
        true
    }
    /// True once a forwarded job may take a local offload slot: the per-checkout lock is held
    /// or would be granted (`tasks_peer::Forward::ready_to_start`). A waiter holding a slot
    /// before that would recreate R-230, so the daemon takes the slot only when this is true.
    /// Never blocks on the network.
    fn ready_to_start(&self, job: JobId) -> bool {
        let _ = job;
        true
    }
    /// Why the job is not ready, in words, for its wait text.
    fn wait_reason(&self, job: JobId) -> Option<String> {
        let _ = job;
        None
    }
    /// Begins driving the registered job: the adapter's own thread locks the checkout, syncs,
    /// submits and follows the peer, appending its output to `log`.
    fn start(&self, job: JobId, log: &Path) -> Result<(), ForwardError> {
        let _ = (job, log);
        Err(ForwardError::Unavailable(
            "no peer support in this daemon".into(),
        ))
    }
    /// Picks up a job the ledger says is running on a peer, after a daemon restart. Read-only:
    /// it only attaches to the peer's job by its id and never submits (R-173). A job with no
    /// remote id ends `Interrupted`: it may have been submitted, so it is neither rerun nor
    /// resubmitted.
    fn resume(&self, job: &JobRecord, log: &Path) -> Result<(), ForwardError> {
        let _ = (job, log);
        Err(ForwardError::Unavailable(
            "no peer support in this daemon".into(),
        ))
    }
    /// Progress of a started job without blocking.
    fn poll(&self, job: JobId) -> ForwardPoll;
    /// Asks the peer to cancel the job; the end arrives through [`Forwarder::poll`].
    fn cancel(&self, job: JobId, reason: &str);
    /// Cancels a job on `peer` by its remote id, outside any running forward. A forced reroute
    /// off an unreachable peer must call this first (or confirm the job gone), so a job never
    /// runs twice (R-166).
    fn cancel_remote(&self, peer: &MachineName, remote_job: i64) -> Result<(), ForwardError>;
    /// Drops everything held for the job (its place in the checkout queue).
    fn forget(&self, job: JobId) {
        let _ = job;
    }
    /// Drops the registrations of jobs that were never started and are not in `keep` (a job
    /// cancelled or finished while it waited must not hold up its checkout's queue, R-230).
    fn forget_unlisted(&self, keep: &std::collections::BTreeSet<JobId>) {
        let _ = keep;
    }
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
    fn registered(&self, _job: JobId) -> bool {
        false
    }
    fn poll(&self, _job: JobId) -> ForwardPoll {
        ForwardPoll::Done(ForwardEnd {
            phase: Phase::Interrupted,
            reason: "no peer support in this daemon".to_string(),
            exit_code: None,
            signal: None,
            remote_job: None,
            nothing_submitted: false,
            samples: Vec::new(),
        })
    }
    fn cancel(&self, _job: JobId, _reason: &str) {}
    fn cancel_remote(&self, peer: &MachineName, _remote_job: i64) -> Result<(), ForwardError> {
        Err(ForwardError::Unavailable(format!(
            "no peer support in this daemon, cannot reach {}",
            peer.0
        )))
    }
}
