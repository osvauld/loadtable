//! R-685: an idle `loadtabled` exits cleanly on SIGTERM, with its socket file removed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn r685_an_idle_tasks2d_exits_on_sigterm() {
    let dir = std::env::temp_dir().join(format!("r685-d-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = dir.join("state");
    let mut child = Command::new(env!("CARGO_BIN_EXE_loadtabled"))
        .arg("serve")
        .arg("--state-dir")
        .arg(&state)
        .arg("--config-dir")
        .arg(dir.join("config"))
        .arg("--lock-dir")
        .arg(dir.join("slots"))
        .arg("--cache-dir")
        .arg(dir.join("cache"))
        .args(["--machine", "sigbox"])
        .env("HOME", &dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = state.join("tasks.sock");
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "the daemon never listened");
        assert!(
            child.try_wait().unwrap().is_none(),
            "the daemon exited early"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the daemon ignored SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "a clean stop exits 0, got {status}");
    assert!(!socket.exists(), "the socket file is removed");
    let _ = std::fs::remove_dir_all(&dir);
}
