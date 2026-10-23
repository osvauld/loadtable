//! Submit-time command rules shared by the CLI, the MCP front end and (later) the daemon's
//! submit handler. Pure functions over an argv: no file, clock, process or network access.
//!
//! * R-191: a `cargo test` with a name filter but no target selector builds every test target of
//!   the selected packages; it is refused with suggestions unless the submitter escapes it
//!   (`--broad-test`), or the policy downgrades the refusal to a warning.
//! * R-139 / R-140: only log-only commands (cargo check, test, clippy, nextest; Gradle test
//!   tasks) are placed on another machine by default; everything else stays here.
//! * R-137 / R-091: Cargo's parallelism is read from every form (`-j N`, `-jN`, `--jobs N`,
//!   `--jobs=N`, `CARGO_BUILD_JOBS=N`), checked against a cap, and replaced by the chosen
//!   machine's cap in an `env` prefix.
//! * R-210: a denied device serial anywhere in the argv, in any case, is refused.

use std::collections::BTreeSet;
use tasks_core::DENIED_DEVICES;
use tasks_timing::RecipeKey;

/// What to do with a broad `cargo test` that carries no escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BroadTestMode {
    /// Refuse the submission.
    #[default]
    Refuse,
    /// Submit it with a warning.
    Warn,
}

impl BroadTestMode {
    /// Parses the policy value `refuse` or `warn`.
    pub fn parse(text: &str) -> Result<BroadTestMode, String> {
        match text.trim() {
            "refuse" => Ok(BroadTestMode::Refuse),
            "warn" => Ok(BroadTestMode::Warn),
            other => Err(format!(
                "broad_test must be \"refuse\" or \"warn\", got {other:?}"
            )),
        }
    }
}

/// A `cargo test` that would build every test target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadTest {
    /// `-p` packages (empty: the package or workspace at the working directory).
    pub packages: Vec<String>,
    /// The name filters that made it broad.
    pub filters: Vec<String>,
}

/// Test targets the caller found on disk, used to word the refusal precisely.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownTargets {
    /// Number of test targets `cargo test` would build (lib, bins, integration tests).
    pub total: usize,
    /// Integration test target names.
    pub test_names: Vec<String>,
}

const SELECTOR_PREFIXES: &[&str] = &["test:", "bin:", "example:", "bench:"];
const SELECTORS: &[&str] = &[
    "lib",
    "bins",
    "doc",
    "tests",
    "benches",
    "examples",
    "all-targets",
];

fn keys(argv: &[String]) -> Vec<RecipeKey> {
    RecipeKey::from_argv(argv, &[])
}

/// `Some` when `argv` is a `cargo test` with a name filter and no target selector. Looks through
/// `env NAME=value` prefixes and `sh -c "..."` chains.
pub fn broad_test(argv: &[String]) -> Option<BroadTest> {
    for k in keys(argv) {
        if k.tool != "cargo" || k.subcommand != "test" {
            continue;
        }
        let filters: Vec<String> = k
            .selection
            .iter()
            .filter_map(|s| s.strip_prefix("filter:"))
            .map(str::to_string)
            .collect();
        let selected = k.selection.iter().any(|s| {
            SELECTORS.contains(&s.as_str()) || SELECTOR_PREFIXES.iter().any(|p| s.starts_with(p))
        });
        if !selected && !filters.is_empty() {
            return Some(BroadTest {
                packages: k.packages.clone(),
                filters,
            });
        }
    }
    None
}

/// The targets worth naming instead: `--lib` plus every integration test whose name contains a
/// filter (or equals a `::` segment of one).
pub fn suggestions(broad: &BroadTest, known: Option<&KnownTargets>) -> Vec<String> {
    let mut out = vec!["--lib".to_string()];
    for name in known.map(|k| k.test_names.as_slice()).unwrap_or(&[]) {
        let hit = broad.filters.iter().any(|f| {
            !f.is_empty() && (name.contains(f.as_str()) || f.split("::").any(|seg| seg == name))
        });
        if hit {
            out.push(format!("--test {name}"));
        }
    }
    out
}

fn scope(broad: &BroadTest) -> String {
    if broad.packages.is_empty() {
        "the package or workspace in this directory".to_string()
    } else {
        broad
            .packages
            .iter()
            .map(|p| format!("-p {p}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The warning text for a broad test that is allowed.
pub fn broad_test_description(broad: &BroadTest, known: Option<&KnownTargets>) -> String {
    let count = known.map_or_else(|| "every".to_string(), |k| format!("{} known", k.total));
    format!(
        "this builds every test target of {} ({count}); name --lib/--test TARGET to build only what the filter runs",
        scope(broad)
    )
}

/// The refusal text, with suggestions and the escape.
pub fn broad_test_refusal(broad: &BroadTest, known: Option<&KnownTargets>) -> String {
    let all = known.map_or_else(
        || "every test target".to_string(),
        |k| format!("all {} test targets", k.total),
    );
    let filters = broad
        .filters
        .iter()
        .map(|f| format!("'{f}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let (noun, verb) = if broad.filters.len() == 1 {
        ("filter", "matches")
    } else {
        ("filters", "match")
    };
    format!(
        "refused: this builds {all} of {}; your {noun} {filters} {verb}: {}. Re-run with those, or pass --broad-test (MCP: broad_test: true) to build every target deliberately.",
        scope(broad),
        suggestions(broad, known).join(", ")
    )
}

/// The device serial from [`DENIED_DEVICES`] that appears anywhere in `argv`, in any case.
pub fn denied_device(argv: &[String]) -> Option<&'static str> {
    DENIED_DEVICES.iter().copied().find(|d| {
        let d = d.to_ascii_lowercase();
        argv.iter().any(|a| a.to_ascii_lowercase().contains(&d))
    })
}

/// Which rule refused a submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// R-191.
    BroadTest,
    /// R-210.
    DeniedDevice,
    /// R-091.
    CargoJobs,
}

impl Rule {
    /// The regression id that names the rule, as the audit log records it.
    pub fn id(self) -> &'static str {
        match self {
            Rule::BroadTest => "R-191",
            Rule::DeniedDevice => "R-210",
            Rule::CargoJobs => "R-091",
        }
    }
}

/// One refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The rule.
    pub rule: Rule,
    /// What is wrong and how to fix it.
    pub message: String,
}

/// What a submission must satisfy.
#[derive(Debug, Clone, Copy)]
pub struct SubmitRules<'a> {
    /// The command.
    pub argv: &'a [String],
    /// The submitter passed `--broad-test`.
    pub broad_test_escape: bool,
    /// Policy for a broad test without escape.
    pub broad_mode: BroadTestMode,
    /// Test targets found on disk, if the caller looked.
    pub known: Option<&'a KnownTargets>,
    /// The cap on Cargo parallelism, when the caller knows it.
    pub max_cargo_jobs: Option<u32>,
}

/// The verdict: refusals stop the submission, warnings are shown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Why the submission must not proceed.
    pub refusals: Vec<Refusal>,
    /// Things to tell the submitter.
    pub warnings: Vec<String>,
}

impl Verdict {
    /// True when nothing refuses the submission.
    pub fn allowed(&self) -> bool {
        self.refusals.is_empty()
    }
}

/// Applies every submit rule to a command.
pub fn check(rules: &SubmitRules<'_>) -> Verdict {
    let mut v = Verdict::default();
    if let Some(d) = denied_device(rules.argv) {
        v.refusals.push(Refusal {
            rule: Rule::DeniedDevice,
            message: format!(
                "refused: the command names the denied device {d} (this phone is never used); name a test device with --device SERIAL"
            ),
        });
    }
    if let Some(broad) = broad_test(rules.argv) {
        match (rules.broad_test_escape, rules.broad_mode) {
            (true, _) => v.warnings.push(format!(
                "{} (--broad-test: building every target deliberately; recorded on the job)",
                broad_test_description(&broad, rules.known)
            )),
            (false, BroadTestMode::Warn) => {
                v.warnings.push(broad_test_description(&broad, rules.known))
            }
            (false, BroadTestMode::Refuse) => v.refusals.push(Refusal {
                rule: Rule::BroadTest,
                message: broad_test_refusal(&broad, rules.known),
            }),
        }
    }
    if let (Some(cap), Some(n)) = (rules.max_cargo_jobs, requested_jobs(rules.argv))
        && n > cap
    {
        v.refusals.push(Refusal {
            rule: Rule::CargoJobs,
            message: format!(
                "refused: cargo parallelism {n} is above this machine's cap of {cap}; drop -j/--jobs/CARGO_BUILD_JOBS or use {cap} or less (the cap is max_cargo_jobs in the machine policy)"
            ),
        });
    }
    v
}

/// Whether a command may be placed on another machine by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementClass {
    /// Log-only: nothing but the log is needed afterwards.
    LogOnly,
    /// Stays on this machine, for this reason.
    StaysLocal(String),
}

/// R-139 / R-140: log-only Cargo commands (check, test, clippy, nextest) and Gradle test tasks
/// may be placed; builds, runs, scripts and unknown programs stay local.
pub fn placement_class(argv: &[String]) -> PlacementClass {
    let ks = keys(argv);
    if ks.is_empty() {
        return PlacementClass::StaysLocal("no command".into());
    }
    for k in &ks {
        match (k.tool.as_str(), k.subcommand.as_str()) {
            ("cargo", "check" | "test" | "clippy" | "nextest" | "nextest run" | "nextest list") => {
                if k.flags.iter().any(|f| f == "no-run") && k.subcommand == "test" {
                    return PlacementClass::StaysLocal(
                        "cargo test --no-run leaves build output that later steps use here".into(),
                    );
                }
            }
            ("cargo", sub) => {
                return PlacementClass::StaysLocal(format!(
                    "cargo {sub} produces build output that stays on this machine; only check, test, clippy and nextest are placed"
                ));
            }
            ("gradle", _) => {
                if k.kind() != tasks_core::Kind::Test {
                    return PlacementClass::StaysLocal(
                        "this Gradle task may produce artifacts that stay on this machine; only test tasks are placed"
                            .into(),
                    );
                }
            }
            (tool, _) => {
                return PlacementClass::StaysLocal(format!(
                    "{tool} is not a log-only build command; scripts and other programs stay on this machine"
                ));
            }
        }
    }
    PlacementClass::LogOnly
}

fn jobs_value(a: &str, next: Option<&String>) -> Option<(Option<u32>, bool)> {
    // (parsed value, consumed next word)
    if a == "-j" || a == "--jobs" {
        return Some((next.and_then(|n| n.parse().ok()), true));
    }
    if let Some(v) = a.strip_prefix("--jobs=") {
        return Some((v.parse().ok(), false));
    }
    if let Some(v) = a.strip_prefix("-j")
        && !v.is_empty()
        && v.chars().all(|c| c.is_ascii_digit())
    {
        return Some((v.parse().ok(), false));
    }
    None
}

fn env_prefix_len(argv: &[String]) -> usize {
    let mut i = 0;
    if argv
        .first()
        .is_some_and(|a| a.rsplit('/').next() == Some("env"))
    {
        i = 1;
    }
    while argv.get(i).is_some_and(|a| is_assignment(a)) {
        i += 1;
    }
    i
}

fn is_assignment(a: &str) -> bool {
    a.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !k.starts_with(|c: char| c.is_ascii_digit())
    })
}

fn is_cargo(argv: &[String]) -> bool {
    argv.get(env_prefix_len(argv))
        .is_some_and(|a| a.rsplit('/').next() == Some("cargo"))
}

/// The Cargo parallelism a command asks for, from any form or the `CARGO_BUILD_JOBS` env prefix.
/// Flags win over the environment, as in Cargo. `None` for non-Cargo commands or no request.
pub fn requested_jobs(argv: &[String]) -> Option<u32> {
    if !is_cargo(argv) {
        return None;
    }
    let pre = env_prefix_len(argv);
    let from_env = argv
        .iter()
        .take(pre)
        .find_map(|a| a.strip_prefix("CARGO_BUILD_JOBS="))
        .and_then(|v| v.parse().ok());
    let mut from_flag = None;
    let mut i = pre;
    while let Some(a) = argv.get(i) {
        if a == "--" {
            break;
        }
        if let Some((v, consumed)) = jobs_value(a, argv.get(i + 1)) {
            from_flag = v.or(from_flag);
            if consumed {
                i += 1;
            }
        }
        i += 1;
    }
    from_flag.or(from_env)
}

/// Drops every Cargo parallelism form before the harness `--`, and `CARGO_BUILD_JOBS=` words in
/// the env prefix. Non-Cargo commands pass through unchanged.
pub fn strip_jobs(argv: &[String]) -> Vec<String> {
    if !is_cargo(argv) {
        return argv.to_vec();
    }
    let pre = env_prefix_len(argv);
    let mut out: Vec<String> = argv
        .iter()
        .take(pre)
        .filter(|a| !a.starts_with("CARGO_BUILD_JOBS="))
        .cloned()
        .collect();
    let mut harness = false;
    let mut i = pre;
    while let Some(a) = argv.get(i) {
        if !harness {
            if a == "--" {
                harness = true;
            } else if let Some((_, consumed)) = jobs_value(a, argv.get(i + 1)) {
                if consumed {
                    i += 1;
                }
                i += 1;
                continue;
            }
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

/// Replaces the command's parallelism by `jobs`, as `CARGO_BUILD_JOBS=jobs` in an `env` prefix.
/// Non-Cargo commands pass through unchanged.
pub fn apply_jobs(argv: &[String], jobs: u32) -> Vec<String> {
    if !is_cargo(argv) {
        return argv.to_vec();
    }
    let stripped = strip_jobs(argv);
    let has_env = stripped
        .first()
        .is_some_and(|a| a.rsplit('/').next() == Some("env"));
    let assignment = format!("CARGO_BUILD_JOBS={jobs}");
    if has_env {
        let mut out = vec![stripped.first().cloned().unwrap_or_default(), assignment];
        out.extend(stripped.into_iter().skip(1));
        out
    } else {
        let mut out = vec!["env".to_string(), assignment];
        out.extend(stripped);
        out
    }
}

/// Distinct packages named by `-p`/`--package` in a command, for display.
pub fn packages_of(argv: &[String]) -> Vec<String> {
    keys(argv)
        .into_iter()
        .flat_map(|k| k.packages)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    fn rules<'a>(argv: &'a [String], escape: bool, mode: BroadTestMode) -> SubmitRules<'a> {
        SubmitRules {
            argv,
            broad_test_escape: escape,
            broad_mode: mode,
            known: None,
            max_cargo_jobs: None,
        }
    }

    // R-191
    #[test]
    fn r191_a_name_filter_without_a_selector_is_broad() {
        for cmd in [
            "cargo test -p core video_call",
            "cargo test video_call -- --nocapture",
            "cargo test -p core -- video_call",
            "env FOO=1 cargo test -p core video_call",
            "cargo +nightly test --workspace foo",
        ] {
            assert!(broad_test(&v(cmd)).is_some(), "{cmd}");
        }
        let b = broad_test(&v("cargo test -p core -p net video_call")).unwrap();
        assert_eq!(b.packages, ["core", "net"]);
        assert_eq!(b.filters, ["video_call"]);
    }

    #[test]
    fn r191_selectors_or_no_filter_are_not_broad() {
        for cmd in [
            "cargo test -p core",
            "cargo test -p core --lib video_call",
            "cargo test -p core --test video_calls video_call",
            "cargo test --doc foo",
            "cargo test --bin tool foo",
            "cargo test --all-targets foo",
            "cargo test -p core -- --nocapture",
            "cargo test -p core -- --skip slow",
            "cargo check -p core foo",
            "cargo build",
            "echo hello",
        ] {
            assert_eq!(broad_test(&v(cmd)), None, "{cmd}");
        }
    }

    #[test]
    fn r191_shell_chains_are_looked_through() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "cd x && cargo test -p core foo".to_string(),
        ];
        assert!(broad_test(&argv).is_some());
    }

    #[test]
    fn r191_refuse_warn_and_the_escape_hatch() {
        let argv = v("cargo test -p p2pcore video_call");
        let known = KnownTargets {
            total: 37,
            test_names: vec!["video_calls".into(), "lifecycle".into()],
        };
        let mut r = rules(&argv, false, BroadTestMode::Refuse);
        r.known = Some(&known);
        let verdict = check(&r);
        assert!(!verdict.allowed());
        let m = &verdict.refusals[0].message;
        assert_eq!(verdict.refusals[0].rule, Rule::BroadTest);
        assert!(m.starts_with("refused: this builds all 37 test targets of -p p2pcore"));
        assert!(
            m.contains("'video_call' matches: --lib, --test video_calls"),
            "{m}"
        );
        assert!(!m.contains("lifecycle"));
        assert!(m.contains("--broad-test"));

        // the escape allows it and says so
        let verdict = check(&rules(&argv, true, BroadTestMode::Refuse));
        assert!(verdict.allowed());
        assert!(verdict.warnings[0].contains("--broad-test: building every target deliberately"));

        // policy can downgrade the refusal to a warning
        let verdict = check(&rules(&argv, false, BroadTestMode::Warn));
        assert!(verdict.allowed());
        assert!(verdict.warnings[0].contains("every test target"));

        // a narrow command is silent
        assert_eq!(
            check(&rules(
                &v("cargo test -p core --lib x"),
                false,
                BroadTestMode::Refuse
            )),
            Verdict::default()
        );
    }

    #[test]
    fn r191_refusal_without_known_targets_names_the_scope_generically() {
        let b = broad_test(&v("cargo test foo bar")).unwrap();
        let m = broad_test_refusal(&b, None);
        assert!(m.contains("every test target of the package or workspace in this directory"));
        assert!(m.contains("'bar', 'foo' match: --lib"));
        assert_eq!(BroadTestMode::parse("warn"), Ok(BroadTestMode::Warn));
        assert!(BroadTestMode::parse("never").is_err());
    }

    // R-210
    #[test]
    fn r210_a_denied_device_is_found_anywhere_in_any_case() {
        assert_eq!(
            denied_device(&v("adb -s zd22257p6g install a.apk")),
            Some("ZD22257P6G")
        );
        assert_eq!(denied_device(&v("adb -s 9BWAY1LSDC install")), None);
        let argv = v("sh -c adb-ZD22257P6G");
        let verdict = check(&rules(&argv, false, BroadTestMode::Refuse));
        assert_eq!(verdict.refusals[0].rule, Rule::DeniedDevice);
    }

    // R-139 / R-140
    #[test]
    fn r139_only_log_only_cargo_commands_are_placed_by_default() {
        for cmd in [
            "cargo check",
            "cargo test -p x",
            "cargo clippy --workspace",
            "cargo nextest run",
            "env A=1 cargo test",
        ] {
            assert_eq!(placement_class(&v(cmd)), PlacementClass::LogOnly, "{cmd}");
        }
        for cmd in [
            "cargo build --release",
            "cargo run",
            "cargo test --no-run",
            "./scripts/build.sh",
            "make",
        ] {
            assert!(
                matches!(placement_class(&v(cmd)), PlacementClass::StaysLocal(_)),
                "{cmd}"
            );
        }
        assert!(matches!(
            placement_class(&[]),
            PlacementClass::StaysLocal(_)
        ));
    }

    #[test]
    fn r140_gradle_test_tasks_are_placed_and_artifact_builds_stay() {
        assert_eq!(
            placement_class(&v("./gradlew :app:testDebugUnitTest")),
            PlacementClass::LogOnly
        );
        assert!(matches!(
            placement_class(&v("./gradlew assembleDebug")),
            PlacementClass::StaysLocal(_)
        ));
    }

    // R-137
    #[test]
    fn r137_every_parallelism_form_is_read() {
        for (cmd, want) in [
            ("cargo test -j 3", Some(3)),
            ("cargo test -j3", Some(3)),
            ("cargo test --jobs 4", Some(4)),
            ("cargo test --jobs=5", Some(5)),
            ("env CARGO_BUILD_JOBS=6 cargo test", Some(6)),
            ("CARGO_BUILD_JOBS=7 cargo test", Some(7)),
            ("env CARGO_BUILD_JOBS=6 cargo test -j2", Some(2)),
            ("cargo test", None),
            ("cargo test -- -j9", None),
            ("make -j8", None),
        ] {
            assert_eq!(requested_jobs(&v(cmd)), want, "{cmd}");
        }
    }

    #[test]
    fn r137_strip_drops_every_form_only_before_harness_args() {
        assert_eq!(
            strip_jobs(&v(
                "cargo test -j 3 -p x --jobs=4 -j5 --jobs 6 -- -j9 --jobs 2"
            )),
            v("cargo test -p x -- -j9 --jobs 2")
        );
        assert_eq!(
            strip_jobs(&v("env CARGO_BUILD_JOBS=8 FOO=1 cargo check")),
            v("env FOO=1 cargo check")
        );
        assert_eq!(strip_jobs(&v("make -j8")), v("make -j8"));
    }

    #[test]
    fn r137_apply_sets_the_cap_in_an_env_prefix_and_passes_other_commands_through() {
        assert_eq!(
            apply_jobs(&v("cargo test -j 3 -p x"), 6),
            v("env CARGO_BUILD_JOBS=6 cargo test -p x")
        );
        assert_eq!(
            apply_jobs(&v("env FOO=1 cargo test --jobs=9"), 2),
            v("env CARGO_BUILD_JOBS=2 FOO=1 cargo test")
        );
        assert_eq!(apply_jobs(&v("make -j8"), 2), v("make -j8"));
        assert_eq!(
            requested_jobs(&apply_jobs(&v("cargo test -j 3"), 6)),
            Some(6)
        );
    }

    // R-091
    #[test]
    fn r091_a_request_above_the_cap_is_refused_with_the_fix() {
        let argv = v("cargo test -j 3");
        let mut r = rules(&argv, false, BroadTestMode::Refuse);
        r.max_cargo_jobs = Some(2);
        let verdict = check(&r);
        assert_eq!(verdict.refusals[0].rule, Rule::CargoJobs);
        assert!(verdict.refusals[0].message.contains("cap of 2"));
        r.max_cargo_jobs = Some(3);
        assert!(check(&r).allowed());
        r.max_cargo_jobs = None;
        assert!(check(&r).allowed());
    }

    #[test]
    fn packages_are_listed_once() {
        assert_eq!(packages_of(&v("cargo test -p b -p a -p b")), ["a", "b"]);
    }
}
