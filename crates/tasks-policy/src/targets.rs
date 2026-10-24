//! The test targets a `cargo test` command would build, read from `cargo metadata` (R-704,
//! R-705). Nothing is built to learn them, so a suite can be split into one step per target
//! before the first compile.

use crate::load::PolicyFs;
use crate::model::{Policy, SplitMode};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use tasks_timing::{ExeKind, TestExe};

/// The executables of the split steps of one suite, and what could not be split.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SuiteTargets {
    /// Targets per split step name.
    pub executables: crate::Executables,
    /// Why a step stays whole, one line each.
    pub notes: Vec<String>,
}

/// Names of the steps of `suite` that ask to be split.
pub fn split_steps<'a>(policy: &'a Policy, suite: &str) -> Vec<&'a crate::StepDef> {
    policy
        .suites
        .get(suite)
        .map(|s| {
            s.steps
                .iter()
                .filter(|d| d.split == Some(SplitMode::CargoTest))
                .collect()
        })
        .unwrap_or_default()
}

/// Reads the targets of every split step of `suite`. `cargo metadata` runs once, through `fs`,
/// and only when a step asks for a split. A step whose targets cannot be told stays whole, with
/// a note.
pub fn suite_targets(fs: &dyn PolicyFs, cwd: &Path, policy: &Policy, suite: &str) -> SuiteTargets {
    let mut out = SuiteTargets::default();
    let steps = split_steps(policy, suite);
    if steps.is_empty() {
        return out;
    }
    let metadata = match fs.cargo_metadata(cwd) {
        Ok(m) => m,
        Err(e) => {
            for d in steps {
                out.notes.push(format!(
                    "step {:?} stays whole: cargo metadata failed ({e})",
                    d.name
                ));
            }
            return out;
        }
    };
    for d in steps {
        match cargo_test_targets(&metadata, &d.command) {
            Ok(list) if list.is_empty() => out.notes.push(format!(
                "step {:?} stays whole: its command selects no test target",
                d.name
            )),
            Ok(list) => {
                out.executables.insert(d.name.clone(), list);
            }
            Err(e) => out
                .notes
                .push(format!("step {:?} stays whole: {e}", d.name)),
        }
    }
    out
}

/// What the command's flags select.
#[derive(Default)]
struct Selection {
    packages: Vec<String>,
    exclude: Vec<String>,
    workspace: bool,
    lib: bool,
    bins: bool,
    bin_names: Vec<String>,
    tests: bool,
    test_names: Vec<String>,
    other_targets: bool,
    doc: bool,
    features: Vec<String>,
    all_features: bool,
    no_default: bool,
}

impl Selection {
    fn explicit_targets(&self) -> bool {
        self.lib
            || self.bins
            || !self.bin_names.is_empty()
            || self.tests
            || !self.test_names.is_empty()
            || self.other_targets
            || self.doc
    }
}

fn parse_command(command: &[String]) -> Result<Selection, String> {
    let mut s = Selection::default();
    let end = command
        .iter()
        .position(|a| a == "--")
        .unwrap_or(command.len());
    let words = command.get(..end).unwrap_or(command);
    let mut it = words.iter();
    // value of `--flag VALUE` or `--flag=VALUE`
    let value = |it: &mut std::slice::Iter<'_, String>, a: &str, flag: &str| -> Option<String> {
        if a == flag {
            it.next().cloned()
        } else {
            a.strip_prefix(&format!("{flag}=")).map(str::to_string)
        }
    };
    while let Some(a) = it.next() {
        let a = a.as_str();
        if a == "--manifest-path" || a.starts_with("--manifest-path=") {
            return Err("it uses --manifest-path".to_string());
        }
        match a {
            "--workspace" | "--all" => s.workspace = true,
            "--lib" => s.lib = true,
            "--bins" => s.bins = true,
            "--tests" => s.tests = true,
            "--benches" | "--examples" | "--all-targets" => s.other_targets = true,
            "--doc" => s.doc = true,
            "--all-features" => s.all_features = true,
            "--no-default-features" => s.no_default = true,
            _ => {
                if let Some(v) = value(&mut it, a, "-p").or_else(|| value(&mut it, a, "--package"))
                {
                    s.packages.push(v);
                } else if let Some(v) = value(&mut it, a, "--exclude") {
                    s.exclude.push(v);
                } else if let Some(v) = value(&mut it, a, "--bin") {
                    s.bin_names.push(v);
                } else if let Some(v) = value(&mut it, a, "--test") {
                    s.test_names.push(v);
                } else if let Some(v) =
                    value(&mut it, a, "--features").or_else(|| value(&mut it, a, "-F"))
                {
                    s.features.extend(
                        v.split([',', ' '])
                            .filter(|f| !f.is_empty())
                            .map(str::to_string),
                    );
                }
            }
        }
    }
    Ok(s)
}

fn strs(v: Option<&Value>) -> Vec<&str> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// The features switched on by the command, with what they imply inside the package.
fn enabled_features(pkg: &Value, sel: &Selection) -> BTreeSet<String> {
    let table = pkg.get("features").and_then(Value::as_object);
    let mut on: BTreeSet<String> = BTreeSet::new();
    let mut work: Vec<String> = Vec::new();
    if sel.all_features {
        work.extend(
            table
                .map(|t| t.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
        );
    } else {
        if !sel.no_default {
            work.push("default".to_string());
        }
        for f in &sel.features {
            // `pkg/feature` names a feature of one package
            let name = f.rsplit_once('/').map_or(f.as_str(), |(_, n)| n);
            work.push(name.to_string());
        }
    }
    while let Some(f) = work.pop() {
        if !on.insert(f.clone()) {
            continue;
        }
        for item in strs(table.and_then(|t| t.get(&f))) {
            // `dep:x` and `x/y` reach other packages; a plain name is a feature of this one
            if !item.contains(':') && !item.contains('/') {
                work.push(item.to_string());
            }
        }
    }
    on
}

fn lib_kind(kinds: &[&str]) -> bool {
    kinds.iter().any(|k| {
        matches!(
            *k,
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
        )
    })
}

/// The test targets `command` (a `cargo test ...` argv) builds, from the JSON of
/// `cargo metadata --no-deps --format-version 1`.
///
/// Packages are those of `-p`/`--package`, else the whole workspace with `--workspace`, else the
/// workspace's default members, less `--exclude`. Targets are the explicit `--lib`, `--bin`,
/// `--test` selections when there are any (so a step that names its tests gets only those), else
/// every target `cargo test` runs by default. A target whose `required-features` the command does
/// not switch on is left out, as cargo leaves it out. Doc tests are not targets.
pub fn cargo_test_targets(metadata: &str, command: &[String]) -> Result<Vec<TestExe>, String> {
    let sel = parse_command(command)?;
    let meta: Value = serde_json::from_str(metadata).map_err(|e| format!("bad metadata: {e}"))?;
    let packages = meta
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("metadata has no packages")?;
    let name_of = |p: &Value| {
        p.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let id_of = |p: &Value| {
        p.get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let members: BTreeSet<String> = strs(meta.get("workspace_members"))
        .into_iter()
        .map(str::to_string)
        .collect();
    let defaults: BTreeSet<String> = {
        let d = strs(meta.get("workspace_default_members"));
        if d.is_empty() {
            members.clone()
        } else {
            d.into_iter().map(str::to_string).collect()
        }
    };
    let selected: Vec<&Value> = if !sel.packages.is_empty() {
        let mut v = Vec::new();
        for want in &sel.packages {
            let p = packages
                .iter()
                .find(|p| &name_of(p) == want)
                .ok_or_else(|| format!("package {want:?} is not in this workspace"))?;
            v.push(p);
        }
        v
    } else if sel.workspace {
        packages
            .iter()
            .filter(|p| members.contains(&id_of(p)))
            .collect()
    } else {
        packages
            .iter()
            .filter(|p| defaults.contains(&id_of(p)))
            .collect()
    };
    let selected = selected
        .into_iter()
        .filter(|p| !sel.exclude.contains(&name_of(p)));

    let explicit = sel.explicit_targets();
    let mut out: Vec<TestExe> = Vec::new();
    for pkg in selected {
        let on = enabled_features(pkg, &sel);
        let targets = pkg.get("targets").and_then(Value::as_array);
        for t in targets.map(Vec::as_slice).unwrap_or_default() {
            let kinds = strs(t.get("kind"));
            let name = t.get("name").and_then(Value::as_str).unwrap_or("");
            let harness = t.get("test").and_then(Value::as_bool).unwrap_or(false);
            let (kind, wanted) = if lib_kind(&kinds) {
                (ExeKind::Lib, !explicit || sel.lib || sel.tests)
            } else if kinds.contains(&"bin") {
                (
                    ExeKind::Bin,
                    !explicit || sel.bins || sel.tests || sel.bin_names.iter().any(|n| n == name),
                )
            } else if kinds.contains(&"test") {
                (
                    ExeKind::Test,
                    !explicit || sel.tests || sel.test_names.iter().any(|n| n == name),
                )
            } else if kinds.contains(&"bench") {
                (ExeKind::Bench, sel.other_targets)
            } else if kinds.contains(&"example") {
                (ExeKind::Example, sel.other_targets)
            } else {
                continue;
            };
            // a name given by --test/--bin is run even where the default would not run it
            let named = match kind {
                ExeKind::Test => sel.test_names.iter().any(|n| n == name),
                ExeKind::Bin => sel.bin_names.iter().any(|n| n == name),
                _ => false,
            };
            if !wanted || !(harness || named) {
                continue;
            }
            if !strs(t.get("required-features"))
                .iter()
                .all(|f| on.contains(*f))
            {
                continue;
            }
            out.push(TestExe {
                package: name_of(pkg),
                name: name.to_string(),
                kind,
                path: String::new(),
            });
        }
    }
    // a --test name that matched nothing is a typo cargo would reject; let it fail there
    for n in &sel.test_names {
        if !out.iter().any(|e| e.kind == ExeKind::Test && &e.name == n) {
            return Err(format!(
                "no test target named {n:?} in the selected packages"
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// p2pcore has a lib, three tests (`calls` needs `dev-tools`) and a bin; `tool` is a second
    /// member with a lib and one test; `xtask` is outside the default members.
    const META: &str = r#"{
      "workspace_members": ["p2pcore 0.1.0", "tool 0.2.0", "xtask 0.1.0"],
      "workspace_default_members": ["p2pcore 0.1.0", "tool 0.2.0"],
      "packages": [
        {"name":"p2pcore","id":"p2pcore 0.1.0",
         "features":{"default":[],"dev-tools":["testing"],"testing":[]},
         "targets":[
           {"name":"p2pcore","kind":["lib"],"test":true},
           {"name":"calls","kind":["test"],"test":true,"required-features":["dev-tools"]},
           {"name":"vault","kind":["test"],"test":true},
           {"name":"lifecycle","kind":["test"],"test":true,"required-features":["testing"]},
           {"name":"p2pd","kind":["bin"],"test":true},
           {"name":"demo","kind":["example"],"test":false},
           {"name":"speed","kind":["bench"],"test":false}]},
        {"name":"tool","id":"tool 0.2.0","features":{},
         "targets":[
           {"name":"tool","kind":["lib","cdylib"],"test":true},
           {"name":"cli","kind":["test"],"test":true}]},
        {"name":"xtask","id":"xtask 0.1.0","features":{},
         "targets":[{"name":"xtask","kind":["bin"],"test":true}]}
      ]}"#;

    fn cmd(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }
    fn labels(r: Result<Vec<TestExe>, String>) -> Vec<String> {
        r.unwrap().iter().map(TestExe::label).collect()
    }

    // R-704
    #[test]
    fn r704_a_package_gives_its_lib_integration_tests_and_bins_and_leaves_out_features_not_on() {
        let l = labels(cargo_test_targets(META, &cmd("cargo test -p p2pcore")));
        assert_eq!(l, ["lib:p2pcore", "test:vault", "bin:p2pd"]);
    }

    // R-704
    #[test]
    fn r704_features_switch_on_the_targets_that_need_them_and_what_they_imply() {
        let l = labels(cargo_test_targets(
            META,
            &cmd("cargo test --release -p p2pcore --features dev-tools --no-fail-fast"),
        ));
        // `dev-tools` implies `testing`, which lifecycle needs
        assert_eq!(
            l,
            [
                "lib:p2pcore",
                "test:calls",
                "test:vault",
                "test:lifecycle",
                "bin:p2pd"
            ]
        );
        let l = labels(cargo_test_targets(
            META,
            &cmd("cargo test -p p2pcore --all-features --lib"),
        ));
        assert_eq!(l, ["lib:p2pcore"]);
    }

    // R-704
    #[test]
    fn r704_no_package_flag_means_the_default_members_and_workspace_means_all() {
        let l = labels(cargo_test_targets(META, &cmd("cargo test")));
        assert_eq!(
            l,
            [
                "lib:p2pcore",
                "test:vault",
                "bin:p2pd",
                "lib:tool",
                "test:cli"
            ]
        );
        let l = labels(cargo_test_targets(
            META,
            &cmd("cargo test --workspace --exclude p2pcore --exclude tool"),
        ));
        assert_eq!(l, ["bin:xtask"]);
    }

    // R-705
    #[test]
    fn r705_an_explicit_test_list_gives_only_those_tests() {
        let l = labels(cargo_test_targets(
            META,
            &cmd("cargo test -p p2pcore --features dev-tools --test calls --test vault"),
        ));
        assert_eq!(l, ["test:calls", "test:vault"]);
        let l = labels(cargo_test_targets(
            META,
            &cmd("cargo test -p p2pcore --lib --bin p2pd"),
        ));
        assert_eq!(l, ["lib:p2pcore", "bin:p2pd"]);
        let l = labels(cargo_test_targets(
            META,
            &cmd("cargo test -p p2pcore --tests"),
        ));
        assert_eq!(l, ["lib:p2pcore", "test:vault", "bin:p2pd"]);
    }

    // R-705
    #[test]
    fn r705_what_cannot_be_split_is_said_not_guessed() {
        let e = cargo_test_targets(META, &cmd("cargo test -p nope")).unwrap_err();
        assert!(e.contains("nope"), "{e}");
        let e = cargo_test_targets(META, &cmd("cargo test -p p2pcore --test typo")).unwrap_err();
        assert!(e.contains("typo"), "{e}");
        let e =
            cargo_test_targets(META, &cmd("cargo test --manifest-path x/Cargo.toml")).unwrap_err();
        assert!(e.contains("manifest-path"), "{e}");
        // doc tests are not executables: nothing to split into
        assert!(
            cargo_test_targets(META, &cmd("cargo test -p p2pcore --doc"))
                .unwrap()
                .is_empty()
        );
        // arguments after `--` belong to the test binaries
        let l = labels(cargo_test_targets(
            META,
            &cmd("cargo test -p tool --lib -- --test-threads=1 -p x"),
        ));
        assert_eq!(l, ["lib:tool"]);
        assert!(cargo_test_targets("not json", &cmd("cargo test")).is_err());
    }
}
