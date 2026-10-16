use super::*;
use crate::workspace_snapshot::tests::{Fixture, REV};

#[test]
fn typed_abi_policy_rejects_arbitrary_commands_and_duplicates() {
    for value in [
        "",
        "all",
        "x86_64;adb install",
        "../arm64-v8a",
        "x86_64,x86_64",
    ] {
        assert!(Abis::parse(value).is_err());
    }
    assert_eq!(Abis::parse("x86_64").unwrap(), Abis::X86);
    assert_eq!(Abis::parse("arm64-v8a").unwrap(), Abis::Arm64);
    assert_eq!(Abis::parse("x86_64,arm64-v8a").unwrap(), Abis::Both);
    assert_eq!(Abis::parse("emulator").unwrap(), Abis::X86);
    assert_eq!(Abis::parse("phone").unwrap(), Abis::Arm64);
    assert_eq!(
        Abis::parse("phone").unwrap().configuration(),
        "debug/arm64-v8a/rust-debug/offline"
    );
}
#[test]
fn plan_is_read_only_gated_and_defaults_to_single_abi_debug_build_only() {
    let f = Fixture::new();
    let before = workspace_snapshot::inspect(&f.source, REV).unwrap();
    let plan = preflight_at(
        &f.source,
        REV,
        Abis::X86,
        &f.root.join("home"),
        &f.root.join("sdk"),
        &f.root.join("jdk"),
    )
    .unwrap();
    assert!(!plan.executable && !plan.installs && !plan.publishes);
    assert_eq!(plan.steps.len(), 2);
    let gradle = plan.steps.last().unwrap();
    assert!(gradle.argv.contains(&"--offline".into()));
    assert!(gradle.argv.contains(&"-Pabis=x86_64".into()));
    assert!(gradle.argv.contains(&"-PrustRelease=false".into()));
    assert!(
        !gradle
            .argv
            .iter()
            .any(|arg| arg.contains("install") || arg.contains("bundle"))
    );
    assert!(!f.root.join("home").exists());
    assert!(!f.source.join("android/app/build").exists());
    assert_eq!(plan.snapshot.digest, before.digest);
    assert!(!plan.blockers.is_empty());
}
#[test]
fn both_abis_plan_two_native_steps_and_only_one_gradle_invocation() {
    let f = Fixture::new();
    let plan = preflight_at(
        &f.source,
        REV,
        Abis::Both,
        &f.root,
        &f.root.join("sdk"),
        &f.root.join("jdk"),
    )
    .unwrap();
    assert_eq!(plan.steps.len(), 3);
    assert!(plan.steps[0].argv.contains(&"x86_64-linux-android".into()));
    assert!(plan.steps[1].argv.contains(&"aarch64-linux-android".into()));
    assert!(
        plan.steps[2]
            .argv
            .contains(&"-Pabis=x86_64,arm64-v8a".into())
    );
    assert!(!plan.steps.iter().any(|step| {
        step.argv
            .iter()
            .any(|arg| arg.contains("build_android.py") || arg.contains("buildlock.py"))
    }));
}
#[test]
fn native_tarball_is_verified_but_missing_or_corrupt_inputs_are_not_repaired() {
    let f = Fixture::new();
    let data = b"synthetic pinned native source";
    let digest = format!("{:x}", Sha256::digest(data));
    f.put(
        "scripts/build_libaom.py",
        &format!("VERSION = \"3.15.2\"\nSHA256 = \"{digest}\"\n"),
    );
    let tarball = f
        .root
        .join("build-cache/p2p_chat/libaom/libaom-3.15.2.tar.gz");
    fs::create_dir_all(tarball.parent().unwrap()).unwrap();
    fs::write(&tarball, data).unwrap();
    let plan = preflight_at(
        &f.source,
        REV,
        Abis::X86,
        &f.root,
        &f.root.join("sdk"),
        &f.root.join("jdk"),
    )
    .unwrap();
    assert!(plan.libaom_tarball_verified);
    assert!(
        !plan.executable,
        "verified tarball alone must never enable execution"
    );
    fs::write(&tarball, "corrupt fixture").unwrap();
    let plan = preflight_at(
        &f.source,
        REV,
        Abis::X86,
        &f.root,
        &f.root.join("sdk"),
        &f.root.join("jdk"),
    )
    .unwrap();
    assert!(!plan.libaom_tarball_verified);
    assert_eq!(fs::read_to_string(tarball).unwrap(), "corrupt fixture");
}
#[test]
fn records_bounded_installation_marker_digests_without_executing_source() {
    let f = Fixture::new();
    for path in [
        "sdk/ndk/28.2.13676358/source.properties",
        "sdk/platforms/android-37.2/package.xml",
        "jdk/release",
    ] {
        let path = f.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "synthetic installation marker").unwrap();
    }
    let plan = preflight_at(
        &f.source,
        REV,
        Abis::Arm64,
        &f.root,
        &f.root.join("sdk"),
        &f.root.join("jdk"),
    )
    .unwrap();
    assert!(
        plan.toolchain_markers
            .iter()
            .all(|marker| marker.present && marker.sha256.as_ref().unwrap().len() == 64)
    );
    // Fixture gradlew exits 1, and wrapper is not a JAR. A plan succeeds because
    // neither it nor any other submitted source/toolchain program was executed.
    assert!(!plan.executable);
}
