//! A gradle job's build state on disk (R-825): whether the modules it builds already hold
//! compiled output and, for a project inside a cargo workspace (native code built by
//! cargo-ndk), whether `target/` holds an Android build. A checkout's `build/` and `target/`
//! are links into the build cache, so a new worktree starts empty, which "this machine passed
//! the command before" could not tell.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tasks_core::JobSpec;
use tasks_timing::{Cache, RecipeKey};

/// Where a gradle job builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GradleBuild {
    /// `<module>/build` of each module its tasks name.
    module_builds: BTreeSet<PathBuf>,
    /// The project, when a task names no module (it runs in every module).
    any_module: Option<PathBuf>,
    /// `target/` of the cargo workspace the project sits in, if any.
    cargo_target: Option<PathBuf>,
}

impl GradleBuild {
    /// The build of `spec`, when it runs gradle.
    pub(crate) fn of(spec: &JobSpec) -> Option<GradleBuild> {
        let key = RecipeKey::from_argv(&spec.argv, &spec.env)
            .into_iter()
            .find(|k| k.tool == "gradle")?;
        let project =
            project_dir(&spec.argv).map_or_else(|| spec.cwd.clone(), |p| spec.cwd.join(p));
        let mut module_builds = BTreeSet::new();
        let mut any_module = None;
        for task in &key.selection {
            let path: Vec<&str> = task.trim_start_matches(':').split(':').collect();
            match path.split_last() {
                Some((_, modules)) if !modules.is_empty() => {
                    let dir = modules.iter().fold(project.clone(), |d, m| d.join(m));
                    module_builds.insert(dir.join("build"));
                }
                _ => any_module = Some(project.clone()),
            }
        }
        let cargo_target = project
            .ancestors()
            .find(|d| d.join("Cargo.lock").is_file())
            .map(|d| d.join("target"));
        Some(GradleBuild {
            module_builds,
            any_module,
            cargo_target,
        })
    }

    /// How the build will start: warm when every module it builds has compiled output and the
    /// workspace's `target/` (if any) holds an Android build; cold otherwise.
    pub(crate) fn start(&self) -> Cache {
        let modules = self
            .module_builds
            .iter()
            .all(|b| has_entries(&b.join("intermediates")))
            && self.any_module.as_ref().is_none_or(|p| {
                std::fs::read_dir(p).is_ok_and(|d| {
                    d.flatten()
                        .any(|e| has_entries(&e.path().join("build/intermediates")))
                })
            });
        let native = self.cargo_target.as_ref().is_none_or(|t| android_build(t));
        if modules && native {
            Cache::Warm
        } else {
            Cache::Cold
        }
    }
}

fn has_entries(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_some())
}

/// `target/<an Android triple>/<profile>/.fingerprint` holds built units.
fn android_build(target: &Path) -> bool {
    std::fs::read_dir(target).is_ok_and(|d| {
        d.flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("-android"))
            .any(|triple| {
                std::fs::read_dir(triple.path()).is_ok_and(|p| {
                    p.flatten()
                        .any(|profile| has_entries(&profile.path().join(".fingerprint")))
                })
            })
    })
}

fn project_dir(argv: &[String]) -> Option<&str> {
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if a == "-p" || a == "--project-dir" {
            return it.next().map(String::as_str);
        }
        if let Some(v) = a.strip_prefix("--project-dir=") {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_core::{Kind, Owner, Placement, Priority, Requirements};

    fn spec(cwd: &Path, argv: &str) -> JobSpec {
        JobSpec {
            owner: Owner::new("o").unwrap(),
            argv: argv.split_whitespace().map(String::from).collect(),
            cwd: cwd.to_path_buf(),
            env: vec![],
            kind: Kind::Package,
            requirements: Requirements::default(),
            placement: Placement::Here,
            after: vec![],
            timeout_secs: None,
            broad_test_escape: false,
            script_escape: false,
            priority: Priority::Normal,
            priority_reason: None,
            fetch: Vec::new(),
            isolate: false,
            light: false,
            retry_alone: false,
            interactive: false,
        }
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }

    #[test]
    fn r825_a_gradle_job_starts_warm_only_when_its_modules_and_native_build_hold_output() {
        let dir = crate::testkit::TempDir::new("gradle-warmth");
        let root = dir.path();
        std::fs::write(root.join("Cargo.lock"), "version = 4\n").unwrap();
        let s = spec(
            root,
            "./android/gradlew -p android --no-daemon :app:assemblePlayRelease",
        );
        let b = GradleBuild::of(&s).unwrap();
        assert_eq!(b.start(), Cache::Cold, "a new worktree");
        touch(&root.join("android/app/build/intermediates/x"));
        assert_eq!(
            b.start(),
            Cache::Cold,
            "the core was never built for Android"
        );
        touch(&root.join("target/release/.fingerprint/host"));
        assert_eq!(
            b.start(),
            Cache::Cold,
            "a desktop build is not the Android one"
        );
        touch(&root.join("target/aarch64-linux-android/release/.fingerprint/p2pcore"));
        assert_eq!(b.start(), Cache::Warm);
    }

    #[test]
    fn r825_a_task_without_a_module_needs_some_module_built_and_no_cargo_means_no_native_check() {
        let dir = crate::testkit::TempDir::new("gradle-warmth-any");
        let root = dir.path();
        let b = GradleBuild::of(&spec(root, "gradle assembleDebug")).unwrap();
        assert_eq!(b.start(), Cache::Cold);
        touch(&root.join("lib/build/intermediates/x"));
        assert_eq!(b.start(), Cache::Warm);
        assert!(GradleBuild::of(&spec(root, "cargo build")).is_none());
    }
}
