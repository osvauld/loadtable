//! Real process/lease fixtures, not the installed coordinator. Helpers self-expire.
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use task_manager_platform::owned_group::OwnedChildGroup;

const MODE: &str = "TASKS_OWNED_GROUP_TEST_MODE";
const READY: &str = "TASKS_OWNED_GROUP_TEST_READY";
const LEASE: &str = "TASKS_OWNED_GROUP_TEST_FD";
// Inheritable FDs are process-global: serialize every spawning test in this binary.
static SPAWN: Mutex<()> = Mutex::new(());

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tasks-owned-group-{}-{time}-{}",
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

fn locked_lease(path: &Path) -> File {
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    // A deliberately inheritable descriptor, separate from stdio/spawn-error pipes.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 64) };
    assert!(fd >= 64, "{}", io::Error::last_os_error());
    unsafe { File::from_raw_fd(fd) }
}

fn lock_available(file: &File) -> bool {
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return true;
    }
    assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
    false
}

fn wait_until(mut test: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !test() {
        assert!(Instant::now() < deadline, "fixture deadline expired");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn fixture_command(mode: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "subprocess_fixture", "--nocapture"])
        .env(MODE, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

#[test]
fn subprocess_fixture() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    if mode == "reject-auto-reap" {
        unsafe {
            libc::signal(libc::SIGCHLD, libc::SIG_IGN);
        }
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("15");
        assert_eq!(
            OwnedChildGroup::spawn(&mut cmd).err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        return;
    }
    let fd: i32 = std::env::var(LEASE).unwrap().parse().unwrap();
    assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
    let ready = PathBuf::from(std::env::var_os(READY).unwrap());
    let deadline = Instant::now() + Duration::from_secs(15);
    if mode == "descendant" {
        // This process keeps the inherited lease even after its leader dies.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
        fs::write(&ready, b"ready").unwrap();
    } else {
        assert!(mode == "leader" || mode == "exiting-leader" || mode == "resistant-leader");
        if mode == "resistant-leader" {
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
        }
        let mut child = fixture_command("descendant").spawn().unwrap();
        if mode == "exiting-leader" {
            wait_until(|| ready.exists());
            std::process::exit(7);
        }
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // Even if the test parent disappears, fixtures do not run indefinitely.
        let _ = child.kill();
        let _ = child.wait();
        return;
    }
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Clone, Copy)]
enum Cleanup {
    Terminate,
    Wait,
    Drop,
}

fn exercise(mode: &str, cleanup: Cleanup) {
    let _serial = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let path = fixture.0.join("stable-slot.lock");
    let ready = fixture.0.join("ready");
    let lease = locked_lease(&path);
    let key = (
        lease.metadata().unwrap().dev(),
        lease.metadata().unwrap().ino(),
    );
    let mut cmd = fixture_command(mode);
    cmd.env(READY, &ready)
        .env(LEASE, lease.as_raw_fd().to_string());
    let group = OwnedChildGroup::spawn(&mut cmd).unwrap();
    drop(lease); // Only the fixture group now holds the actual flock lease.
    wait_until(|| ready.exists());
    let observer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    assert!(
        !lock_available(&observer),
        "descendant did not retain the lease"
    );
    if mode == "exiting-leader" {
        wait_until(|| group.has_exited().unwrap());
        // Native identity may be ESRCH, but the direct child remains pinned.
    }
    let mut unrelated = Command::new("/bin/sleep");
    unrelated
        .arg("15")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let outside = OwnedChildGroup::spawn(&mut unrelated).unwrap();
    match cleanup {
        Cleanup::Terminate => {
            let start = Instant::now();
            let status = group.terminate(Duration::from_millis(200)).unwrap();
            if mode == "exiting-leader" {
                assert_eq!(status.code(), Some(7));
            } else if mode == "resistant-leader" {
                assert!(start.elapsed() >= Duration::from_millis(200));
                assert_eq!(status.signal(), Some(libc::SIGKILL));
            } else {
                assert_eq!(status.signal(), Some(libc::SIGTERM));
            }
        }
        Cleanup::Wait => assert_eq!(group.wait().unwrap().code(), Some(7)),
        Cleanup::Drop => drop(group),
    }
    wait_until(|| lock_available(&observer));
    assert!(
        !outside.has_exited().unwrap(),
        "cleanup reached an unrelated group"
    );
    let metadata = fs::metadata(&path).unwrap();
    assert_eq!(
        key,
        (metadata.dev(), metadata.ino()),
        "slot inode was replaced"
    );
}

#[test]
fn term_exit_of_leader_does_not_strand_resistant_descendant_lease() {
    exercise("leader", Cleanup::Terminate);
}
#[test]
fn resistant_leader_gets_grace_then_kill() {
    exercise("resistant-leader", Cleanup::Terminate);
}
#[test]
fn auto_reaping_is_rejected_before_spawning_a_group() {
    let _serial = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        fixture_command("reject-auto-reap")
            .status()
            .unwrap()
            .success()
    );
}
#[test]
fn already_exited_leader_stays_pinned_through_escalation() {
    exercise("exiting-leader", Cleanup::Terminate);
}
#[test]
fn normal_completion_cleans_descendants_before_reaping() {
    exercise("exiting-leader", Cleanup::Wait);
}
#[test]
fn ordinary_scope_drop_cleans_owned_group_and_lease() {
    exercise("leader", Cleanup::Drop);
}

#[test]
fn lost_reaping_ownership_is_an_error_not_signal_authority() {
    let _serial = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let group = OwnedChildGroup::spawn(&mut cmd).unwrap();
    let mut status = 0;
    // Deliberately violate exclusive reaping to exercise the fail-closed guard.
    assert_eq!(
        unsafe { libc::waitpid(group.id() as i32, &mut status, 0) },
        group.id() as i32
    );
    assert_eq!(
        group.has_exited().unwrap_err().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert_eq!(
        group.terminate(Duration::ZERO).unwrap_err().raw_os_error(),
        Some(libc::ECHILD)
    );
}
