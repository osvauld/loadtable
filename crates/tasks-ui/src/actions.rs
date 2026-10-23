//! What a row's buttons may do, with the reason when they may not (R-206), and how the
//! queue-stopped banner reads (R-189). Pure.

use crate::format;
use tasks_daemon::api::{DaemonView, JobView};

/// One button's availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Avail {
    /// Usable.
    Enabled,
    /// Greyed out; the text is the tooltip that says why.
    Disabled(String),
}

impl Avail {
    /// True when the button works.
    pub fn is_enabled(&self) -> bool {
        matches!(self, Avail::Enabled)
    }

    /// The tooltip of a disabled button.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Avail::Enabled => None,
            Avail::Disabled(r) => Some(r),
        }
    }
}

/// The actions of one job row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actions {
    /// Cancel the job.
    pub cancel: Avail,
    /// Open its log.
    pub log: Avail,
}

/// Which actions apply to a job. The local daemon is the authority for every job in its
/// ledger, forwarded ones included: cancelling a forward cancels the local job and the daemon
/// stops the remote one. A forward's log only exists on this machine once the daemon mirrors
/// it (`log_path`); until then the button says so instead of opening an empty viewer.
pub fn actions_for(job: &JobView, local_machine: &str) -> Actions {
    let finished = job.phase.is_terminal();
    let cancel = if finished {
        Avail::Disabled("Already finished: there is nothing to cancel.".into())
    } else if job.cancel_requested_by.is_some() {
        Avail::Disabled("A cancel was already requested; the daemon is stopping the job.".into())
    } else {
        Avail::Enabled
    };
    let remote = job
        .machine
        .as_deref()
        .or(job.forwarded_to.as_ref().map(|f| f.0.as_str()))
        .is_some_and(|m| m != local_machine);
    let log = if job.started_at_ms.is_none() && !finished {
        Avail::Disabled("No log yet: the job has not started.".into())
    } else if job.log_path.is_some() {
        Avail::Enabled
    } else if remote {
        Avail::Disabled(
            "The log lives on the other machine and this daemon holds no copy of it.".into(),
        )
    } else {
        Avail::Disabled("This job has no log.".into())
    };
    Actions { cancel, log }
}

/// The banner text when the queue is held, or `None` when it runs (R-189). Names who stopped
/// it, how long ago, and how to resume.
pub fn queue_banner(d: &DaemonView) -> Option<String> {
    if !d.draining && d.execution_enabled {
        return None;
    }
    let mut text = String::new();
    if d.draining {
        let who = d.drained_by.as_deref().unwrap_or("someone (not recorded)");
        text.push_str(&format!("Queue drained by {who}"));
        if let Some(at) = d.drained_at_ms.filter(|_| d.now_ms > 0) {
            let secs = u64::try_from((d.now_ms - at).max(0) / 1000).unwrap_or(0);
            text.push_str(&format!(", {}", format::ago(secs)));
        }
        text.push_str(". Running jobs finish; queued jobs wait. Press Resume to start them.");
    } else {
        text.push_str("Execution is stopped (it persists across restarts). Queued jobs wait; resume with `tasks2 start-worker`.");
    }
    Some(text)
}

/// What the drain button does: `(label, cancel_flag)` for `Request::Drain`.
pub fn drain_button(d: &DaemonView) -> (&'static str, bool) {
    if d.draining {
        ("Resume", true)
    } else {
        ("Drain", false)
    }
}

/// The label a cancel confirmation uses.
pub fn cancel_prompt(job: &JobView) -> String {
    format!(
        "Cancel job #{} ({}, {})?",
        job.id.0,
        format::agent_of(&job.owner),
        format::truncate(&format::command_display(&job.argv), 60)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn r189_a_stopped_queue_says_who_stopped_it_how_long_ago_and_how_to_resume() {
        let mut d = fixtures::daemon("laptop");
        assert_eq!(queue_banner(&d), None);
        d.draining = true;
        d.drained_by = Some("lead".into());
        d.drained_at_ms = Some(d.now_ms - 125_000);
        let b = queue_banner(&d).unwrap_or_default();
        assert!(b.contains("drained by lead"), "{b}");
        assert!(b.contains("2m05s ago"), "{b}");
        assert!(b.contains("Resume"), "{b}");
        assert_eq!(drain_button(&d), ("Resume", true));
        d.drained_by = None;
        d.drained_at_ms = None;
        let b = queue_banner(&d).unwrap_or_default();
        assert!(b.contains("not recorded"), "{b}");
        assert!(!b.contains("worker not running"));
        d.draining = false;
        assert_eq!(drain_button(&d), ("Drain", false));
        d.execution_enabled = false;
        assert!(queue_banner(&d).unwrap_or_default().contains("stopped"));
    }

    #[test]
    fn r206_remote_and_finished_jobs_have_disabled_actions_with_a_tooltip() {
        let running = fixtures::job(10, "claude@laptop", fixtures::Phase::Running);
        let a = actions_for(&running, "laptop");
        assert!(a.cancel.is_enabled() && a.log.is_enabled());

        let mut done = fixtures::job(11, "claude@laptop", fixtures::Phase::Passed);
        done.log_path = Some("/tmp/11.log".into());
        let a = actions_for(&done, "laptop");
        assert!(a.cancel.reason().unwrap_or_default().contains("finished"));
        assert!(a.log.is_enabled());

        let mut fwd = fixtures::job(12, "claude@laptop", fixtures::Phase::Running);
        fwd.machine = Some("nixos".into());
        fwd.forwarded_to = Some(("nixos".into(), Some(77)));
        fwd.log_path = None;
        let a = actions_for(&fwd, "laptop");
        assert!(
            a.cancel.is_enabled(),
            "the local daemon can cancel a forward"
        );
        assert!(a.log.reason().unwrap_or_default().contains("other machine"));

        let queued = fixtures::job(13, "claude@laptop", fixtures::Phase::Queued);
        let a = actions_for(&queued, "laptop");
        assert!(a.log.reason().unwrap_or_default().contains("not started"));

        let mut cancelling = running.clone();
        cancelling.cancel_requested_by = Some("claude@laptop".into());
        assert!(!actions_for(&cancelling, "laptop").cancel.is_enabled());
    }
}
