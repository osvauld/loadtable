//! A cargo job's build state on disk (R-800): its workspace, the dependencies its lockfile
//! names, and what its output directory already holds for the job's profile. The rules that
//! turn these into warm or cold are in [`tasks_timing::warmth`].

use std::path::{Path, PathBuf};
use tasks_core::JobSpec;
use tasks_timing::warmth::{lock_deps, run_warmth, start_warmth};
use tasks_timing::{Cache, RecipeKey, Timeline};

/// Where a cargo job builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CargoBuild {
    /// `<target dir>[/<triple>]/<profile dir>`.
    profile_dir: PathBuf,
    /// Dependencies the workspace's `Cargo.lock` names; `None` when it could not be read.
    lock_deps: Option<u32>,
}

impl CargoBuild {
    /// The build of `spec`, when it runs cargo.
    pub(crate) fn of(spec: &JobSpec) -> Option<CargoBuild> {
        let key = RecipeKey::from_argv(&spec.argv, &spec.env)
            .into_iter()
            .find(|k| k.tool == "cargo")?;
        let start = manifest_path(&spec.argv)
            .map(|m| spec.cwd.join(m))
            .and_then(|m| m.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| spec.cwd.clone());
        let start = std::fs::canonicalize(&start).unwrap_or(start);
        let lock_dir = start.ancestors().find(|d| d.join("Cargo.lock").is_file());
        let ws = lock_dir
            .or_else(|| start.ancestors().find(|d| d.join("Cargo.toml").is_file()))?
            .to_path_buf();
        let lock_deps = lock_dir
            .and_then(|d| std::fs::read_to_string(d.join("Cargo.lock")).ok())
            .map(|t| lock_deps(&t));
        let target = spec
            .env
            .iter()
            .find(|(k, _)| k == "CARGO_TARGET_DIR")
            .map(|(_, v)| spec.cwd.join(v))
            .unwrap_or_else(|| ws.join("target"));
        let mut profile_dir = target;
        if !key.target.is_empty() {
            profile_dir.push(&key.target);
        }
        profile_dir.push(match key.profile.as_str() {
            "" | "dev" | "test" => "debug",
            "bench" => "release",
            other => other,
        });
        Some(CargoBuild {
            profile_dir,
            lock_deps,
        })
    }

    /// How the build will start: warm when its output directory already holds most of its
    /// dependencies.
    pub(crate) fn start(&self) -> Cache {
        let units = std::fs::read_dir(self.profile_dir.join(".fingerprint"))
            .ok()
            .map(|d| u32::try_from(d.count()).unwrap_or(u32::MAX));
        start_warmth(units, self.lock_deps)
    }

    /// How the finished run started, from what it compiled.
    pub(crate) fn after(&self, timeline: &Timeline) -> Cache {
        run_warmth(timeline.deps_built, self.lock_deps)
    }
}

fn manifest_path(argv: &[String]) -> Option<&str> {
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if a == "--manifest-path" {
            return it.next().map(String::as_str);
        }
        if let Some(v) = a.strip_prefix("--manifest-path=") {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_core::{Kind, Owner, Placement, Priority, Requirements};

    fn spec(cwd: &Path, argv: &[&str]) -> JobSpec {
        JobSpec {
            owner: Owner::new("o").unwrap(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_path_buf(),
            env: vec![],
            kind: Kind::Compile,
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

    fn lock(deps: usize) -> String {
        let mut t = String::from("version = 4\n");
        for i in 0..deps {
            t.push_str(&format!(
                "\n[[package]]\nname = \"d{i}\"\nversion = \"1.0.0\"\nsource = \"registry+x\"\n"
            ));
        }
        t
    }

    #[test]
    fn r800_a_job_starts_warm_only_when_its_profile_output_holds_its_dependencies() {
        let dir = crate::testkit::TempDir::new("warmth");
        let ws = dir.path();
        std::fs::write(ws.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(ws.join("Cargo.lock"), lock(8)).unwrap();
        let sub = ws.join("crates/app");
        std::fs::create_dir_all(&sub).unwrap();
        let b = CargoBuild::of(&spec(&sub, &["cargo", "build", "-p", "app"])).unwrap();
        assert_eq!(b.lock_deps, Some(8));
        assert_eq!(b.start(), Cache::Cold, "no target yet");
        let fp = ws.join("target/debug/.fingerprint");
        std::fs::create_dir_all(&fp).unwrap();
        std::fs::create_dir(fp.join("d0-1")).unwrap();
        assert_eq!(b.start(), Cache::Cold, "one of eight dependencies");
        std::fs::create_dir(fp.join("d1-1")).unwrap();
        assert_eq!(b.start(), Cache::Warm);
        // a release build reads its own directory, a cross build its triple's
        let r = CargoBuild::of(&spec(&sub, &["cargo", "build", "--release"])).unwrap();
        assert_eq!(r.start(), Cache::Cold);
        let x = CargoBuild::of(&spec(
            &sub,
            &["cargo", "build", "--target", "aarch64-linux-android"],
        ))
        .unwrap();
        assert!(
            x.profile_dir
                .ends_with("target/aarch64-linux-android/debug")
        );
        // not cargo: nothing to judge from disk
        assert_eq!(CargoBuild::of(&spec(&sub, &["make"])), None);
    }

    #[test]
    fn r800_a_manifest_path_names_the_workspace() {
        let dir = crate::testkit::TempDir::new("warmth");
        let exp = dir.path().join("experiments/panel");
        std::fs::create_dir_all(&exp).unwrap();
        std::fs::write(exp.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(exp.join("Cargo.lock"), lock(3)).unwrap();
        let b = CargoBuild::of(&spec(
            dir.path(),
            &[
                "cargo",
                "test",
                "--manifest-path",
                "experiments/panel/Cargo.toml",
                "--lib",
            ],
        ))
        .unwrap();
        assert_eq!(b.lock_deps, Some(3));
        assert!(b.profile_dir.ends_with("experiments/panel/target/debug"));
    }

    #[test]
    fn r800_a_finished_run_is_cold_when_it_compiled_a_quarter_of_its_dependencies() {
        let b = CargoBuild {
            profile_dir: PathBuf::from("/x/target/debug"),
            lock_deps: Some(400),
        };
        let mut tl = Timeline::new();
        tl.deps_built = 3;
        assert_eq!(b.after(&tl), Cache::Warm);
        tl.deps_built = 120;
        assert_eq!(b.after(&tl), Cache::Cold);
    }
}
