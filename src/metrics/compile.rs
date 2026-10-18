//! Live compile attribution: which crates (or Gradle/Kotlin tasks) a job's process tree is
//! compiling right now, from the processes the sampler already reads for that job.
//!
//! Cheap by construction: only members of a job's tree are considered (never a /proc walk of
//! its own), and `/proc/PID/cmdline` is read only for members whose `comm` names a compiler
//! process (rustc, rustdoc, clippy-driver), a wrapper (sccache), a build-script run or Cargo.
//! Cargo/rustc only for now: everything Cargo-specific is in this module, so data-driven
//! definitions for other tools can replace [`attribute`] later.
use serde::{Deserialize, Serialize};

/// At most this many entries per job (largest elapsed first).
pub const MAX_PER_JOB: usize = 32;
/// Bound on one cmdline read (rustc command lines can be long; the flags we need come first).
const MAX_CMDLINE: usize = 64 * 1024;

/// One compiler (or build tool) process of a job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Compiling {
    /// The build tool (`cargo` for now).
    pub tool: String,
    /// The process (`rustc`, `rustdoc`, `build-script-build`, `compile`, `java`...).
    #[serde(rename = "proc")]
    pub proc_name: String,
    /// What it builds: the crate (`--crate-name`), a running build script's crate, or the
    /// `-p` packages of a Cargo process.
    pub unit: String,
    /// Cargo tool only: the same as `unit` (the crate), for readers that ask for crates.
    #[serde(rename = "crate", default, skip_serializing_if = "Option::is_none")]
    pub crate_name: Option<String>,
    /// `lib` | `bin` | `test` | `build-script` (compiling one) | `build-script-run` |
    /// `doctest` | `doc` | `cargo`.
    pub kind: String,
    pub pid: u32,
    pub elapsed_s: f64,
    /// Cores over the last sample interval (lifetime average on first sight).
    pub cpu: Option<f64>,
    pub rss_bytes: u64,
}

/// Process, unit (crate) and kind of one process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attributed {
    /// `rustc`, `clippy-driver`, `rustdoc`, `build-script-build` or `cargo`.
    pub proc_name: String,
    /// The crate (`--crate-name`), the crate of a running build script, or the `-p`
    /// packages of a Cargo process.
    pub unit: String,
    pub kind: &'static str,
}

/// Is `comm` worth a cmdline read? (Kernel comm is the executable or script name, cut to 15
/// bytes: `build-script-build` becomes `build-script-bu`.) Cargo/rustc only for now; other
/// build tools will come with data-driven tool definitions.
pub fn interesting_comm(comm: &str) -> bool {
    matches!(
        comm,
        "rustc" | "rustdoc" | "clippy-driver" | "sccache" | "cargo" | "cargo-clippy"
    ) || comm.starts_with("build-script")
}

fn base(word: &str) -> &str {
    std::path::Path::new(word)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(word)
}

/// `--flag VALUE` or `--flag=VALUE`.
fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().map(String::as_str);
        }
        if let Some(v) = a.strip_prefix(flag).and_then(|v| v.strip_prefix('=')) {
            return Some(v);
        }
    }
    None
}

/// `X-<hex hash>` -> `X` (Cargo's build directory names: `build/serde-0a1b2c3d4e5f6a7b`).
fn strip_hash(name: &str) -> &str {
    match name.rsplit_once('-') {
        Some((stem, hash))
            if !stem.is_empty()
                && hash.len() >= 8
                && hash.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            stem
        }
        _ => name,
    }
}

/// Crate kind of a rustc/clippy-driver command line.
fn rustc_kind(args: &[String], crate_name: &str) -> &'static str {
    if args.iter().any(|a| a == "--test") {
        return "test";
    }
    if crate_name.starts_with("build_script_") {
        return "build-script";
    }
    let lib = args
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            if a == "--crate-type" {
                args.get(i + 1).map(String::as_str)
            } else {
                a.strip_prefix("--crate-type=")
            }
        })
        .flat_map(|t| t.split(','))
        .any(|t| {
            matches!(
                t,
                "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
            )
        });
    if lib { "lib" } else { "bin" }
}

/// Attribution of one process from its argv (`/proc/PID/cmdline`) and comm. Compiler
/// wrappers (`sccache rustc ...`, `clippy-driver rustc ...`) and interpreter-run scripts
/// (`/bin/sh .../rustc ...`, whose argv[0] is the interpreter) are recognised by the last of
/// the leading three words whose basename is the compiler.
pub fn attribute(comm: &str, argv: &[String]) -> Option<Attributed> {
    let lead = argv.len().min(3);
    for i in 0..lead {
        let name = base(&argv[i]);
        let args = &argv[i + 1..];
        if matches!(name, "rustc" | "clippy-driver" | "rustdoc") {
            if i + 1 < lead && base(&args[0]) == "rustc" {
                continue;
            }
            let unit = value_of(args, "--crate-name")?.to_owned();
            let kind = if name == "rustdoc" {
                if args.iter().any(|a| a == "--test") {
                    "doctest"
                } else {
                    "doc"
                }
            } else {
                rustc_kind(args, &unit)
            };
            return Some(Attributed {
                proc_name: name.to_owned(),
                unit,
                kind,
            });
        }
        if name == "build-script-build" || (i == 0 && comm.starts_with("build-script")) {
            let dir = std::path::Path::new(&argv[i])
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("");
            return Some(Attributed {
                proc_name: "build-script-build".into(),
                unit: strip_hash(dir).to_owned(),
                kind: "build-script-run",
            });
        }
        if name == "cargo" || name == "cargo-clippy" {
            let mut packages = vec![];
            let mut it = args.iter();
            while let Some(a) = it.next() {
                if a == "--" {
                    break;
                }
                if a == "-p" || a == "--package" {
                    if let Some(v) = it.next() {
                        packages.push(v.clone());
                    }
                } else if let Some(v) = a.strip_prefix("--package=") {
                    packages.push(v.to_owned());
                } else if let Some(v) = a.strip_prefix("-p").filter(|v| !v.is_empty()) {
                    packages.push(v.to_owned());
                }
            }
            return Some(Attributed {
                proc_name: name.to_owned(),
                unit: packages.join(","),
                kind: "cargo",
            });
        }
    }
    None
}

/// `/proc/PID/cmdline` split on NUL (bounded). None for a vanished process or a kernel thread.
pub fn read_cmdline(pid: u32) -> Option<Vec<String>> {
    use std::io::Read;
    let mut bytes = vec![];
    std::fs::File::open(format!("/proc/{pid}/cmdline"))
        .ok()?
        .take(MAX_CMDLINE as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(
        bytes
            .split(|b| *b == 0)
            .filter(|w| !w.is_empty())
            .map(|w| String::from_utf8_lossy(w).into_owned())
            .collect(),
    )
}

/// Same name for matching: Cargo package `profile-sync` is crate `profile_sync`.
pub fn normalize_crate(name: &str) -> String {
    name.trim().replace('-', "_").to_ascii_lowercase()
}

/// Does entry `c` concern crate/package/task `wanted`? Exact (normalised) crate name, or one
/// of the comma/space separated packages/tasks of a Cargo or Gradle entry.
pub fn concerns(c: &Compiling, wanted: &str) -> bool {
    let wanted = normalize_crate(wanted);
    if wanted.is_empty() {
        return true;
    }
    normalize_crate(&c.unit) == wanted
        || c.unit
            .split([',', ' '])
            .any(|part| normalize_crate(part) == wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn at(comm: &str, words: &[&str]) -> Option<(String, &'static str, String)> {
        attribute(comm, &v(words)).map(|a| (a.unit, a.kind, a.proc_name))
    }

    #[test]
    fn rustc_command_lines_give_crate_and_kind() {
        let lib = [
            "/home/u/.rustup/toolchains/stable/bin/rustc",
            "--crate-name",
            "serde",
            "--edition=2021",
            "src/lib.rs",
            "--crate-type",
            "lib",
            "--emit=dep-info,metadata,link",
        ];
        assert_eq!(
            at("rustc", &lib),
            Some(("serde".into(), "lib", "rustc".into()))
        );
        let test = [
            "rustc",
            "--crate-name",
            "profile_sync",
            "--edition=2024",
            "tests/profile_sync.rs",
            "--test",
            "-C",
            "opt-level=0",
        ];
        assert_eq!(
            at("rustc", &test),
            Some(("profile_sync".into(), "test", "rustc".into()))
        );
        let unit = [
            "rustc",
            "--crate-name",
            "video",
            "--crate-type",
            "lib",
            "--test",
        ];
        assert_eq!(
            at("rustc", &unit).unwrap().1,
            "test",
            "--test wins over --crate-type"
        );
        let bin = [
            "rustc",
            "--crate-name=tasks",
            "--crate-type=bin",
            "src/main.rs",
        ];
        assert_eq!(at("rustc", &bin).unwrap().1, "bin");
        assert_eq!(
            at("rustc", &["rustc", "--crate-name", "tool", "src/main.rs"])
                .unwrap()
                .1,
            "bin"
        );
        assert_eq!(
            at(
                "rustc",
                &["rustc", "--crate-name", "sd", "--crate-type", "proc-macro"]
            )
            .unwrap()
            .1,
            "lib"
        );
        let build = [
            "rustc",
            "--crate-name",
            "build_script_build",
            "--crate-type",
            "bin",
            "build.rs",
        ];
        assert_eq!(at("rustc", &build).unwrap().1, "build-script");
        assert_eq!(
            at("rustc", &["rustc", "-vV"]),
            None,
            "version probes are not compiles"
        );
    }

    #[test]
    fn wrappers_scripts_doc_tests_build_scripts_and_cargo() {
        let wrapped = [
            "/usr/bin/sccache",
            "/opt/rust/bin/rustc",
            "--crate-name",
            "iroh",
            "--crate-type",
            "lib",
        ];
        assert_eq!(
            at("sccache", &wrapped),
            Some(("iroh".into(), "lib", "rustc".into()))
        );
        let script = [
            "/bin/sh",
            "/tmp/fake/rustc",
            "--crate-name",
            "profile_sync",
            "--test",
        ];
        assert_eq!(
            at("rustc", &script),
            Some(("profile_sync".into(), "test", "rustc".into()))
        );
        let clippy = [
            "clippy-driver",
            "rustc",
            "--crate-name",
            "core_lib",
            "--crate-type",
            "lib",
        ];
        assert_eq!(at("clippy-driver", &clippy).unwrap().0, "core_lib");
        let doctest = [
            "rustdoc",
            "--edition=2021",
            "--crate-name",
            "video",
            "--test",
            "src/lib.rs",
        ];
        assert_eq!(
            at("rustdoc", &doctest),
            Some(("video".into(), "doctest", "rustdoc".into()))
        );
        let run = ["/w/target/debug/build/ring-0a1b2c3d4e5f6a7b/build-script-build"];
        assert_eq!(
            at("build-script-bu", &run),
            Some((
                "ring".into(),
                "build-script-run",
                "build-script-build".into()
            ))
        );
        let cargo = [
            "cargo",
            "test",
            "--offline",
            "-p",
            "video",
            "--package=net",
            "-pcore",
        ];
        assert_eq!(
            at("cargo", &cargo),
            Some(("video,net,core".into(), "cargo", "cargo".into()))
        );
        assert_eq!(at("sleep", &["sleep", "30"]), None);
        assert!(interesting_comm("build-script-bu") && interesting_comm("sccache"));
        assert!(!interesting_comm("sh") && !interesting_comm("java"));
    }

    #[test]
    fn crate_filter_matches_normalised_names_and_package_lists() {
        let c = |unit: &str| Compiling {
            tool: "cargo".into(),
            proc_name: "rustc".into(),
            unit: unit.into(),
            crate_name: Some(unit.into()),
            kind: "lib".into(),
            pid: 1,
            elapsed_s: 0.0,
            cpu: None,
            rss_bytes: 0,
        };
        assert!(concerns(&c("profile_sync"), "profile-sync"));
        assert!(concerns(&c("video,net"), "net"));
        assert!(!concerns(&c("profile_sync_extra"), "profile_sync"));
        assert!(concerns(&c("x"), ""));
        let json = serde_json::to_value(c("x")).unwrap();
        assert_eq!(
            (
                json["unit"].as_str(),
                json["crate"].as_str(),
                json["proc"].as_str()
            ),
            (Some("x"), Some("x"), Some("rustc"))
        );
    }
}
