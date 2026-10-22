//! What the scheduler does besides admitting: the log-size watchdog, the stuck-job detector,
//! the periodic holder audit and retention, and the alert evaluation.

use crate::alerts::AlertLevel;
use crate::holders::Holder;
use crate::logs;
use crate::machine_policy::MachinePolicy;
use crate::scheduler::{RunningJob, Scheduler, lock};
use crate::startup::{group_of, terminate_group};
use crate::stuck::tail_is_terminal;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use tasks_admission::kind_name;
use tasks_core::{JobId, SlotKind};
use tasks_platform::Level;

/// The lock file of a slot, as the platform's `FlockSlots` names it.
pub fn slot_lock_path(dir: &std::path::Path, kind: SlotKind, index: u32) -> PathBuf {
    dir.join(format!("{}-{index}.lock", kind_name(kind)))
}

const MIB: u64 = 1 << 20;

impl Scheduler {
    /// Stops a running job for a stated reason; the job ends `Failed` with the reason in its
    /// summary rather than `Cancelled`.
    pub(crate) fn kill(&self, rj: &RunningJob, reason: String) {
        {
            let mut r = lock(&rj.kill_reason);
            if r.is_some() {
                return;
            }
            *r = Some(reason);
        }
        rj.token.cancel();
        self.wake.notify();
    }

    /// The log-size watchdog and the stuck-job detector (R-095, R-096).
    pub(crate) fn supervise(&self, policy: &MachinePolicy, now_ms: i64) {
        let jobs: Vec<Arc<RunningJob>> = lock(&self.running).values().cloned().collect();
        let hard = policy.log_hard_cap_bytes();
        for rj in jobs {
            if lock(&rj.kill_reason).is_some() {
                continue;
            }
            let len = std::fs::metadata(&rj.log_path)
                .map(|m| m.len())
                .unwrap_or(0);
            if len > hard {
                self.kill(
                    &rj,
                    format!(
                        "killed by tasks: the log grew past the hard cap of {} MiB",
                        hard / MIB
                    ),
                );
                continue;
            }
            let group = *lock(&rj.group);
            let cpu = group
                .and_then(|g| self.platform.procs.usage(g).ok())
                .map(|u| u.cpu_user_s + u.cpu_sys_s);
            let quiet = lock(&rj.stuck).quiet_for_window(now_ms, len, cpu);
            if quiet {
                let tail = logs::tail(&rj.log_path, 40, false).unwrap_or_default();
                if tail_is_terminal(&tail) {
                    self.kill(
                        &rj,
                        format!(
                            "killed as stuck: the log tail shows a terminal failure, and neither the log nor the group's CPU moved for {} s",
                            policy.stuck_window_secs()
                        ),
                    );
                }
            }
        }
    }

    /// Retention every hour and the holder audit on the configured period (R-049).
    pub(crate) fn maintenance(&self, policy: &MachinePolicy, now_ms: i64) {
        let (audit_due, prune_due) = {
            let mut st = lock(&self.state);
            let audit_ms =
                i64::try_from(policy.audit_secs().saturating_mul(1000)).unwrap_or(i64::MAX);
            let audit_due = st.last_audit_ms == 0 || now_ms - st.last_audit_ms >= audit_ms;
            if audit_due {
                st.last_audit_ms = now_ms;
            }
            let prune_due = st.last_prune_ms == 0 || now_ms - st.last_prune_ms >= 3_600_000;
            if prune_due {
                st.last_prune_ms = now_ms;
            }
            (audit_due, prune_due)
        };
        if prune_due {
            let _ = self.guard("prune", self.ledger.prune(&policy.retention()));
        }
        if audit_due {
            self.audit_holders(policy);
        }
    }

    /// Audits slot holders: tracked, orphan of an ended job (its group is stopped), or
    /// untracked (named, never signalled) (R-049).
    pub fn audit_holders(&self, policy: &MachinePolicy) -> Vec<String> {
        let mut report = Vec::new();
        let tracked: BTreeSet<(SlotKind, u32)> = lock(&self.running)
            .values()
            .filter_map(|r| r.slot)
            .collect();
        let layout = policy.layout();
        for (kind, n) in &layout.counts {
            for index in 0..*n {
                if tracked.contains(&(*kind, index)) || self.slot_is_free(*kind, index) {
                    continue;
                }
                let path = slot_lock_path(&self.cfg.lock_dir, *kind, index);
                let holders = self.platform.holders.holders(&path);
                let line = self.classify(policy, *kind, index, &holders);
                lock(&self.state)
                    .orphan_text
                    .insert((*kind, index), line.clone());
                report.push(line);
            }
        }
        report
    }

    /// True when the slot can be taken right now. Probing takes and drops the lock.
    pub(crate) fn slot_is_free(&self, kind: SlotKind, index: u32) -> bool {
        matches!(self.platform.locks.try_acquire(kind, index), Ok(Some(_)))
    }

    fn classify(
        &self,
        policy: &MachinePolicy,
        kind: SlotKind,
        index: u32,
        holders: &[Holder],
    ) -> String {
        let slot = format!("{}-{index}", kind_name(kind));
        if holders.is_empty() {
            return format!("{slot} is held by work this daemon cannot name");
        }
        let mut parts = Vec::new();
        for h in holders {
            let mut text = h.describe();
            if let Some(j) = h.origin_job {
                let rec = self.ledger.get(j).ok().flatten();
                match rec {
                    Some(r) if r.phase.is_terminal() => {
                        text.push_str(", which has ended");
                        // an orphan of an ended job: stop its group, never anyone else's
                        if let Some(l) = r.lease.as_ref() {
                            let grace = policy.cancel_grace();
                            let stopped =
                                terminate_group(&*self.platform.procs, group_of(l), grace, &|d| {
                                    std::thread::sleep(d)
                                });
                            match stopped {
                                Ok(true) => text.push_str(" (its group was stopped)"),
                                Ok(false) => text.push_str(" (its group is already gone)"),
                                Err(e) => text.push_str(&format!(" (could not stop it: {e})")),
                            }
                        }
                    }
                    Some(_) => text.push_str(" (job is live elsewhere)"),
                    None => text.push_str(" (job unknown to this ledger)"),
                }
            } else {
                text.push_str(" UNTRACKED");
            }
            parts.push(text);
        }
        format!("{slot} is held by {}", parts.join("; "))
    }

    /// Recomputes which alerts hold and reconciles the book (R-220, R-221, R-222).
    pub(crate) fn evaluate_alerts(&self, policy: &MachinePolicy, now_ms: i64) {
        let mut wanted: Vec<(String, AlertLevel, String)> = Vec::new();
        if !self.policy_problems().is_empty() {
            wanted.push((
                "machine-policy-invalid".into(),
                AlertLevel::Warn,
                format!("machine policy: {}", self.policy_problems().join("; ")),
            ));
        }
        for (id, path, reserve) in [
            (
                "disk-cache",
                &self.cfg.cache_dir,
                policy.cache_reserve_bytes(),
            ),
            (
                "disk-state",
                &self.cfg.state_dir,
                policy.state_reserve_bytes(),
            ),
        ] {
            if reserve == 0 {
                continue;
            }
            match self.platform.disk.free_bytes(path) {
                Ok(free) if free < reserve => wanted.push((
                    id.into(),
                    AlertLevel::Critical,
                    format!(
                        "{} has {} free, below its reserve of {}",
                        path.display(),
                        tasks_admission::gib(free),
                        tasks_admission::gib(reserve)
                    ),
                )),
                Ok(free) if free < reserve.saturating_mul(2) => wanted.push((
                    id.into(),
                    AlertLevel::Warn,
                    format!(
                        "{} has {} free, under twice its reserve of {}",
                        path.display(),
                        tasks_admission::gib(free),
                        tasks_admission::gib(reserve)
                    ),
                )),
                Ok(_) => {}
                Err(e) => wanted.push((
                    format!("{id}-unreadable"),
                    AlertLevel::Info,
                    format!("cannot measure {}: {e}", path.display()),
                )),
            }
        }
        // memory pressure must last before it alerts
        let level = self
            .platform
            .pressure
            .snapshot(&policy.psi())
            .map(|p| p.memory)
            .unwrap_or(Level::Normal);
        {
            let mut st = lock(&self.state);
            if level >= Level::Warn {
                let since = *st.mem_pressure_since.get_or_insert(now_ms);
                let need = i64::try_from(policy.memory_alert_secs().saturating_mul(1000))
                    .unwrap_or(i64::MAX);
                if now_ms - since >= need {
                    wanted.push((
                        "memory-pressure".into(),
                        if level == Level::Critical {
                            AlertLevel::Critical
                        } else {
                            AlertLevel::Warn
                        },
                        format!(
                            "memory pressure has stayed at {level:?} for {} s",
                            (now_ms - since) / 1000
                        ),
                    ));
                }
            } else {
                st.mem_pressure_since = None;
            }
            if let Some(e) = &st.readings_error {
                wanted.push((
                    "pressure-unreadable".into(),
                    AlertLevel::Warn,
                    format!("admission cannot measure the machine: {e}"),
                ));
            }
            if let Some((at, text)) = &st.last_error
                && now_ms - at < 60_000
            {
                wanted.push((
                    "ledger-error".into(),
                    AlertLevel::Warn,
                    format!("recent ledger error: {text}"),
                ));
            }
        }
        wanted.extend(self.orphan_alerts(policy, now_ms));
        lock(&self.alerts).reconcile(wanted, now_ms);
    }

    /// A slot held with no live job alerts only after the grace period (R-222).
    fn orphan_alerts(
        &self,
        policy: &MachinePolicy,
        now_ms: i64,
    ) -> Vec<(String, AlertLevel, String)> {
        let tracked: BTreeSet<(SlotKind, u32)> = lock(&self.running)
            .values()
            .filter_map(|r| r.slot)
            .collect();
        let grace =
            i64::try_from(policy.slot_grace_secs().saturating_mul(1000)).unwrap_or(i64::MAX);
        let mut out = Vec::new();
        let layout = policy.layout();
        let mut busy_now = BTreeSet::new();
        for (kind, n) in &layout.counts {
            for index in 0..*n {
                let key = (*kind, index);
                if tracked.contains(&key) || self.slot_is_free(*kind, index) {
                    continue;
                }
                busy_now.insert(key);
                let mut st = lock(&self.state);
                let since = *st.orphan_since.entry(key).or_insert(now_ms);
                if now_ms - since >= grace {
                    let who = st.orphan_text.get(&key).cloned().unwrap_or_else(|| {
                        format!(
                            "{}-{index} is held by work this daemon has not named yet",
                            kind_name(*kind)
                        )
                    });
                    out.push((
                        format!("slot-held-{}-{index}", kind_name(*kind)),
                        AlertLevel::Warn,
                        format!(
                            "{who}; no live job has held it for {} s",
                            (now_ms - since) / 1000
                        ),
                    ));
                }
            }
        }
        let mut st = lock(&self.state);
        st.orphan_since.retain(|k, _| busy_now.contains(k));
        st.orphan_text.retain(|k, _| busy_now.contains(k));
        out
    }

    /// Problems found in the machine policy at load time.
    pub(crate) fn policy_problems(&self) -> Vec<String> {
        self.policy().problems.clone()
    }

    /// Ends a job's cancel path: used by the API.
    pub(crate) fn stop_running(&self, id: JobId) -> bool {
        let rj = lock(&self.running).get(&id).cloned();
        match rj {
            Some(rj) => {
                rj.token.cancel();
                self.wake.notify();
                true
            }
            None => false,
        }
    }
}
