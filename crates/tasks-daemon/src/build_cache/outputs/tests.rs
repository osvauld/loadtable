use super::*;
use crate::testkit::TempDir;
use std::fs;

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

struct Fx {
    dir: TempDir,
}

impl Fx {
    fn new() -> Fx {
        Fx {
            dir: TempDir::new("outputs"),
        }
    }
    fn cache(&self) -> PathBuf {
        self.dir.path().join("cache")
    }
    fn store(&self) -> PathBuf {
        self.dir.path().join("state/peer-store")
    }
    /// A local checkout with a Cargo manifest and a `.git` directory.
    fn checkout(&self, name: &str) -> PathBuf {
        let root = fs::canonicalize(self.dir.path())
            .unwrap()
            .join("work")
            .join(name);
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        root
    }
    /// A synced copy of a peer's checkout.
    fn synced(&self, peer: &str, id: &str) -> PathBuf {
        let tree = self.store().join(peer).join(id).join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("Cargo.toml"), "[workspace]\n").unwrap();
        tree
    }
    fn plan(&self, cwd: &Path, a: &[&str]) -> OutputPlan {
        plan(&self.cache(), &self.store(), cwd, &argv(a))
    }
}

fn link_target(link: &Path) -> PathBuf {
    fs::read_link(link).unwrap()
}

// R-402: `target` becomes a symlink into the cache root, keyed by the canonical checkout path.
#[test]
fn r402_a_local_checkout_gets_a_target_link_keyed_by_its_canonical_path() {
    let fx = Fx::new();
    let root = fx.checkout("app");
    let placed = fx.plan(&root, &["cargo", "build"]).apply();
    assert_eq!(placed.created, 1);
    let real = link_target(&root.join("target"));
    let key = sha_hex(root.as_os_str().as_encoded_bytes());
    assert_eq!(
        real,
        fx.cache().join("tasks2/checkouts").join(key).join("target")
    );
    assert!(real.is_dir(), "the cache directory exists");
    // a symlinked path to the same checkout lands on the same directory
    let alias = fx.dir.path().join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    let again = fx.plan(&alias, &["cargo", "build"]).apply();
    assert_eq!(again.created, 0);
    assert_eq!(again.links, vec![(root.join("target"), real)]);
}

// R-402: a copy under the peer store is keyed by (origin peer, checkout id), not by its path.
#[test]
fn r402_a_synced_copy_is_keyed_by_peer_and_checkout_and_the_link_sits_in_the_tree_root() {
    let fx = Fx::new();
    let tree = fx.synced("laptop-node", "aa11");
    let sub = tree.join("crates/x");
    fs::create_dir_all(&sub).unwrap();
    let placed = fx.plan(&sub, &["cargo", "test"]).apply();
    assert_eq!(placed.created, 1, "{placed:?}");
    let real = link_target(&fs::canonicalize(&tree).unwrap().join("target"));
    assert_eq!(
        real,
        fx.cache()
            .join("tasks2/checkouts/peer/laptop-node/aa11/target")
    );
    // another peer's copy of a same-named checkout never shares it
    let other = fx.synced("mac-node", "aa11");
    fx.plan(&other, &["cargo", "test"]).apply();
    assert_ne!(link_target(&other.join("target")), real);
}

// R-402: after retention deletes the copy and a later sync recreates it, the output is still there.
#[test]
fn r402_a_recreated_copy_finds_the_same_output_directory() {
    let fx = Fx::new();
    let tree = fx.synced("laptop-node", "aa11");
    let real = {
        fx.plan(&tree, &["cargo", "build"]).apply();
        let r = link_target(&fs::canonicalize(&tree).unwrap().join("target"));
        fs::write(r.join("artifact"), "warm").unwrap();
        r
    };
    fs::remove_dir_all(fx.store().join("laptop-node/aa11")).unwrap();
    let tree = fx.synced("laptop-node", "aa11");
    fx.plan(&tree, &["cargo", "build"]).apply();
    let again = link_target(&fs::canonicalize(&tree).unwrap().join("target"));
    assert_eq!(again, real);
    assert_eq!(fs::read_to_string(again.join("artifact")).unwrap(), "warm");
}

// R-403 (earlier release): an existing entry is never replaced, moved or deleted.
#[test]
fn r403_an_existing_directory_is_preserved_and_reported_not_cache_backed() {
    let fx = Fx::new();
    let root = fx.checkout("app");
    fs::create_dir_all(root.join("target/debug")).unwrap();
    fs::write(root.join("target/debug/old"), "local").unwrap();
    let plan = fx.plan(&root, &["cargo", "build"]);
    let placed = plan.apply();
    assert_eq!(placed.created, 0);
    assert_eq!(placed.preserved, vec![root.join("target")]);
    assert!(placed.links.is_empty());
    assert!(
        !fs::symlink_metadata(root.join("target"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(root.join("target/debug/old")).unwrap(),
        "local"
    );
    assert!(
        !fx.cache().join("tasks2").exists(),
        "nothing created in the cache"
    );
}

#[test]
fn r403_an_existing_symlink_or_file_is_left_exactly_as_it_is() {
    let fx = Fx::new();
    let root = fx.checkout("app");
    let elsewhere = fx.dir.path().join("mine");
    fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join("target")).unwrap();
    let placed = fx.plan(&root, &["cargo", "build"]).apply();
    assert_eq!(placed.created, 0);
    assert_eq!(placed.links, vec![(root.join("target"), elsewhere.clone())]);
    assert_eq!(link_target(&root.join("target")), elsewhere);
    // a plain file named target is preserved too
    let other = fx.checkout("other");
    fs::write(other.join("target"), "file").unwrap();
    let placed = fx.plan(&other, &["cargo", "build"]).apply();
    assert_eq!(placed.preserved, vec![other.join("target")]);
    assert_eq!(fs::read_to_string(other.join("target")).unwrap(), "file");
}

// R-403: a placement that races another keeps the winner.
#[test]
fn r403_a_racing_placement_keeps_the_winner() {
    let fx = Fx::new();
    let root = fx.checkout("app");
    let plan = fx.plan(&root, &["cargo", "build"]);
    // the other placer links first, to a directory of its own
    let winner = fx.dir.path().join("winner");
    fs::create_dir_all(&winner).unwrap();
    std::os::unix::fs::symlink(&winner, root.join("target")).unwrap();
    let placed = plan.apply();
    assert_eq!(placed.created, 0);
    assert_eq!(placed.links, vec![(root.join("target"), winner.clone())]);
    assert_eq!(link_target(&root.join("target")), winner);
    // and when the winner left a real directory, it is kept
    let other = fx.checkout("other");
    let plan = fx.plan(&other, &["cargo", "build"]);
    fs::create_dir_all(other.join("target")).unwrap();
    let placed = plan.apply();
    assert_eq!(placed.preserved, vec![other.join("target")]);
}

// R-403: a link whose cache directory was cleaned away gets the directory back, nothing else.
#[test]
fn r403_a_dangling_cache_link_gets_its_directory_recreated_and_stays_a_needed_output() {
    let fx = Fx::new();
    let root = fx.checkout("app");
    fx.plan(&root, &["cargo", "build"]).apply();
    let real = link_target(&root.join("target"));
    fs::remove_dir_all(&real).unwrap();
    let plan = fx.plan(&root, &["cargo", "build"]);
    assert!(
        plan.needs_new_output(),
        "the cache disk will get new output"
    );
    let placed = plan.apply();
    assert_eq!(placed.created, 0);
    assert!(real.is_dir());
    assert_eq!(link_target(&root.join("target")), real);
}

// R-404: only a job that needs NEW cache output waits for the disk reserve.
#[test]
fn r404_new_output_is_needed_until_the_links_exist() {
    let fx = Fx::new();
    let root = fx.checkout("app");
    let plan = fx.plan(&root, &["cargo", "build"]);
    assert!(plan.needs_new_output(), "a link is still to be made");
    plan.apply();
    assert!(!fx.plan(&root, &["cargo", "build"]).needs_new_output());
    // a kept local directory writes on its own disk, not the cache disk
    let local = fx.checkout("local");
    fs::create_dir_all(local.join("target")).unwrap();
    assert!(!fx.plan(&local, &["cargo", "build"]).needs_new_output());
    // no checkout found: the plan cannot tell, so the job is treated as writing
    let bare = fx.dir.path().join("bare");
    fs::create_dir_all(&bare).unwrap();
    assert!(fx.plan(&bare, &["cargo", "build"]).needs_new_output());
}

// The earlier release chose links by program name.
#[test]
fn r402_links_follow_the_program_not_just_the_checkout() {
    let fx = Fx::new();
    let root = fx.checkout("app");
    fs::create_dir_all(root.join("android/app")).unwrap();
    fs::write(root.join("android/settings.gradle"), "").unwrap();
    // npm and friends get nothing
    let p = fx.plan(&root, &["npm", "run", "build"]).apply();
    assert!(p.links.is_empty() && p.preserved.is_empty());
    assert!(fs::symlink_metadata(root.join("target")).is_err());
    // cargo (also behind transparent env assignments) gets only target
    let p = fx.plan(&root, &["env", "A=1", "cargo", "check"]).apply();
    assert_eq!(p.created, 1);
    assert!(fs::symlink_metadata(root.join("android/build")).is_err());
    // a shell may run either, so it gets every link
    let p = fx
        .plan(&root, &["sh", "-c", "./gradlew assembleDebug"])
        .apply();
    assert_eq!(p.links.len(), 3, "{p:?}");
    assert!(root.join("android/app/build").is_symlink());
    assert!(root.join("android/build").is_symlink());
}

#[test]
fn r402_an_independent_nested_workspace_gets_its_own_target() {
    let fx = Fx::new();
    let root = fx.checkout("app");
    let nested = root.join("tools/gen");
    fs::create_dir_all(nested.join("src")).unwrap();
    fs::write(nested.join("Cargo.toml"), "[workspace]\n").unwrap();
    // the root workspace excludes it, so it is its own workspace
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nexclude = [\"tools/gen\"]\n",
    )
    .unwrap();
    let p = fx.plan(&nested.join("src"), &["cargo", "build"]).apply();
    assert_eq!(p.created, 2, "{p:?}");
    let a = link_target(&root.join("target"));
    let b = link_target(&nested.join("target"));
    assert_ne!(a, b);
    assert!(b.starts_with(fx.cache().join("tasks2/checkouts")));
}

#[test]
fn r402_a_missing_working_directory_is_a_note_and_not_a_failure() {
    let fx = Fx::new();
    let p = fx.plan(&fx.dir.path().join("gone"), &["cargo", "build"]);
    let placed = p.apply();
    assert!(placed.links.is_empty());
    assert!(placed.notes[0].contains("cannot resolve the working directory"));
}

// R-405: local.properties is written from the machine's SDK, never over an existing file.
mod android {
    use super::*;

    fn project(fx: &Fx) -> PathBuf {
        let p = fx.dir.path().join("w/android");
        fs::create_dir_all(p.join("app")).unwrap();
        fs::write(p.join("settings.gradle.kts"), "").unwrap();
        p
    }
    fn sdk(fx: &Fx) -> String {
        let s = fx.dir.path().join("sdk");
        fs::create_dir_all(&s).unwrap();
        s.to_string_lossy().into_owned()
    }

    #[test]
    fn r405_local_properties_is_written_from_android_home() {
        let fx = Fx::new();
        let p = project(&fx);
        let sdk = sdk(&fx);
        let line =
            ensure_android_local_properties(&argv(&["./gradlew", "build"]), &p.join("app"), |n| {
                (n == "ANDROID_HOME").then(|| sdk.clone())
            })
            .unwrap()
            .unwrap();
        assert!(line.contains("ANDROID_HOME"), "{line}");
        let body = fs::read_to_string(p.join("local.properties")).unwrap();
        assert!(body.contains(&format!("sdk.dir={sdk}\n")), "{body}");
    }

    #[test]
    fn r405_sdk_root_is_the_second_choice_and_a_missing_directory_is_skipped() {
        let fx = Fx::new();
        let p = project(&fx);
        let sdk = sdk(&fx);
        ensure_android_local_properties(&argv(&["gradle", "build"]), &p, |n| match n {
            "ANDROID_HOME" => Some("/no/such/sdk".into()),
            "ANDROID_SDK_ROOT" => Some(sdk.clone()),
            _ => None,
        })
        .unwrap();
        assert!(
            fs::read_to_string(p.join("local.properties"))
                .unwrap()
                .contains(&sdk)
        );
    }

    #[test]
    fn r405_an_existing_file_is_never_touched() {
        let fx = Fx::new();
        let p = project(&fx);
        fs::write(p.join("local.properties"), "sdk.dir=/mine\n").unwrap();
        let sdk = sdk(&fx);
        let r = ensure_android_local_properties(&argv(&["./gradlew"]), &p, |_| Some(sdk.clone()));
        assert_eq!(r.unwrap(), None);
        assert_eq!(
            fs::read_to_string(p.join("local.properties")).unwrap(),
            "sdk.dir=/mine\n"
        );
    }

    #[test]
    fn r405_no_sdk_fails_naming_the_file_and_other_commands_are_left_alone() {
        let fx = Fx::new();
        let p = project(&fx);
        let e = ensure_android_local_properties(&argv(&["./gradlew"]), &p, |_| None).unwrap_err();
        assert!(e.to_string().contains("local.properties"), "{e}");
        assert!(!p.join("local.properties").exists());
        assert_eq!(
            ensure_android_local_properties(&argv(&["cargo", "build"]), &p, |_| None).unwrap(),
            None
        );
    }
}
