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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvPolicy {
    /// Default `CARGO_BUILD_JOBS` when neither the job nor the daemon sets it
    /// (R-091: 2, not the CPU count). `None` leaves it unset.
    pub default_cargo_jobs: Option<u32>,
    /// Names removed from the finished environment whoever supplied them (a job's own
    /// compiler wrapper must not override the machine's, R-400).
    pub scrub: Vec<String>,
    /// Prefixes removed likewise (`SCCACHE_`).
    pub scrub_prefixes: Vec<String>,
    /// Directories appended to the job's `PATH` when it lacks them (R-543): the places the
    /// daemon itself looks for tools, so a program a job runs by name (`env X=1 cargo`) is
    /// found the way the daemon found it. Order kept, nothing repeated.
    pub path_extra: Vec<String>,
    /// Variables the machine sets last; they win over the job and the daemon's environment.
    pub force: Vec<(String, String)>,
}

fn allowed(name: &str) -> bool {
    ENV_ALLOWLIST.contains(&name) || ENV_PREFIX_ALLOWLIST.iter().any(|p| name.starts_with(p))
}

/// `path` followed by each of `extra` it does not already contain (colon separated; empty
/// entries dropped, so a missing PATH becomes just the extras).
pub fn with_dirs(path: &str, extra: &[String]) -> String {
    let mut dirs: Vec<&str> = path.split(':').filter(|d| !d.is_empty()).collect();
    for e in extra {
        if !e.is_empty() && !dirs.contains(&e.as_str()) {
            dirs.push(e);
        }
    }
    dirs.join(":")
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
    if !policy.path_extra.is_empty() {
        let path = map.get("PATH").cloned().unwrap_or_default();
        map.insert("PATH".to_string(), with_dirs(&path, &policy.path_extra));
    }
    // the machine's own settings are applied after everything a job could have supplied
    map.retain(|k, _| {
        !policy.scrub.iter().any(|s| s == k)
            && !policy
                .scrub_prefixes
                .iter()
                .any(|p| k.starts_with(p.as_str()))
    });
    for (k, v) in &policy.force {
        map.insert(k.clone(), v.clone());
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

    // R-543
    #[test]
    fn r543_extra_dirs_follow_the_path_in_order_without_repeats() {
        let ex = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            with_dirs(
                "/usr/bin:/bin",
                &ex(&["/h/.cargo/bin", "/usr/bin", "/h/.cargo/bin"])
            ),
            "/usr/bin:/bin:/h/.cargo/bin"
        );
        assert_eq!(with_dirs("", &ex(&["/a", "/b"])), "/a:/b");
        assert_eq!(with_dirs("/a", &[]), "/a");
    }

    // R-543
    #[test]
    fn r543_build_env_adds_the_extra_dirs_to_the_parent_or_the_jobs_own_path() {
        let policy = EnvPolicy {
            path_extra: vec!["/h/.cargo/bin".into()],
            ..EnvPolicy::default()
        };
        let get =
            |env: Vec<(String, String)>| env.into_iter().find(|(k, _)| k == "PATH").map(|(_, v)| v);
        let from_parent = build_env(JobId(1), vec![kv("PATH", "/usr/bin")], &[], &policy);
        assert_eq!(get(from_parent).as_deref(), Some("/usr/bin:/h/.cargo/bin"));
        let from_job = build_env(
            JobId(1),
            vec![kv("PATH", "/usr/bin")],
            &[kv("PATH", "/x")],
            &policy,
        );
        assert_eq!(get(from_job).as_deref(), Some("/x:/h/.cargo/bin"));
        let none = build_env(JobId(1), vec![], &[], &policy);
        assert_eq!(get(none).as_deref(), Some("/h/.cargo/bin"));
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
        let env = build_env(JobId(7), parent, &[], &EnvPolicy::default());
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
            ..EnvPolicy::default()
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
            &EnvPolicy::default(),
        );
        assert!(env.contains(&kv("PATH", "/b")));
        assert!(env.contains(&kv("FOO", "1")));
    }

    // R-400: the machine's compiler-cache settings are applied after the job's environment and
    // beat any wrapper or sccache variable the job (or the daemon's own environment) supplied.
    #[test]
    fn r400_forced_variables_beat_the_job_and_scrubbed_ones_vanish() {
        let policy = EnvPolicy {
            scrub: vec!["RUSTC_WRAPPER".into()],
            scrub_prefixes: vec!["SCCACHE_".into()],
            force: vec![
                kv("RUSTC_WRAPPER", "/machine/sccache"),
                kv("SCCACHE_DIR", "/machine/dir"),
            ],
            ..EnvPolicy::default()
        };
        let env = build_env(
            JobId(1),
            vec![kv("RUSTC_WRAPPER", "/parent/w"), kv("SCCACHE_BUCKET", "b")],
            &[
                kv("RUSTC_WRAPPER", "/job/evil"),
                kv("SCCACHE_DIR", "/job/dir"),
                kv("SCCACHE_CACHE_SIZE", "1T"),
            ],
            &policy,
        );
        assert!(env.contains(&kv("RUSTC_WRAPPER", "/machine/sccache")));
        assert!(env.contains(&kv("SCCACHE_DIR", "/machine/dir")));
        assert!(
            !env.iter()
                .any(|(k, _)| k == "SCCACHE_BUCKET" || k == "SCCACHE_CACHE_SIZE")
        );
        // without a forced value a scrubbed name is simply gone: an explicit uncached build
        let bare = EnvPolicy {
            scrub: vec!["RUSTC_WRAPPER".into()],
            ..EnvPolicy::default()
        };
        let env = build_env(JobId(1), vec![], &[kv("RUSTC_WRAPPER", "/job/evil")], &bare);
        assert!(!env.iter().any(|(k, _)| k == "RUSTC_WRAPPER"));
    }

    // R-405: the SDK variables Gradle needs on a sanitized environment are allowlisted.
    #[test]
    fn r405_the_android_sdk_variables_are_allowlisted() {
        for name in [
            "ANDROID_HOME",
            "ANDROID_SDK_ROOT",
            "JAVA_HOME",
            "GRADLE_USER_HOME",
        ] {
            assert!(ENV_ALLOWLIST.contains(&name), "{name}");
        }
    }
}
