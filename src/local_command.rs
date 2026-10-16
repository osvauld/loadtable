//! Explicit local command contract, NOT the immutable source-suite contract.
//! argv/cwd are durable; workspace contents are live and trusted, not snapshotted.
use crate::{Cost, JobSpec, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
pub const SUITE: &str = "local.command";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Compile,
    Run,
    Device(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub argv: Vec<String>,
    pub resource: Resource,
}
impl Recipe {
    pub fn validate(&self) -> Result<()> {
        if self.argv.is_empty()
            || self.argv.len() > 256
            || self.argv[0].is_empty()
            || self.argv.iter().any(|s| s.contains('\0'))
            || serde_json::to_vec(self)?.len() > 16384
        {
            return Err("invalid/bounded command argv required".into());
        }
        if self.argv.iter().any(|s| {
            Path::new(s)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| ["buildlock.py", "build_android.py"].contains(&n))
        }) {
            return Err(
                "do not nest admission helpers; use direct Cargo/Gradle or typed Android trial"
                    .into(),
            );
        }
        if let Some(index) = self.cargo_index() {
            for assignment in &self.argv[..index] {
                if let Some(value) = assignment.strip_prefix("CARGO_BUILD_JOBS=") {
                    validate_jobs(value)?;
                }
            }
            let mut args = self.argv[index + 1..].iter();
            while let Some(arg) = args.next() {
                if arg == "--" {
                    break;
                }
                if arg == "-j" || arg == "--jobs" {
                    validate_jobs(args.next().ok_or("Cargo jobs requires a value")?)?;
                } else if let Some(value) = arg
                    .strip_prefix("--jobs=")
                    .or_else(|| arg.strip_prefix("-j"))
                {
                    validate_jobs(value)?;
                }
            }
        }
        if let Resource::Device(s) = &self.resource
            && (s.is_empty()
                || s.len() > 128
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)))
        {
            return Err("invalid device serial".into());
        }
        Ok(())
    }
    pub fn from_spec(spec: &JobSpec) -> Result<Self> {
        if spec.suite != SUITE
            || spec.revision != "live-workspace"
            || !Path::new(&spec.source).is_absolute()
        {
            return Err("invalid live command contract".into());
        }
        let r: Self = serde_json::from_str(&spec.configuration)?;
        r.validate()?;
        Ok(r)
    }
    pub fn spec(&self, owner: String, cwd: PathBuf) -> Result<JobSpec> {
        self.validate()?;
        Ok(JobSpec {
            suite: SUITE.into(),
            configuration: serde_json::to_string(self)?,
            cost: Cost::CpuSimulation,
            owner,
            source: cwd
                .into_os_string()
                .into_string()
                .map_err(|_| "UTF-8 cwd required")?,
            revision: "live-workspace".into(),
        })
    }
    pub fn stages(&self) -> Vec<&'static str> {
        match self.resource {
            Resource::Compile if self.compile_step().is_some() => vec!["build", "run"],
            Resource::Compile => vec!["build"],
            _ => vec!["run"],
        }
    }
    pub fn argv_for(&self, stage: &str) -> Result<Vec<String>> {
        if !self.stages().contains(&stage) {
            return Err("command stage outside approved recipe".into());
        }
        Ok(if stage == "build" {
            self.compile_step().unwrap_or_else(|| self.argv.clone())
        } else {
            self.argv.clone()
        })
    }
    // Only transparent env assignments are understood; never parse shell text.
    fn cargo_index(&self) -> Option<usize> {
        let mut index = 0;
        if Path::new(self.argv.first()?).file_name()?.to_str()? == "env" {
            index = 1;
            if self.argv.get(index)?.as_str() == "--" {
                index += 1;
            }
            while self.argv.get(index)?.contains('=') {
                index += 1;
            }
        }
        (Path::new(self.argv.get(index)?).file_name()?.to_str()? == "cargo").then_some(index)
    }
    fn compile_step(&self) -> Option<Vec<String>> {
        let index = self.cargo_index()?;
        let sub = self
            .argv
            .iter()
            .enumerate()
            .skip(index + 1)
            .find(|(_, s)| !s.starts_with('+'))?
            .0;
        if self.argv[sub] != "test" || self.argv.iter().any(|s| s == "--no-run") {
            return None;
        }
        let mut out = self.argv[..=sub].to_vec();
        let mut args = self.argv[sub + 1..].iter();
        while let Some(a) = args.next() {
            if a == "--" {
                break;
            }
            if [
                "-p",
                "--package",
                "--test",
                "--bench",
                "--bin",
                "--example",
                "--features",
                "-F",
                "--target",
                "--target-dir",
                "--profile",
                "-j",
                "--jobs",
                "--manifest-path",
                "--exclude",
                "--color",
                "--message-format",
                "-Z",
                "--config",
            ]
            .contains(&a.as_str())
            {
                out.push(a.clone());
                if let Some(v) = args.next() {
                    out.push(v.clone());
                }
            } else if a.starts_with('-') {
                out.push(a.clone());
            }
        }
        out.push("--no-run".into());
        Some(out)
    }
}
fn validate_jobs(value: &str) -> Result<()> {
    match value.parse::<u32>() {
        Ok(1 | 2) => Ok(()),
        _ => Err("trial safety: Cargo jobs must be 1 or 2 (default 2)".into()),
    }
}
/// Output links handled by one placement call. Existing entries are never
/// replaced: real directories/files stay local and are reported, not migrated.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Placement {
    /// Newly created cache-backed links.
    pub linked: Vec<PathBuf>,
    /// Existing non-link entries left untouched (NOT cache-backed).
    pub preserved: Vec<PathBuf>,
}
impl Placement {
    pub fn detail(&self) -> String {
        let mut out = String::new();
        for path in &self.linked {
            out += &format!("cache output link placed: {}\n", path.display());
        }
        for path in &self.preserved {
            out += &format!(
                "existing local output preserved, not cache-backed: {}\n",
                path.display()
            );
        }
        out
    }
}
/// Same non-destructive output placement as the old helper, but namespaced by
/// full canonical checkout path to avoid colliding same-named worktrees.
/// An independent Cargo workspace nested below the Git root (the cwd's own
/// workspace root) additionally gets its own target link, keyed by its full
/// canonical path. No environment (CARGO_TARGET_DIR etc.) is set or changed.
#[cfg(target_os = "linux")]
pub fn place_outputs(cwd: &Path) -> Result<Placement> {
    place_outputs_in(cwd, || {
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME required")?);
        let cache = home.join("build-cache");
        crate::cache::require_space(&[&cache])?;
        Ok(cache)
    })
}
#[cfg(target_os = "linux")]
fn place_outputs_in(
    cwd: &Path,
    space_checked_cache: impl FnOnce() -> Result<PathBuf>,
) -> Result<Placement> {
    let mut placement = Placement::default();
    let result = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()?;
    if !result.status.success() {
        return Ok(placement);
    }
    let root = std::fs::canonicalize(String::from_utf8(result.stdout)?.trim())?;
    let root_cargo = root.join("Cargo.toml").is_file();
    let nested = cargo_workspace_root(&std::fs::canonicalize(cwd)?, &root)
        .filter(|workspace| *workspace != root);
    if !root_cargo && nested.is_none() {
        return Ok(placement);
    }
    let cache = space_checked_cache()?;
    if root_cargo {
        place_links(
            &root,
            &cache,
            &["target", "android/build", "android/app/build"],
            &mut placement,
        )?;
    }
    if let Some(workspace) = nested {
        // Android outputs remain a Git-root convention; nested gets target only.
        place_links(&workspace, &cache, &["target"], &mut placement)?;
    }
    Ok(placement)
}
#[cfg(target_os = "linux")]
fn place_links(root: &Path, cache: &Path, rels: &[&str], placement: &mut Placement) -> Result<()> {
    use sha2::{Digest, Sha256};
    let key = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
    for rel in rels {
        let link = root.join(rel);
        match std::fs::symlink_metadata(&link) {
            // Never replace, move or delete an existing entry.
            Ok(meta) => {
                if !meta.file_type().is_symlink() {
                    placement.preserved.push(link);
                }
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        if !link.parent().is_some_and(Path::is_dir) {
            continue;
        }
        let real = cache.join("p2p_chat/tasks/checkouts").join(&key).join(rel);
        std::fs::create_dir_all(&real)?;
        match std::os::unix::fs::symlink(real, &link) {
            Ok(()) => placement.linked.push(link),
            // A concurrent placement won; its entry is kept as-is.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
#[cfg(target_os = "linux")]
/// Conservative model of Cargo's workspace-root discovery for a canonical cwd,
/// bounded to the Git root: nearest manifest; its own `[workspace]`; an explicit
/// `package.workspace`; else the nearest ancestor `[workspace]` that does not
/// exclude it; else the package itself. Any unreadable/unparseable manifest
/// yields None (no nested placement), leaving diagnosis to Cargo.
fn cargo_workspace_root(cwd: &Path, git_root: &Path) -> Option<PathBuf> {
    let within = |path: &Path| path.starts_with(git_root);
    let manifest = |dir: &Path| -> Option<toml::Table> {
        toml::from_str(&std::fs::read_to_string(dir.join("Cargo.toml")).ok()?).ok()
    };
    let package = cwd
        .ancestors()
        .take_while(|dir| within(dir))
        .find(|dir| dir.join("Cargo.toml").is_file())?;
    let own = manifest(package)?;
    if own.contains_key("workspace") {
        return Some(package.into());
    }
    if let Some(pointer) = own
        .get("package")
        .and_then(|p| p.get("workspace"))
        .and_then(|w| w.as_str())
    {
        let workspace = std::fs::canonicalize(package.join(pointer)).ok()?;
        return within(&workspace).then_some(workspace);
    }
    for dir in package.ancestors().skip(1).take_while(|dir| within(dir)) {
        if !dir.join("Cargo.toml").is_file() {
            continue;
        }
        let Some(workspace) = manifest(dir)?.get("workspace").cloned() else {
            continue;
        };
        let excluded = workspace
            .get("exclude")
            .and_then(|e| e.as_array())
            .is_some_and(|list| {
                list.iter()
                    .filter_map(|e| e.as_str())
                    .any(|e| package.starts_with(dir.join(e)))
            });
        if !excluded {
            return Some(dir.into());
        }
    }
    Some(package.into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cargo_parallelism_is_bounded_for_direct_and_transparent_env_commands() {
        for prefix in [vec!["cargo"], vec!["env", "VPX_DIR=/synthetic", "cargo"]] {
            for flags in [
                vec!["-j", "3"],
                vec!["--jobs=8"],
                vec!["-j0"],
                vec!["--jobs", "-1"],
            ] {
                let argv = prefix
                    .iter()
                    .chain(["test"].iter())
                    .chain(flags.iter())
                    .map(|s| s.to_string())
                    .collect();
                assert!(
                    Recipe {
                        argv,
                        resource: Resource::Compile
                    }
                    .validate()
                    .is_err()
                );
            }
        }
        let recipe = Recipe {
            argv: [
                "env",
                "VPX_DIR=/synthetic",
                "cargo",
                "test",
                "--jobs",
                "2",
                "some_filter",
            ]
            .map(String::from)
            .to_vec(),
            resource: Resource::Compile,
        };
        recipe.validate().unwrap();
        assert_eq!(recipe.stages(), vec!["build", "run"]);
        assert_eq!(
            recipe.argv_for("build").unwrap(),
            [
                "env",
                "VPX_DIR=/synthetic",
                "cargo",
                "test",
                "--jobs",
                "2",
                "--no-run"
            ]
        );
        let mut override_recipe = recipe.clone();
        override_recipe.argv.insert(1, "CARGO_BUILD_JOBS=32".into());
        assert!(override_recipe.validate().is_err());
        // Explicit trusted shell commands aren't parsed as Cargo commands.
        let shell = Recipe {
            argv: vec!["sh".into(), "-c".into(), "printf synthetic".into()],
            resource: Resource::Compile,
        };
        shell.validate().unwrap();
        assert_eq!(shell.stages(), vec!["build"]);
    }
    #[test]
    fn explicit_command_approval_is_required_and_completion_has_no_snapshot_claim() {
        use crate::{
            Phase, Store,
            execution::{ProcessIdentity, StageTiming},
        };
        let r = Recipe {
            argv: vec!["true".into()],
            resource: Resource::Run,
        };
        let mut s = Store::memory().unwrap();
        let j = s
            .submit(&r.spec("synthetic".into(), "/tmp".into()).unwrap())
            .unwrap();
        assert!(s.approve(j.id).is_err());
        assert!(s.approve_android_trial(j.id).is_err());
        s.approve_command(j.id).unwrap();
        assert!(s.approve_command(j.id).is_err());
        let job = s
            .claim(ProcessIdentity {
                pid: 1,
                start_ticks: 1,
            })
            .unwrap()
            .unwrap();
        crate::catalog::validate_claimed(&job).unwrap();
        let mut e = job.execution.unwrap();
        assert!(e.local_command);
        assert!(e.snapshot.is_none());
        e.stages.push(StageTiming {
            stage: "run".into(),
            seconds: Some(1.0),
            wait_seconds: 0.0,
            exit_code: Some(0),
            signal: None,
        });
        s.update_execution(j.id, Phase::Running, e).unwrap();
        s.complete_success(j.id).unwrap();
        let job = s.job(j.id).unwrap();
        assert_eq!(job.phase, Phase::Passed);
        assert!(job.execution.unwrap().detail.contains("no source snapshot"));
    }
    #[test]
    fn nested_legacy_admission_is_rejected() {
        let r = Recipe {
            argv: vec!["python3".into(), "scripts/build_android.py".into()],
            resource: Resource::Compile,
        };
        assert!(r.validate().is_err());
    }
    #[test]
    fn cargo_test_has_distinct_compile_and_run_without_filter_rewrite() {
        let r = Recipe {
            argv: [
                "cargo",
                "+1.99.0",
                "test",
                "-p",
                "core",
                "filter",
                "--",
                "--test-threads=1",
            ]
            .map(String::from)
            .to_vec(),
            resource: Resource::Compile,
        };
        assert_eq!(r.stages(), ["build", "run"]);
        assert_eq!(
            r.argv_for("build").unwrap(),
            ["cargo", "+1.99.0", "test", "-p", "core", "--no-run"]
        );
        assert_eq!(r.argv_for("run").unwrap(), r.argv);
    }
    #[test]
    fn recipes_reject_invalid_argv_and_do_not_claim_source_revision() {
        let r = Recipe {
            argv: vec!["true".into()],
            resource: Resource::Run,
        };
        let s = r.spec("synthetic".into(), "/tmp".into()).unwrap();
        assert_eq!(s.revision, "live-workspace");
        assert_eq!(Recipe::from_spec(&s).unwrap().stages(), ["run"]);
        assert!(
            Recipe {
                argv: vec![],
                resource: Resource::Compile
            }
            .validate()
            .is_err()
        );
        assert!(
            Recipe {
                argv: vec!["bad\0".into()],
                resource: Resource::Compile
            }
            .validate()
            .is_err()
        );
    }
    #[cfg(target_os = "linux")]
    mod placement {
        use super::super::*;
        use sha2::{Digest, Sha256};
        use std::fs;
        use std::os::unix::fs::MetadataExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        /// Synthetic fixture: a fresh Git checkout plus a private cache root.
        struct Fixture {
            base: PathBuf,
            cache: PathBuf,
        }
        impl Fixture {
            fn new() -> Self {
                let base = std::env::temp_dir().join(format!(
                    "tasks-placement-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir(&base).unwrap();
                let base = fs::canonicalize(base).unwrap();
                let cache = base.join("cache");
                fs::create_dir(&cache).unwrap();
                Self { base, cache }
            }
            fn checkout(&self, name: &str) -> PathBuf {
                let root = self.base.join(name);
                fs::create_dir_all(&root).unwrap();
                let status = std::process::Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(&root)
                    .status()
                    .unwrap();
                assert!(status.success());
                root
            }
            fn put(&self, path: PathBuf, text: &str) -> PathBuf {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, text).unwrap();
                path
            }
            fn place(&self, cwd: &Path) -> Placement {
                place_outputs_in(cwd, || Ok(self.cache.clone())).unwrap()
            }
            fn cached(&self, root: &Path, rel: &str) -> PathBuf {
                let key = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
                self.cache
                    .join("p2p_chat/tasks/checkouts")
                    .join(key)
                    .join(rel)
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.base);
            }
        }
        const WORKSPACE: &str = "[workspace]\n";
        const PACKAGE: &str = "[package]\nname = \"synthetic\"\nversion = \"0.1.0\"\n";
        fn link(path: &Path) -> PathBuf {
            fs::read_link(path).unwrap()
        }
        #[test]
        fn root_workspace_placement_is_unchanged() {
            let f = Fixture::new();
            let root = f.checkout("root");
            f.put(root.join("Cargo.toml"), WORKSPACE);
            fs::create_dir_all(root.join("src")).unwrap();
            fs::create_dir_all(root.join("android/app")).unwrap();
            let placed = f.place(&root.join("src"));
            assert_eq!(
                placed.linked,
                [
                    root.join("target"),
                    root.join("android/build"),
                    root.join("android/app/build")
                ]
            );
            assert!(placed.preserved.is_empty());
            for rel in ["target", "android/build", "android/app/build"] {
                assert_eq!(link(&root.join(rel)), f.cached(&root, rel));
                assert!(f.cached(&root, rel).is_dir());
            }
            // Idempotent: existing links are neither replaced nor reported.
            assert_eq!(f.place(&root), Placement::default());
        }
        #[test]
        fn nested_independent_workspace_gets_its_own_cache_target_for_any_argv() {
            let f = Fixture::new();
            let root = f.checkout("root");
            f.put(root.join("Cargo.toml"), WORKSPACE);
            let nested = root.join("experiments/independent");
            f.put(nested.join("Cargo.toml"), &format!("{PACKAGE}{WORKSPACE}"));
            fs::create_dir_all(nested.join("tests")).unwrap();
            // Placement is keyed on the durable live cwd only, so direct Cargo
            // and compound shell argv are placed identically.
            for (index, argv) in [
                vec!["cargo", "test", "--offline", "-j", "2"],
                vec![
                    "sh",
                    "-c",
                    "cargo test --offline -j 2 && cargo clippy --offline -j 2",
                ],
            ]
            .into_iter()
            .enumerate()
            {
                let recipe = Recipe {
                    argv: argv.into_iter().map(String::from).collect(),
                    resource: Resource::Compile,
                };
                let spec = recipe
                    .spec("synthetic".into(), nested.join("tests"))
                    .unwrap();
                assert_eq!(Recipe::from_spec(&spec).unwrap().argv, recipe.argv);
                let placed = f.place(Path::new(&spec.source));
                if index == 0 {
                    assert_eq!(placed.linked, [root.join("target"), nested.join("target")]);
                } else {
                    assert_eq!(placed, Placement::default());
                }
                assert_eq!(link(&root.join("target")), f.cached(&root, "target"));
                assert_eq!(link(&nested.join("target")), f.cached(&nested, "target"));
                assert_ne!(f.cached(&root, "target"), f.cached(&nested, "target"));
                assert!(f.cached(&nested, "target").is_dir());
            }
            // Root-only Android outputs are not invented for a nested workspace.
            assert!(fs::symlink_metadata(nested.join("android")).is_err());
        }
        #[test]
        fn nested_workspace_below_a_non_cargo_git_root_is_placed() {
            let f = Fixture::new();
            let root = f.checkout("root");
            let nested = root.join("rust");
            f.put(nested.join("Cargo.toml"), WORKSPACE);
            assert_eq!(f.place(&nested).linked, [nested.join("target")]);
            assert_eq!(link(&nested.join("target")), f.cached(&nested, "target"));
            assert!(fs::symlink_metadata(root.join("target")).is_err());
        }
        #[test]
        fn populated_existing_nested_target_is_preserved_and_reported() {
            let f = Fixture::new();
            let root = f.checkout("root");
            f.put(root.join("Cargo.toml"), WORKSPACE);
            let nested = root.join("experiments/independent");
            f.put(nested.join("Cargo.toml"), WORKSPACE);
            let marker = f.put(nested.join("target/debug/marker"), "synthetic output");
            let before = fs::symlink_metadata(nested.join("target")).unwrap();
            let placed = f.place(&nested);
            assert_eq!(placed.linked, [root.join("target")]);
            assert_eq!(placed.preserved, [nested.join("target")]);
            assert!(placed.detail().contains("preserved, not cache-backed"));
            let after = fs::symlink_metadata(nested.join("target")).unwrap();
            assert!(after.is_dir() && !after.file_type().is_symlink());
            assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
            assert_eq!(fs::read_to_string(marker).unwrap(), "synthetic output");
            assert!(!f.cached(&nested, "target").exists());
            // A populated root target is likewise left alone and reported.
            let f = Fixture::new();
            let root = f.checkout("root");
            f.put(root.join("Cargo.toml"), WORKSPACE);
            f.put(root.join("target/marker"), "synthetic output");
            let placed = f.place(&root);
            assert!(placed.linked.is_empty());
            assert_eq!(placed.preserved, [root.join("target")]);
            assert!(fs::symlink_metadata(root.join("target")).unwrap().is_dir());
        }
        #[test]
        fn member_package_uses_the_enclosing_workspace_target() {
            let f = Fixture::new();
            let root = f.checkout("root");
            f.put(
                root.join("Cargo.toml"),
                "[workspace]\nmembers = [\"crates/a\"]\n",
            );
            let member = root.join("crates/a");
            f.put(member.join("Cargo.toml"), PACKAGE);
            assert_eq!(f.place(&member).linked, [root.join("target")]);
            assert!(fs::symlink_metadata(member.join("target")).is_err());
            // An explicit package.workspace pointer is honoured as well.
            let pointed = root.join("crates/b");
            f.put(
                pointed.join("Cargo.toml"),
                &format!("{PACKAGE}workspace = \"../..\"\n"),
            );
            assert_eq!(f.place(&pointed), Placement::default());
            assert!(fs::symlink_metadata(pointed.join("target")).is_err());
        }
        #[test]
        fn excluded_or_standalone_package_is_its_own_workspace() {
            let f = Fixture::new();
            let root = f.checkout("root");
            f.put(
                root.join("Cargo.toml"),
                "[workspace]\nexclude = [\"tools/excluded\"]\n",
            );
            let excluded = root.join("tools/excluded");
            f.put(excluded.join("Cargo.toml"), PACKAGE);
            assert_eq!(
                f.place(&excluded).linked,
                [root.join("target"), excluded.join("target")]
            );
            assert_eq!(
                link(&excluded.join("target")),
                f.cached(&excluded, "target")
            );
            // Root manifest is a plain package (no workspace): nested package is
            // its own workspace root under Cargo's rules.
            let f = Fixture::new();
            let root = f.checkout("root");
            f.put(root.join("Cargo.toml"), PACKAGE);
            let nested = root.join("sub");
            f.put(nested.join("Cargo.toml"), PACKAGE);
            assert_eq!(
                f.place(&nested).linked,
                [root.join("target"), nested.join("target")]
            );
        }
        #[test]
        fn same_nested_layout_in_different_checkouts_is_isolated() {
            let f = Fixture::new();
            let mut targets = Vec::new();
            for name in ["one/checkout", "two/checkout"] {
                let root = f.checkout(name);
                f.put(root.join("Cargo.toml"), WORKSPACE);
                let nested = root.join("experiments/independent");
                f.put(nested.join("Cargo.toml"), WORKSPACE);
                f.place(&nested);
                targets.push(link(&root.join("target")));
                targets.push(link(&nested.join("target")));
            }
            let unique: std::collections::BTreeSet<_> = targets.iter().collect();
            assert_eq!(unique.len(), 4);
        }
        #[test]
        fn outside_git_or_without_manifests_nothing_is_placed() {
            let f = Fixture::new();
            let plain = f.base.join("not-git");
            f.put(plain.join("Cargo.toml"), WORKSPACE);
            let mut checked = false;
            let placed = place_outputs_in(&plain, || {
                checked = true;
                Ok(f.cache.clone())
            })
            .unwrap();
            assert_eq!(placed, Placement::default());
            assert!(!checked);
            let root = f.checkout("root");
            fs::create_dir_all(root.join("docs")).unwrap();
            assert_eq!(f.place(&root.join("docs")), Placement::default());
            // Unparseable manifests are left to Cargo; no nested placement.
            let broken = root.join("broken");
            f.put(broken.join("Cargo.toml"), "[workspace\n");
            assert_eq!(f.place(&broken), Placement::default());
            assert!(fs::read_dir(&f.cache).unwrap().next().is_none());
        }
    }
}
