//! `loadtabled confine` really closes the home folder (R-821): Landlock, checked against the
//! kernel, not against our own reading of the rules.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn home() -> PathBuf {
    let h = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("confine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&h);
    std::fs::create_dir_all(h.join("Photos")).unwrap();
    std::fs::create_dir_all(h.join("build-cache")).unwrap();
    std::fs::write(h.join("Photos/me.jpg"), "private").unwrap();
    std::fs::write(h.join("build-cache/out.txt"), "built").unwrap();
    std::fs::canonicalize(&h).unwrap()
}

fn confined(home: &Path, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loadtabled"))
        .arg("confine")
        .arg("--closed")
        .arg(home)
        .arg("--write")
        .arg(home.join("build-cache"))
        .args(["--", "/bin/sh", "-c", script])
        .output()
        .unwrap()
}

#[test]
fn r821_a_confined_job_cannot_open_the_home_folder_but_can_use_the_cache() {
    let h = home();
    let probe = confined(&h, "true");
    let err = String::from_utf8_lossy(&probe.stderr);
    if err.contains("does not enforce Landlock") {
        eprintln!("skipped: this kernel has no Landlock ({err})");
        return;
    }
    assert!(probe.status.success(), "{err}");

    let read_private = confined(&h, &format!("cat {}/Photos/me.jpg", h.display()));
    assert!(!read_private.status.success(), "a photo was read");
    assert!(String::from_utf8_lossy(&read_private.stdout).is_empty());

    let list_home = confined(&h, &format!("ls {}", h.display()));
    assert!(!list_home.status.success(), "the home folder was listed");

    let write_home = confined(&h, &format!("echo x > {}/new.txt", h.display()));
    assert!(!write_home.status.success());
    assert!(
        !h.join("new.txt").exists(),
        "a file was written into the home folder"
    );

    let use_cache = confined(
        &h,
        &format!(
            "cat {0}/build-cache/out.txt && echo more > {0}/build-cache/new.txt",
            h.display()
        ),
    );
    assert!(
        use_cache.status.success(),
        "{}",
        String::from_utf8_lossy(&use_cache.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&use_cache.stdout), "built");
    assert!(h.join("build-cache/new.txt").exists());

    // the restriction survives into everything the job starts, and the system stays usable
    let nested = confined(
        &h,
        &format!(
            "sh -c 'cat {}/Photos/me.jpg' || ls /usr/bin >/dev/null",
            h.display()
        ),
    );
    assert!(nested.status.success());
    assert!(!String::from_utf8_lossy(&nested.stdout).contains("private"));
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn r821_a_bad_helper_line_runs_nothing() {
    let out = Command::new(env!("CARGO_BIN_EXE_loadtabled"))
        .args(["confine", "--", "/bin/sh", "-c", "echo ran"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("ran"));
}
