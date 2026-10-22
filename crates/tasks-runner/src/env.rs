//! Job environment: an explicit allowlist plus job-provided variables (R-090).

use std::collections::BTreeMap;
use tasks_core::JobId;

/// Variables copied from the daemon's environment when present.
pub const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    "SHELL",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "RUSTC_WRAPPER",
    "CARGO_TARGET_DIR",
    "PKG_CONFIG_PATH",
    "LD_LIBRARY_PATH",
    "ANDROID_HOME",
    "ANDROID_SDK_ROOT",
    "JAVA_HOME",
    "GRADLE_USER_HOME",
    "LIBCLANG_PATH",
    "BINDGEN_EXTRA_CLANG_ARGS",
];

/// Prefixes of allowlisted variables (nix-ld, compiler cache settings).
pub const ENV_PREFIX_ALLOWLIST: &[&str] = &["NIX_LD", "SCCACHE_"];

/// Environment policy values supplied by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvPolicy {
    /// Default `CARGO_BUILD_JOBS` when neither the job nor the daemon sets it
    /// (R-091: 2, not the CPU count). `None` leaves it unset.
    pub default_cargo_jobs: Option<u32>,
}

fn allowed(name: &str) -> bool {
    ENV_ALLOWLIST.contains(&name) || ENV_PREFIX_ALLOWLIST.iter().any(|p| name.starts_with(p))
}

/// Builds the complete child environment: allowlisted parent variables, then the
/// job's own variables (which win), the cargo jobs default, and `TASKS_JOB_ID`
/// (always set last). The result is sorted by name and has no duplicates.
pub fn build_env(
    job: JobId,
    parent: impl IntoIterator<Item = (String, String)>,
    job_env: &[(String, String)],
    policy: &EnvPolicy,
) -> Vec<(String, String)> {
    let mut map: BTreeMap<String, String> =
        parent.into_iter().filter(|(k, _)| allowed(k)).collect();
    for (k, v) in job_env {
        map.insert(k.clone(), v.clone());
    }
    if let Some(n) = policy.default_cargo_jobs {
        map.entry("CARGO_BUILD_JOBS".to_string())
            .or_insert_with(|| n.to_string());
    }
    map.insert("TASKS_JOB_ID".to_string(), job.0.to_string());
    map.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[test]
    fn r090_only_allowlisted_parent_variables_pass() {
        let parent = vec![
            kv("PATH", "/bin"),
            kv("ANDROID_HOME", "/sdk"),
            kv("JAVA_HOME", "/jdk"),
            kv("LIBCLANG_PATH", "/clang"),
            kv("NIX_LD_LIBRARY_PATH", "/lib"),
            kv("SECRET_TOKEN", "x"),
            kv("TASKS_JOB_ID", "999"),
            kv("SSH_AUTH_SOCK", "/s"),
        ];
        let env = build_env(
            JobId(7),
            parent,
            &[],
            &EnvPolicy {
                default_cargo_jobs: None,
            },
        );
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "ANDROID_HOME",
                "JAVA_HOME",
                "LIBCLANG_PATH",
                "NIX_LD_LIBRARY_PATH",
                "PATH",
                "TASKS_JOB_ID"
            ]
        );
        assert!(env.contains(&kv("TASKS_JOB_ID", "7")));
    }

    #[test]
    fn r091_cargo_jobs_default_is_a_default_not_an_override() {
        let p = EnvPolicy {
            default_cargo_jobs: Some(2),
        };
        let env = build_env(JobId(1), vec![], &[], &p);
        assert!(env.contains(&kv("CARGO_BUILD_JOBS", "2")));
        let env = build_env(JobId(1), vec![], &[kv("CARGO_BUILD_JOBS", "4")], &p);
        assert!(env.contains(&kv("CARGO_BUILD_JOBS", "4")));
    }

    #[test]
    fn job_env_wins_over_parent() {
        let env = build_env(
            JobId(1),
            vec![kv("PATH", "/a")],
            &[kv("PATH", "/b"), kv("FOO", "1")],
            &EnvPolicy {
                default_cargo_jobs: None,
            },
        );
        assert!(env.contains(&kv("PATH", "/b")));
        assert!(env.contains(&kv("FOO", "1")));
    }
}
