//! The submit rules every entry shares: the local API (jobs, plans, suites) and the peer
//! receiver apply the same checks, so the CLI, the MCP front end and forwarded jobs get the same
//! answer (R-191, R-210, R-091, R-153).
//!
//! The rules themselves are pure and live in `tasks_policy::submit_rules`; this module feeds
//! them what only the daemon knows: the machine policy (broad-test refuse or warn, the Cargo
//! cap) and the test targets on disk.

use crate::scheduler::Scheduler;
use std::path::Path;
use tasks_core::JobSpec;
use tasks_policy::submit_rules::{self, KnownTargets, SubmitRules};

/// Test targets of the package a `cargo test` names, counted from the layout on disk. Only
/// used to word a refusal; `None` when the package cannot be found.
pub fn scan_known_targets(cwd: &Path, packages: &[String]) -> Option<KnownTargets> {
    let mut dirs = vec![];
    match packages {
        [] => dirs.push(cwd.to_path_buf()),
        [p] => {
            dirs.push(cwd.join("crates").join(p));
            dirs.push(cwd.join(p));
        }
        _ => return None,
    }
    let dir = dirs.into_iter().find(|d| d.join("Cargo.toml").is_file())?;
    let names = |sub: &str| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir.join(sub))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "rs") && p.is_file() {
                    p.file_stem().map(|s| s.to_string_lossy().into_owned())
                } else if p.join("main.rs").is_file() {
                    p.file_name().map(|s| s.to_string_lossy().into_owned())
                } else {
                    None
                }
            })
            .collect();
        v.sort();
        v
    };
    let tests = names("tests");
    let bins = names("src/bin").len() + usize::from(dir.join("src/main.rs").is_file());
    let lib = usize::from(dir.join("src/lib.rs").is_file());
    Some(KnownTargets {
        total: lib + bins + tests.len(),
        test_names: tests,
    })
}

/// Where a submission comes from; a forwarded job's working directory is the origin's, so the
/// disk scan and the Cargo cap do not apply to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Via {
    /// The local socket API.
    Local,
    /// A peer's forward (R-153). Cargo parallelism above this machine's cap is not refused: it
    /// is lowered at launch like any local job (R-137).
    Peer,
}

impl Scheduler {
    /// Applies the submit rules to one job spec. `Ok` carries the warnings; `Err` the refusal
    /// text (one line per refusing rule).
    pub(crate) fn check_spec(&self, spec: &JobSpec, via: Via) -> Result<Vec<String>, String> {
        let policy = self.policy();
        // a denied serial named as the device resource is refused like one in argv (R-210)
        let mut hay = spec.argv.clone();
        if let Some(d) = &spec.requirements.device {
            hay.push(d.0.clone());
        }
        let known = match via {
            Via::Local => scan_known_targets(&spec.cwd, &submit_rules::packages_of(&spec.argv)),
            Via::Peer => None,
        };
        let verdict = submit_rules::check(&SubmitRules {
            argv: &hay,
            broad_test_escape: spec.broad_test_escape,
            broad_mode: policy.broad_test_mode(),
            known: known.as_ref(),
            max_cargo_jobs: match via {
                Via::Local => Some(policy.cargo_max_jobs()),
                Via::Peer => None,
            },
        });
        if verdict.allowed() {
            Ok(verdict.warnings)
        } else {
            Err(verdict
                .refusals
                .iter()
                .map(|r| r.message.clone())
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r191_known_targets_are_counted_from_the_layout() {
        let dir = std::env::temp_dir().join(format!("tasks-daemon-known-{}", std::process::id()));
        let pkg = dir.join("crates/core");
        std::fs::create_dir_all(pkg.join("tests")).unwrap();
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::write(pkg.join("Cargo.toml"), "[package]\nname=\"core\"\n").unwrap();
        std::fs::write(pkg.join("src/lib.rs"), "").unwrap();
        std::fs::write(pkg.join("tests/video_calls.rs"), "").unwrap();
        std::fs::write(pkg.join("tests/other.rs"), "").unwrap();
        let k = scan_known_targets(&dir, &["core".to_string()]).unwrap();
        assert_eq!(
            (k.total, k.test_names.clone()),
            (3, vec!["other".to_string(), "video_calls".to_string()])
        );
        assert_eq!(scan_known_targets(&dir, &["a".into(), "b".into()]), None);
        assert_eq!(scan_known_targets(&dir, &["missing".into()]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
