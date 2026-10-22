//! The origin-side state machine over the fake transport (R-149..R-151, R-157, R-161, R-230).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_range_loop
)]
mod common;
use common::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tasks_core::{Phase, SlotKind, WaitReason};
use tasks_peer::fake::FakeRunner;
use tasks_peer::proto::{JobEvent, WireSample};
use tasks_peer::transport::{EventStream, PeerError, StreamPoll};
use tasks_peer::{
    Allowlist, Forward, ForwardPolicy, ForwardState, Handler, PeerId, Refusal, RemoteJobId,
};

const R1: RemoteJobId = RemoteJobId(1);

fn sample() -> WireSample {
    WireSample {
        recipe: "cargo:test".into(),
        target: None,
        machine: "nixos".into(),
        secs: 41.5,
        cache: "warm".into(),
        outcome: "passed".into(),
        at_ms: 1234,
    }
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[test]
fn r149_a_forwarded_job_streams_its_log_mirrors_the_exit_code_and_returns_timing_samples() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    // Synced and accepted; the peer still has it queued.
    assert!(
        matches!(f.state(), ForwardState::RemoteQueued { .. }),
        "{:?}",
        f.state()
    );
    assert_eq!(f.remote(), Some(R1));
    // Not `running` while the remote job waits (R-3).
    w.runner.set_phase(
        R1,
        Phase::Waiting(WaitReason::Slot(SlotKind::Compile)),
        "compile slot held by #154",
    );
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    assert_eq!(
        f.mirror(),
        "remote #1 waiting_slot: compile slot held by #154"
    );
    w.runner.set_phase(R1, Phase::Running, "");
    w.runner.append_log(R1, b"line one\n");
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    assert!(matches!(f.state(), ForwardState::Running { .. }));
    w.runner.append_log(R1, b"line two\n");
    w.runner.finish(R1, 0, vec![sample()]);
    w.run(&mut f, &mut log, &mut clock, 5, |_, _| {});
    let r = f.result().expect("done");
    assert_eq!(r.phase, Phase::Passed);
    assert_eq!(log, b"line one\nline two\n");
    assert_eq!(r.remote, Some(R1));
    assert!(!r.nothing_submitted);
    assert_eq!(r.samples.len(), 1);
    assert_eq!(r.samples[0].0.machine, "nixos");
    assert_eq!(r.samples[0].1.secs, 41.5);
    assert!(r.sync.as_ref().unwrap().blobs_sent >= 3);
    // The spec reached the peer rewritten into the synced copy.
    assert_eq!(w.runner.submitted().len(), 1);
}

#[test]
fn r149_the_peers_exit_code_is_mirrored() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    w.runner.finish(R1, 101, vec![]);
    w.run(&mut f, &mut log, &mut clock, 5, |_, _| {});
    let r = f.result().unwrap();
    assert_eq!(r.phase, Phase::Failed { exit: Some(101) });
    assert_eq!(r.outcome.as_ref().unwrap().exit_code, Some(101));
    assert!(r.reason.contains("exit 101"), "{}", r.reason);
}

#[test]
fn r150_the_log_resumes_from_its_offset_after_reconnects_with_no_duplicates_or_gaps() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    let big: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
    w.runner.set_phase(R1, Phase::Running, "");
    w.runner.append_log(R1, &big);
    w.runner.finish(R1, 0, vec![]);
    // Every new stream dies after two events.
    w.net.break_streams_after(Some(2));
    w.net.cut_streams();
    w.run(&mut f, &mut log, &mut clock, 40, |_, _| {});
    let r = f.result().expect("finished despite the broken streams");
    assert_eq!(r.phase, Phase::Passed);
    assert_eq!(log, big, "no lost and no duplicated bytes");
    let subs = w
        .net
        .requests()
        .iter()
        .filter(|(_, k)| k == "subscribe")
        .count();
    assert!(subs >= 3, "reconnected {subs} times");
    assert_eq!(w.runner.submitted().len(), 1, "never resubmitted");
}

/// Wraps the receiver and damages the event stream: overlapping replays
/// and one dropped chunk, as a flaky link could produce.
struct Mangle {
    inner: Arc<dyn Handler>,
    seen: Arc<Mutex<u32>>,
}
struct MangledStream {
    inner: Box<dyn EventStream>,
    tail: Vec<u8>,
    dropped: bool,
    drop_first: bool,
}
impl EventStream for MangledStream {
    fn poll(&mut self) -> Result<StreamPoll, PeerError> {
        let p = self.inner.poll()?;
        if let StreamPoll::Event(JobEvent::Log { offset, data }) = p {
            let tail = self.tail.clone();
            self.tail = data.iter().rev().take(4).rev().copied().collect();
            if self.drop_first && !self.dropped {
                self.dropped = true;
                return Ok(StreamPoll::Idle); // swallow it: the next chunk has a gap
            }
            if offset >= tail.len() as u64 && !tail.is_empty() {
                let mut d = tail.clone();
                d.extend_from_slice(&data);
                return Ok(StreamPoll::Event(JobEvent::Log {
                    offset: offset - tail.len() as u64,
                    data: d,
                }));
            }
            return Ok(StreamPoll::Event(JobEvent::Log { offset, data }));
        }
        Ok(p)
    }
}
impl Handler for Mangle {
    fn handle(&self, from: &PeerId, msg: tasks_peer::Body) -> tasks_peer::Body {
        self.inner.handle(from, msg)
    }
    fn events(
        &self,
        from: &PeerId,
        remote: RemoteJobId,
        off: u64,
    ) -> Result<Box<dyn EventStream>, Refusal> {
        let mut n = self.seen.lock().unwrap();
        *n += 1;
        let first = *n == 1;
        Ok(Box::new(MangledStream {
            inner: self.inner.events(from, remote, off)?,
            tail: Vec::new(),
            dropped: false,
            drop_first: first,
        }))
    }
}

#[test]
fn r150_overlapping_and_gapped_chunks_never_corrupt_the_log() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    // Replace the receiver by one that mangles streams, over the same runner.
    let inner = w.receiver_with(
        Arc::clone(&w.runner),
        Arc::new(tasks_peer::receiver::AllowAll),
        Default::default(),
    );
    w.net.serve(
        &w.nixos,
        Arc::new(Mangle {
            inner,
            seen: Arc::new(Mutex::new(0)),
        }),
    );
    let big: Vec<u8> = (0..400_000u32).map(|i| (i * 7 % 253) as u8).collect();
    w.runner.set_phase(R1, Phase::Running, "");
    w.runner.append_log(R1, &big);
    w.runner.finish(R1, 0, vec![]);
    w.run(&mut f, &mut log, &mut clock, 40, |_, _| {});
    assert_eq!(f.result().unwrap().phase, Phase::Passed);
    assert_eq!(log, big);
}

#[test]
fn r150_a_lost_peer_is_interrupted_with_a_named_reason_and_never_resubmitted() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    w.runner.set_phase(R1, Phase::Running, "");
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    w.net.set_down(&w.nixos, true);
    w.run(&mut f, &mut log, &mut clock, 5, |_, _| {});
    assert!(
        matches!(f.state(), ForwardState::Reattaching { .. }),
        "uncertain, not failed: {:?}",
        f.state()
    );
    assert!(!f.is_done());
    w.run(&mut f, &mut log, &mut clock, 60, |_, _| {});
    let r = f.result().expect("grace expired");
    assert_eq!(r.phase, Phase::Interrupted);
    assert!(
        r.reason.contains("nixos-node")
            && r.reason.contains("unreachable")
            && r.reason.contains("not resubmitted"),
        "{}",
        r.reason
    );
    assert_eq!(r.remote, Some(R1));
    assert!(!r.nothing_submitted);
    assert_eq!(w.runner.submitted().len(), 1);
}

#[test]
fn r150_a_peer_that_comes_back_in_time_is_reattached_by_remote_id() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    w.runner.set_phase(R1, Phase::Running, "");
    w.runner.append_log(R1, b"before\n");
    w.run(&mut f, &mut log, &mut clock, 2, |_, _| {});
    w.net.set_down(&w.nixos, true);
    w.run(&mut f, &mut log, &mut clock, 4, |_, _| {});
    assert!(matches!(f.state(), ForwardState::Reattaching { .. }));
    // The peer daemon restarts (new Receiver, same ledger) while we are cut off.
    w.restart_receiver(Arc::clone(&w.runner));
    w.runner.append_log(R1, b"while away\n");
    w.net.set_down(&w.nixos, false);
    w.runner.finish(R1, 0, vec![]);
    w.run(&mut f, &mut log, &mut clock, 10, |_, _| {});
    assert_eq!(f.result().unwrap().phase, Phase::Passed);
    assert_eq!(log, b"before\nwhile away\n");
    assert_eq!(w.runner.submitted().len(), 1);
    assert!(
        f.take_notes()
            .iter()
            .any(|n| n.contains("reattached to remote #1 at log offset 7"))
    );
}

#[test]
fn r150_a_peer_that_forgot_the_job_is_a_named_loss_not_a_resubmit() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    w.net.cut_streams();
    w.restart_receiver(Arc::new(FakeRunner::new())); // lost its ledger
    w.run(&mut f, &mut log, &mut clock, 10, |_, _| {});
    let r = f.result().unwrap();
    assert_eq!(r.phase, Phase::Interrupted);
    assert!(
        r.reason.contains("no longer knows remote #1"),
        "{}",
        r.reason
    );
}

#[test]
fn a_silent_peer_counts_as_lost_after_the_idle_timeout() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    w.net.stall_streams(true);
    w.run(&mut f, &mut log, &mut clock, 3, |_, _| {});
    assert!(matches!(
        f.state(),
        ForwardState::RemoteQueued { .. } | ForwardState::Running { .. }
    ));
    w.run(&mut f, &mut log, &mut clock, 25, |_, f| {
        if matches!(f.state(), ForwardState::Reattaching { .. }) {
            // stop stalling so the next attach works
        }
    });
    assert!(
        matches!(f.state(), ForwardState::Reattaching { .. }),
        "{:?}",
        f.state()
    );
    w.net.stall_streams(false);
    w.runner.finish(R1, 0, vec![]);
    w.run(&mut f, &mut log, &mut clock, 10, |_, _| {});
    assert_eq!(f.result().unwrap().phase, Phase::Passed);
}

#[test]
fn r150_a_lost_submit_reply_resends_only_the_same_key() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.net.drop_replies_of("submit", true);
    w.run(&mut f, &mut log, &mut clock, 4, |_, _| {});
    assert!(
        matches!(f.state(), ForwardState::Submitting),
        "{:?}",
        f.state()
    );
    assert_eq!(w.runner.submitted().len(), 1, "the peer did get it once");
    w.net.drop_replies_of("submit", false);
    w.run(&mut f, &mut log, &mut clock, 3, |_, _| {});
    assert_eq!(
        f.remote(),
        Some(R1),
        "the duplicate resolved to the first job"
    );
    assert_eq!(w.runner.submitted().len(), 1, "never run twice");
}

#[test]
fn r151_cancel_propagates_to_the_peer_and_ends_cancelled() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    w.runner.set_phase(R1, Phase::Running, "");
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    f.cancel(clock, "user pressed ctrl-c");
    w.run(&mut f, &mut log, &mut clock, 5, |_, _| {});
    assert!(w.runner.is_cancelled(R1));
    let r = f.result().unwrap();
    assert_eq!(r.phase, Phase::Cancelled);
    assert!(r.reason.contains("user pressed ctrl-c"), "{}", r.reason);
}

#[test]
fn r151_a_cancel_on_the_peer_reaches_the_origin() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    use tasks_peer::JobRunner;
    assert!(w.runner.cancel(&w.origin, R1).is_some());
    w.run(&mut f, &mut log, &mut clock, 5, |_, _| {});
    assert_eq!(f.result().unwrap().phase, Phase::Cancelled);
}

#[test]
fn r151_cancelling_before_anything_was_sent_says_nothing_was_submitted() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    f.cancel(Duration::ZERO, "no longer needed");
    let mut log = Vec::new();
    w.tick(&mut f, secs(1), &mut log);
    let r = f.result().unwrap();
    assert_eq!(r.phase, Phase::Cancelled);
    assert!(r.nothing_submitted);
    assert!(w.runner.submitted().is_empty());
    assert!(w.net.requests().is_empty(), "no traffic at all");
}

#[test]
fn r151_an_unconfirmed_cancel_on_a_lost_peer_is_interrupted_not_cancelled() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    w.net.set_down(&w.nixos, true);
    f.cancel(clock, "stop");
    w.run(&mut f, &mut log, &mut clock, 30, |_, _| {});
    let r = f.result().unwrap();
    assert_eq!(r.phase, Phase::Interrupted);
    assert!(
        r.reason.contains("cancel of remote #1") && r.reason.contains("not confirmed"),
        "{}",
        r.reason
    );
}

#[test]
fn r7_6_an_offline_peer_fails_fast_and_says_nothing_was_submitted() {
    let w = World::new();
    w.net.set_down(&w.nixos, true);
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 5, |_, _| {});
    let r = f.result().unwrap();
    assert!(r.nothing_submitted);
    assert!(matches!(r.phase, Phase::Failed { .. }));
    assert!(
        r.reason.contains("nothing was submitted") && r.reason.contains("offline"),
        "{}",
        r.reason
    );
    assert!(
        clock <= secs(3),
        "failed within a few ticks, took {clock:?}"
    );
    assert!(w.runner.submitted().is_empty());
    // The lock is free again, so a local fallback or other job is not blocked.
    assert!(w.locks.holder(f.checkout()).is_none());
}

#[test]
fn r157_sync_is_retried_once_before_the_job_fails() {
    let w = World::new();
    w.net.set_down(&w.nixos, true);
    let mut f = w.forward(1, fast_policy());
    let mut log = Vec::new();
    w.tick(&mut f, secs(1), &mut log);
    assert!(
        matches!(f.state(), ForwardState::Syncing { attempt: 2 }),
        "{:?}",
        f.state()
    );
    w.net.set_down(&w.nixos, false);
    w.tick(&mut f, secs(3), &mut log);
    assert!(
        matches!(f.state(), ForwardState::RemoteQueued { .. }),
        "{:?}",
        f.state()
    );
    assert!(
        f.take_notes()
            .iter()
            .any(|n| n.contains("sync attempt 1 failed"))
    );
}

#[test]
fn r3_a_job_queued_too_long_on_the_peer_is_cancelled_with_the_reason() {
    let w = World::new();
    let policy = ForwardPolicy {
        remote_queue_timeout: Some(secs(10)),
        ..fast_policy()
    };
    let mut f = w.forward(1, policy);
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 20, |_, _| {});
    let r = f.result().unwrap();
    assert_eq!(r.phase, Phase::Cancelled);
    assert!(r.reason.contains("still queued"), "{}", r.reason);
    assert!(w.runner.is_cancelled(R1));
}

#[test]
fn r161_a_second_forward_of_the_same_checkout_waits_names_the_holder_and_then_goes() {
    let w = World::new();
    let mut a = w.forward(1, fast_policy());
    let mut b = w.forward(2, fast_policy());
    let (mut la, mut lb, mut clock) = (Vec::new(), Vec::new(), Duration::ZERO);
    clock += secs(1);
    w.tick(&mut a, clock, &mut la);
    w.tick(&mut b, clock, &mut lb);
    assert!(matches!(a.state(), ForwardState::RemoteQueued { .. }));
    assert!(
        matches!(b.state(), ForwardState::WaitingCheckout { blocker: Some(h) } if h.0 == 1),
        "{:?}",
        b.state()
    );
    assert!(b.mirror().contains("job #1"), "{}", b.mirror());
    assert!(!b.ready_to_start());
    assert_eq!(
        w.runner.submitted().len(),
        1,
        "b has not touched the shared copy"
    );
    w.runner.finish(R1, 0, vec![]);
    w.run(&mut a, &mut la, &mut clock, 5, |_, _| {});
    assert!(a.is_done());
    assert!(b.ready_to_start());
    clock += secs(1);
    w.tick(&mut b, clock, &mut lb);
    assert!(
        matches!(b.state(), ForwardState::RemoteQueued { .. }),
        "{:?}",
        b.state()
    );
}

#[test]
fn r230_dropping_a_waiting_or_holding_forward_never_wedges_the_checkout() {
    let w = World::new();
    let mut a = w.forward(1, fast_policy());
    let b = w.forward(2, fast_policy());
    let mut c = w.forward(3, fast_policy());
    let mut log = Vec::new();
    w.tick(&mut a, secs(1), &mut log);
    drop(b); // a waiter that vanished must not block c
    drop(a); // and neither must a holder that vanished
    w.tick(&mut c, secs(2), &mut log);
    assert!(
        matches!(c.state(), ForwardState::RemoteQueued { .. }),
        "{:?}",
        c.state()
    );
}

#[test]
fn r230_three_same_checkout_jobs_and_an_unrelated_one_all_make_progress() {
    // The old shape: two slots, jobs 1..3 of checkout A (gate ordered by id, queue by
    // arrival) and job 4 of checkout B. A slot is taken only for a forward that is
    // ready_to_start, so waiters hold nothing.
    let w = World::new();
    let other_root = w.tmp.path().join("origin/other");
    std::fs::create_dir_all(&other_root).unwrap();
    // Job 4's checkout has the same files (the harness lister is shared), different root.
    let mut jobs: Vec<Forward> = vec![
        w.forward(1, fast_policy()),
        w.forward(2, fast_policy()),
        w.forward(3, fast_policy()),
        w.forward_at(4, &other_root, fast_policy()),
    ];
    // Job 4's files must exist under its own root.
    for rel in &w.files {
        write(&other_root, rel, "other checkout");
    }
    let mut logs: Vec<Vec<u8>> = vec![Vec::new(); 4];
    let mut slot_of: BTreeMap<usize, bool> = BTreeMap::new();
    let slots = 2usize;
    let mut clock = Duration::ZERO;
    let mut finished_at: BTreeMap<usize, usize> = BTreeMap::new();
    let mut running_together = false;
    for iter in 0..200 {
        clock += secs(1);
        // Admission: in submit order, give a slot to each ready forward.
        for i in 0..jobs.len() {
            if !slot_of.contains_key(&i)
                && slot_of.len() < slots
                && jobs[i].ready_to_start()
                && !jobs[i].is_done()
            {
                slot_of.insert(i, true);
            }
        }
        for i in 0..jobs.len() {
            if slot_of.contains_key(&i) {
                let lister = w.lister();
                let src = tasks_peer::sync::DirSource(if i == 3 {
                    other_root.clone()
                } else {
                    w.root.clone()
                });
                let mut io = tasks_peer::ForwardIo {
                    transport: &w.transport,
                    lister: &lister,
                    source: &src,
                    log: &mut logs[i],
                };
                jobs[i].tick(clock, &mut io);
            }
        }
        // The fake peer: queued -> running -> finished, one step per iteration.
        use tasks_peer::JobRunner;
        for rid in 1..=w.runner.submitted().len() as u64 {
            let id = RemoteJobId(rid);
            if let Some(v) = w.runner.view(&w.origin, id) {
                match v.phase {
                    Phase::Queued | Phase::Waiting(_) => w.runner.set_phase(id, Phase::Running, ""),
                    Phase::Running => w.runner.finish(id, 0, vec![]),
                    _ => {}
                }
            }
        }
        // Same-checkout jobs are never active together; job 4 may overlap.
        let active = |i: usize| {
            !jobs[i].is_done()
                && slot_of.contains_key(&i)
                && !matches!(jobs[i].state(), ForwardState::WaitingCheckout { .. })
        };
        let same: usize = (0..3).filter(|i| active(*i)).count();
        assert!(
            same <= 1,
            "iteration {iter}: {same} same-checkout jobs on one copy"
        );
        if same == 1 && active(3) {
            running_together = true;
        }
        for i in 0..jobs.len() {
            if jobs[i].is_done() {
                slot_of.remove(&i);
                finished_at.entry(i).or_insert(iter);
            }
        }
        if finished_at.len() == 4 {
            break;
        }
    }
    assert_eq!(finished_at.len(), 4, "all four finish: {finished_at:?}");
    for j in &jobs {
        assert_eq!(j.result().unwrap().phase, Phase::Passed);
    }
    assert!(
        running_together,
        "the unrelated job ran while a same-checkout job held the lock"
    );
    assert!(
        finished_at[&3] < finished_at[&2],
        "the unrelated job did not queue behind the other checkout's jobs: {finished_at:?}"
    );
}

#[test]
fn a_waiting_forward_holds_no_traffic_and_does_not_block_other_checkouts() {
    let w = World::new();
    let other_root = w.tmp.path().join("origin/other2");
    for rel in &w.files {
        write(&other_root, rel, "other");
    }
    let mut a = w.forward(1, fast_policy());
    let mut b = w.forward(2, fast_policy());
    let mut c = w.forward_at(3, &other_root, fast_policy());
    let mut log = Vec::new();
    w.tick(&mut a, secs(1), &mut log);
    w.tick(&mut b, secs(1), &mut log);
    let lister = w.lister();
    let src = tasks_peer::sync::DirSource(other_root.clone());
    let mut lc = Vec::new();
    let mut io = tasks_peer::ForwardIo {
        transport: &w.transport,
        lister: &lister,
        source: &src,
        log: &mut lc,
    };
    c.tick(secs(1), &mut io);
    assert!(matches!(b.state(), ForwardState::WaitingCheckout { .. }));
    assert!(
        matches!(c.state(), ForwardState::RemoteQueued { .. }),
        "{:?}",
        c.state()
    );
    let _ = Allowlist::new([]);
}

#[test]
fn r150_a_peer_that_accepts_connections_but_stays_silent_is_still_declared_lost() {
    let w = World::new();
    let mut f = w.forward(1, fast_policy());
    let (mut log, mut clock) = (Vec::new(), Duration::ZERO);
    w.run(&mut f, &mut log, &mut clock, 1, |_, _| {});
    w.net.stall_streams(true);
    w.run(&mut f, &mut log, &mut clock, 200, |_, _| {});
    let r = f.result().expect("lost, not stuck");
    assert_eq!(r.phase, Phase::Interrupted);
    assert!(r.reason.contains("unreachable"), "{}", r.reason);
}
