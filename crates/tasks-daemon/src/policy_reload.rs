//! The machine policy file is applied when it changes, without a daemon restart (R-798).
//!
//! The daemon loop looks at the file's modified time and length before each tick. A changed
//! file that loads cleanly replaces the policy in force; a file that cannot be read or is
//! refused keeps the policy in force and sets its problems, so the `machine-policy-invalid`
//! alert names them. A removed file means the defaults. Keys read only at startup
//! ([`crate::machine_policy::RESTART_ONLY`] and `[peers]`) keep their running values and are reported as waiting for
//! the next start. Every applied or refused reload goes to the daemon log and the audit log.

use crate::machine_policy::MachinePolicy;
use crate::scheduler::{Scheduler, lock};
use std::sync::{Arc, PoisonError};
use std::time::SystemTime;
use tasks_ledger::{AuditEvent, action};

/// The file's modified time and length; `None` when it does not exist.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &std::path::Path) -> Stamp {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// What a reload did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reload {
    /// The file is as last seen.
    Unchanged,
    /// The file changed and its values are in force; `changed` lists `key: old -> new`.
    Applied {
        changed: Vec<String>,
        restart: Vec<String>,
    },
    /// The file changed but was refused; the policy in force stays.
    Refused(Vec<String>),
}

impl Scheduler {
    /// Applies the machine policy file if it changed since the last look. The first look only
    /// notes the file: the daemon loaded it at startup.
    pub(crate) fn reload_policy_if_changed(&self) -> Reload {
        let path = self.cfg.policy_path();
        let now = stamp(&path);
        {
            let mut st = lock(&self.state);
            match st.policy_stamp {
                None => {
                    st.policy_stamp = Some(now);
                    return Reload::Unchanged;
                }
                Some(seen) if seen == now => return Reload::Unchanged,
                Some(_) => st.policy_stamp = Some(now),
            }
        }
        let env = |k: &str| {
            self.parent_env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        let before = self.policy();
        let outcome = match MachinePolicy::load_strict(&path, &env) {
            Ok(mut next) => {
                next.keep_restart_only(&before);
                let (changed, _) = next.changes_from(&before);
                let restart = restart_pending(&path, &env, &before);
                self.set_policy(next);
                Reload::Applied { changed, restart }
            }
            Err(problems) => {
                let mut kept = (*before).clone();
                kept.problems = problems.clone();
                kept.problems
                    .push("machine policy file refused; the policy in force is kept".into());
                self.set_policy(kept);
                Reload::Refused(problems)
            }
        };
        self.report_reload(&path, &outcome);
        self.wake().notify();
        outcome
    }

    fn set_policy(&self, next: MachinePolicy) {
        *self.policy.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(next);
    }

    fn report_reload(&self, path: &std::path::Path, outcome: &Reload) {
        let (line, ev) = match outcome {
            Reload::Unchanged => return,
            Reload::Applied { changed, restart } => {
                let mut line = format!(
                    "machine policy reloaded from {}: {}",
                    path.display(),
                    if changed.is_empty() {
                        "no value changed".to_string()
                    } else {
                        changed.join(", ")
                    }
                );
                if !restart.is_empty() {
                    line.push_str(&format!(
                        "; {} take effect at the next daemon start",
                        restart.join(", ")
                    ));
                }
                let ev = AuditEvent::new(action::POLICY_RELOAD)
                    .detail("applied", true)
                    .detail("changed", changed.clone())
                    .detail("restart_pending", restart.clone());
                (line, ev)
            }
            Reload::Refused(problems) => {
                let line = format!(
                    "machine policy {} refused, the policy in force is kept: {}",
                    path.display(),
                    problems.join("; ")
                );
                let ev = AuditEvent::new(action::POLICY_RELOAD)
                    .detail("applied", false)
                    .detail("problems", problems.clone());
                (line, ev)
            }
        };
        crate::daemon_log::say(line);
        self.audit(ev);
    }
}

/// The startup-only keys whose file value differs from the running one.
fn restart_pending(
    path: &std::path::Path,
    env: &dyn Fn(&str) -> Option<String>,
    running: &MachinePolicy,
) -> Vec<String> {
    MachinePolicy::load_strict(path, env)
        .map(|wanted| wanted.changes_from(running).1)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Rig;

    fn write(r: &Rig, text: &str) {
        let p = r.sch.cfg.policy_path();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        // a different length always changes the stamp, even within the clock's resolution
        std::fs::write(&p, text).unwrap();
    }

    #[test]
    fn r798_a_changed_policy_file_is_applied_without_a_restart() {
        let r = Rig::new();
        assert_eq!(
            r.sch.reload_policy_if_changed(),
            Reload::Unchanged,
            "first look"
        );
        assert_eq!(r.sch.reload_policy_if_changed(), Reload::Unchanged);
        write(&r, "[slots]\noffload = 7\n");
        match r.sch.reload_policy_if_changed() {
            Reload::Applied { changed, restart } => {
                assert!(
                    changed
                        .iter()
                        .any(|c| c.starts_with("slots.offload:") && c.ends_with("-> 7")),
                    "{changed:?}"
                );
                assert!(restart.is_empty(), "{restart:?}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(r.sch.policy().u("slots.offload"), 7);
        assert!(r.sch.policy().file_loaded);
        assert_eq!(r.sch.reload_policy_if_changed(), Reload::Unchanged);
    }

    #[test]
    fn r798_a_refused_file_keeps_the_policy_in_force_and_raises_its_problems() {
        let r = Rig::new();
        r.sch.reload_policy_if_changed();
        write(&r, "[slots]\noffload = 7\n");
        r.sch.reload_policy_if_changed();
        write(&r, "[slots]\noffload = \"many\"\nbogus = 1\n");
        assert!(matches!(
            r.sch.reload_policy_if_changed(),
            Reload::Refused(p) if !p.is_empty()
        ));
        assert_eq!(r.sch.policy().u("slots.offload"), 7, "kept");
        assert!(!r.sch.policy_problems().is_empty());
        // fixing the file clears the problems
        write(&r, "[slots]\noffload = 6\n");
        assert!(matches!(
            r.sch.reload_policy_if_changed(),
            Reload::Applied { .. }
        ));
        assert_eq!(r.sch.policy().u("slots.offload"), 6);
        assert!(r.sch.policy_problems().is_empty());
    }

    #[test]
    fn r798_startup_only_keys_keep_their_running_value_and_wait_for_a_restart() {
        let r = Rig::new();
        r.sch.reload_policy_if_changed();
        let before = r.sch.policy().u("daemon.nofile_target");
        write(&r, &format!("[daemon]\nnofile_target = {}\n", before + 1));
        match r.sch.reload_policy_if_changed() {
            Reload::Applied { restart, .. } => {
                assert_eq!(restart, vec!["daemon.nofile_target".to_string()]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(r.sch.policy().u("daemon.nofile_target"), before);
    }

    #[test]
    fn r798_a_removed_file_means_the_defaults() {
        let r = Rig::new();
        r.sch.reload_policy_if_changed();
        write(&r, "[slots]\noffload = 7\n");
        r.sch.reload_policy_if_changed();
        std::fs::remove_file(r.sch.cfg.policy_path()).unwrap();
        assert!(matches!(
            r.sch.reload_policy_if_changed(),
            Reload::Applied { .. }
        ));
        assert_eq!(
            r.sch.policy().u("slots.offload"),
            MachinePolicy::defaults().u("slots.offload")
        );
        assert!(!r.sch.policy().file_loaded);
    }
}
