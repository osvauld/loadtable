//! How the queue explains itself: a waiting job's class and why, and a forwarded job that the
//! peer holds back. Pure text from values; the clock is passed in.

use tasks_admission::Effective;
use tasks_admission::kind_name;
use tasks_core::{Phase, Priority, WaitReason};
use tasks_ledger::JobRecord;

/// "20m", "1h05m": a wait in words.
fn span(ms: u64) -> String {
    let mins = ms / 60_000;
    if mins >= 60 {
        format!("{}h{:02}m", mins / 60, mins % 60)
    } else if mins >= 1 {
        format!("{mins}m")
    } else {
        format!("{}s", ms / 1000)
    }
}

/// Why a waiting job is in the class the queue orders it by. Empty for an ordinary normal job.
///
/// - `blocking`: the reason it was claimed with;
/// - `background`: that it rises to normal after the aging wait, and how long it has waited;
/// - a job aging has lifted: that it was submitted `background` and rose after the wait.
pub fn class_note(
    declared: Priority,
    reason: Option<&str>,
    eff: &Effective,
    waited_ms: u64,
    aging_ms: u64,
) -> String {
    match (declared, eff.is_aged()) {
        (Priority::Blocking, _) => match reason {
            Some(r) => format!("blocking: {r}"),
            None => "blocking".to_string(),
        },
        (Priority::Background, true) => format!(
            "submitted background, rose to {} after {} (aging); waited {}",
            eff.class,
            span(aging_ms),
            span(waited_ms)
        ),
        (Priority::Background, false) if aging_ms > 0 => format!(
            "background: rises to normal after {} (waited {})",
            span(aging_ms),
            span(waited_ms)
        ),
        (Priority::Background, false) => "background (aging is off)".to_string(),
        (Priority::Normal, _) => String::new(),
    }
}

/// The peer's reason for a forwarded job whose local row says `Running` while the peer still
/// holds it back on its own gates or slots. `None` when the job is not forwarded, is not marked
/// running here, or the peer says it runs (R-015: the local phase is not rewritten; this is
/// only how it is shown).
pub fn remote_wait(r: &JobRecord) -> Option<String> {
    let fw = r.forward.as_ref()?;
    if r.phase != Phase::Running {
        return None;
    }
    let peer = &fw.peer.0;
    match r.remote_phase.as_ref()? {
        Phase::Queued => Some(format!("queued on {peer}")),
        Phase::Waiting(w) => Some(match w {
            WaitReason::Slot(k) => format!("waiting on {peer}: no free {} slot", kind_name(*k)),
            WaitReason::Pressure(t) | WaitReason::Placement(t) => {
                format!("waiting on {peer}: {t}")
            }
            WaitReason::Dependency(j) => format!("waiting on {peer}: after job #{}", j.0),
            WaitReason::Disk => format!("waiting on {peer}: low disk space"),
            WaitReason::Device(d) => format!("waiting on {peer}: device {} busy", d.0),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_admission::Waiter;
    use tasks_admission::effective;
    use tasks_core::{JobId, MachineName, SlotKind};
    use tasks_ledger::Forward;

    const MIN: u64 = 60_000;

    fn eff(declared: Priority, waited: u64) -> Effective {
        let mut w = Waiter::new(JobId(1), SlotKind::Compile, 0);
        w.priority = declared;
        effective(&w, waited, 20 * MIN)
    }

    #[test]
    fn r340_the_note_says_why_the_class_is_what_it_is() {
        let n = |d, reason: Option<&str>, waited| {
            class_note(d, reason, &eff(d, waited), waited, 20 * MIN)
        };
        assert_eq!(n(Priority::Normal, None, 5 * MIN), "");
        assert_eq!(
            n(Priority::Blocking, Some("release gate"), 5 * MIN),
            "blocking: release gate"
        );
        assert_eq!(
            n(Priority::Background, None, 5 * MIN),
            "background: rises to normal after 20m (waited 5m)"
        );
        assert_eq!(
            n(Priority::Background, None, 65 * MIN),
            "submitted background, rose to normal after 20m (aging); waited 1h05m"
        );
        let off = class_note(
            Priority::Background,
            None,
            &eff(Priority::Background, 0),
            0,
            0,
        );
        assert_eq!(off, "background (aging is off)");
    }

    fn forwarded(phase: Phase, remote: Option<Phase>) -> JobRecord {
        let spec = tasks_core::JobSpec {
            owner: tasks_core::Owner::new("alice").unwrap(),
            argv: vec!["cargo".into(), "check".into()],
            cwd: "/work".into(),
            env: vec![],
            kind: tasks_core::Kind::Compile,
            requirements: tasks_core::Requirements::default(),
            placement: tasks_core::Placement::Anywhere,
            after: vec![],
            timeout_secs: None,
            broad_test_escape: false,
            priority: Priority::Normal,
            priority_reason: None,
            fetch: Vec::new(),
        };
        let store = tasks_ledger::MemoryStore::new();
        let id = tasks_ledger::Store::submit(&store, &spec).unwrap();
        let mut r = tasks_ledger::Store::get(&store, id).unwrap().unwrap();
        r.phase = phase;
        r.remote_phase = remote;
        r.forward = Some(Forward {
            peer: MachineName("nixos".into()),
            remote_job: Some(3),
            remote_key: None,
        });
        r
    }

    // R-349
    #[test]
    fn r349_a_job_the_peer_holds_back_shows_the_peers_reason() {
        let held = forwarded(
            Phase::Running,
            Some(Phase::Waiting(WaitReason::Pressure(
                "CPU contended: pressure 63% > 40% for compile".into(),
            ))),
        );
        assert_eq!(
            remote_wait(&held).as_deref(),
            Some("waiting on nixos: CPU contended: pressure 63% > 40% for compile")
        );
        let slot = forwarded(
            Phase::Running,
            Some(Phase::Waiting(WaitReason::Slot(SlotKind::Compile))),
        );
        assert_eq!(
            remote_wait(&slot).as_deref(),
            Some("waiting on nixos: no free compile slot")
        );
        let queued = forwarded(Phase::Running, Some(Phase::Queued));
        assert_eq!(remote_wait(&queued).as_deref(), Some("queued on nixos"));
        // running there, unknown there, or not marked running here: nothing to add
        assert_eq!(
            remote_wait(&forwarded(Phase::Running, Some(Phase::Running))),
            None
        );
        assert_eq!(remote_wait(&forwarded(Phase::Running, None)), None);
        assert_eq!(
            remote_wait(&forwarded(Phase::Queued, Some(Phase::Queued))),
            None
        );
        // a job that never left this machine
        let mut local = forwarded(Phase::Running, Some(Phase::Queued));
        local.forward = None;
        assert_eq!(remote_wait(&local), None);
    }
}
