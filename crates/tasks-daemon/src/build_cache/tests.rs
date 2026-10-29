use super::outputs::Placed;
use super::*;
use crate::testkit::TempDir;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn exe(dir: &Path, name: &str, mode: u32) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    fs::write(&p, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
    p
}

fn kv(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}

fn settings() -> Settings {
    Settings {
        sccache: true,
        sccache_size: "30G".into(),
    }
}

// R-400: sccache is found as an absolute, executable regular file: PATH first, then
// ~/.cargo/bin, which a service's PATH often lacks.
#[test]
fn r400_sccache_is_found_on_path_then_in_cargo_bin_and_must_be_executable() {
    let t = TempDir::new("find");
    let a = t.path().join("a");
    let b = t.path().join("b");
    exe(&a, "sccache", 0o644); // not executable: skipped
    let good = exe(&b, "sccache", 0o755);
    assert_eq!(find_sccache([a.clone(), b.clone()]), Some(good.clone()));
    assert_eq!(find_sccache([PathBuf::from("rel/dir")]), None);
    // PATH wins over the fallback
    let home = t.path().join("home");
    let cargo = exe(&home.join(".cargo/bin"), "sccache", 0o755);
    let env = vec![
        kv("PATH", b.to_str().unwrap()),
        kv("HOME", home.to_str().unwrap()),
    ];
    let c = BuildCache::discover(&t.path().join("cache"), &t.path().join("ps"), &env);
    assert_eq!(c.sccache(), Some(good.as_path()));
    // without it on PATH the fallback is used
    let env = vec![
        kv("PATH", "/nonexistent"),
        kv("HOME", home.to_str().unwrap()),
    ];
    let c = BuildCache::discover(&t.path().join("cache"), &t.path().join("ps"), &env);
    assert_eq!(c.sccache(), Some(cargo.as_path()));
    // and nothing found is an explicit no
    let c = BuildCache::discover(&t.path().join("cache"), &t.path().join("ps"), &[]);
    assert_eq!(c.sccache(), None);
}

// R-400: the machine's settings are forced and every job-side route to another wrapper scrubbed.
#[test]
fn r400_the_policy_forces_the_machines_wrapper_directory_size_and_scrubs_the_rest() {
    let t = TempDir::new("env");
    let bin = exe(&t.path().join("bin"), "sccache", 0o755);
    let c = BuildCache::discover(&t.path().join("cache"), &t.path().join("ps"), &[])
        .with_sccache(Some(bin.clone()));
    let p = c.env_policy(&settings(), Some(2));
    assert_eq!(p.default_cargo_jobs, Some(2));
    let forced = |n: &str| p.force.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    assert_eq!(
        forced("RUSTC_WRAPPER"),
        Some(bin.to_string_lossy().into_owned())
    );
    assert_eq!(
        forced("SCCACHE_DIR"),
        Some(
            t.path()
                .join("cache/sccache")
                .to_string_lossy()
                .into_owned()
        )
    );
    assert_eq!(forced("SCCACHE_CACHE_SIZE"), Some("30G".into()));
    assert_eq!(forced("CARGO_INCREMENTAL"), Some("0".into()));
    assert_eq!(forced("CARGO_PROFILE_DEV_DEBUG"), Some("0".into()));
    assert_eq!(forced("CARGO_PROFILE_TEST_DEBUG"), Some("0".into()));
    for name in [
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
    ] {
        assert!(p.scrub.iter().any(|s| s == name), "{name}");
    }
    assert_eq!(p.scrub_prefixes, ["SCCACHE_"]);
    // no sccache: still scrubbed, nothing forced but the Cargo settings
    let none = c.clone().with_sccache(None).env_policy(&settings(), None);
    assert!(
        none.force
            .iter()
            .all(|(k, _)| k != "RUSTC_WRAPPER" && k != "SCCACHE_DIR")
    );
    assert!(none.scrub.iter().any(|s| s == "RUSTC_WRAPPER"));
}

// R-405: GRADLE_USER_HOME is the cache root's gradle directory, only when it exists.
#[test]
fn r405_the_gradle_home_is_forced_only_when_the_directory_exists() {
    let t = TempDir::new("gradle");
    let c = BuildCache::discover(&t.path().join("cache"), &t.path().join("ps"), &[]);
    assert_eq!(c.gradle_home(), None);
    assert!(
        c.env_policy(&settings(), None)
            .force
            .iter()
            .all(|(k, _)| k != "GRADLE_USER_HOME")
    );
    fs::create_dir_all(t.path().join("cache/gradle")).unwrap();
    let home = t.path().join("cache/gradle");
    assert_eq!(c.gradle_home(), Some(home.clone()));
    assert!(
        c.env_policy(&settings(), None)
            .force
            .contains(&kv("GRADLE_USER_HOME", home.to_str().unwrap()))
    );
}

// R-407 / R-408: the header says what is in force, or why nothing is.
#[test]
fn r407_the_header_names_wrapper_links_and_kept_directories() {
    let t = TempDir::new("header");
    let c = BuildCache::discover(&t.path().join("cache"), &t.path().join("ps"), &[])
        .with_sccache(Some("/usr/bin/sccache".into()));
    let placed = Placed {
        links: vec![("/w/target".into(), "/cache/x/target".into())],
        preserved: vec!["/w/android/build".into()],
        ..Placed::default()
    };
    let h = c.header(&settings(), &placed);
    assert_eq!(
        h,
        format!(
            "cache: sccache {}/cache/sccache, target -> /cache/x/target, \
             /w/android/build kept local (existing directory)",
            t.path().display()
        )
    );
    let off = Settings {
        sccache: false,
        ..settings()
    };
    assert!(
        c.header(&off, &Placed::default())
            .starts_with("cache: sccache is off")
    );
    let bare = c.clone().with_sccache(None);
    assert!(
        bare.header(&settings(), &Placed::default())
            .starts_with("cache: uncached build: sccache was not found")
    );
}

// R-405, R-829: a peer may fetch through exactly the links the placement creates, and only
// into this cache.
#[test]
fn r405_the_fetch_policy_allows_the_placed_link_names_into_this_cache() {
    let t = TempDir::new("fetch-policy");
    let c = BuildCache::discover(&t.path().join("cache"), &t.path().join("ps"), &[]);
    let p = c.fetch_policy();
    assert_eq!(
        p.allowed_links,
        ["target", "android/build", "android/app/build"]
    );
    assert_eq!(p.cache_root.as_deref(), Some(c.root()));
}

// R-401: the cache settings are policy keys with the earlier release's values as defaults.
#[test]
fn r401_the_cache_policy_keys_default_to_auto_and_thirty_gib() {
    let p = crate::machine_policy::MachinePolicy::defaults();
    assert!(p.sccache_enabled());
    assert_eq!(p.sccache_size(), "30G");
    let bad = crate::machine_policy::MachinePolicy::from_text(
        Some("[cache]\nsccache = \"maybe\"\n"),
        &|_| None,
    );
    assert!(!bad.problems.is_empty());
}
