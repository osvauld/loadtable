//! Environment-failure classification of failed job logs.
//!
//! A failed job's log tail (bounded) is matched line by line against a small table of known
//! environment failures. A match means the MACHINE lacks something (a capability), not that
//! the command is wrong: it raises a `missing_capability` alert for that node and records a
//! mark placement consults (see `capability`). Plain substring rules, no regex engine.
//!
//! | capability          | matched line (case-sensitive)                                     |
//! |---------------------|-------------------------------------------------------------------|
//! | `libclang`          | `Unable to find libclang`                                         |
//! | `linker`            | `linker ` ... ` not found` (e.g. "error: linker `cc` not found")  |
//! | `disk-space`        | `No space left on device`                                         |
//! | `native-lib:NAME`   | `could not find native static library` (NAME from the backticks) |
//! | `android-sdk`       | `SDK location not found`, `ANDROID_HOME`/`ANDROID_SDK_ROOT` + not set/found/invalid, `sdk.dir`, NDK not installed/configured |
//! | `pkg-config`        | `pkg-config` ... `not found` / `could not be found`               |
//! | `java`              | `java: command not found`, `JAVA_HOME is not set`                 |
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Log bytes read from the end of a failed job's log.
pub const TAIL_BYTES: u64 = 256 * 1024;

/// One known environment failure.
pub struct Pattern {
    /// Capability family, e.g. `libclang`, `native-lib`.
    pub family: &'static str,
    pub matches: fn(&str) -> bool,
    /// `{node} lacks ...` wording.
    pub lacks: &'static str,
    pub fix: &'static str,
}

fn has(line: &str, needle: &str) -> bool {
    line.contains(needle)
}

fn after(line: &str, first: &str, then: &[&str]) -> bool {
    line.find(first)
        .is_some_and(|i| then.iter().any(|t| line[i + first.len()..].contains(t)))
}

pub static PATTERNS: [Pattern; 7] = [
    Pattern {
        family: "libclang",
        matches: |l| has(l, "Unable to find libclang"),
        lacks: "libclang (bindgen)",
        fix: "install libclang on that machine (NixOS: add llvmPackages.libclang to the worker \
              environment and set LIBCLANG_PATH, or pass it through the worker's allowlisted \
              env); meanwhile run bindgen builds elsewhere (placement skips it)",
    },
    Pattern {
        family: "linker",
        matches: |l| after(l, "linker ", &[" not found"]),
        lacks: "a C linker",
        fix: "install the linker/toolchain (cc/gcc/clang) on that machine or put it on the \
              worker's PATH",
    },
    Pattern {
        family: "disk-space",
        matches: |l| has(l, "No space left on device"),
        lacks: "free disk space",
        fix: "free space on that machine's build/cache disk (see `tasks alerts` disk entries and \
              `tasks tmp-usage`); never delete lock files or another agent's data",
    },
    Pattern {
        family: "native-lib",
        matches: |l| has(l, "could not find native static library"),
        lacks: "a native static library",
        fix: "install the named native library (static) on that machine or build it there \
              first; check PKG_CONFIG_PATH/LIBRARY_PATH in the worker's env allowlist",
    },
    Pattern {
        family: "android-sdk",
        matches: |l| {
            has(l, "SDK location not found")
                || has(l, "sdk.dir")
                || ((has(l, "ANDROID_HOME") || has(l, "ANDROID_SDK_ROOT"))
                    && (has(l, "not set")
                        || has(l, "not found")
                        || has(l, "invalid")
                        || has(l, "does not exist")))
                || (has(l, "NDK")
                    && (has(l, "not installed")
                        || has(l, "not configured")
                        || has(l, "No version of NDK")))
        },
        lacks: "the Android SDK/NDK",
        fix: "install the Android SDK/NDK on that machine and set ANDROID_HOME for its tasks \
              daemon (Gradle local.properties sdk.dir is created from it)",
    },
    Pattern {
        family: "pkg-config",
        matches: |l| after(l, "pkg-config", &["not found", "could not be found"]),
        lacks: "pkg-config or a pkg-config package",
        fix: "install pkg-config and the named development package on that machine",
    },
    Pattern {
        family: "java",
        matches: |l| has(l, "java: command not found") || has(l, "JAVA_HOME is not set"),
        lacks: "a Java runtime/JDK",
        fix: "install a JDK on that machine and set JAVA_HOME for its tasks daemon",
    },
];

/// A classified log line.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    /// `libclang`, `native-lib:vpx`, ...
    pub capability: String,
    pub family: &'static str,
    pub line: String,
}

impl Finding {
    pub fn pattern(&self) -> &'static Pattern {
        pattern(self.family).expect("finding of a known family")
    }
}

pub fn pattern(family: &str) -> Option<&'static Pattern> {
    PATTERNS.iter().find(|p| p.family == family)
}

/// `native-lib:NAME` takes NAME from the first backtick-quoted word.
fn capability_of(family: &str, line: &str) -> String {
    if family == "native-lib"
        && let Some(start) = line.find('`')
        && let Some(len) = line[start + 1..].find('`')
    {
        let name = &line[start + 1..start + 1 + len];
        if !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
        {
            return format!("native-lib:{name}");
        }
    }
    family.to_string()
}

/// The first matching line per capability, in log order. Lines are trimmed and bounded.
pub fn classify(text: &str) -> Vec<Finding> {
    let mut out: Vec<Finding> = vec![];
    for line in text.lines() {
        for p in &PATTERNS {
            if (p.matches)(line) {
                let capability = capability_of(p.family, line);
                if !out.iter().any(|f| f.capability == capability) {
                    let mut shown: String = line.trim().chars().take(300).collect();
                    if line.trim().chars().count() > 300 {
                        shown.push('…');
                    }
                    out.push(Finding {
                        capability,
                        family: p.family,
                        line: shown,
                    });
                }
                break;
            }
        }
    }
    out
}

/// The last `max` bytes of a log (from a line start when cut), lossily decoded.
pub fn tail(path: &Path, max: u64) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(max.min(len) as usize);
    file.take(max).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(if start > 0 {
        match text.find('\n') {
            Some(i) => text[i + 1..].to_string(),
            None => text,
        }
    } else {
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line job 456 failed with on the worker (bindgen for webrtc-audio-processing).
    const JOB_456: &str = "  thread 'main' panicked at /home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/bindgen-0.69.5/lib.rs:622:31:\n  Unable to find libclang: \"couldn't find any valid shared libraries matching: ['libclang.so', 'libclang-*.so', 'libclang.so.*', 'libclang-*.so.*'], set the `LIBCLANG_PATH` environment variable to a path where one of these files can be found (invalid: [])\"\n";

    #[test]
    fn the_job_456_libclang_line_is_classified() {
        let f = classify(JOB_456);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].capability, "libclang");
        assert!(f[0].line.starts_with("Unable to find libclang: \"couldn't find any valid shared libraries matching: ['libclang.so',"), "{}", f[0].line);
        assert!(f[0].pattern().fix.contains("libclang"));
    }

    #[test]
    fn every_pattern_matches_its_known_lines_and_not_ordinary_failures() {
        let cases = [
            ("error: linker `cc` not found", "linker"),
            (
                "  = note: linker `aarch64-linux-gnu-gcc` not found",
                "linker",
            ),
            (
                "error: failed to write: No space left on device (os error 28)",
                "disk-space",
            ),
            (
                "error: could not find native static library `vpx`, perhaps an -L flag is missing?",
                "native-lib:vpx",
            ),
            (
                "SDK location not found. Define a valid SDK location with an ANDROID_HOME environment variable or by setting the sdk.dir path",
                "android-sdk",
            ),
            (
                "ANDROID_HOME is not set and \"android\" command not in your PATH",
                "android-sdk",
            ),
            ("NDK is not installed", "android-sdk"),
            ("The pkg-config command could not be found.", "pkg-config"),
            (
                "Package opus was not found in the pkg-config search path. pkg-config exited: not found",
                "pkg-config",
            ),
            ("sh: line 1: java: command not found", "java"),
            (
                "ERROR: JAVA_HOME is not set and no 'java' command could be found",
                "java",
            ),
        ];
        for (line, want) in cases {
            let f = classify(line);
            assert_eq!(f.len(), 1, "{line}: {f:?}");
            assert_eq!(f[0].capability, want, "{line}");
        }
        for ordinary in [
            "error[E0425]: cannot find value `x` in this scope",
            "test result: FAILED. 3 passed; 1 failed",
            "thread 'main' panicked at src/lib.rs:3:5: assertion failed",
            "warning: unused variable: `linker`",
            "error: could not compile `foo` (lib) due to 2 previous errors",
        ] {
            assert!(classify(ordinary).is_empty(), "{ordinary}");
        }
    }

    #[test]
    fn one_finding_per_capability_in_log_order_and_tail_is_bounded() {
        let text = format!("{JOB_456}{JOB_456}error: linker `cc` not found\n");
        let f = classify(&text);
        let caps: Vec<_> = f.iter().map(|f| f.capability.as_str()).collect();
        assert_eq!(caps, ["libclang", "linker"]);
        let dir = std::env::temp_dir().join(format!("tasks-classify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("job.log");
        let mut body = "filler line\n".repeat(50_000);
        body.push_str(JOB_456);
        std::fs::write(&log, &body).unwrap();
        let t = tail(&log, 4096).unwrap();
        assert!(t.len() <= 4096);
        assert!(!t.starts_with("ller"), "starts at a line boundary");
        assert_eq!(classify(&t)[0].capability, "libclang");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
