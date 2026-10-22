//! The driver threads: one per started forward, one per resumed job.
//!
//! A driver ticks its [`Forward`] on the adapter's own thread and publishes what the scheduler
//! reads through [`Forwarder::poll`]. It never holds a lock the scheduler needs while it waits
//! on the network: the `Forward` mutex is taken for one tick at a time, and
//! `ready_to_start` uses `try_lock`.

use super::{Inner, Job, lost_end};
use crate::forwarder::{ForwardEnd, ForwardError, ForwardProgress};
use crate::scheduler::lock;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tasks_core::Phase;
use tasks_peer::proto::{JobEvent, WireOutcome};
use tasks_peer::sync::DirSource;
use tasks_peer::transport::{PeerError, StreamPoll, Transport};
use tasks_peer::{ForwardIo, ForwardResult, ForwardState, LogSink, PeerId, Refusal, RemoteJobId};

/// The local job log as a sink for the peer's output. It holds only the peer's bytes, so its
/// length is the offset a reattach resumes from; audit notes go to a sibling file instead.
struct FileLog {
    file: File,
    path: PathBuf,
}

impl FileLog {
    fn open(path: &Path) -> std::io::Result<FileLog> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        Ok(FileLog {
            file: OpenOptions::new().create(true).append(true).open(path)?,
            path: path.to_path_buf(),
        })
    }

    /// Appends an audit line to `<log>.forward`.
    fn note(&self, line: &str) {
        let mut p = self.path.as_os_str().to_os_string();
        p.push(".forward");
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{line}");
        }
    }
}

impl LogSink for FileLog {
    fn len(&self) -> u64 {
        self.file.metadata().map(|m| m.len()).unwrap_or(0)
    }
    fn append(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.file.write_all(data)
    }
}

fn thread_err(e: std::io::Error) -> ForwardError {
    ForwardError::Unavailable(format!("cannot start a forwarding thread: {e}"))
}

pub(super) fn spawn_forward<T: Transport + 'static>(
    inner: Arc<Inner<T>>,
    job: Arc<Job>,
    log: PathBuf,
) -> Result<(), ForwardError> {
    std::thread::Builder::new()
        .name(format!("forward-{}", job.id.0))
        .spawn(move || drive_forward(&inner, &job, &log))
        .map(|_| ())
        .map_err(thread_err)
}

pub(super) fn spawn_resume<T: Transport + 'static>(
    inner: Arc<Inner<T>>,
    job: Arc<Job>,
    peer: PeerId,
    remote: Option<RemoteJobId>,
    log: PathBuf,
) -> Result<(), ForwardError> {
    std::thread::Builder::new()
        .name(format!("resume-{}", job.id.0))
        .spawn(move || drive_resume(&inner, &job, &peer, remote, &log))
        .map(|_| ())
        .map_err(thread_err)
}

fn publish(job: &Job, progress: ForwardProgress) {
    lock(&job.published).progress = Some(progress);
}

fn finish(job: &Job, end: ForwardEnd) {
    lock(&job.published).end = Some(end);
}

fn end_of(r: &ForwardResult) -> ForwardEnd {
    let (exit_code, signal, described) = match &r.outcome {
        Some(o) => (o.exit_code, o.signal, o.description.clone()),
        None => (None, None, String::new()),
    };
    let reason = if r.reason.is_empty() {
        described
    } else {
        r.reason.clone()
    };
    ForwardEnd {
        phase: r.phase.clone(),
        reason,
        exit_code,
        signal,
        remote_job: r.remote.and_then(|r| i64::try_from(r.0).ok()),
        nothing_submitted: r.nothing_submitted,
        samples: r.samples.clone(),
    }
}

fn drive_forward<T: Transport>(inner: &Inner<T>, job: &Job, log_path: &Path) {
    let mut log = match FileLog::open(log_path) {
        Ok(l) => l,
        Err(e) => {
            // nothing has been sent yet, so the job may still run here
            let mut end = lost_end(&format!(
                "cannot open the job log {}: {e}; nothing was submitted",
                log_path.display()
            ));
            end.phase = Phase::Failed { exit: None };
            end.nothing_submitted = true;
            finish(job, end);
            return;
        }
    };
    let source = DirSource(job.root.clone());
    loop {
        if inner.stop.load(Ordering::SeqCst) {
            return;
        }
        let now = inner.now();
        let cancel = lock(&job.published).cancel.take();
        let mut guard = lock(&job.fwd);
        let Some(fwd) = guard.as_mut() else {
            return;
        };
        if let Some(reason) = cancel {
            fwd.cancel(now, &reason);
        }
        {
            let mut io = ForwardIo {
                transport: &inner.transport,
                lister: &*inner.opts.lister,
                source: &source,
                log: &mut log,
            };
            fwd.tick(now, &mut io);
        }
        for n in fwd.take_notes() {
            log.note(&n);
        }
        let remote = fwd.remote();
        let remote_phase = match fwd.state() {
            ForwardState::RemoteQueued { phase, .. } => Some(phase.clone()),
            ForwardState::Running { .. } => Some(Phase::Running),
            _ => None,
        };
        let progress = ForwardProgress {
            mirror: fwd.mirror(),
            remote_job: remote.and_then(|r| i64::try_from(r.0).ok()),
            accepted: remote.is_some(),
            remote_phase,
        };
        let done = fwd.result().map(end_of);
        drop(guard);
        match done {
            Some(end) => {
                log.note(&format!("forward ended: {:?}: {}", end.phase, end.reason));
                // the Forward released the checkout lock when it finished
                finish(job, end);
                return;
            }
            None => publish(job, progress),
        }
        std::thread::sleep(inner.opts.tick_every);
    }
}

fn outcome_end(
    o: &WireOutcome,
    remote: RemoteJobId,
    samples: Vec<(tasks_timing::SampleKey, tasks_timing::Sample)>,
) -> ForwardEnd {
    ForwardEnd {
        phase: o.phase(),
        reason: o.description.clone(),
        exit_code: o.exit_code,
        signal: o.signal,
        remote_job: i64::try_from(remote.0).ok(),
        nothing_submitted: false,
        samples,
    }
}

/// Follows a job a previous daemon forwarded, by its remote id only. It never submits: the
/// worst outcome is `Interrupted` after the peer stayed unreachable for the grace period.
fn drive_resume<T: Transport>(
    inner: &Inner<T>,
    job: &Job,
    peer: &PeerId,
    remote: Option<RemoteJobId>,
    log_path: &Path,
) {
    resume_body(inner, job, peer, remote, log_path);
    if let Some((checkout, owner)) = &job.resumed_lock {
        inner.locks.release(checkout, *owner);
    }
}

fn resume_body<T: Transport>(
    inner: &Inner<T>,
    job: &Job,
    peer: &PeerId,
    remote: Option<RemoteJobId>,
    log_path: &Path,
) {
    let Some(remote) = remote else {
        finish(
            job,
            lost_end(&format!(
                "the daemon restarted before {peer} answered the submission; it may have started \
                 the job, so it is neither rerun nor resubmitted"
            )),
        );
        return;
    };
    let Ok(mut log) = FileLog::open(log_path) else {
        finish(job, lost_end("cannot open the job log after a restart"));
        return;
    };
    let policy = &inner.opts.policy;
    let mut lost_since: Option<Duration> = None;
    let remote_id = i64::try_from(remote.0).ok();
    loop {
        if inner.stop.load(Ordering::SeqCst) {
            return;
        }
        if lock(&job.published).cancel.take().is_some() {
            // best effort; the job still ends through the stream or the grace period
            if let Ok(mut c) = inner.transport.connect(peer, policy.connect_timeout) {
                let _ = c.request(tasks_peer::proto::Body::Cancel { remote });
            }
        }
        let attempt = (|| -> Result<Option<ForwardEnd>, PeerError> {
            let mut conn = inner.transport.connect(peer, policy.connect_timeout)?;
            let mut stream = conn.subscribe(remote, log.len())?;
            lost_since = None;
            let mut last = inner.now();
            loop {
                if inner.stop.load(Ordering::SeqCst) {
                    return Ok(None);
                }
                match stream.poll()? {
                    StreamPoll::Event(ev) => {
                        last = inner.now();
                        match ev {
                            JobEvent::Phase { phase, detail } => publish(
                                job,
                                ForwardProgress {
                                    mirror: format!("{remote} {detail}"),
                                    remote_job: remote_id,
                                    accepted: true,
                                    remote_phase: Some(phase),
                                },
                            ),
                            JobEvent::Log { offset, data } => {
                                let have = log.len();
                                if offset > have {
                                    return Err(PeerError::Protocol("gap in the log".into()));
                                }
                                let skip = usize::try_from(have - offset).unwrap_or(usize::MAX);
                                if let Some(rest) = data.get(skip..)
                                    && !rest.is_empty()
                                {
                                    log.append(rest)
                                        .map_err(|e| PeerError::Protocol(e.to_string()))?;
                                }
                            }
                            JobEvent::Finished {
                                outcome,
                                samples,
                                log_len,
                            } => {
                                if log.len() < log_len {
                                    return Err(PeerError::Protocol("log incomplete".into()));
                                }
                                let samples = samples
                                    .iter()
                                    .filter_map(tasks_peer::proto::WireSample::to_sample)
                                    .collect();
                                return Ok(Some(outcome_end(&outcome, remote, samples)));
                            }
                            JobEvent::Heartbeat => {}
                        }
                    }
                    StreamPoll::Idle => {
                        if inner.now().saturating_sub(last) >= policy.stream_idle {
                            return Err(PeerError::LinkLost("no events".into()));
                        }
                        std::thread::sleep(inner.opts.tick_every);
                    }
                    StreamPoll::Closed => {
                        return Err(PeerError::LinkLost("stream closed before the end".into()));
                    }
                }
            }
        })();
        match attempt {
            Ok(Some(end)) => {
                finish(job, end);
                return;
            }
            Ok(None) => return,
            Err(PeerError::Refused(Refusal::UnknownJob { .. })) => {
                let mut end = lost_end(&format!(
                    "{peer} no longer knows {remote}; result unknown; not resubmitted"
                ));
                end.remote_job = remote_id;
                finish(job, end);
                return;
            }
            Err(e) => {
                let now = inner.now();
                let since = *lost_since.get_or_insert(now);
                if now.saturating_sub(since) >= policy.lost_grace {
                    let mut end = lost_end(&format!(
                        "peer {peer} unreachable for {:?} after a daemon restart ({e}); result \
                         unknown, {remote} may still be running there; not resubmitted",
                        policy.lost_grace
                    ));
                    end.remote_job = remote_id;
                    finish(job, end);
                    return;
                }
                publish(
                    job,
                    ForwardProgress {
                        mirror: format!("{remote} link lost, reattaching: {e}"),
                        remote_job: remote_id,
                        accepted: true,
                        remote_phase: None,
                    },
                );
                std::thread::sleep(policy.reattach_interval.min(Duration::from_millis(500)));
            }
        }
    }
}
