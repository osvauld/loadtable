use super::super::{Kind, Lease, MIN_MEMORY, Pressure, acquire_at, open_lock, status_at, take};
use super::*;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Tests that start long-lived synthetic daemons run one at a time.
pub(crate) static SERIAL: Mutex<()> = Mutex::new(());

pub(crate) fn free() -> crate::Result<Pressure> {
    Ok(Pressure {
        available_bytes: MIN_MEMORY,
        cpu_some_avg10: 0.0,
        memory_some_avg10: 0.0,
        memory_full_avg10: 0.0,
        io_some_avg10: 0.0,
        io_full_avg10: 0.0,
    })
}

/// Isolated admission root; kills recorded synthetic daemons on drop.
pub(crate) struct Fixture {
    pub root: PathBuf,
    pub pids: Vec<i32>,
}
impl Fixture {
    pub fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "tasks-lease-leak-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self { root, pids: vec![] }
    }
    pub fn lock(&self) -> PathBuf {
        self.root.join("p2p-build.lock")
    }
    pub fn compile_lease(&self, who: &str) -> Lease {
        acquire_at(&self.root, Kind::Compile, who, "synthetic", free, || false)
            .unwrap()
            .unwrap()
    }
    /// Whether a fresh open file description can take the lock now.
    pub fn lock_free(&self) -> bool {
        let f = open_lock(&self.lock(), false).unwrap();
        take(&f).unwrap()
    }
    pub fn wait_free(&self, secs: u64) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if self.lock_free() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
    pub fn read_pid(&mut self, file: &std::path::Path) -> i32 {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pid) = std::fs::read_to_string(file)
                .ok()
                .and_then(|t| t.trim().parse().ok())
            {
                self.pids.push(pid);
                return pid;
            }
            assert!(Instant::now() < end, "{} never written", file.display());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for pid in &self.pids {
            unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Runs `script` as a synthetic admitted job: the lease is inherited at fd 9
/// of THIS child only (dup2 in the child), exactly as Lease::inherit lends it
/// to a real job, without making it inheritable by other tests' children.
pub(crate) fn run_job_with_lease(lease: &Lease, job: i64, script: &str, arg: &std::path::Path) {
    let fd = lease.file.as_raw_fd();
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", script, "sh"])
        .arg(arg)
        .env("TASKS_JOB_ID", job.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(move || {
            // dup2(9, 9) is a no-op that keeps FD_CLOEXEC: when the lease already IS fd 9
            // (it depends on what other test threads have open), clear the flag instead, or
            // the lease closes at exec and the "job" never holds it (the old flake).
            if fd == 9 {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(fd, 9) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    assert!(cmd.status().unwrap().success());
}

#[test]
fn parses_fdinfo_cmdline_and_job_environment() {
    let holder = "pos:\t0\nflags:\t02100001\nmnt_id:\t164\nino:\t17327\n\
                  lock:\t1: FLOCK  ADVISORY  WRITE 91356 00:33:17327 0 EOF\n";
    assert!(fdinfo_has_flock(holder));
    assert!(!fdinfo_has_flock(
        "pos:\t0\nflags:\t02100001\nino:\t17327\n"
    ));
    assert!(!fdinfo_has_flock(
        "lock:\t1: POSIX  ADVISORY  WRITE 1 00:33:17327 0 EOF\n"
    ));
    assert_eq!(
        cmdline_text(b"sccache\0--start-server\0", "sccache"),
        "sccache --start-server"
    );
    assert_eq!(cmdline_text(b"", "kworker/0\n"), "[kworker/0]");
    let long = cmdline_text(&[b'x'; 500], "x");
    assert_eq!(long.chars().count(), MAX_CMD);
    assert_eq!(
        tasks_job_in_environ(b"HOME=/h\0TASKS_JOB_ID=154\0PATH=/bin\0"),
        Some(154)
    );
    assert_eq!(tasks_job_in_environ(b"XTASKS_JOB_ID=1\0"), None);
    assert_eq!(tasks_job_in_environ(b"TASKS_JOB_ID=nope\0"), None);
}

/// The reported bug, reproduced with the real admission code: a job spawns a
/// daemonizing child (new session, reparented) that inherits the lease and
/// outlives the job. The lock stays held (deliberate flock semantics: tasks
/// does not steal it), and every diagnostic names that daemon, its command
/// and originating TASKS_JOB_ID; waiters that merely have the file open are
/// not reported. When the daemon exits the same inode is free again.
#[test]
fn daemonized_child_that_outlives_its_job_is_named_as_the_holder() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut f = Fixture::new("holder");
    let lease = f.compile_lease("synthetic-owner:job-4242");
    let inode = std::fs::metadata(f.lock()).unwrap().ino();
    // While the job's own admission is alive its note explains the holder.
    assert_eq!(untracked_holders_of(&f), None);
    let pidfile = f.root.join("daemon.pid");
    run_job_with_lease(
        &lease,
        4242,
        "setsid sh -c 'echo $$ > \"$1\"; exec sleep 120' sh \"$1\" </dev/null >/dev/null 2>&1 &",
        &pidfile,
    );
    let daemon = f.read_pid(&pidfile);
    // A process that only has the lock file open (like a polling waiter).
    let opener = f.root.join("opener.pid");
    let mut opener_child = Command::new("/bin/sh")
        .args([
            "-c",
            "exec 8<\"$1\"; echo $$ > \"$2\"; exec sleep 120",
            "sh",
        ])
        .arg(f.lock())
        .arg(&opener)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let opener = f.read_pid(&opener);
    // Reaped below through its Child handle, not killed by pid on drop.
    f.pids.pop();
    // The job has ended and its admission process exits (note removed).
    drop(lease);
    assert!(!f.holder_note_exists());
    assert!(!f.lock_free(), "the escaped daemon still holds the lease");

    let found = holders(&f.lock());
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].pid, daemon as u32);
    assert_ne!(found[0].pid, opener as u32);
    assert!(found[0].cmd.starts_with("sleep 120"), "{found:?}");
    assert_eq!(found[0].tasks_job, Some(4242));
    assert_eq!(
        describe_text(&found[0]),
        format!("pid {daemon} (sleep 120) from job #4242")
    );
    // Admission's waiting message, `tasks admission` and the snapshot scan.
    let text = untracked_holders_of(&f).unwrap();
    assert_eq!(text, format!("pid {daemon} (sleep 120) from job #4242"));
    let status = status_at(&f.root, free().unwrap()).unwrap();
    let compile = status
        .resources
        .iter()
        .find(|r| r.path == f.lock())
        .unwrap();
    assert!(compile.busy && compile.holder.is_none());
    assert_eq!(compile.holder_processes, found);
    let held =
        crate::metrics::procfs::parse_flocks(&std::fs::read_to_string("/proc/locks").unwrap());
    let scan = crate::metrics::slots::scan(&f.root, &held);
    let slot = scan.slots.iter().find(|s| s.name == "p2p-build").unwrap();
    assert!(slot.busy && slot.holder_who.is_none());
    assert_eq!(slot.holder_processes, found);

    // Recovery is the holder exiting; the lock inode is unchanged.
    unsafe { libc::kill(daemon, libc::SIGKILL) };
    assert!(f.wait_free(10));
    assert_eq!(std::fs::metadata(f.lock()).unwrap().ino(), inode);
    assert!(holders(&f.lock()).is_empty());
    let _ = opener_child.kill();
    let _ = opener_child.wait();
}

impl Fixture {
    fn holder_note_exists(&self) -> bool {
        self.lock().with_extension("holder").exists()
    }
}
fn untracked_holders_of(f: &Fixture) -> Option<String> {
    super::super::untracked_holders(&f.root, &["p2p-build.lock".to_string()])
}
