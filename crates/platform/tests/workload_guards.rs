//! Guardian SIGKILL with an armed wrapper, including a blocked pre-exec child.
//! Synthetic descriptors/groups only; no worker, admission or ledger activation.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use task_manager_platform::guarded_group::START;
use task_manager_platform::owned_group::OwnedChildGroup;
use task_manager_platform::workload_guard::{READY, Report, read_report, serve};

static SERIAL: Mutex<()> = Mutex::new(());
const MODE: &str = "TASKS_WORKLOAD_GUARD_FIXTURE";
const ROOT: &str = "TASKS_WORKLOAD_GUARD_ROOT";
const FD: &str = "TASKS_WORKLOAD_GUARD_FD";
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tasks-workload-watch-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct TestChild(Child);
impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn until(mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(6);
    while !ready() {
        assert!(Instant::now() < end, "fixture deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn helper(mode: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "subprocess_fixture", "--nocapture"])
        .env(MODE, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    cmd
}
fn busy(file: &File) -> bool {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return false;
    }
    assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
    true
}
fn report_text(report: Report) -> String {
    match report {
        Report::Exited(status) => format!("code={:?} signal={:?}", status.code(), status.signal()),
        Report::ExecutionError(errno) => format!("execution-error={errno}"),
    }
}

#[test]
fn subprocess_fixture() {
    let Ok(role) = std::env::var(MODE) else {
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os(ROOT).unwrap());
    let fd: i32 = std::env::var(FD).unwrap().parse().unwrap();
    assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
    // Bounds fixtures even if the entire test tree is accidentally interrupted.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(15));
        std::process::exit(99);
    });
    match role.as_str() {
        "combined-controller" => {
            let phase = std::env::var("TASKS_WORKLOAD_GUARD_PHASE").unwrap();
            let (mut controller, guardian) = UnixStream::pair().unwrap();
            controller
                .set_read_timeout(Some(Duration::from_secs(6)))
                .unwrap();
            let mut cmd = helper("combined-guardian");
            cmd.stdin(Stdio::from(OwnedFd::from(guardian)));
            let mut child = TestChild(cmd.spawn().unwrap());
            let mut ready = [0];
            controller.read_exact(&mut ready).unwrap();
            assert_eq!(ready, [READY]);
            if phase != "before" {
                controller.write_all(&[START]).unwrap();
                match phase.as_str() {
                    "spawn" => until(|| {
                        fs::metadata(root.join("spawn-blocked")).is_ok_and(|m| m.len() == 1)
                    }),
                    "running" | "guardian-running" => {
                        until(|| root.join("workload-ready").exists())
                    }
                    "partial" => until(|| root.join("partial-report").exists()),
                    _ => {}
                }
            }
            if matches!(phase.as_str(), "normal" | "exec-error" | "early-exit") {
                assert!(child.0.wait().unwrap().success());
                fs::write(root.join("finished"), b"cleaned").unwrap();
                return;
            }
            fs::write(root.join("guardian-ready"), b"armed").unwrap();
            if phase == "guardian-running" {
                until(|| root.join("kill-guardian").exists());
                child.0.kill().unwrap();
                assert_eq!(child.0.wait().unwrap().signal(), Some(libc::SIGKILL));
                return;
            }
            loop {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        "combined-guardian" => {
            use task_manager_platform::supervisor::{self, Reason};
            let controller =
                UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
            let phase = std::env::var("TASKS_WORKLOAD_GUARD_PHASE").unwrap();
            let (guardian, wrapper) = UnixStream::pair().unwrap();
            let mut cmd = helper(match phase.as_str() {
                "partial" => "partial-wrapper",
                "early-exit" => "exiting-wrapper",
                _ => "wrapper",
            });
            cmd.stdin(Stdio::from(OwnedFd::from(wrapper)));
            let group = OwnedChildGroup::spawn(&mut cmd).unwrap();
            let outcome =
                supervisor::supervise(group, guardian, controller, Duration::from_millis(20));
            let phase = std::env::var("TASKS_WORKLOAD_GUARD_PHASE").unwrap();
            if matches!(phase.as_str(), "normal" | "exec-error") {
                assert_eq!(outcome.reason, Reason::CommandReported);
                assert!(outcome.error.is_none());
                outcome.cleanup.unwrap();
                fs::write(root.join("report"), report_text(outcome.report.unwrap())).unwrap();
            } else {
                assert_eq!(
                    outcome.reason,
                    if phase == "early-exit" {
                        Reason::WrapperExited
                    } else {
                        Reason::ControllerLost
                    }
                );
                if phase == "early-exit" {
                    assert!(outcome.report.is_none());
                    eprintln!(
                        "synthetic early-wrapper cleanup (not completion): {:?}",
                        outcome.cleanup
                    );
                }
                if phase == "partial" {
                    assert!(outcome.report.is_none());
                }
                // Cleanup errors must stay errors, never imply success/replay.
                fs::write(
                    root.join("interrupted"),
                    format!("{:?} cleanup={:?}", outcome.reason, outcome.cleanup),
                )
                .unwrap();
            }
        }
        "partial-wrapper" | "exiting-wrapper" => {
            // Deliberately incomplete protocol peer, not a real workload watcher.
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            let mut control =
                UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
            control.write_all(&[READY]).unwrap();
            let mut start = [0];
            control.read_exact(&mut start).unwrap();
            assert_eq!(start, [START]);
            if role == "exiting-wrapper" {
                return;
            }
            control.write_all(b"D\0").unwrap();
            fs::write(root.join("partial-report"), b"partial").unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        "guardian" => {
            let phase = std::env::var("TASKS_WORKLOAD_GUARD_PHASE").unwrap();
            let (mut guardian, wrapper) = UnixStream::pair().unwrap();
            assert_ne!(
                unsafe { libc::fcntl(guardian.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            guardian
                .set_read_timeout(Some(Duration::from_secs(6)))
                .unwrap();
            let mut cmd = helper("wrapper");
            cmd.stdin(Stdio::from(OwnedFd::from(wrapper)));
            if phase == "bad-grant" {
                // No command is ever granted: this fixture needs only a private
                // direct-child wait, not a signal to an already-empty group.
                let mut child = TestChild(cmd.process_group(0).spawn().unwrap());
                let mut ready = [0];
                guardian.read_exact(&mut ready).unwrap();
                assert_eq!(ready, [READY]);
                guardian.write_all(b"X").unwrap();
                assert_eq!(child.0.wait().unwrap().signal(), Some(libc::SIGKILL));
                fs::write(root.join("finished"), b"rejected").unwrap();
                return;
            }
            let group = OwnedChildGroup::spawn(&mut cmd).unwrap();
            let mut ready = [0];
            guardian.read_exact(&mut ready).unwrap();
            assert_eq!(ready, [READY]);
            if phase != "before" {
                guardian.write_all(&[START]).unwrap();
                match phase.as_str() {
                    "spawn" => until(|| {
                        fs::metadata(root.join("spawn-blocked")).is_ok_and(|m| m.len() == 1)
                    }),
                    "running" | "term-window" => until(|| root.join("workload-ready").exists()),
                    _ => {}
                }
                if phase == "term-window" {
                    // Test-only signal of our exclusive unreaped direct child's group.
                    assert!(!group.has_exited().unwrap());
                    assert_eq!(unsafe { libc::killpg(group.id() as i32, libc::SIGTERM) }, 0);
                }
                if matches!(
                    phase.as_str(),
                    "terminal" | "normal" | "exec-error" | "term-window"
                ) {
                    let report = read_report(&mut guardian).unwrap();
                    let text = report_text(report);
                    if phase == "term-window" {
                        assert!(
                            text.contains("signal=Some(15)"),
                            "caught TERM must reset on workload exec: {text}"
                        );
                    }
                    fs::write(root.join("report"), text).unwrap();
                    // Wrapper is still alive AFTER command result; never disarmed.
                    assert!(!group.has_exited().unwrap());
                    if matches!(phase.as_str(), "normal" | "exec-error") {
                        group.terminate(Duration::from_millis(20)).unwrap();
                        fs::write(root.join("finished"), b"cleaned").unwrap();
                        return;
                    }
                }
            }
            fs::write(root.join("guardian-ready"), b"armed").unwrap();
            // Harness SIGKILL: no Rust Drop/reaper cleanup can save the workload.
            loop {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        "wrapper" => {
            let phase = std::env::var("TASKS_WORKLOAD_GUARD_PHASE").unwrap();
            let control = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
            let mode = if matches!(phase.as_str(), "terminal" | "normal") {
                "exiting"
            } else if phase == "term-window" {
                "term-root"
            } else {
                "resistant"
            };
            let mut cmd = if phase == "exec-error" {
                Command::new(root.join("nonexistent-command"))
            } else {
                helper(mode)
            };
            // Deliberately unsafe builder settings: serve must override both.
            if phase == "running" {
                cmd.stdin(Stdio::inherit()).process_group(0);
            }
            cmd.env(
                "TASKS_WORKLOAD_EXPECTED_PGID",
                unsafe { libc::getpid() }.to_string(),
            );
            if phase == "spawn" {
                let notify = File::create(root.join("spawn-blocked")).unwrap();
                // File opened before fork; only async-signal-safe syscalls below.
                unsafe {
                    cmd.pre_exec(move || {
                        let byte = b"1";
                        if libc::write(notify.as_raw_fd(), byte.as_ptr().cast(), 1) != 1 {
                            return Err(io::Error::last_os_error());
                        }
                        loop {
                            libc::pause();
                        }
                    });
                }
            }
            serve(&mut cmd, control).unwrap();
            panic!("armed wrapper must never return");
        }
        "resistant" | "exiting" | "term-root" => {
            let expected: i32 = std::env::var("TASKS_WORKLOAD_EXPECTED_PGID")
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::getpgrp() }, expected);
            assert_eq!(std::io::stdin().read(&mut [0]).unwrap(), 0);
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join("entered-once"))
                .unwrap();
            if role == "resistant" {
                unsafe {
                    libc::signal(libc::SIGTERM, libc::SIG_IGN);
                }
            }
            let _child = TestChild(helper("descendant").spawn().unwrap());
            until(|| root.join("descendant-ready").exists());
            fs::write(root.join("workload-ready"), b"ready").unwrap();
            if role == "exiting" {
                std::process::exit(7);
            }
            loop {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        "descendant" => {
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            fs::write(root.join("descendant-ready"), b"ready").unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        "refuse-nonleader" | "refuse-inheritable" => {
            let (_peer, control) = UnixStream::pair().unwrap();
            if role == "refuse-inheritable" {
                assert_eq!(unsafe { libc::getpid() }, unsafe { libc::getpgrp() });
                assert_eq!(
                    unsafe { libc::fcntl(control.as_raw_fd(), libc::F_SETFD, 0) },
                    0
                );
            } else {
                assert_ne!(unsafe { libc::getpid() }, unsafe { libc::getpgrp() });
            }
            let mut cmd = helper("resistant");
            let error = serve(&mut cmd, control).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
        _ => panic!("unknown role"),
    }
}

fn phase_case(phase: &str) {
    run_case(phase, "guardian");
}
fn run_case(phase: &str, role: &str) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let lock = fixture.0.join("stable-slot.lock");
    let lease = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&lock)
        .unwrap();
    let key = (
        lease.metadata().unwrap().dev(),
        lease.metadata().unwrap().ino(),
    );
    assert!(!busy(&lease));
    let fd = unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_DUPFD, 64) };
    assert!(fd >= 64);
    let inherited = unsafe { File::from_raw_fd(fd) };
    let mut cmd = helper(role);
    cmd.env(ROOT, &fixture.0)
        .env(FD, fd.to_string())
        .env("TASKS_WORKLOAD_GUARD_PHASE", phase);
    let mut guardian = TestChild(cmd.spawn().unwrap());
    drop(inherited);
    drop(lease);
    let normal = matches!(phase, "normal" | "exec-error" | "bad-grant" | "early-exit");
    until(|| {
        fixture
            .0
            .join(if normal { "finished" } else { "guardian-ready" })
            .exists()
    });
    let observer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock)
        .unwrap();
    if !normal {
        assert!(busy(&observer));
    }
    let mut outside_cmd = Command::new("/bin/sleep");
    outside_cmd
        .arg("15")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let outside = OwnedChildGroup::spawn(&mut outside_cmd).unwrap();
    if phase == "guardian-running" {
        fs::write(fixture.0.join("kill-guardian"), b"kill").unwrap();
    } else if !normal {
        guardian.0.kill().unwrap();
    }
    let status = guardian.0.wait().unwrap();
    if normal || phase == "guardian-running" {
        assert!(status.success());
    } else {
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
    until(|| !busy(&observer));
    if phase == "early-exit" {
        assert!(
            fs::read_to_string(fixture.0.join("interrupted"))
                .unwrap()
                .starts_with("WrapperExited")
        );
    }
    if phase == "guardian-running" {
        assert!(!fixture.0.join("report").exists());
        assert!(!fixture.0.join("interrupted").exists());
    }
    if role == "combined-controller" && !normal && phase != "guardian-running" {
        assert!(
            fs::read_to_string(fixture.0.join("interrupted"))
                .unwrap()
                .starts_with("ControllerLost")
        );
    }
    assert!(!outside.has_exited().unwrap());
    let metadata = fs::metadata(&lock).unwrap();
    assert_eq!(key, (metadata.dev(), metadata.ino()));
    if matches!(
        phase,
        "before" | "spawn" | "bad-grant" | "exec-error" | "partial" | "early-exit"
    ) {
        assert!(!fixture.0.join("entered-once").exists());
    } else {
        assert!(fixture.0.join("entered-once").exists());
    }
    if matches!(phase, "terminal" | "normal") {
        assert!(
            fs::read_to_string(fixture.0.join("report"))
                .unwrap()
                .contains("code=Some(7)")
        );
    }
    if phase == "exec-error" {
        assert_eq!(
            fs::read_to_string(fixture.0.join("report")).unwrap(),
            format!("execution-error={}", libc::ENOENT)
        );
    }
}
#[test]
fn guardian_sigkill_before_grant_prevents_entry() {
    phase_case("before");
}
#[test]
fn guardian_sigkill_during_pre_exec_releases_lease_without_entry() {
    phase_case("spawn");
}
#[test]
fn guardian_sigkill_during_execution_cleans_resistant_descendant() {
    phase_case("running");
}
#[test]
fn guardian_sigkill_after_command_exit_still_cleans_descendant() {
    phase_case("terminal");
}
#[test]
fn guardian_sigkill_during_term_grace_keeps_watcher_alive() {
    phase_case("term-window");
}
#[test]
fn normal_cleanup_preserves_command_status_not_wrapper_sigkill() {
    phase_case("normal");
}
#[test]
fn invalid_grant_kills_wrapper_without_command_entry() {
    phase_case("bad-grant");
}
#[test]
fn spawn_error_is_reported_without_execution_or_early_wrapper_exit() {
    phase_case("exec-error");
}
#[test]
fn nonleader_is_refused_without_signalling_any_group() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let mut cmd = helper("refuse-nonleader");
    cmd.env(ROOT, &fixture.0).env(FD, "0");
    assert!(cmd.status().unwrap().success());
    assert!(!fixture.0.join("entered-once").exists());
}
#[test]
fn inheritable_endpoint_is_refused_before_arming() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let mut cmd = helper("refuse-inheritable");
    cmd.env(ROOT, &fixture.0).env(FD, "0");
    // This dedicated rejection fixture starts no command/descendants.
    assert!(cmd.process_group(0).status().unwrap().success());
    assert!(!fixture.0.join("entered-once").exists());
}
#[test]
fn combined_controller_loss_before_grant_prevents_entry() {
    run_case("before", "combined-controller");
}
#[test]
fn combined_controller_loss_during_pre_exec_releases_lease() {
    run_case("spawn", "combined-controller");
}
#[test]
fn combined_controller_loss_cleans_resistant_descendant() {
    run_case("running", "combined-controller");
}
#[test]
fn combined_guardian_sigkill_leaves_no_report_and_wrapper_cleans_group() {
    run_case("guardian-running", "combined-controller");
}
#[test]
fn combined_normal_result_preserves_command_status() {
    run_case("normal", "combined-controller");
}
#[test]
fn combined_spawn_error_is_not_command_success() {
    run_case("exec-error", "combined-controller");
}
#[test]
fn early_wrapper_exit_and_native_cleanup_error_are_not_completion() {
    run_case("early-exit", "combined-controller");
}
#[test]
fn partial_wrapper_report_does_not_block_controller_loss() {
    run_case("partial", "combined-controller");
}
#[test]
fn malformed_and_truncated_reports_are_not_terminal_results() {
    for frame in [
        vec![b'Z', 0, 0, 0, 0],
        vec![b'D', 0, 0, 0x13, 0x7f],
        vec![b'D', 255, 255, 255, 255],
        vec![b'D', 0],
    ] {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(&frame).unwrap();
        drop(writer);
        assert!(read_report(&mut reader).is_err());
    }
}
