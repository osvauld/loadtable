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
    /// Offload client slot (`p2p-offload-N.lock`): no CPU PSI gate, so only the
    /// clients in OFFLOAD_CLIENTS may use it. Stored as `"offload"`; releases
    /// without it refuse to approve/execute such a job (fail closed).
    Offload,
}
/// argv[0] basenames (after an optional `env NAME=value...` prefix) admitted to
/// the offload slot. A guard against accidental local heavy work, not a sandbox.
pub const OFFLOAD_CLIENTS: [&str; 2] = ["tasks-offload", "offload-portable-desktop.sh"];
/// True for a live command recorded with the offload resource. Unparseable
/// configurations are not offloads (they fail later, in the general pool).
pub fn is_offload(spec: &JobSpec) -> bool {
    spec.suite == SUITE
        && serde_json::from_str::<Recipe>(&spec.configuration)
            .is_ok_and(|r| r.resource == Resource::Offload)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub argv: Vec<String>,
    pub resource: Resource,
}
impl Recipe {
    /// Validates against this process's own `TASKS_MAX_CARGO_JOBS` (daemon
    /// approval and worker execution). Invalid configuration fails closed.
    pub fn validate(&self) -> Result<()> {
        self.check(|| Ok(crate::admission::thresholds()?.max_cargo_jobs))
    }
    /// Validates with an explicit Cargo jobs cap (1..=64).
    pub fn validate_with_max(&self, max_cargo_jobs: u64) -> Result<()> {
        self.check(|| Ok(max_cargo_jobs))
    }
    // The cap is resolved only for Cargo commands.
    fn check(&self, max_cargo_jobs: impl FnOnce() -> Result<u64>) -> Result<()> {
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
            let max = max_cargo_jobs()?;
            let jobs = |value: &str| validate_jobs(value, max);
            for assignment in &self.argv[..index] {
                if let Some(value) = assignment.strip_prefix("CARGO_BUILD_JOBS=") {
                    jobs(value)?;
                }
            }
            let mut args = self.argv[index + 1..].iter();
            while let Some(arg) = args.next() {
                if arg == "--" {
                    break;
                }
                if arg == "-j" || arg == "--jobs" {
                    jobs(args.next().ok_or("Cargo jobs requires a value")?)?;
                } else if let Some(value) = arg
                    .strip_prefix("--jobs=")
                    .or_else(|| arg.strip_prefix("-j"))
                {
                    jobs(value)?;
                }
            }
        }
        if self.resource == Resource::Offload && tool_index(&self.argv, &OFFLOAD_CLIENTS).is_none()
        {
            return Err(format!(
                "--offload admits only an offload client as the command ({}, optionally after \
                 `env NAME=value`): the offload slot skips CPU PSI, so it must not run local \
                 work. Use --run for already-built local commands or the default compile slot",
                OFFLOAD_CLIENTS.join(" or ")
            )
            .into());
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
    /// Client-side submission check. A client shell's environment is not
    /// forwarded, so the cap is the last worker's recorded value (else this
    /// process's own); daemon approval and the worker re-check their own.
    pub fn spec(&self, owner: String, cwd: PathBuf) -> Result<JobSpec> {
        self.check(crate::admission::submission_max_cargo_jobs)?;
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
        tool_index(&self.argv, &["cargo"])
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
fn validate_jobs(value: &str, max: u64) -> Result<()> {
    match value.parse::<u64>() {
        Ok(jobs) if (1..=max).contains(&jobs) => Ok(()),
        _ if max == crate::admission::DEFAULT_MAX_CARGO_JOBS => {
            Err("trial safety: Cargo jobs must be 1 or 2 (default 2)".into())
        }
        _ => Err(format!(
            "trial safety: Cargo jobs must be 1..={max} ({})",
            crate::admission::MAX_CARGO_JOBS_ENV
        )
        .into()),
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
    /// How the Git root was found when it was not by `git` itself, or why
    /// placement was skipped. Never an error: links are only an optimization.
    pub note: Option<String>,
}
impl Placement {
    pub fn detail(&self) -> String {
        let mut out = String::new();
        if let Some(note) = &self.note {
            out += &format!("output placement: {note}\n");
        }
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
/// What a placement would do, computed without creating anything.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputPlan {
    cache: PathBuf,
    /// (link in the checkout, cache-backed directory it will point to).
    new_links: Vec<(PathBuf, PathBuf)>,
    preserved: Vec<PathBuf>,
    note: Option<String>,
}
impl OutputPlan {
    /// The filesystems that will receive NEW output because of this job's
    /// placement: the cache disk when a new cache-backed link is needed, and
    /// each preserved local output directory (its own filesystem, which may
    /// not be the cache disk). Already-placed links need no reserve.
    pub fn reserve_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if !self.new_links.is_empty() {
            paths.push(self.cache.clone());
        }
        paths.extend(self.preserved.iter().cloned());
        paths
    }
    /// Create the planned links. An entry that appeared meanwhile is kept as-is.
    fn apply(self) -> Result<Placement> {
        let mut placement = Placement {
            preserved: self.preserved,
            note: self.note,
            ..Default::default()
        };
        for (link, real) in self.new_links {
            std::fs::create_dir_all(&real)?;
            match std::os::unix::fs::symlink(real, &link) {
                Ok(()) => placement.linked.push(link),
                // A concurrent placement won; its entry is kept as-is.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(placement)
    }
}
/// Result of one placement attempt under the disk reserve.
#[derive(Debug, PartialEq)]
pub enum Placed {
    Done(Placement),
    /// Nothing was created; this filesystem is below its reserve.
    Blocked(crate::cache::DiskSpace),
}
/// One non-waiting attempt with this process's cache root, `git` from PATH and
/// its effective cache reserve. Used by the worker's pre-admission wait.
#[cfg(target_os = "linux")]
pub fn try_place_outputs(cwd: &Path) -> Result<Placed> {
    try_place_outputs_in(
        cwd,
        &crate::cache::cache_root()?,
        "git".as_ref(),
        crate::cache::disk_space,
    )
}
/// Post-admission placement (normally a no-op: the worker already placed the
/// outputs before queueing). A reserve shortfall here means space dropped while
/// the job held/waited for its slot; it fails rather than wait while holding a
/// shared slot that other jobs may be able to use.
#[cfg(target_os = "linux")]
pub fn place_outputs(cwd: &Path) -> Result<Placement> {
    match try_place_outputs(cwd)? {
        Placed::Done(placement) => Ok(placement),
        Placed::Blocked(space) => Err(format!(
            "insufficient disk space: {} (dropped below the reserve after queue admission; \
             resubmit to wait for space before admission)",
            space.detail()
        )
        .into()),
    }
}
/// Plan, then require the reserve only on filesystems that receive new output
/// (see [`OutputPlan::reserve_paths`]), then create the links.
#[cfg(target_os = "linux")]
pub fn try_place_outputs_in(
    cwd: &Path,
    cache: &Path,
    git: &std::ffi::OsStr,
    mut probe: impl FnMut(&Path) -> Result<crate::cache::DiskSpace>,
) -> Result<Placed> {
    let plan = plan_outputs(cwd, cache, git)?;
    for path in plan.reserve_paths() {
        let space = probe(&path)?;
        if !space.admitted() {
            return Ok(Placed::Blocked(space));
        }
    }
    Ok(Placed::Done(plan.apply()?))
}
/// Repeat `attempt` until it places (Some) or `on_wait` declines to keep
/// waiting (None, e.g. cancellation). `on_wait` owns logging and the sleep.
pub fn wait_for_placement(
    mut attempt: impl FnMut() -> Result<Placed>,
    mut on_wait: impl FnMut(&crate::cache::DiskSpace) -> Result<bool>,
) -> Result<Option<Placement>> {
    loop {
        match attempt()? {
            Placed::Done(placement) => return Ok(Some(placement)),
            Placed::Blocked(space) => {
                if !on_wait(&space)? {
                    return Ok(None);
                }
            }
        }
    }
}
/// The Git worktree root for `cwd`, by `git rev-parse --show-toplevel`. If git
/// is not installed, falls back to the nearest ancestor holding a `.git` entry
/// (directory, or file for linked worktrees/submodules), which is the same
/// top level for ordinary checkouts. `Ok(None)` outside any Git checkout.
#[cfg(target_os = "linux")]
fn git_root(cwd: &Path, git: &std::ffi::OsStr) -> Result<(Option<PathBuf>, Option<String>)> {
    // A missing cwd would otherwise surface as a bare ENOENT from the spawn.
    if !cwd.is_dir() {
        return Err(format!("live cwd is not an existing directory: {}", cwd.display()).into());
    }
    match std::process::Command::new(git)
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(result) if result.status.success() => Ok((
            Some(std::fs::canonicalize(
                String::from_utf8(result.stdout)?.trim(),
            )?),
            None,
        )),
        Ok(_) => Ok((None, None)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cwd = std::fs::canonicalize(cwd)?;
            let root = cwd
                .ancestors()
                .find(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok())
                .map(Path::to_path_buf);
            let note = match &root {
                Some(root) => format!(
                    "git not found on the worker PATH; Git root taken from its .git entry: {}",
                    root.display()
                ),
                None => "git not found on the worker PATH and no .git entry above the cwd; \
                         placement skipped"
                    .into(),
            };
            Ok((root, Some(note)))
        }
        Err(e) => Err(format!(
            "output placement could not run `{} rev-parse` in {}: {e}",
            git.to_string_lossy(),
            cwd.display()
        )
        .into()),
    }
}
/// Same non-destructive output placement as the old helper, but namespaced by
/// full canonical checkout path to avoid colliding same-named worktrees.
/// An independent Cargo workspace nested below the Git root (the cwd's own
/// workspace root) additionally gets its own target link, keyed by its full
/// canonical path. No environment (CARGO_TARGET_DIR etc.) is set or changed.
#[cfg(target_os = "linux")]
fn plan_outputs(cwd: &Path, cache: &Path, git: &std::ffi::OsStr) -> Result<OutputPlan> {
    let mut plan = OutputPlan {
        cache: cache.to_path_buf(),
        ..Default::default()
    };
    let (root, note) = git_root(cwd, git)?;
    plan.note = note;
    let Some(root) = root else {
        return Ok(plan);
    };
    let root_cargo = root.join("Cargo.toml").is_file();
    let nested = cargo_workspace_root(&std::fs::canonicalize(cwd)?, &root)
        .filter(|workspace| *workspace != root);
    if root_cargo {
        plan_links(
            &root,
            &["target", "android/build", "android/app/build"],
            &mut plan,
        )?;
    }
    if let Some(workspace) = nested {
        // Android outputs remain a Git-root convention; nested gets target only.
        plan_links(&workspace, &["target"], &mut plan)?;
    }
    Ok(plan)
}
#[cfg(target_os = "linux")]
fn plan_links(root: &Path, rels: &[&str], plan: &mut OutputPlan) -> Result<()> {
    use sha2::{Digest, Sha256};
    let key = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
    for rel in rels {
        let link = root.join(rel);
        match std::fs::symlink_metadata(&link) {
            // Never replace, move or delete an existing entry.
            Ok(meta) => {
                if !meta.file_type().is_symlink() {
                    plan.preserved.push(link);
                }
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        if !link.parent().is_some_and(Path::is_dir) {
            continue;
        }
        let real = plan
            .cache
            .join("p2p_chat/tasks/checkouts")
            .join(&key)
            .join(rel);
        plan.new_links.push((link, real));
    }
    Ok(())
}
/// Gradle commands (`gradlew`/`gradle`, directly or after transparent `env`
/// assignments) in an `android/` Gradle project lacking the git-ignored
/// `local.properties`: create it with `sdk.dir` from this worker's
/// ANDROID_HOME/ANDROID_SDK_ROOT when one names an existing directory, else
/// fail (before admission) naming the file. Commands run with a sanitized
/// environment, so Gradle cannot find the SDK any other way. An existing file
/// is never touched; nothing is copied from another checkout.
pub fn ensure_android_local_properties(
    argv: &[String],
    cwd: &Path,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Option<String>> {
    if tool_index(argv, &["gradlew", "gradle"]).is_none() {
        return Ok(None);
    }
    let Some(project) = cwd.ancestors().find(|dir| {
        dir.file_name().is_some_and(|n| n == "android")
            && (dir.join("settings.gradle").is_file() || dir.join("settings.gradle.kts").is_file())
    }) else {
        return Ok(None);
    };
    let properties = project.join("local.properties");
    if std::fs::symlink_metadata(&properties).is_ok() {
        return Ok(None);
    }
    for name in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        let Some(sdk) = env(name).map(PathBuf::from) else {
            continue;
        };
        // Properties files are Latin-1 with backslash escapes; keep to plain ASCII.
        let Some(text) = sdk.to_str().filter(|s| {
            sdk.is_absolute() && sdk.is_dir() && s.bytes().all(|b| (b' '..=b'~').contains(&b))
        }) else {
            continue;
        };
        let body = format!(
            "# Created by tasks from the worker's {name}; git-ignored per-checkout file.\nsdk.dir={}\n",
            text.replace('\\', "\\\\")
        );
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&properties)
        {
            Ok(mut file) => file.write_all(body.as_bytes())?,
            // Created concurrently (e.g. by the user): theirs is kept.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        return Ok(Some(format!(
            "created {} with sdk.dir={text} from the worker's {name}\n",
            properties.display()
        )));
    }
    Err(format!(
        "Gradle project {} has no local.properties (it is git-ignored, so a fresh worktree lacks it) \
         and neither ANDROID_HOME nor ANDROID_SDK_ROOT names an existing SDK directory in the task \
         worker's environment; create {} containing `sdk.dir=/absolute/path/to/Android/Sdk` and resubmit",
        project.display(),
        properties.display()
    )
    .into())
}
/// Index of `names` as the program, directly or after transparent `env`
/// assignments. Only these are understood; shell text is never parsed.
fn tool_index(argv: &[String], names: &[&str]) -> Option<usize> {
    let mut index = 0;
    if Path::new(argv.first()?).file_name()?.to_str()? == "env" {
        index = 1;
        if argv.get(index)?.as_str() == "--" {
            index += 1;
        }
        while argv.get(index)?.contains('=') {
            index += 1;
        }
    }
    names
        .contains(&Path::new(argv.get(index)?).file_name()?.to_str()?)
        .then_some(index)
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
    fn cargo(flags: &[&str]) -> Recipe {
        Recipe {
            argv: ["cargo", "test"]
                .iter()
                .chain(flags)
                .map(|s| s.to_string())
                .collect(),
            resource: Resource::Compile,
        }
    }
    #[test]
    fn default_cargo_jobs_cap_rejects_three_with_the_historical_text() {
        let error = cargo(&["-j", "3"])
            .validate_with_max(crate::admission::DEFAULT_MAX_CARGO_JOBS)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "trial safety: Cargo jobs must be 1 or 2 (default 2)");
        // The process-environment path (TASKS_MAX_CARGO_JOBS unset in tests).
        assert_eq!(
            cargo(&["-j", "3"]).validate().unwrap_err().to_string(),
            "trial safety: Cargo jobs must be 1 or 2 (default 2)"
        );
        cargo(&["-j", "2"]).validate().unwrap();
    }
    #[test]
    fn raised_cargo_jobs_cap_accepts_up_to_max_in_every_form() {
        for (prefix, flags) in [
            (vec![], vec!["-j", "8"]),
            (vec![], vec!["-j8"]),
            (vec![], vec!["--jobs", "8"]),
            (vec![], vec!["--jobs=8"]),
            (vec!["env", "CARGO_BUILD_JOBS=8"], vec![]),
        ] {
            let mut ok = cargo(&flags);
            ok.argv.splice(0..0, prefix.iter().map(|s| s.to_string()));
            ok.validate_with_max(8).unwrap();
            let mut too_many = ok.clone();
            for arg in &mut too_many.argv {
                *arg = arg.replace('8', "9");
            }
            assert_eq!(
                too_many.validate_with_max(8).unwrap_err().to_string(),
                "trial safety: Cargo jobs must be 1..=8 (TASKS_MAX_CARGO_JOBS)"
            );
        }
        for bad in ["0", "-1", "garbage", ""] {
            assert!(cargo(&["--jobs", bad]).validate_with_max(64).is_err());
        }
        assert!(cargo(&["-j", "65"]).validate_with_max(64).is_err());
        // Non-Cargo commands never consult the cap.
        Recipe {
            argv: vec!["make".into(), "-j".into(), "64".into()],
            resource: Resource::Compile,
        }
        .validate_with_max(2)
        .unwrap();
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
                match self.try_place(cwd, GIT, ample()).unwrap() {
                    Placed::Done(placement) => placement,
                    Placed::Blocked(space) => panic!("unexpected block: {space:?}"),
                }
            }
            fn try_place(
                &self,
                cwd: &Path,
                git: &str,
                probe: impl FnMut(&Path) -> Result<crate::cache::DiskSpace>,
            ) -> Result<Placed> {
                try_place_outputs_in(cwd, &self.cache, git.as_ref(), probe)
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
        const GIT: &str = "git";
        const MISSING_GIT: &str = "/nonexistent/tasks-test-bin/git";
        const RESERVE: u64 = 20 * 1024 * 1024 * 1024;
        /// Injectable space probe: every filesystem reports `available` bytes.
        fn probe_with(
            available: u64,
            seen: &std::cell::RefCell<Vec<PathBuf>>,
        ) -> impl FnMut(&Path) -> Result<crate::cache::DiskSpace> + '_ {
            move |path: &Path| {
                seen.borrow_mut().push(path.to_path_buf());
                Ok(crate::cache::DiskSpace {
                    path: path.to_path_buf(),
                    available_bytes: available,
                    reserve_bytes: RESERVE,
                })
            }
        }
        fn ample() -> impl FnMut(&Path) -> Result<crate::cache::DiskSpace> {
            |path: &Path| {
                Ok(crate::cache::DiskSpace {
                    path: path.to_path_buf(),
                    available_bytes: RESERVE,
                    reserve_bytes: RESERVE,
                })
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
        // ---- Disk reserve only for new output, on the receiving filesystem ----
        #[test]
        fn existing_links_with_low_disk_proceed_without_a_reserve_check() {
            let f = Fixture::new();
            let root = f.checkout("root");
            f.put(root.join("Cargo.toml"), WORKSPACE);
            assert_eq!(f.place(&root).linked, [root.join("target")]);
            let seen = std::cell::RefCell::new(vec![]);
            let placed = f.try_place(&root, GIT, probe_with(0, &seen)).unwrap();
            assert_eq!(placed, Placed::Done(Placement::default()));
            assert!(seen.borrow().is_empty(), "{:?}", seen.borrow());
        }
        #[test]
        fn new_link_with_low_disk_waits_then_proceeds_once_space_appears() {
            let f = Fixture::new();
            let root = f.checkout("root");
            f.put(root.join("Cargo.toml"), WORKSPACE);
            let seen = std::cell::RefCell::new(vec![]);
            let attempts = std::cell::Cell::new(0);
            let mut waits = vec![];
            let placed = wait_for_placement(
                || {
                    attempts.set(attempts.get() + 1);
                    // Below the reserve for the first three probes.
                    let available = if attempts.get() > 3 {
                        RESERVE
                    } else {
                        RESERVE - 1
                    };
                    f.try_place(&root, GIT, probe_with(available, &seen))
                },
                |space| {
                    // Nothing is created while waiting.
                    assert!(fs::symlink_metadata(root.join("target")).is_err());
                    assert!(!space.admitted());
                    waits.push(space.detail());
                    Ok(true)
                },
            )
            .unwrap()
            .unwrap();
            assert_eq!(waits.len(), 3);
            assert!(waits[0].starts_with(&format!("{}: ", f.cache.display())));
            assert!(waits[0].ends_with("20.0 GiB available, 20.0 GiB reserve"));
            assert_eq!(placed.linked, [root.join("target")]);
            assert_eq!(link(&root.join("target")), f.cached(&root, "target"));
            // The cache filesystem is what is probed for a new cache-backed link.
            assert!(seen.borrow().iter().all(|p| *p == f.cache));
        }
        #[test]
        fn cancel_during_the_disk_wait_creates_nothing() {
            let f = Fixture::new();
            let root = f.checkout("root");
            f.put(root.join("Cargo.toml"), WORKSPACE);
            let seen = std::cell::RefCell::new(vec![]);
            let mut polls = 0;
            let outcome = wait_for_placement(
                || f.try_place(&root, GIT, probe_with(0, &seen)),
                |_| {
                    polls += 1;
                    Ok(polls < 2) // cancellation observed on the second poll
                },
            )
            .unwrap();
            assert_eq!(outcome, None);
            assert_eq!(polls, 2);
            assert!(fs::symlink_metadata(root.join("target")).is_err());
            assert!(fs::read_dir(&f.cache).unwrap().next().is_none());
        }
        #[test]
        fn preserved_local_output_is_checked_on_its_own_filesystem() {
            let f = Fixture::new();
            let root = f.checkout("root");
            f.put(root.join("Cargo.toml"), WORKSPACE);
            f.place(&root);
            let nested = root.join("experiments/independent");
            f.put(nested.join("Cargo.toml"), WORKSPACE);
            f.put(nested.join("target/marker"), "synthetic output");
            let seen = std::cell::RefCell::new(vec![]);
            // Only the kept local target receives output; the cache is not probed.
            let blocked = f.try_place(&nested, GIT, probe_with(0, &seen)).unwrap();
            let Placed::Blocked(space) = blocked else {
                panic!("expected a block, got {blocked:?}");
            };
            assert_eq!(space.path, nested.join("target"));
            assert_eq!(*seen.borrow(), [nested.join("target")]);
            seen.borrow_mut().clear();
            let placed = f
                .try_place(&nested, GIT, probe_with(RESERVE, &seen))
                .unwrap();
            let Placed::Done(placed) = placed else {
                panic!("expected placement");
            };
            assert_eq!(placed.preserved, [nested.join("target")]);
            assert!(placed.linked.is_empty());
            // The real probe measures the filesystem holding that directory.
            let real = crate::cache::disk_space(&nested.join("target")).unwrap();
            assert!(real.available_bytes > 0);
        }
        // ---- Missing git ----
        #[test]
        fn missing_git_falls_back_to_the_dot_git_root_and_says_so() {
            let f = Fixture::new();
            let root = f.checkout("root");
            f.put(root.join("Cargo.toml"), WORKSPACE);
            fs::create_dir_all(root.join("src")).unwrap();
            let Placed::Done(placed) = f
                .try_place(&root.join("src"), MISSING_GIT, ample())
                .unwrap()
            else {
                panic!("expected placement");
            };
            assert_eq!(placed.linked, [root.join("target")]);
            assert_eq!(link(&root.join("target")), f.cached(&root, "target"));
            assert!(placed.detail().contains("git not found on the worker PATH"));
            // A linked worktree's `.git` is a file; it marks the root as well.
            let worktree = f.base.join("worktree");
            f.put(
                worktree.join(".git"),
                "gitdir: /synthetic/.git/worktrees/w\n",
            );
            f.put(worktree.join("Cargo.toml"), WORKSPACE);
            let Placed::Done(placed) = f.try_place(&worktree, MISSING_GIT, ample()).unwrap() else {
                panic!("expected placement");
            };
            assert_eq!(placed.linked, [worktree.join("target")]);
        }
        #[test]
        fn missing_git_outside_a_checkout_skips_placement_without_error() {
            let f = Fixture::new();
            let plain = f.base.join("not-git");
            f.put(plain.join("Cargo.toml"), WORKSPACE);
            let Placed::Done(placed) = f.try_place(&plain, MISSING_GIT, ample()).unwrap() else {
                panic!("expected no-op placement");
            };
            assert!(placed.linked.is_empty() && placed.preserved.is_empty());
            assert!(
                placed.detail().contains("placement skipped"),
                "{}",
                placed.detail()
            );
            assert!(fs::symlink_metadata(plain.join("target")).is_err());
            // Other failures name git or the cwd instead of a bare ENOENT.
            let not_executable = f.base.join("git-dir");
            fs::create_dir(&not_executable).unwrap();
            let error = f
                .try_place(&plain, not_executable.to_str().unwrap(), ample())
                .unwrap_err()
                .to_string();
            assert!(error.contains("rev-parse"), "{error}");
            let error = f
                .try_place(&f.base.join("gone"), GIT, ample())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("live cwd is not an existing directory"),
                "{error}"
            );
        }
        // ---- Android local.properties in a fresh worktree ----
        fn android_project(f: &Fixture) -> PathBuf {
            let root = f.checkout("fresh-worktree");
            let android = root.join("android");
            f.put(android.join("settings.gradle.kts"), "// synthetic\n");
            fs::create_dir_all(android.join("app")).unwrap();
            android
        }
        fn argv(args: &[&str]) -> Vec<String> {
            args.iter().map(|s| s.to_string()).collect()
        }
        #[test]
        fn gradle_in_a_fresh_worktree_gets_local_properties_from_the_sdk_env() {
            let f = Fixture::new();
            let android = android_project(&f);
            let sdk = f.base.join("Sdk");
            fs::create_dir(&sdk).unwrap();
            let env =
                |name: &str| (name == "ANDROID_SDK_ROOT").then(|| sdk.clone().into_os_string());
            let note = ensure_android_local_properties(
                &argv(&["./gradlew", ":app:compileDebugKotlin"]),
                &android.join("app"),
                env,
            )
            .unwrap()
            .unwrap();
            assert!(note.contains("local.properties") && note.contains("ANDROID_SDK_ROOT"));
            let text = fs::read_to_string(android.join("local.properties")).unwrap();
            assert!(
                text.ends_with(&format!("\nsdk.dir={}\n", sdk.display())),
                "{text}"
            );
            // Existing files are never rewritten.
            fs::write(android.join("local.properties"), "sdk.dir=/user/choice\n").unwrap();
            assert_eq!(
                ensure_android_local_properties(&argv(&["gradle", "build"]), &android, env)
                    .unwrap(),
                None
            );
            assert_eq!(
                fs::read_to_string(android.join("local.properties")).unwrap(),
                "sdk.dir=/user/choice\n"
            );
        }
        #[test]
        fn gradle_without_an_sdk_fails_naming_local_properties() {
            let f = Fixture::new();
            let android = android_project(&f);
            // ANDROID_HOME pointing at a missing directory is not used.
            let missing = f.base.join("no-sdk").into_os_string();
            for env_argv in [
                argv(&["./gradlew", ":app:compileDebugKotlin"]),
                argv(&["env", "JAVA_OPTS=-Xmx1g", "/opt/gradle/bin/gradle", "build"]),
            ] {
                let error = ensure_android_local_properties(&env_argv, &android, |name| {
                    (name == "ANDROID_HOME").then(|| missing.clone())
                })
                .unwrap_err()
                .to_string();
                assert!(error.contains("local.properties"), "{error}");
                assert!(error.contains("sdk.dir="), "{error}");
            }
            assert!(fs::symlink_metadata(android.join("local.properties")).is_err());
            // Non-Gradle commands, and Gradle outside an android/ project, are untouched.
            for (args, cwd) in [
                (argv(&["cargo", "test", "-j", "2"]), android.clone()),
                (argv(&["./gradlew", "build"]), f.base.clone()),
            ] {
                assert_eq!(
                    ensure_android_local_properties(&args, &cwd, |_| None).unwrap(),
                    None
                );
            }
        }
        #[test]
        fn outside_git_or_without_manifests_nothing_is_placed() {
            let f = Fixture::new();
            let plain = f.base.join("not-git");
            f.put(plain.join("Cargo.toml"), WORKSPACE);
            let seen = std::cell::RefCell::new(vec![]);
            let placed = f.try_place(&plain, GIT, probe_with(0, &seen)).unwrap();
            assert_eq!(placed, Placed::Done(Placement::default()));
            assert!(seen.borrow().is_empty(), "no reserve without placement");
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
#[cfg(test)]
#[path = "local_command_offload_tests.rs"]
mod offload_tests;
