//! Per-host submission policy, written in HUML (<https://huml.io>).
//!
//! File: `TASKS_POLICY`, else `$XDG_CONFIG_HOME/tasks/policy.huml`, else
//! `~/.config/tasks/policy.huml`. A missing file means the built-in defaults. The document is
//! strict: unknown keys and bad values are errors (never silently ignored). A malformed file
//! is reported loudly (daemon log, `tasks admission`, every submit) and the DEFAULTS apply,
//! so an error never switches enforcement off.
//!
//! ```huml
//! %HUML v0.2.0
//! broad_test: "refuse"   # or "warn"
//! ```
//!
//! Environment override (wins over the file): `TASKS_BROAD_TEST=refuse|warn`.
//!
//! Policy is applied at SUBMISSION only (`Recipe::spec`: `tasks run`/`enqueue`, MCP `submit`,
//! a peer's inbound submission, which applies the receiving host's own policy). Approval and
//! execution never re-evaluate a queued job.
//!
//! Seam for later work (not implemented): a trusted system layer and tighten-only user/
//! project layers would each produce a [`Policy`] and be combined before [`check`]; the
//! Cargo-specific detection lives in `metrics::testscope`.
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const POLICY_ENV: &str = "TASKS_POLICY";
pub const BROAD_TEST_ENV: &str = "TASKS_BROAD_TEST";

/// What to do with a `cargo test` that has a name filter but no target selector.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BroadTest {
    /// Refuse at submit (exit 2 / tool error, nothing queued) unless the submitter passes
    /// `--broad-test` (MCP `broad_test: true`), which is recorded on the job.
    #[default]
    Refuse,
    /// Submit it with a one-line warning.
    Warn,
}

impl BroadTest {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refuse => "refuse",
            Self::Warn => "warn",
        }
    }
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.trim() {
            "refuse" => Ok(Self::Refuse),
            "warn" => Ok(Self::Warn),
            other => Err(format!(
                "broad_test must be \"refuse\" or \"warn\", got {other:?}"
            )),
        }
    }
}

/// The policy document.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    pub broad_test: BroadTest,
    /// Health monitor thresholds (`health::` section; `health::config`). Each
    /// `TASKS_HEALTH_*` environment variable wins over its key. Read when the daemon starts.
    #[serde(
        default,
        skip_serializing_if = "crate::health::config::HealthPolicy::is_empty"
    )]
    pub health: crate::health::config::HealthPolicy,
}

/// The policy in force and where each value came from (what `tasks admission` shows).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effective {
    pub policy: Policy,
    /// The file consulted (whether or not it exists).
    pub path: PathBuf,
    /// Whether that file exists.
    pub file_present: bool,
    /// `default` (no file), `file`, or `default (file invalid)` / `default (file unreadable)`.
    pub source: String,
    /// `default`, `file` or `env` (`TASKS_BROAD_TEST`).
    pub broad_test_source: String,
    /// Errors in the file or the environment; the defaults (or the valid parts) apply.
    pub errors: Vec<String>,
}

impl Effective {
    /// Text for `tasks admission` (stderr) and the daemon log.
    pub fn summary(&self) -> String {
        let mut out = format!(
            "policy: broad_test={} (from {}); file {} ({})\n",
            self.policy.broad_test.as_str(),
            self.broad_test_source,
            self.path.display(),
            self.source
        );
        if !self.policy.health.is_empty() {
            // Compact JSON of the keys the file sets (TASKS_HEALTH_* variables still win).
            let set = serde_json::to_string(&self.policy.health).unwrap_or_default();
            out.push_str(&format!(
                "policy: health {set} (from file; TASKS_HEALTH_* variables win)\n"
            ));
        }
        for e in &self.errors {
            out.push_str(&format!("policy ERROR: {e}\n"));
        }
        out
    }
}

/// The policy file location (see module docs).
pub fn path_from(get: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
    if let Some(p) = get(POLICY_ENV).filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(x) = get("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        return Path::new(&x).join("tasks/policy.huml");
    }
    let home = get("HOME").unwrap_or_default();
    Path::new(&home).join(".config/tasks/policy.huml")
}

/// The 1-based line of the first `key:` in `text`, to point a serde error at it.
fn line_of_key(text: &str, error: &str) -> Option<usize> {
    let key = error.split('`').nth(1)?;
    text.lines()
        .position(|l| {
            l.trim_start()
                .strip_prefix(key)
                .is_some_and(|rest| rest.starts_with(':'))
        })
        .map(|i| i + 1)
}

/// Parses a policy document. Errors name the line when the parser or the key lookup can.
pub fn parse(text: &str) -> Result<Policy, String> {
    // A document with only the version line and comments sets nothing (huml-rs refuses an
    // empty root): the defaults.
    let empty = text.lines().all(|l| {
        let t = l.trim();
        t.is_empty() || t.starts_with('#') || t.starts_with("%HUML")
    });
    if empty {
        return Ok(Policy::default());
    }
    huml_rs::serde::from_str::<Policy>(text).map_err(|e| {
        let e = e.to_string();
        match line_of_key(text, &e) {
            Some(line) if !e.contains("line ") => format!("line {line}: {e}"),
            _ => e,
        }
    })
}

/// Loads the policy from `get` (the environment, injectable for tests).
pub fn load_with(get: &dyn Fn(&str) -> Option<OsString>) -> Effective {
    let path = path_from(get);
    let mut errors = vec![];
    let mut file_present = true;
    let (mut policy, source, mut broad_test_source) = match std::fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            file_present = false;
            (Policy::default(), "default".to_string(), "default")
        }
        Err(e) => {
            errors.push(format!("{}: {e}; using the defaults", path.display()));
            (
                Policy::default(),
                "default (file unreadable)".into(),
                "default",
            )
        }
        Ok(bytes) if bytes.len() > 64 * 1024 => {
            errors.push(format!(
                "{}: larger than 64 KiB; using the defaults",
                path.display()
            ));
            (
                Policy::default(),
                "default (file invalid)".into(),
                "default",
            )
        }
        Ok(bytes) => match parse(&String::from_utf8_lossy(&bytes)) {
            Ok(p) => (p, "file".into(), "file"),
            Err(e) => {
                errors.push(format!("{}: {e}; using the defaults", path.display()));
                (
                    Policy::default(),
                    "default (file invalid)".into(),
                    "default",
                )
            }
        },
    };
    if let Some(value) = get(BROAD_TEST_ENV) {
        match BroadTest::parse(&value.to_string_lossy()) {
            Ok(b) => {
                policy.broad_test = b;
                broad_test_source = "env";
            }
            Err(e) => errors.push(format!("{BROAD_TEST_ENV}: {e}; ignored")),
        }
    }
    Effective {
        policy,
        path,
        file_present,
        source,
        broad_test_source: broad_test_source.into(),
        errors,
    }
}

/// The policy of this process's environment, read fresh (a changed file applies to the next
/// submission without a restart).
pub fn effective() -> Effective {
    load_with(&|name| std::env::var_os(name))
}

/// Outcome of the policy check of one submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to say.
    Allowed,
    /// Allowed with warnings (one line each).
    Warn(Vec<String>),
    /// Refused: the message (starts with `refused: `).
    Refused(String),
}

/// Checks a live command's argv (run from `cwd`) against the policy. `escape` is the
/// submitter's explicit `--broad-test` / `broad_test: true`. Policy errors become warnings
/// on every submission (the defaults apply).
pub fn check(e: &Effective, argv: &[String], cwd: &Path, escape: bool) -> Verdict {
    use crate::metrics::testscope;
    let mut warnings: Vec<String> = e
        .errors
        .iter()
        .map(|err| format!("policy error (defaults apply): {err}"))
        .collect();
    if let Some(broad) = testscope::broad_test(argv) {
        let text = testscope::describe(&broad, cwd);
        match (e.policy.broad_test, escape) {
            (_, true) => warnings.push(format!(
                "{text} (--broad-test: building every target deliberately; recorded on the job)"
            )),
            (BroadTest::Warn, false) => warnings.push(text),
            (BroadTest::Refuse, false) => {
                let mut message = testscope::refusal(&broad, cwd);
                for w in &warnings {
                    message.push_str(&format!("\n{w}"));
                }
                return Verdict::Refused(message);
            }
        }
    }
    if warnings.is_empty() {
        Verdict::Allowed
    } else {
        Verdict::Warn(warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |k| map.get(k).cloned()
    }

    /// A file in this test's own temp dir, removed when the guard drops.
    struct Temp(PathBuf);
    impl Temp {
        fn new(test: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("tasks-policy-{test}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, name: &str, text: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, text).unwrap();
            p
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn v(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn the_committed_sample_parses() {
        let text = include_str!("../deploy/policy.huml");
        assert_eq!(parse(text).unwrap().broad_test, BroadTest::Refuse);
    }

    #[test]
    fn the_health_section_parses_strictly() {
        let p = parse(
            "%HUML v0.2.0\nbroad_test: \"warn\"\nhealth::\n  slot_orphan_s: 300\n  notify: \"off\"\n",
        )
        .unwrap();
        assert_eq!(p.broad_test, BroadTest::Warn);
        assert_eq!(p.health.slot_orphan_s, Some(300));
        assert_eq!(p.health.notify.as_deref(), Some("off"));
        assert_eq!(p.health.interval_s, None);
        let typo = parse("%HUML v0.2.0\nhealth::\n  slot_orphan: 300\n").unwrap_err();
        assert!(typo.contains("slot_orphan"), "{typo}");
    }

    #[test]
    fn values_unknown_keys_and_bad_values() {
        assert_eq!(
            parse("%HUML v0.2.0\nbroad_test: \"warn\"\n")
                .unwrap()
                .broad_test,
            BroadTest::Warn
        );
        assert_eq!(parse("%HUML v0.2.0\n").unwrap(), Policy::default());
        let typo = parse("%HUML v0.2.0\n# c\nbroad_tests: \"warn\"\n").unwrap_err();
        assert!(
            typo.contains("broad_tests") && typo.contains("line 3"),
            "{typo}"
        );
        let bad = parse("%HUML v0.2.0\nbroad_test: \"maybe\"\n").unwrap_err();
        assert!(bad.contains("maybe"), "{bad}");
        let rules = parse("%HUML v0.2.0\nrules::\n  - :: tool: \"go\"\n").unwrap_err();
        assert!(rules.contains("rules"), "not supported yet: {rules}");
        assert!(parse("broad_test: : :\n").is_err());
    }

    #[test]
    fn missing_file_defaults_env_wins_and_bad_files_fall_back_loudly() {
        let t = Temp::new("load");
        let missing = load_with(&env(&[(POLICY_ENV, "/nonexistent/tasks/policy.huml")]));
        assert_eq!(missing.policy, Policy::default());
        assert_eq!(
            (
                missing.source.as_str(),
                missing.file_present,
                missing.errors.len()
            ),
            ("default", false, 0)
        );
        let warn = t.file("warn.huml", "%HUML v0.2.0\nbroad_test: \"warn\"\n");
        let file = load_with(&env(&[(POLICY_ENV, warn.to_str().unwrap())]));
        assert_eq!(file.policy.broad_test, BroadTest::Warn);
        assert_eq!(
            (file.broad_test_source.as_str(), file.source.as_str()),
            ("file", "file")
        );
        let both = load_with(&env(&[
            (POLICY_ENV, warn.to_str().unwrap()),
            (BROAD_TEST_ENV, "refuse"),
        ]));
        assert_eq!(both.policy.broad_test, BroadTest::Refuse);
        assert_eq!(both.broad_test_source, "env");
        let bad_env = load_with(&env(&[
            (POLICY_ENV, warn.to_str().unwrap()),
            (BROAD_TEST_ENV, "off"),
        ]));
        assert_eq!(bad_env.policy.broad_test, BroadTest::Warn);
        assert!(bad_env.errors[0].contains(BROAD_TEST_ENV));
        let broken = t.file(
            "broken.huml",
            "%HUML v0.2.0\nbroad_test: \"warn\"\nbogus: 1\n",
        );
        let fallback = load_with(&env(&[(POLICY_ENV, broken.to_str().unwrap())]));
        assert_eq!(
            fallback.policy.broad_test,
            BroadTest::Refuse,
            "never disabled by an error"
        );
        assert_eq!(fallback.source, "default (file invalid)");
        assert!(
            fallback.summary().contains("policy ERROR") && fallback.summary().contains("bogus")
        );
        let xdg = path_from(&env(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")]));
        assert_eq!(xdg, PathBuf::from("/x/tasks/policy.huml"));
        assert_eq!(
            path_from(&env(&[("HOME", "/h")])),
            PathBuf::from("/h/.config/tasks/policy.huml")
        );
    }

    #[test]
    fn refuse_warn_and_the_escape_hatch() {
        let t = Temp::new("check");
        let cwd = Path::new("/nonexistent");
        let refuse = load_with(&env(&[(POLICY_ENV, "/nonexistent/p.huml")]));
        let warn = load_with(&env(&[
            (POLICY_ENV, "/nonexistent/p.huml"),
            (BROAD_TEST_ENV, "warn"),
        ]));
        let broad = v("cargo test -p x video_call");
        assert!(matches!(check(&refuse, &broad, cwd, false),
            Verdict::Refused(m) if m.starts_with("refused: this builds every test target of -p x; your filter 'video_call' matches: --lib.")));
        assert!(matches!(check(&refuse, &broad, cwd, true),
            Verdict::Warn(w) if w.len() == 1 && w[0].contains("--broad-test: building every target deliberately")));
        assert!(matches!(check(&warn, &broad, cwd, false),
            Verdict::Warn(w) if w == ["this builds every test target of -p x (every); name --lib/--test TARGET to build only what the filter runs"]));
        for allowed in [
            "cargo test -p x",
            "cargo test -p x --lib f",
            "cargo build -p x",
            "python3 t.py f",
        ] {
            assert_eq!(
                check(&refuse, &v(allowed), cwd, false),
                Verdict::Allowed,
                "{allowed}"
            );
        }
        // A forwarded job is judged by its inner command.
        let fwd = v(
            "tasks peer-forward --state /s --peer beta --who a --resource run -- cargo test -p x f",
        );
        assert!(matches!(
            check(&refuse, &fwd, cwd, false),
            Verdict::Refused(_)
        ));
        // A broken policy file warns on every submission and still refuses (defaults).
        let broken = t.file("broken2.huml", "%HUML v0.2.0\nnope: 1\n");
        let e = load_with(&env(&[(POLICY_ENV, broken.to_str().unwrap())]));
        assert!(
            matches!(check(&e, &v("cargo build"), cwd, false), Verdict::Warn(w) if w[0].contains("policy error"))
        );
        assert!(matches!(check(&e, &broad, cwd, false), Verdict::Refused(_)));
    }
}
