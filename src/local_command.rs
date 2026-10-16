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
/// Same non-destructive output placement as the old helper, but namespaced by
/// full canonical checkout path to avoid colliding same-named worktrees.
#[cfg(target_os = "linux")]
pub fn place_outputs(cwd: &Path) -> Result<()> {
    use sha2::{Digest, Sha256};
    let result = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()?;
    if !result.status.success() {
        return Ok(());
    }
    let root = std::fs::canonicalize(String::from_utf8(result.stdout)?.trim())?;
    if !root.join("Cargo.toml").is_file() {
        return Ok(());
    }
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME required")?);
    let cache = home.join("build-cache");
    crate::cache::require_space(&[&cache])?;
    let key = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
    for rel in ["target", "android/build", "android/app/build"] {
        let link = root.join(rel);
        match std::fs::symlink_metadata(&link) {
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        if !link.parent().is_some_and(Path::is_dir) {
            continue;
        }
        let real = cache.join("p2p_chat/tasks/checkouts").join(&key).join(rel);
        std::fs::create_dir_all(&real)?;
        std::os::unix::fs::symlink(real, link)?;
    }
    Ok(())
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
}
