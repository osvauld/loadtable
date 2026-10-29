use super::*;
use crate::RecipeKey as K;

fn a(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}
fn one(s: &str) -> K {
    let v = K::from_argv(&a(s), &[]);
    assert_eq!(v.len(), 1, "{s}");
    v.into_iter().next().unwrap()
}
fn shell(script: &str) -> Vec<K> {
    K::from_argv(&["bash".into(), "-c".into(), script.into()], &[])
}

#[test]
fn r120_j_flag_order_and_output_flags_are_ignored() {
    let base = one("cargo test -p b -p a --features x,y --release");
    assert_eq!(
        base,
        one("cargo test --release -j 8 -p a -p b --features y --features x")
    );
    assert_eq!(
        base,
        one(
            "cargo test -j8 --jobs=2 -pa -pb -F x -F y -r --color always -q --locked --message-format=json"
        )
    );
    assert_eq!(
        base.to_string(),
        "cargo test pkg=a,b feat=x,y profile=release"
    );
    assert_eq!(
        base,
        one(
            "cargo test --profile release -p a -p b --features=x,y --manifest-path /other/checkout/Cargo.toml"
        )
    );
}

#[test]
fn env_jobs_and_assignments_ignored() {
    let base = K::from_argv(
        &a("cargo test -p a"),
        &[("CARGO_BUILD_JOBS".into(), "4".into())],
    );
    assert_eq!(base, K::from_argv(&a("cargo test -p a"), &[]));
    assert_eq!(
        base,
        K::from_argv(&a("env FOO=1 BAR=2 cargo test -p a"), &[])
    );
    assert_eq!(
        base,
        K::from_argv(&a("RUST_LOG=debug cargo test -p a"), &[])
    );
}

#[test]
fn different_work_gives_different_keys() {
    let base = one("cargo test -p a --features x");
    for other in [
        "cargo test -p a --features y",
        "cargo test -p a",
        "cargo test -p a --features x --release",
        "cargo test -p a --features x --target aarch64-apple-darwin",
        "cargo test -p b --features x",
        "cargo check -p a --features x",
        "cargo test -p a --features x --lib",
        "cargo test -p a --features x --all-features",
        "cargo test -p a --features x --no-default-features",
        "cargo test -p a --features x --workspace",
        "cargo test -p a --features x -- lifecycle",
    ] {
        assert_ne!(base, one(other), "{other}");
    }
}

#[test]
fn selection_forms() {
    assert_eq!(
        one("cargo test --test calls --test gcall").selection,
        ["test:calls", "test:gcall"]
    );
    assert_eq!(
        one("cargo test --lib --bins --doc").selection,
        ["bins", "doc", "lib"]
    );
    let k = one("cargo test -p a -- --nocapture --test-threads=1 lifecycle --ignored");
    assert_eq!(k.selection, ["filter:lifecycle", "ignored"]);
    assert_eq!(k, one("cargo test -p a -- --ignored lifecycle"));
    assert_eq!(one("cargo test foo").selection, ["filter:foo"]);
}

#[test]
fn target_from_flag_or_env_and_toolchain() {
    let env = [(
        "CARGO_BUILD_TARGET".to_string(),
        "aarch64-apple-darwin".to_string(),
    )];
    let k = K::from_argv(&a("cargo check"), &env);
    assert_eq!(k[0], one("cargo check --target aarch64-apple-darwin"));
    assert_ne!(one("cargo +nightly test"), one("cargo test"));
}

#[test]
fn clippy_tail_kept_for_non_test_commands() {
    let k = one("cargo clippy --workspace --all-targets -- -D warnings");
    assert_eq!(k.rest, ["-D", "warnings"]);
    assert_eq!(k.kind(), Kind::Compile);
}

#[test]
fn shell_wrappers_unwrap_and_chains_give_one_key_each() {
    let direct = one("cargo test -p a");
    assert_eq!(shell("cargo test -p a"), vec![direct.clone()]);
    let chain =
        shell("cd /some/checkout && ulimit -n 65536; cargo test -j 6 -p a && cargo clippy -p a");
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0], direct);
    assert_eq!(chain[1].subcommand, "clippy");
    assert_eq!(
        K::from_argv(
            &["/bin/sh".into(), "-ec".into(), "cargo test -p a".into()],
            &[]
        ),
        vec![direct]
    );
}

#[test]
fn unsupported_shell_script_is_one_verbatim_key() {
    let v = shell("cargo test -p a | tee log");
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].tool, "bash");
    let v2 = shell("cd /x");
    assert_eq!(v2.len(), 1);
    assert_eq!(v2[0].tool, "bash");
}

#[test]
fn gradle_tasks_sorted_and_noise_ignored() {
    let base = one("./gradlew :app:assembleDebug :app:testDebugUnitTest");
    assert_eq!(
        base,
        one(
            "gradle --console=plain :app:testDebugUnitTest -q --info :app:assembleDebug --no-daemon"
        )
    );
    assert_eq!(
        base,
        one(
            "/x/y/gradlew --console plain --max-workers 4 :app:assembleDebug :app:testDebugUnitTest -p android"
        )
    );
    assert_eq!(
        base.to_string(),
        "gradle sel=:app:assembleDebug,:app:testDebugUnitTest"
    );
    assert_ne!(
        base,
        one("./gradlew :app:assembleDebug :app:testDebugUnitTest -Pabis=arm64-v8a")
    );
    assert_ne!(
        base,
        one("./gradlew :app:assembleDebug :app:testDebugUnitTest -x lint")
    );
}

#[test]
fn gradle_properties_that_only_stamp_the_output_are_not_the_work() {
    let release = one("./gradlew :app:assemblePlayRelease -Pabis=arm64-v8a");
    assert_eq!(
        release,
        one(
            "./gradlew :app:assemblePlayRelease -Pabis=arm64-v8a -PversionCode=1632 \
             -PversionName=0.2.4 -PrequireReleaseSigning=true -Dfile.encoding=UTF-8"
        )
    );
    assert_ne!(
        release,
        one("./gradlew :app:assemblePlayRelease -Pabis=x86_64,arm64-v8a"),
        "the ABIs are work"
    );
}

#[test]
fn r828_a_session_id_in_a_path_is_not_part_of_the_work() {
    let a = one(
        "sh /tmp/claude-1000/-home-abe-x/4bf4aba5-6e93-471e-af39-e303d43bfca9/scratchpad/check.sh",
    );
    let b = one(
        "sh /tmp/claude-1000/-home-abe-x/0f3a8fa8-5bf9-4c19-92ca-6849a16f4b4d/scratchpad/check.sh",
    );
    assert_eq!(a, b);
    assert_eq!(
        a.to_string(),
        "sh /tmp/claude-1000/-home-abe-x/<id>/scratchpad/check.sh"
    );
    // a word that is not a path, and an id that is not a whole component, are left alone
    let id = "4bf4aba5-6e93-471e-af39-e303d43bfca9";
    assert_eq!(one(&format!("make {id}")).rest, [id]);
    assert_eq!(
        one(&format!("make out/{id}.log")).rest,
        [format!("out/{id}.log")]
    );
    // cargo's and gradle's arguments are parsed, not kept, so they are not touched
    assert_eq!(
        one(&format!("cargo run -- /x/{id}/y")).rest,
        [format!("/x/{id}/y")]
    );
}

#[test]
fn other_tools_keep_args_verbatim() {
    let k = one("/usr/bin/make -j4 all");
    assert_eq!((k.tool.as_str(), k.rest.len()), ("make", 2));
    assert_eq!(k.to_string(), "make -j4 all");
    assert_eq!(k.kind(), Kind::Run);
    assert!(K::from_argv(&[], &[]).is_empty());
}

#[test]
fn probes_and_kinds() {
    for p in ["true", "echo hi", "sleep 5", "false"] {
        assert_eq!(one(p).kind(), Kind::Probe, "{p}");
    }
    assert_eq!(shell("exit 3")[0].kind(), Kind::Probe);
    assert_eq!(
        K::from_argv(&["sh".into(), "-c".into(), "exit 3".into()], &[])[0].kind(),
        Kind::Probe
    );
    assert_eq!(one("cargo test").kind(), Kind::Test);
    assert_eq!(one("cargo test --no-run").kind(), Kind::Compile);
    assert_eq!(one("cargo build --release").kind(), Kind::Compile);
    assert_eq!(one("cargo run").kind(), Kind::Run);
    assert_eq!(
        one("./gradlew :app:connectedDebugAndroidTest").kind(),
        Kind::Device
    );
    assert_eq!(one("./gradlew :app:testDebugUnitTest").kind(), Kind::Test);
    assert_eq!(one("./gradlew assembleDebug").kind(), Kind::Package);
    assert_eq!(one("./gradlew clean").kind(), Kind::Compile);
}

#[test]
fn display_is_stable_and_nextest_has_subcommand() {
    assert_eq!(
        one("cargo test -p a --workspace --all-features --target t --test x").to_string(),
        "cargo test pkg=a profile=dev target=t sel=test:x flags=all-features,workspace"
    );
    assert_eq!(one("cargo nextest run -p a").subcommand, "nextest run");
    assert_eq!(one("cargo nextest run -p a").kind(), Kind::Test);
}

#[test]
fn r676_a_chain_is_keyed_by_the_whole_chain_not_its_first_command() {
    let hk = |s: &str| K::history_key(&["sh".into(), "-c".into(), s.into()], &[]).unwrap();
    let alone = hk("build_core.sh");
    let chain = hk("build_core.sh && xcodegen && run_sim.sh");
    assert_ne!(alone, chain);
    assert_eq!(alone, "build_core.sh");
    assert_eq!(chain, "build_core.sh && xcodegen && run_sim.sh");
    // the same chain spelled with a setup `cd` and other whitespace is the same key
    assert_eq!(chain, hk("cd /x; build_core.sh  &&  xcodegen\n run_sim.sh"));
    // a plain command keeps its own recipe string
    assert_eq!(
        K::history_key(&a("cargo test -p x"), &[]).unwrap(),
        one("cargo test -p x").to_string()
    );
    assert_eq!(K::history_key(&[], &[]), None);
}
