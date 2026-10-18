//! "Broad" `cargo test` builds: a name filter without a target selector makes Cargo build
//! (and run) EVERY test target of the selected packages, although the filter usually lives in
//! one of them. This module detects that from the argv, counts the package's known test
//! targets and words the submit-time warning or refusal; `crate::policy` decides which.
//! Cargo only for now: everything Cargo-specific about test scope lives here.
use super::command::{CargoInvocation, cargo_invocation};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Cargo flags (of `cargo test`) that take a value as the next word.
const VALUE_FLAGS: &[&str] = &[
    "-p",
    "--package",
    "-F",
    "--features",
    "--target",
    "--target-dir",
    "--manifest-path",
    "--profile",
    "-j",
    "--jobs",
    "--test",
    "--bin",
    "--example",
    "--bench",
    "--exclude",
    "--color",
    "--message-format",
    "-Z",
    "--config",
    "-C",
    "--lockfile-path",
];

/// Target selectors: with any of these the user chose what to build.
const SELECTORS: &[&str] = &[
    "--lib",
    "--test",
    "--tests",
    "--bin",
    "--bins",
    "--doc",
    "--example",
    "--examples",
    "--bench",
    "--benches",
    "--all-targets",
];

/// libtest options (after `--`) that take a value.
const HARNESS_VALUE_FLAGS: &[&str] = &[
    "--test-threads",
    "--skip",
    "--logfile",
    "--format",
    "--color",
    "-Z",
    "--report-time",
    "--shuffle-seed",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BroadTest {
    /// `-p`/`--package` values (empty: the package or workspace at the cwd).
    pub packages: Vec<String>,
    /// The name filter(s) that made it broad.
    pub filters: Vec<String>,
}

/// The `cargo` invocation of an argv (directly, behind `env K=V`, inside `sh -c`), unwrapping
/// a forwarded job (`tasks peer-forward ... -- CMD`): the command that actually builds.
fn cargo_of(argv: &[String]) -> Option<CargoInvocation> {
    let rest = super::command::strip_env(argv);
    if crate::peer_link::is_forward_argv(rest) {
        let inner = rest
            .iter()
            .position(|a| a == "--")
            .map(|i| &rest[i + 1..])?;
        return cargo_invocation(inner);
    }
    cargo_invocation(argv)
}

/// The arguments after `test`/`t` of a `cargo [+toolchain] [global flags] test ...`.
fn test_args(inv: &CargoInvocation) -> Option<&[String]> {
    let mut i = 0;
    let args = &inv.args;
    while i < args.len() {
        let a = args[i].as_str();
        if a.starts_with('+') {
            i += 1;
            continue;
        }
        if matches!(a, "-Z" | "--config" | "-C" | "--color") {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        return matches!(a, "test" | "t").then(|| &args[i + 1..]);
    }
    None
}

/// `Some` when the argv is a `cargo test` with a name filter (before or after `--`) and no
/// target selector (`--lib`, `--test T`, `--bin B`, `--doc`, ... see [`SELECTORS`]).
/// `None` for anything else, including non-test Cargo commands.
pub fn broad_test(argv: &[String]) -> Option<BroadTest> {
    let inv = cargo_of(argv)?;
    let args = test_args(&inv)?;
    let mut packages = vec![];
    let mut filters = vec![];
    let mut selected = false;
    let mut harness = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if harness {
            if HARNESS_VALUE_FLAGS.contains(&a) {
                i += 2;
                continue;
            }
            if !a.starts_with('-') {
                filters.push(a.to_owned());
            }
            i += 1;
            continue;
        }
        if a == "--" {
            harness = true;
            i += 1;
            continue;
        }
        if SELECTORS.contains(&a.split('=').next().unwrap_or(a)) {
            selected = true;
        }
        if a == "-p" || a == "--package" {
            if let Some(v) = args.get(i + 1) {
                packages.push(v.clone());
            }
            i += 2;
            continue;
        }
        if let Some(v) = a.strip_prefix("--package=") {
            packages.push(v.to_owned());
        } else if a.len() > 2 && a.starts_with("-p") {
            packages.push(a[2..].to_owned());
        }
        if VALUE_FLAGS.contains(&a) {
            i += 2;
            continue;
        }
        if !a.starts_with('-') {
            filters.push(a.to_owned());
        }
        i += 1;
    }
    (!selected && !filters.is_empty()).then_some(BroadTest { packages, filters })
}

/// `Some(true)` for a broad `cargo test`, `Some(false)` for another `cargo test`, `None` for
/// other commands (the per-job `broad_test_build` flag).
pub fn broad_flag(argv: &[String]) -> Option<bool> {
    let inv = cargo_of(argv)?;
    test_args(&inv)?;
    Some(broad_test(argv).is_some())
}

/// Test targets of one package that `cargo test` builds, counted from its layout: the lib
/// (`src/lib.rs` or `[lib]`), bins (`src/main.rs`, `src/bin/*.rs`, `src/bin/*/main.rs`,
/// `[[bin]]`), integration tests (`tests/*.rs`, `tests/*/main.rs`, `[[test]]`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownTargets {
    pub package: String,
    pub lib: u32,
    pub bins: u32,
    pub tests: u32,
    /// Integration test target names (`tests/NAME.rs`, `tests/NAME/main.rs`, `[[test]]`).
    pub test_names: Vec<String>,
}
impl KnownTargets {
    pub fn total(&self) -> u32 {
        self.lib + self.bins + self.tests
    }
}

fn read_manifest(dir: &Path) -> Option<toml::Table> {
    let path = dir.join("Cargo.toml");
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > 256 * 1024 {
        return None;
    }
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn package_name(table: &toml::Table) -> Option<&str> {
    table.get("package")?.get("name")?.as_str()
}

/// Workspace member directories (exact paths and `dir/*` globs; at most 256).
fn members(root: &Path, table: &toml::Table) -> Vec<PathBuf> {
    let mut out = vec![root.to_path_buf()];
    let patterns = table
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    for p in patterns.iter().filter_map(|p| p.as_str()) {
        if let Some(prefix) = p.strip_suffix("/*") {
            if let Ok(entries) = std::fs::read_dir(root.join(prefix)) {
                for e in entries.flatten().take(256) {
                    out.push(e.path());
                }
            }
        } else if !p.contains('*') {
            out.push(root.join(p));
        }
        if out.len() > 256 {
            break;
        }
    }
    out
}

/// Target names: `name.rs` files and `name/main.rs` dirs directly in `dir`, sorted.
fn target_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut names: Vec<String> = entries
        .flatten()
        .take(4096)
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
    names.sort();
    names
}

fn count_targets(dir: &Path) -> u32 {
    target_names(dir).len() as u32
}

fn array_len(table: &toml::Table, key: &str) -> u32 {
    table
        .get(key)
        .and_then(|v| v.as_array())
        .map_or(0, |a| a.len() as u32)
}

fn auto(table: &toml::Table, key: &str) -> bool {
    table
        .get("package")
        .and_then(|p| p.get(key))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// Known test targets of `package` (or of the package at `cwd` when `None`), from the
/// workspace around `cwd`. None when the package is not found (virtual manifest without
/// `-p`, unknown name, unreadable manifests): the warning then says "every".
pub fn known_targets(cwd: &Path, package: Option<&str>) -> Option<KnownTargets> {
    let root = super::warmth::workspace_root(cwd);
    let dir = match package {
        None => {
            // The nearest manifest with a [package] from cwd up to the workspace root.
            cwd.ancestors()
                .take_while(|d| d.starts_with(&root))
                .find(|d| read_manifest(d).is_some_and(|t| package_name(&t).is_some()))?
                .to_path_buf()
        }
        Some(name) => {
            let table = read_manifest(&root)?;
            members(&root, &table).into_iter().find(|d| {
                read_manifest(d).is_some_and(|t| {
                    package_name(&t).is_some_and(|n| {
                        super::compile::normalize_crate(n) == super::compile::normalize_crate(name)
                    })
                })
            })?
        }
    };
    let table = read_manifest(&dir)?;
    let name = package_name(&table)?.to_owned();
    let lib = u32::from(dir.join("src/lib.rs").is_file() || table.contains_key("lib"));
    let mut bins = array_len(&table, "bin");
    if auto(&table, "autobins") {
        // Explicit [[bin]] entries usually name these same files; prefer the larger view.
        let auto_bins =
            u32::from(dir.join("src/main.rs").is_file()) + count_targets(&dir.join("src/bin"));
        bins = bins.max(auto_bins);
    }
    let mut test_names: Vec<String> = table
        .get("test")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_owned))
        .collect();
    if auto(&table, "autotests") {
        for n in target_names(&dir.join("tests")) {
            if !test_names.contains(&n) {
                test_names.push(n);
            }
        }
    }
    let tests = (test_names.len() as u32).max(array_len(&table, "test"));
    Some(KnownTargets {
        package: name,
        lib,
        bins,
        tests,
        test_names,
    })
}

/// What a broad test builds and the known target count (Cargo: from the package layout).
fn scope_words(broad: &BroadTest, cwd: &Path) -> (String, Option<KnownTargets>) {
    match broad.packages.as_slice() {
        [one] => (format!("-p {one}"), known_targets(cwd, Some(one))),
        [] => {
            let known = known_targets(cwd, None);
            let what = known
                .as_ref()
                .map(|k| format!("-p {}", k.package))
                .unwrap_or_else(|| "this workspace".into());
            (what, known)
        }
        many => (format!("-p {}", many.join(" -p ")), None),
    }
}

/// The one-line warning for a broad test:
/// `this builds every test target of -p X (N known); name --lib/--test TARGET to build only
/// what the filter runs`.
pub fn describe(broad: &BroadTest, cwd: &Path) -> String {
    let (what, known) = scope_words(broad, cwd);
    let count = known
        .map(|k| format!("{} known", k.total()))
        .unwrap_or_else(|| "every".into());
    format!(
        "this builds every test target of {what} ({count}); name --lib/--test TARGET to build only what the filter runs"
    )
}

/// Targets matching the filters: always `--lib`, plus every integration test whose name
/// contains a filter (or equals a `::` segment of one).
pub fn suggestions(filters: &[String], known: Option<&KnownTargets>) -> Vec<String> {
    let mut out = vec!["--lib".to_string()];
    for name in known.map(|k| k.test_names.as_slice()).unwrap_or(&[]) {
        let hit = filters.iter().any(|f| {
            !f.is_empty() && (name.contains(f.as_str()) || f.split("::").any(|seg| seg == name))
        });
        if hit {
            out.push(format!("--test {name}"));
        }
    }
    out
}

/// The refusal for a broad test under `broad_test = "refuse"`:
/// `refused: this builds all 37 test targets of -p p2pcore; your filter 'video_call' matches:
/// --lib, --test video_calls. Re-run with those, or pass --broad-test to build every target
/// deliberately.`
pub fn refusal(broad: &BroadTest, cwd: &Path) -> String {
    let escape = "pass --broad-test (MCP: broad_test: true) to build every target deliberately";
    let (what, known) = scope_words(broad, cwd);
    let all = match &known {
        Some(k) => format!("all {} test targets", k.total()),
        None => "every test target".to_string(),
    };
    let filters = broad
        .filters
        .iter()
        .map(|f| format!("'{f}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let plural = if broad.filters.len() == 1 {
        "filter"
    } else {
        "filters"
    };
    format!(
        "refused: this builds {all} of {what}; your {plural} {filters} {}: {}. Re-run with those, or {escape}.",
        if broad.filters.len() == 1 {
            "matches"
        } else {
            "match"
        },
        suggestions(&broad.filters, known.as_ref()).join(", ")
    )
}

/// [`describe`] for a submitted job (its recorded argv and checkout), if it is broad.
pub fn job_warning(job: &crate::Job) -> Option<String> {
    let broad = broad_test(&super::history::argv(job))?;
    Some(describe(&broad, Path::new(&job.spec.source)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn a_name_filter_without_a_selector_is_broad() {
        let b = broad_test(&v("cargo test -p x filter")).unwrap();
        assert_eq!(b.packages, ["x"]);
        assert_eq!(b.filters, ["filter"]);
        assert!(broad_test(&v("cargo test --offline --locked -j 2 -p x some_test")).is_some());
        // A filter after `--` without a selector is broad as well.
        assert!(broad_test(&v("cargo test -p x -- filter")).is_some());
        assert!(broad_test(&v("cargo t --package=x filter")).is_some());
        assert!(broad_test(&v("cargo +nightly test -px filter")).is_some());
        // env K=V cargo ... and env -- K=V.
        let env = broad_test(&v(
            "env RUST_LOG=debug CARGO_TERM_COLOR=never cargo test -p x f",
        ))
        .unwrap();
        assert_eq!(env.packages, ["x"]);
        assert!(broad_test(&v("RUST_BACKTRACE=1 cargo test f")).is_some());
        // Inside sh -c text (one argv word) too: wrapping does not hide it.
        let sh = vec![
            "sh".to_string(),
            "-c".into(),
            "cd x && RUST_LOG=1 cargo test -p x f".into(),
        ];
        assert!(broad_test(&sh).is_some());
    }

    #[test]
    fn selectors_or_no_filter_are_not_broad() {
        for cmd in [
            "cargo test -p x --lib filter",
            "cargo test -p x --test t",
            "cargo test -p x --test t -- filter",
            "cargo test -p x --test=t filter",
            "cargo test -p x --doc filter",
            "cargo test -p x --bin tasks filter",
            "cargo test -p x --all-targets filter",
            "cargo test -p x",
            "cargo test -p x -- --nocapture --test-threads 1",
            "cargo test --features a,b -p x --target-dir t",
            "cargo build -p x filter",
            "cargo check",
            "echo cargo test filter",
        ] {
            assert!(broad_test(&v(cmd)).is_none(), "{cmd}");
        }
        assert_eq!(broad_flag(&v("cargo test -p x --lib f")), Some(false));
        assert_eq!(broad_flag(&v("cargo test -p x f")), Some(true));
        assert_eq!(broad_flag(&v("cargo build")), None);
        // A forwarded job's inner command decides.
        let fwd = v(
            "tasks peer-forward --state /s --peer beta --who a --resource compile -- cargo test -p x f",
        );
        assert_eq!(broad_flag(&fwd), Some(true));
    }

    #[test]
    fn known_targets_are_counted_from_the_package_layout() {
        let root = std::env::temp_dir().join(format!("tasks-testscope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let pkg = root.join("crates/sync");
        for d in ["src/bin/tool", "tests/suite"] {
            std::fs::create_dir_all(pkg.join(d)).unwrap();
        }
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        std::fs::write(
            pkg.join("Cargo.toml"),
            "[package]\nname = \"profile-sync\"\n",
        )
        .unwrap();
        for f in [
            "src/lib.rs",
            "src/main.rs",
            "src/bin/extra.rs",
            "src/bin/tool/main.rs",
            "tests/a.rs",
            "tests/b.rs",
            "tests/suite/main.rs",
        ] {
            std::fs::write(pkg.join(f), "").unwrap();
        }
        let k = known_targets(&root, Some("profile_sync")).unwrap();
        assert_eq!((k.lib, k.bins, k.tests, k.total()), (1, 3, 3, 7));
        assert_eq!(k.test_names, ["a", "b", "suite"]);
        assert_eq!(
            suggestions(
                &["video_call".into()],
                Some(&KnownTargets {
                    test_names: vec!["video_calls".into(), "audio".into()],
                    ..Default::default()
                })
            ),
            ["--lib", "--test video_calls"]
        );
        assert_eq!(
            known_targets(&pkg.join("tests"), None).unwrap().package,
            "profile-sync"
        );
        assert!(known_targets(&root, None).is_none(), "virtual manifest");
        assert!(known_targets(&root, Some("nope")).is_none());
        let w = |cmd: &str| {
            let broad = broad_test(&v(cmd))?;
            Some((describe(&broad, &root), refusal(&broad, &root)))
        };
        assert_eq!(
            w("cargo test -p profile-sync some_case").unwrap().0,
            "this builds every test target of -p profile-sync (7 known); name --lib/--test TARGET to build only what the filter runs"
        );
        assert_eq!(
            w("cargo test -p nope f").unwrap().0,
            "this builds every test target of -p nope (every); name --lib/--test TARGET to build only what the filter runs"
        );
        assert!(w("cargo test -p profile-sync --test a f").is_none());
        // The refusal names the count and the targets the filter can be in.
        assert_eq!(
            w("cargo test -p profile-sync suite").unwrap().1,
            "refused: this builds all 7 test targets of -p profile-sync; your filter 'suite' matches: --lib, --test suite. Re-run with those, or pass --broad-test (MCP: broad_test: true) to build every target deliberately."
        );
        assert!(
            w("cargo test -p profile-sync a::x")
                .unwrap()
                .1
                .contains("matches: --lib, --test a.")
        );
        assert!(
            w("cargo test -p nope f")
                .unwrap()
                .1
                .starts_with("refused: this builds every test target of -p nope;")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
