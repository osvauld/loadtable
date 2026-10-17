//! Node setup checks (docs/design/node-setup-policy.md, "Required checks"): the promises a
//! node's declaration makes, verified by the daemon on every health pass and by `tasks doctor`.
//!
//! * Declared capabilities ([`probe`]): `TASKS_DECLARED_CAPABILITIES` (comma list, set by the
//!   node's declaration, e.g. `libclang,jdk,android-sdk,cargo-ndk,htop`) names what this node
//!   promises. A declared capability that does not resolve raises `declared_capability:NODE:NAME`
//!   and, when placement knows the family, a transient missing-capability mark (published in
//!   `health.capabilities`, not persisted) so `anywhere` routes commands that need it elsewhere.
//!   Undeclared capabilities are probed too (for `tasks doctor`) but never alert.
//! * Compiler cache ([`compiler_cache`]): when this daemon's `RUSTC_WRAPPER` is sccache, its
//!   server should run outside jobs (a declared user service) and hold no slot-lock descriptor.
//!   `compiler-cache:NODE:sccache` reports a server started inside a job (`TASKS_JOB_ID` in its
//!   environment), holding a `p2p-*.lock` descriptor, or not running at all (info).
//!
//! Probes and checks are pure over injected lookups (environment, file existence, directory
//! listing, /proc readers), so they are tested with fixtures.
use super::alert::{Observation, Severity};
use super::capability::CapabilityMark;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const DECLARED_ENV: &str = "TASKS_DECLARED_CAPABILITIES";
/// Probed by `tasks doctor` even when not declared.
pub const KNOWN: [&str; 5] = ["libclang", "jdk", "android-sdk", "cargo-ndk", "htop"];

/// One capability probe's result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub name: String,
    pub declared: bool,
    pub ok: bool,
    /// Where it was found, or why not.
    pub detail: String,
    pub fix: String,
}

/// Lookups a probe needs (injected in tests).
pub struct Env<'a> {
    pub var: &'a dyn Fn(&str) -> Option<OsString>,
    pub exists: &'a dyn Fn(&Path) -> bool,
    /// File names in a directory (bounded by the caller).
    pub list: &'a dyn Fn(&Path) -> Vec<String>,
}

impl Env<'_> {
    fn path_var(&self, name: &str) -> Option<PathBuf> {
        (self.var)(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }
    fn on_path(&self, tool: &str) -> Option<PathBuf> {
        let path = (self.var)("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join(tool))
            .find(|p| (self.exists)(p))
    }
}

/// The declared list (lowercase, deduplicated, `java` = `jdk`).
pub fn declared(var: &dyn Fn(&str) -> Option<OsString>) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    if let Some(v) = var(DECLARED_ENV) {
        for name in v.to_string_lossy().split(',') {
            let name = match name.trim().to_lowercase().as_str() {
                "" => continue,
                "java" => "jdk".to_string(),
                other => other.to_string(),
            };
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// The missing-capability family placement consults (`capability::needed`), if any.
pub fn family(name: &str) -> Option<&'static str> {
    match name {
        "libclang" => Some("libclang"),
        "jdk" => Some("java"),
        "android-sdk" | "cargo-ndk" => Some("android-sdk"),
        _ => None,
    }
}

pub fn probe(name: &str, declared: bool, env: &Env) -> ProbeResult {
    let found = |detail: String| (true, detail);
    let (ok, detail, fix) = match name {
        "libclang" => {
            let r = match env.path_var("LIBCLANG_PATH") {
                None => (false, "LIBCLANG_PATH is not set".to_string()),
                Some(dir) => {
                    let libs: Vec<String> = (env.list)(&dir)
                        .into_iter()
                        .filter(|n| n.starts_with("libclang") && n.contains(".so"))
                        .collect();
                    match libs.first() {
                        Some(lib) => found(format!("{}", dir.join(lib).display())),
                        None => (
                            false,
                            format!("LIBCLANG_PATH={} has no libclang*.so", dir.display()),
                        ),
                    }
                }
            };
            (
                r.0,
                r.1,
                "declare libclang for bindgen: NixOS llvmPackages.libclang with LIBCLANG_PATH \
                 set in the daemon environment (nix/module.nix)",
            )
        }
        "jdk" => {
            let java = env
                .path_var("JAVA_HOME")
                .map(|h| h.join("bin/java"))
                .filter(|p| (env.exists)(p))
                .or_else(|| env.on_path("java"));
            let r = match java {
                Some(p) => found(p.display().to_string()),
                None => (false, "no java in JAVA_HOME/bin or on PATH".into()),
            };
            (
                r.0,
                r.1,
                "declare a JDK (JAVA_HOME and java on the daemon's PATH)",
            )
        }
        "android-sdk" => {
            let sdk = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
                .iter()
                .find_map(|v| env.path_var(v));
            let r = match sdk {
                None => (false, "ANDROID_HOME/ANDROID_SDK_ROOT not set".to_string()),
                Some(dir)
                    if (env.exists)(&dir.join("platform-tools"))
                        || (env.exists)(&dir.join("platforms")) =>
                {
                    found(dir.display().to_string())
                }
                Some(dir) => (
                    false,
                    format!("{} has no platform-tools/ or platforms/", dir.display()),
                ),
            };
            (
                r.0,
                r.1,
                "declare the Android SDK (ANDROID_HOME in the daemon environment)",
            )
        }
        "cargo-ndk" => {
            let cargo_bin = env
                .path_var("CARGO_HOME")
                .or_else(|| env.path_var("HOME").map(|h| h.join(".cargo")))
                .map(|c| c.join("bin/cargo-ndk"))
                .filter(|p| (env.exists)(p));
            let r = match env.on_path("cargo-ndk").or(cargo_bin) {
                Some(p) => found(p.display().to_string()),
                None => (false, "cargo-ndk not on PATH or in CARGO_HOME/bin".into()),
            };
            (r.0, r.1, "declare cargo-ndk in the node's packages")
        }
        tool => {
            let r = match env.on_path(tool) {
                Some(p) => found(p.display().to_string()),
                None => (false, format!("{tool} not on PATH")),
            };
            (r.0, r.1, "add it to the node's declared packages")
        }
    };
    ProbeResult {
        name: name.into(),
        declared,
        ok,
        detail,
        fix: fix.into(),
    }
}

/// Declared capabilities plus [`KNOWN`] ones, probed.
pub fn probe_all(declared: &[String], env: &Env) -> Vec<ProbeResult> {
    let mut names: Vec<&str> = declared.iter().map(String::as_str).collect();
    for k in KNOWN {
        if !names.contains(&k) {
            names.push(k);
        }
    }
    names
        .into_iter()
        .map(|n| probe(n, declared.iter().any(|d| d == n), env))
        .collect()
}

/// Alerts for declared capabilities that did not resolve.
pub fn capability_alerts(node: &str, probes: &[ProbeResult]) -> Vec<Observation> {
    probes
        .iter()
        .filter(|p| p.declared && !p.ok)
        .map(|p| {
            Observation::new("declared_capability", Severity::Warn, node)
                .subject(p.name.clone())
                .summary(format!(
                    "{node} lacks declared capability {}: {}",
                    p.name, p.detail
                ))
                .evidence(format!("{DECLARED_ENV} declares {}", p.name))
                .fix(p.fix.clone())
                .human()
        })
        .collect()
}

/// Transient marks (placement skips) for failed declared probes with a known family.
pub fn probe_marks(
    node: &str,
    probes: &[ProbeResult],
    now: f64,
    ttl_s: f64,
) -> Vec<CapabilityMark> {
    probes
        .iter()
        .filter(|p| p.declared && !p.ok)
        .filter_map(|p| {
            Some(CapabilityMark {
                node: node.into(),
                capability: p.name.clone(),
                family: family(&p.name)?.into(),
                job: 0,
                remote_job: None,
                kind_key: String::new(),
                line: p.detail.clone(),
                marked_at: now,
                expires_at: now + ttl_s,
                source: "declared capability probe".into(),
            })
        })
        .collect()
}

/// A compiler-cache server process as the check sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheServer {
    pub pid: u32,
    pub cmd: String,
    /// `TASKS_JOB_ID` in its environment: it was started inside that job.
    pub job: Option<String>,
    /// Slot-lock files (`p2p-*.lock`) it has open.
    pub lock_fds: Vec<String>,
}

/// Is `wrapper` (the daemon's `RUSTC_WRAPPER`) sccache?
pub fn uses_sccache(wrapper: Option<&str>) -> bool {
    wrapper
        .and_then(|w| Path::new(w).file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "sccache")
}

/// `compiler-cache:NODE:sccache`: the server must run outside jobs and hold no lease.
pub fn compiler_cache(
    node: &str,
    wrapper: Option<&str>,
    servers: &[CacheServer],
) -> Vec<Observation> {
    if !uses_sccache(wrapper) {
        return vec![];
    }
    const FIX: &str = "run the compiler-cache server as the declared user service outside jobs \
        with no idle exit (node-setup-policy; nix/module.nix: SCCACHE_IDLE_TIMEOUT=0); a server \
        started by a build inherits that job's slot lease and keeps the slot busy";
    let base = |severity| {
        Observation::new("compiler-cache", severity, node)
            .subject("sccache")
            .fix(FIX)
    };
    if servers.is_empty() {
        return vec![base(Severity::Info).summary(format!(
            "{node}: RUSTC_WRAPPER is sccache but no sccache server runs; the next build starts \
             one inside its job"
        ))];
    }
    let bad: Vec<&CacheServer> = servers
        .iter()
        .filter(|s| s.job.is_some() || !s.lock_fds.is_empty())
        .collect();
    if bad.is_empty() {
        return vec![];
    }
    let mut o = base(Severity::Warn)
        .summary(format!(
            "{node}: sccache server runs inside a job or holds a slot-lock descriptor"
        ))
        .human();
    for s in bad {
        let mut line = format!("pid {} ({})", s.pid, s.cmd);
        if let Some(job) = &s.job {
            line.push_str(&format!(", started inside job #{job} (TASKS_JOB_ID)"));
        }
        if !s.lock_fds.is_empty() {
            line.push_str(&format!(", holds {}", s.lock_fds.join(", ")));
        }
        o = o.evidence(line);
    }
    vec![o]
}

/// Live lookups for [`probe`]: this process's environment and filesystem (bounded listing).
pub fn live_probes(var: &dyn Fn(&str) -> Option<OsString>) -> Vec<ProbeResult> {
    let exists = |p: &Path| p.exists();
    let list = |d: &Path| -> Vec<String> {
        std::fs::read_dir(d)
            .map(|rd| {
                rd.flatten()
                    .take(4096)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    let env = Env {
        var,
        exists: &exists,
        list: &list,
    };
    probe_all(&declared(var), &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    struct Fixture {
        vars: HashMap<&'static str, &'static str>,
        files: HashSet<PathBuf>,
        dirs: HashMap<PathBuf, Vec<String>>,
    }
    impl Fixture {
        fn check(&self, name: &str) -> ProbeResult {
            let var = |n: &str| self.vars.get(n).map(OsString::from);
            let exists = |p: &Path| self.files.contains(p);
            let list = |d: &Path| self.dirs.get(d).cloned().unwrap_or_default();
            probe(
                name,
                true,
                &Env {
                    var: &var,
                    exists: &exists,
                    list: &list,
                },
            )
        }
    }

    #[test]
    fn capability_probes_resolve_declared_tools() {
        let mut f = Fixture {
            vars: HashMap::from([("PATH", "/usr/bin:/opt/bin"), ("HOME", "/home/u")]),
            files: HashSet::from([PathBuf::from("/opt/bin/htop")]),
            dirs: HashMap::new(),
        };
        let r = f.check("libclang");
        assert!(
            !r.ok && r.detail.contains("LIBCLANG_PATH is not set"),
            "{r:?}"
        );
        f.vars.insert("LIBCLANG_PATH", "/nix/store/x-clang/lib");
        assert!(!f.check("libclang").ok, "empty dir");
        f.dirs.insert(
            "/nix/store/x-clang/lib".into(),
            vec!["libclang.so.18".into()],
        );
        let r = f.check("libclang");
        assert!(r.ok && r.detail.ends_with("libclang.so.18"), "{r:?}");
        assert!(!f.check("jdk").ok);
        f.vars.insert("JAVA_HOME", "/jdk");
        f.files.insert("/jdk/bin/java".into());
        assert!(f.check("jdk").ok);
        assert!(!f.check("android-sdk").ok);
        f.vars.insert("ANDROID_HOME", "/sdk");
        assert!(f.check("android-sdk").detail.contains("no platform-tools"));
        f.files.insert("/sdk/platform-tools".into());
        assert!(f.check("android-sdk").ok);
        assert!(!f.check("cargo-ndk").ok);
        f.files.insert("/home/u/.cargo/bin/cargo-ndk".into());
        assert!(f.check("cargo-ndk").ok);
        assert!(f.check("htop").ok);
        assert!(!f.check("rg").ok);
    }

    #[test]
    fn declared_list_alerts_and_marks_only_declared_failures() {
        let var = |n: &str| {
            (n == DECLARED_ENV).then(|| OsString::from(" libclang, Java,htop,,libclang "))
        };
        assert_eq!(declared(&var), ["libclang", "jdk", "htop"]);
        let probes = vec![
            ProbeResult {
                name: "libclang".into(),
                declared: true,
                ok: false,
                detail: "LIBCLANG_PATH is not set".into(),
                fix: "f".into(),
            },
            ProbeResult {
                name: "htop".into(),
                declared: true,
                ok: false,
                detail: "htop not on PATH".into(),
                fix: "f".into(),
            },
            ProbeResult {
                name: "jdk".into(),
                declared: false,
                ok: false,
                detail: "x".into(),
                fix: "f".into(),
            },
        ];
        let o = capability_alerts("nixos", &probes);
        let ids: Vec<String> = o.iter().map(|o| o.id()).collect();
        assert_eq!(
            ids,
            [
                "declared_capability:nixos:libclang",
                "declared_capability:nixos:htop"
            ]
        );
        let marks = probe_marks("nixos", &probes, 10.0, 60.0);
        assert_eq!(
            marks.len(),
            1,
            "htop has no placement family; jdk is undeclared"
        );
        assert_eq!(marks[0].family, "libclang");
        assert_eq!(marks[0].evidence(), "probe: LIBCLANG_PATH is not set");
    }

    #[test]
    fn compiler_cache_server_must_run_outside_jobs_without_a_lease() {
        let wrapper = Some("/home/u/.cargo/bin/sccache");
        assert!(
            compiler_cache("nixos", None, &[]).is_empty(),
            "no sccache configured"
        );
        let o = compiler_cache("nixos", wrapper, &[]);
        assert_eq!(o[0].id(), "compiler-cache:nixos:sccache");
        assert_eq!(o[0].severity, Some(Severity::Info));
        let good = CacheServer {
            pid: 9,
            cmd: "sccache".into(),
            job: None,
            lock_fds: vec![],
        };
        assert!(compiler_cache("nixos", wrapper, std::slice::from_ref(&good)).is_empty());
        let bad = CacheServer {
            pid: 777,
            cmd: "sccache".into(),
            job: Some("154".into()),
            lock_fds: vec!["/tmp/claude-1000/p2p-build.lock".into()],
        };
        let o = compiler_cache("nixos", wrapper, &[good, bad]);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].severity, Some(Severity::Warn));
        assert_eq!(o[0].id(), "compiler-cache:nixos:sccache");
        assert_eq!(
            o[0].evidence,
            [
                "pid 777 (sccache), started inside job #154 (TASKS_JOB_ID), holds /tmp/claude-1000/p2p-build.lock"
            ]
        );
    }
}
