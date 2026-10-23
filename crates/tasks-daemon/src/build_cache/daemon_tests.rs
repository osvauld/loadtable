//! The cache rules as the scheduler applies them to real jobs (R-400 to R-408).

use super::{BuildCache, Settings};
use crate::testkit::Rig;
use std::fs;
use std::path::{Path, PathBuf};
use tasks_core::{JobId, Kind, Phase};

fn kv(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}

/// An executable named `sccache` in a fresh directory.
fn fake_sccache(r: &Rig) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = r.dir.path().join("tools");
    fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("sccache");
    fs::write(&bin, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn rig_with(sccache: bool) -> Rig {
    let r = Rig::new();
    let bin = sccache.then(|| fake_sccache(&r));
    r.sch.set_build_cache(
        BuildCache::discover(&r.cfg.cache_dir, &r.cfg.state_dir.join("peer-store"), &[])
            .with_sccache(bin),
    );
    r
}

/// A Git checkout with a Cargo manifest below the rig directory.
fn checkout(r: &Rig, name: &str) -> PathBuf {
    let root = fs::canonicalize(r.dir.path())
        .unwrap()
        .join("work")
        .join(name);
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    root
}

fn submit_in(r: &Rig, cwd: &Path, argv: &[&str], env: Vec<(String, String)>) -> JobId {
    let mut spec = r.spec("o", argv, Kind::Compile);
    spec.cwd = cwd.to_path_buf();
    spec.env = env;
    r.submit(&spec)
}

fn env_of(r: &Rig, n: usize) -> Vec<(String, String)> {
    r.procs.spawned()[n].env.clone()
}

fn var<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

// R-400: the machine's wrapper and cache directory are set after sanitization; a job's own
// wrapper, cache directory and cache size are ignored.
#[test]
fn r400_a_job_cannot_override_the_machines_wrapper_or_cache_directory() {
    let r = rig_with(true);
    let root = checkout(&r, "app");
    let id = submit_in(
        &r,
        &root,
        &["cargo", "build"],
        vec![
            kv("RUSTC_WRAPPER", "/job/evil"),
            kv("RUSTC_WORKSPACE_WRAPPER", "/job/evil"),
            kv("CARGO_BUILD_RUSTC_WRAPPER", "/job/evil"),
            kv("SCCACHE_DIR", "/job/dir"),
            kv("SCCACHE_CACHE_SIZE", "1T"),
            kv("SCCACHE_BUCKET", "leak"),
            kv("CARGO_INCREMENTAL", "1"),
        ],
    );
    r.tick();
    r.until_running(id);
    let env = env_of(&r, 0);
    assert_eq!(
        var(&env, "RUSTC_WRAPPER"),
        Some(r.dir.path().join("tools/sccache").to_str().unwrap())
    );
    assert_eq!(
        var(&env, "SCCACHE_DIR"),
        Some(r.cfg.cache_dir.join("sccache").to_str().unwrap())
    );
    assert_eq!(var(&env, "SCCACHE_CACHE_SIZE"), Some("30G"));
    assert_eq!(var(&env, "CARGO_INCREMENTAL"), Some("0"));
    assert_eq!(var(&env, "CARGO_PROFILE_DEV_DEBUG"), Some("0"));
    for gone in [
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "SCCACHE_BUCKET",
    ] {
        assert_eq!(var(&env, gone), None, "{gone}");
    }
}

// R-408: no sccache is an explicit uncached build with a note, never a failure.
#[test]
fn r408_without_sccache_the_job_runs_uncached_and_says_so() {
    let r = rig_with(false);
    let id = submit_in(
        &r,
        r.dir.path(),
        &["cargo", "build"],
        vec![kv("RUSTC_WRAPPER", "/job/evil")],
    );
    r.tick();
    r.until_running(id);
    let env = env_of(&r, 0);
    assert_eq!(
        var(&env, "RUSTC_WRAPPER"),
        None,
        "the job's wrapper is not used either"
    );
    assert_eq!(var(&env, "CARGO_INCREMENTAL"), Some("0"));
    let log = r.log_text(id);
    let first = log
        .lines()
        .find(|l| l.starts_with("[tasks] cache:"))
        .unwrap();
    assert!(
        first.starts_with("[tasks] cache: uncached build: sccache was not found"),
        "{log}"
    );
    assert!(!matches!(
        r.phase(id),
        Phase::NotStarted { .. } | Phase::Failed { .. }
    ));
}

#[test]
fn r408_the_policy_can_turn_sccache_off() {
    let r = rig_with(true);
    let policy = crate::machine_policy::MachinePolicy::from_text(
        Some("[cache]\nsccache = \"off\"\nsccache_size_gib = 5\n"),
        &|_| None,
    );
    assert!(policy.problems.is_empty(), "{:?}", policy.problems);
    let settings = Settings::of(&policy);
    assert!(!settings.sccache);
    assert_eq!(settings.sccache_size, "5G");
    let cache = r.sch.build_cache();
    let env = cache.env_policy(&settings, None);
    assert!(env.force.iter().all(|(k, _)| k != "RUSTC_WRAPPER"));
    assert!(
        cache
            .wrapper(&settings)
            .unwrap_err()
            .contains("cache.sccache")
    );
}

// R-407: the first line of the output says what cache the job runs under.
#[test]
fn r407_the_output_starts_with_the_cache_line() {
    let r = rig_with(true);
    let root = checkout(&r, "app");
    let id = submit_in(&r, &root, &["cargo", "build"], vec![]);
    r.tick();
    r.until_running(id);
    let log = r.log_text(id);
    let first = log
        .lines()
        .find(|l| l.starts_with("[tasks] cache:"))
        .unwrap();
    let real = fs::read_link(root.join("target")).unwrap();
    assert!(
        real.starts_with(r.cfg.cache_dir.join("tasks2/checkouts")),
        "{real:?}"
    );
    assert_eq!(
        first,
        format!(
            "[tasks] cache: sccache {}, target -> {}",
            r.cfg.cache_dir.join("sccache").display(),
            real.display()
        )
    );
}

// R-402: a job forwarded from a peer runs in the synced copy; `target` there is a link into the
// cache keyed by (peer, checkout), so it survives the copy.
#[test]
fn r402_a_forwarded_job_gets_the_peers_key() {
    let r = rig_with(true);
    let tree = r.cfg.state_dir.join("peer-store/laptop-node/aa11bb/tree");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("Cargo.toml"), "[workspace]\n").unwrap();
    let mut spec = r.spec("o", &["cargo", "build"], Kind::Compile);
    spec.cwd = tree.clone();
    let mut job = tasks_ledger::NewJob::new(spec);
    job.origin = Some(tasks_ledger::Origin {
        peer: "laptop-node".into(),
        key: "k1".into(),
    });
    let id = r.ledger.submit_job(&job).unwrap();
    r.tick();
    r.until_running(id);
    let real = fs::read_link(tree.join("target")).unwrap();
    assert_eq!(
        real,
        fs::canonicalize(&r.cfg.cache_dir)
            .unwrap_or(r.cfg.cache_dir.clone())
            .join("tasks2/checkouts/peer/laptop-node/aa11bb/target")
            .components()
            .collect::<PathBuf>()
    );
}

// R-404: a job that needs NEW cache output waits while the cache disk is below the reserve; one
// whose links already exist does not, and the wait names the cache disk.
#[test]
fn r404_new_cache_output_waits_for_the_disk_reserve_and_existing_links_do_not() {
    let r = rig_with(true);
    let fresh = checkout(&r, "fresh");
    let warm = checkout(&r, "warm");
    // `warm` already has its link (placed by an earlier job)
    let real = r.cfg.cache_dir.join("tasks2/checkouts/warm/target");
    fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, warm.join("target")).unwrap();

    r.machine.set_disk(1 << 30);
    let a = submit_in(&r, &fresh, &["cargo", "build"], vec![]);
    let b = submit_in(&r, &warm, &["cargo", "build"], vec![]);
    r.tick();
    let rec = r.record(a);
    assert!(
        matches!(rec.phase, Phase::Waiting(tasks_core::WaitReason::Disk)),
        "{rec:?}"
    );
    let text = rec.wait_text.unwrap();
    assert!(
        text.contains("disk: cache has 1 GiB free, reserve 20 GiB"),
        "{text}"
    );
    assert!(
        fs::symlink_metadata(fresh.join("target")).is_err(),
        "nothing is created while it waits"
    );
    r.until_running(b);

    r.machine.set_disk(100 << 30);
    r.tick();
    r.until_running(a);
    assert!(
        fs::symlink_metadata(fresh.join("target"))
            .unwrap()
            .is_symlink()
    );
}

// R-405: Gradle uses the shared home when it exists; an Android job may not redirect Cargo.
#[test]
fn r405_gradle_gets_the_shared_home_and_refuses_a_cargo_target_override() {
    let r = rig_with(false);
    let android = r.dir.path().join("w/android");
    fs::create_dir_all(&android).unwrap();
    fs::write(android.join("settings.gradle"), "").unwrap();
    fs::write(android.join("local.properties"), "sdk.dir=/x\n").unwrap();
    // no gradle directory yet: the job's own value stays
    let id = submit_in(
        &r,
        &android,
        &["./gradlew", "assembleDebug"],
        vec![kv("GRADLE_USER_HOME", "/job/home")],
    );
    r.tick();
    r.until_running(id);
    assert_eq!(var(&env_of(&r, 0), "GRADLE_USER_HOME"), Some("/job/home"));
    r.procs.exit(r.procs.pgid(0), 0);
    r.until_done(id);

    fs::create_dir_all(r.cfg.cache_dir.join("gradle")).unwrap();
    let id = submit_in(
        &r,
        &android,
        &["./gradlew", "assembleDebug"],
        vec![kv("GRADLE_USER_HOME", "/job/home")],
    );
    r.tick();
    r.until_running(id);
    assert_eq!(
        var(&env_of(&r, 1), "GRADLE_USER_HOME"),
        Some(r.cfg.cache_dir.join("gradle").to_str().unwrap())
    );
    r.procs.exit(r.procs.pgid(1), 0);
    r.until_done(id);

    let bad = submit_in(
        &r,
        &android,
        &["./gradlew", "assembleDebug"],
        vec![kv("CARGO_TARGET_DIR", "/elsewhere")],
    );
    r.tick();
    match r.phase(bad) {
        Phase::NotStarted { reason } => assert!(reason.contains("CARGO_TARGET_DIR"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(r.procs.spawn_count(), 2);
}

#[test]
fn r405_a_gradle_job_without_local_properties_or_sdk_does_not_start() {
    let r = rig_with(false);
    let android = r.dir.path().join("w/android");
    fs::create_dir_all(&android).unwrap();
    fs::write(android.join("settings.gradle"), "").unwrap();
    let id = submit_in(&r, &android, &["./gradlew", "build"], vec![]);
    r.tick();
    match r.phase(id) {
        Phase::NotStarted { reason } => assert!(reason.contains("local.properties"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

// R-401: the cache root is the daemon's cache directory, `~/build-cache` by default.
#[test]
fn r401_the_cache_root_defaults_to_build_cache_under_home_and_can_be_moved() {
    use crate::config::{ConfigArgs, DaemonConfig};
    let env = |k: &str| (k == "HOME").then(|| "/home/u".to_string());
    let cfg = DaemonConfig::resolve(&ConfigArgs::default(), &env).unwrap();
    assert_eq!(cfg.cache_dir, Path::new("/home/u/build-cache"));
    let moved = ConfigArgs {
        cache_dir: Some("/mnt/big/cache".into()),
        ..ConfigArgs::default()
    };
    assert_eq!(
        DaemonConfig::resolve(&moved, &env).unwrap().cache_dir,
        Path::new("/mnt/big/cache")
    );
    let env2 = |k: &str| match k {
        "HOME" => Some("/home/u".to_string()),
        "TASKS_CACHE_DIR" => Some("/data/c".to_string()),
        _ => None,
    };
    assert_eq!(
        DaemonConfig::resolve(&ConfigArgs::default(), &env2)
            .unwrap()
            .cache_dir,
        Path::new("/data/c")
    );
}

// R-407: `tasks2 cache` shows the wrapper, the root and the disk against the reserve.
#[test]
fn r407_the_report_names_the_wrapper_the_root_and_the_disk_against_the_reserve() {
    let r = rig_with(true);
    fs::create_dir_all(r.cfg.cache_dir.join("gradle")).unwrap();
    let policy = crate::machine_policy::MachinePolicy::defaults();
    let sccache = r.dir.path().join("tools");
    let env = vec![kv("PATH", sccache.to_str().unwrap())];
    let text = super::report::report(&r.cfg, &policy, &env, &*r.machine);
    assert!(
        text.contains(&format!("cache root: {}", r.cfg.cache_dir.display())),
        "{text}"
    );
    assert!(text.contains("compiler cache: sccache "), "{text}");
    assert!(text.contains("limit 30G"), "{text}");
    assert!(text.contains("gradle home: "), "{text}");
    assert!(
        text.contains("disk: 100 GiB free, reserve 20 GiB: ok"),
        "{text}"
    );
    r.machine.set_disk(3 << 30);
    let text = super::report::report(&r.cfg, &policy, &[], &*r.machine);
    assert!(
        text.contains("compiler cache: none (uncached build"),
        "{text}"
    );
    assert!(text.contains("BELOW the reserve"), "{text}");
}
