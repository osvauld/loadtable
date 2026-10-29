//! The Seatbelt profile run for real under `/usr/bin/sandbox-exec` (macOS only): what a
//! confined job can open, list and resolve (R-821, R-831).
#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::process::Command;
use tasks_platform::confine::{Confine, wrap_macos};

/// A fake home folder: a private file, a store three folders deep, a cache right under it.
struct Home {
    root: PathBuf,
}

impl Home {
    fn new(name: &str) -> Home {
        let base = std::env::temp_dir().canonicalize().unwrap();
        let root = base.join(format!("lt-confine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".local/state/loadtable/peer-store/tree/crates/a"))
            .unwrap();
        std::fs::write(
            root.join(".local/state/loadtable/peer-store/tree/crates/a/Cargo.toml"),
            "x",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("build-cache")).unwrap();
        std::fs::create_dir_all(root.join("Pictures")).unwrap();
        std::fs::write(root.join("Pictures/private.txt"), "no").unwrap();
        Home { root }
    }

    fn store(&self) -> PathBuf {
        self.root.join(".local/state/loadtable/peer-store")
    }

    fn confine(&self) -> Confine {
        Confine {
            helper: vec![],
            closed: vec![self.root.clone()],
            write: vec![self.root.join("build-cache"), self.store()],
            read: vec![],
        }
    }

    /// Runs `sh -c script` confined, in `cwd`; true when it exits 0.
    fn run(&self, cwd: &Path, script: &str) -> bool {
        let argv = wrap_macos(
            &self.confine(),
            &["/bin/sh".into(), "-c".into(), script.into()],
        );
        Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(cwd)
            .status()
            .unwrap()
            .success()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn r831_a_confined_job_resolves_its_own_folder_and_globs_inside_it() {
    let h = Home::new("resolve");
    let tree = h.store().join("tree");
    assert!(
        h.run(&tree, "realpath \"$PWD\" >/dev/null"),
        "realpath of the synced copy"
    );
    assert!(
        h.run(&tree, "/bin/pwd -P >/dev/null"),
        "pwd -P walks every parent"
    );
    assert!(
        h.run(&tree, "cat \"$PWD\"/crates/*/Cargo.toml >/dev/null"),
        "a glob from the absolute path"
    );
    assert!(
        h.run(&tree, "echo y > \"$PWD/new\""),
        "the store is writable"
    );
}

#[test]
fn r831_the_folders_above_stay_unlisted_and_the_rest_of_home_closed() {
    let h = Home::new("closed");
    let tree = h.store().join("tree");
    let root = h.root.display();
    assert!(
        !h.run(&tree, &format!("ls '{root}/.local' >/dev/null")),
        "a folder above is not listed"
    );
    assert!(
        !h.run(&tree, &format!("ls '{root}' >/dev/null")),
        "home is not listed"
    );
    assert!(
        !h.run(
            &tree,
            &format!("cat '{root}/Pictures/private.txt' >/dev/null")
        ),
        "a private file stays closed"
    );
    assert!(
        !h.run(&tree, &format!("ls '{root}/Pictures' >/dev/null")),
        "a private folder stays closed"
    );
    assert!(
        h.run(&tree, &format!("ls '{root}/build-cache' >/dev/null")),
        "the cache is open"
    );
}
