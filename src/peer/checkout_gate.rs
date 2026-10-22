//! Admission-time gate for forwarded jobs of one checkout.
//!
//! A forwarded job (`tasks peer-forward`) takes the per-checkout client lock (see
//! `forward::sync_and_submit`) AFTER it already holds an offload slot, because the slot is what
//! starts the bridge that takes the lock. A second `--anywhere` job of the same checkout used
//! to sit in a slot (phase `running`) for as long as the first one ran. The slot cannot be handed
//! back from inside the bridge, so the wait moves in front of the slot: the worker's offload
//! admission asks this gate and stays queued (holding nothing, reported as `waiting_for`) while
//!
//! * the checkout's client lock is busy (another forward or a `tasks-offload`), or
//! * another job of the same checkout is already running in a slot (its bridge may not have
//!   taken the lock yet), or
//! * an earlier (lower id) job of the same checkout is queued ahead, so two waiters do not both
//!   take a slot the moment the holder ends.
//!
//! The decision is advisory: the bridge still takes the real lock, so a race only costs the old
//! behaviour for that one job.
use super::forward::{checkout_lock_path, try_flock};
use crate::{Job, Phase};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

/// How often the gate re-reads the ledger and the lock (admission polls every 100 ms).
const GATE_EVERY: Duration = Duration::from_secs(1);

/// Why forwarded job `job` of checkout `top` must stay queued, from the other jobs of the
/// ledger and whether the lock is busy. `None` admits (and for a job that is not forwarded).
pub fn blocker(job: &Job, top: &str, jobs: &[Job], lock_busy: bool) -> Option<String> {
    if !is_forward(job) {
        return None;
    }
    let me = job.id;
    let mut running = None;
    let mut ahead = None;
    for job in jobs.iter().filter(|j| j.id != me && same_checkout(j, top)) {
        match job.phase {
            Phase::Running => running = running.or(Some(job.id)),
            Phase::WaitingRun if job.id < me => ahead = ahead.or(Some(job.id)),
            _ => {}
        }
    }
    if let Some(id) = running {
        Some(format!("this checkout in use by #{id}"))
    } else if let Some(id) = ahead {
        Some(format!("behind #{id} (same checkout)"))
    } else {
        lock_busy.then(|| "this checkout in use (offload lock held)".into())
    }
}

fn is_forward(job: &Job) -> bool {
    crate::local_command::Recipe::from_spec(&job.spec)
        .is_ok_and(|r| crate::peer_link::is_forward_argv(&r.argv))
}

/// A forwarded job whose bridge works on a checkout containing `top`.
fn same_checkout(job: &Job, top: &str) -> bool {
    is_forward(job) && std::path::Path::new(&job.spec.source).starts_with(top)
}

/// Rate-limited [`blocker`] for one job, for the admission wait loop.
pub struct Gate {
    job: Job,
    top: String,
    lock: Option<std::path::PathBuf>,
    last: Cell<Option<Instant>>,
    text: RefCell<Option<String>>,
}

impl Gate {
    /// `None` when the job is not a forwarded job or its checkout cannot be determined (no gate).
    pub fn for_job(job: &Job) -> Option<Self> {
        let recipe = crate::local_command::Recipe::from_spec(&job.spec).ok()?;
        if !crate::peer_link::is_forward_argv(&recipe.argv) {
            return None;
        }
        let top =
            task_manager_peer::select::git_toplevel(std::path::Path::new(&job.spec.source)).ok()?;
        let top = top.to_str()?.to_owned();
        let cid =
            task_manager_peer::select::checkout_id(&task_manager_peer::select::hostname(), &top);
        Some(Self {
            job: job.clone(),
            lock: checkout_lock_path(&cid),
            top,
            last: Cell::new(None),
            text: RefCell::new(None),
        })
    }

    /// The current reason to keep waiting; `read` supplies the ledger's jobs.
    pub fn check(&self, read: impl FnOnce() -> Vec<Job>) -> Option<String> {
        if self.last.get().is_some_and(|t| t.elapsed() < GATE_EVERY) {
            return self.text.borrow().clone();
        }
        self.last.set(Some(Instant::now()));
        let busy = self
            .lock
            .as_ref()
            .is_some_and(|p| matches!(try_flock(p), Ok(None)));
        let text = blocker(&self.job, &self.top, &read(), busy);
        *self.text.borrow_mut() = text.clone();
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_command::{Recipe, Resource};

    fn job(id: i64, phase: Phase, source: &str, forward: bool) -> Job {
        let words: &[&str] = if forward {
            &[
                "tasks",
                "peer-forward",
                "--state",
                "/s",
                "--peer",
                "p",
                "--who",
                "w",
                "--",
                "cargo",
                "test",
            ]
        } else {
            &["tasks-offload", "--", "true"]
        };
        let argv = words.iter().map(|w| w.to_string()).collect();
        let spec = Recipe {
            broad_test: false,
            argv,
            resource: Resource::Offload,
        }
        .spec("o".into(), source.into())
        .unwrap();
        Job {
            id,
            spec,
            phase,
            submitted_at: 0,
            execution: None,
        }
    }

    #[test]
    fn running_job_of_the_checkout_holds_the_next_one_back() {
        let me = job(9, Phase::WaitingRun, "/w/c", true);
        let jobs = [job(4, Phase::Running, "/w/c/sub", true), me.clone()];
        assert_eq!(
            blocker(&me, "/w/c", &jobs, false).as_deref(),
            Some("this checkout in use by #4")
        );
    }

    #[test]
    fn earlier_waiter_goes_first_and_later_one_does_not_block() {
        let early = job(3, Phase::WaitingRun, "/w/c", true);
        let late = job(9, Phase::WaitingRun, "/w/c", true);
        let jobs = [early.clone(), late.clone()];
        assert_eq!(
            blocker(&late, "/w/c", &jobs, false).as_deref(),
            Some("behind #3 (same checkout)")
        );
        assert_eq!(blocker(&early, "/w/c", &jobs, false), None);
    }

    #[test]
    fn running_beats_waiting_and_lock_alone_still_blocks() {
        let me = job(9, Phase::WaitingRun, "/w/c", true);
        let jobs = [
            job(3, Phase::WaitingRun, "/w/c", true),
            job(5, Phase::Running, "/w/c", true),
            me.clone(),
        ];
        assert_eq!(
            blocker(&me, "/w/c", &jobs, false).as_deref(),
            Some("this checkout in use by #5")
        );
        assert_eq!(
            blocker(&me, "/w/c", std::slice::from_ref(&me), true).as_deref(),
            Some("this checkout in use (offload lock held)")
        );
        assert_eq!(blocker(&me, "/w/c", std::slice::from_ref(&me), false), None);
    }

    #[test]
    fn other_checkouts_finished_jobs_and_plain_offloads_do_not_count() {
        let me = job(9, Phase::WaitingRun, "/w/c", true);
        let jobs = [
            job(1, Phase::Running, "/w/other", true),
            job(2, Phase::Passed, "/w/c", true),
            job(3, Phase::Running, "/w/c", false),
            me.clone(),
        ];
        assert_eq!(blocker(&me, "/w/c", &jobs, false), None);
        // A job that is not forwarded is never gated.
        let plain = job(10, Phase::WaitingRun, "/w/c", false);
        assert_eq!(blocker(&plain, "/w/c", &jobs, true), None);
    }
}
