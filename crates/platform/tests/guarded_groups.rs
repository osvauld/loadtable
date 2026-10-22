//! Separate controller/guardian/workload processes. No worker/ledger/runtime bypass.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use task_manager_platform::guarded_group::{Reason, START, supervise};
use task_manager_platform::owned_group::OwnedChildGroup;

static SERIAL: Mutex<()> = Mutex::new(());
const MODE: &str = "TASKS_GUARD_FIXTURE_MODE";
const ROOT: &str = "TASKS_GUARD_FIXTURE_ROOT";
const FD: &str = "TASKS_GUARD_FIXTURE_FD";

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tasks-guard-{}-{stamp}-{}",
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
struct Controller(Child);
impl Drop for Controller {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn until(mut done: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(6);
    while !done() {
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
        .stderr(Stdio::null());
    cmd
}
fn busy(file: &File) -> bool {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return false;
    }
    assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
    true
}

#[test]
fn subprocess_fixture() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os(ROOT).unwrap());
    let fd: i32 = std::env::var(FD).unwrap().parse().unwrap();
    assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
    // Fixture-only expiry, including if a test harness/guardian is unexpectedly killed.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(15));
        std::process::exit(99);
    });
    match mode.as_str() {
        "before-grant" | "running" | "normal" => {
            let (mut controller, guardian) = UnixStream::pair().unwrap();
            assert_ne!(
                unsafe { libc::fcntl(controller.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            let mut cmd = helper("guardian");
            cmd.env(
                "TASKS_GUARD_WORKLOAD",
                if mode == "normal" {
                    "exiting"
                } else {
                    "resistant"
                },
            );
            cmd.stdin(Stdio::from(OwnedFd::from(guardian)));
            let mut child = Controller(cmd.spawn().unwrap());
            until(|| root.join("guardian-ready").exists());
            if mode != "before-grant" {
                controller.write_all(&[START]).unwrap();
                until(|| root.join("workload-ready").exists());
            }
            fs::write(root.join("controller-ready"), b"ready").unwrap();
            if mode == "normal" {
                assert!(child.0.wait().unwrap().success());
                return;
            }
            // Harness kills this process with SIGKILL. Its sole writer endpoint
            // must not have leaked through guardian/workload exec.
            loop {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        "guardian" => {
            let control = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
            fs::write(root.join("guardian-ready"), b"ready").unwrap();
            let mut cmd = helper(&std::env::var("TASKS_GUARD_WORKLOAD").unwrap());
            let outcome = supervise(&mut cmd, control, Duration::from_millis(50)).unwrap();
            let status = match outcome.status {
                Some(status) => format!("code={:?} signal={:?}", status.code(), status.signal()),
                None => "None".to_owned(),
            };
            fs::write(
                root.join("result"),
                format!("{:?} {status}", outcome.reason),
            )
            .unwrap();
        }
        "resistant" | "exiting" => {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join("entered-once"))
                .unwrap();
            if mode == "resistant" {
                unsafe {
                    libc::signal(libc::SIGTERM, libc::SIG_IGN);
                }
            }
            let _child = Controller(helper("descendant").spawn().unwrap());
            until(|| root.join("descendant-ready").exists());
            fs::write(root.join("workload-ready"), b"ready").unwrap();
            if mode == "exiting" {
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
        _ => panic!("unknown fixture mode"),
    }
}

fn crash_case(mode: &str, expected: &str) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let path = fixture.0.join("stable-slot.lock");
    let lease = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let key = (
        lease.metadata().unwrap().dev(),
        lease.metadata().unwrap().ino(),
    );
    assert!(!busy(&lease));
    let fd = unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_DUPFD, 64) };
    assert!(fd >= 64);
    let inherited = unsafe { File::from_raw_fd(fd) };
    let mut cmd = helper(mode);
    cmd.env(ROOT, &fixture.0).env(FD, fd.to_string());
    let mut controller = Controller(cmd.spawn().unwrap());
    drop(inherited);
    drop(lease);
    until(|| fixture.0.join("controller-ready").exists());
    let observer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    // The normal-completion case may already have finished before observation.
    if mode != "normal" {
        assert!(busy(&observer));
    }
    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("15")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let outside = OwnedChildGroup::spawn(&mut cmd).unwrap();
    if mode != "normal" {
        controller.0.kill().unwrap();
    }
    let status = controller.0.wait().unwrap();
    if mode != "normal" {
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    } else {
        assert!(status.success());
    }
    until(|| fixture.0.join("result").exists());
    until(|| !busy(&observer));
    let result = fs::read_to_string(fixture.0.join("result")).unwrap();
    assert!(result.starts_with(expected), "{result}");
    if mode == "before-grant" {
        assert!(result.ends_with("None"));
        assert!(!fixture.0.join("entered-once").exists());
    } else if mode == "normal" {
        assert!(
            result.contains("code=Some(7)"),
            "original exit 7 must be retained: {result}"
        );
    } else {
        assert!(fixture.0.join("entered-once").exists());
        assert!(
            result.contains("signal=Some(9)"),
            "resistant leader must be killed: {result}"
        );
    }
    assert!(!outside.has_exited().unwrap());
    let m = fs::metadata(&path).unwrap();
    assert_eq!(key, (m.dev(), m.ino()));
}
#[test]
fn sigkill_before_grant_never_enters_workload() {
    crash_case("before-grant", "ControllerLostBeforeStart");
}
#[test]
fn sigkill_during_execution_releases_resistant_descendant_lease() {
    crash_case("running", "ControllerLost");
}
#[test]
fn normal_leader_exit_cleans_descendants_and_preserves_status() {
    crash_case("normal", "LeaderExited");
}
#[test]
fn buffered_grant_followed_by_eof_does_not_start() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (mut controller, guardian) = UnixStream::pair().unwrap();
    controller.write_all(&[START]).unwrap();
    drop(controller);
    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("15");
    let outcome = supervise(&mut cmd, guardian, Duration::ZERO).unwrap();
    assert_eq!(outcome.reason, Reason::ControllerLostBeforeStart);
    assert!(outcome.status.is_none());
}
#[test]
fn malformed_grant_is_not_start_authority() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (mut controller, guardian) = UnixStream::pair().unwrap();
    controller.write_all(b"X").unwrap();
    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("15");
    assert_eq!(
        supervise(&mut cmd, guardian, Duration::ZERO)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}
#[test]
fn inheritable_guard_endpoint_is_rejected() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_controller, guardian) = UnixStream::pair().unwrap();
    assert_eq!(
        unsafe { libc::fcntl(guardian.as_raw_fd(), libc::F_SETFD, 0) },
        0
    );
    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("15");
    assert_eq!(
        supervise(&mut cmd, guardian, Duration::ZERO)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}
