use super::*;
use crate::admission::holders::tests::{Fixture, SERIAL, run_job_with_lease};
use std::os::unix::fs::PermissionsExt;

/// Synthetic sccache: a client that autostarts a detached "server" (new
/// session, reparented, keeps every inherited descriptor, like sccache 0.15)
/// on first use; `--start-server` fails with "address in use" while one runs;
/// `--show-stats` succeeds only when one runs. The server records its env.
const FAKE_SCCACHE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
alive() { [ -s "$dir/server.pid" ] && kill -0 "$(cat "$dir/server.pid")" 2>/dev/null; }
start() {
  setsid sh -c 'env > "$1/server.env.tmp"; mv "$1/server.env.tmp" "$1/server.env"; echo $$ > "$1/server.pid.tmp"; mv "$1/server.pid.tmp" "$1/server.pid"; exec sleep 120' sh "$dir" </dev/null >/dev/null 2>&1 &
  i=0; while ! alive && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done
}
case "$1" in
  --start-server) if alive; then echo "sccache: Server startup failed: Address in use" >&2; exit 2; fi; start; echo "sccache: Starting the server...";;
  --show-stats) alive || exit 1; echo "Compile requests 0";;
  *) alive || start;;
esac
"#;

fn fake_wrapper(f: &Fixture) -> std::path::PathBuf {
    let dir = f.root.join("bin");
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("sccache");
    std::fs::write(&path, FAKE_SCCACHE).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn job_env(wrapper: &Path, f: &Fixture) -> Vec<(OsString, OsString)> {
    vec![
        ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
        ("RUSTC_WRAPPER".into(), wrapper.into()),
        ("SCCACHE_DIR".into(), f.root.join("cache").into()),
    ]
}

#[test]
fn recognises_server_based_wrappers_only() {
    for w in [
        "sccache",
        "/nix/store/abc-sccache-0.15.0/bin/sccache",
        "sccache.exe",
    ] {
        assert!(is_server_wrapper(OsStr::new(w)), "{w}");
    }
    for w in ["", "/usr/bin/ccache", "/opt/not-sccache", "/bin/sccacher"] {
        assert!(!is_server_wrapper(OsStr::new(w)), "{w}");
    }
}

/// Control: without the fix, a job whose first wrapper call autostarts the
/// server leaks its compile lease to that server after the job ends.
#[test]
fn server_autostarted_by_a_job_keeps_its_lease_after_the_job() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut f = Fixture::new("cache-leak");
    let wrapper = fake_wrapper(&f);
    let lease = f.compile_lease("synthetic-owner:job-154");
    run_job_with_lease(&lease, 154, "\"$1\" rustc -vV", &wrapper);
    let server = f.read_pid(&wrapper.with_file_name("server.pid"));
    drop(lease);
    assert!(!f.lock_free(), "the autostarted server inherited the lease");
    let found = crate::admission::holders::holders(&f.lock());
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        (found[0].pid, found[0].tasks_job),
        (server as u32, Some(154))
    );
    unsafe { libc::kill(server, libc::SIGKILL) };
    assert!(f.wait_free(10));
}

/// The fix: the worker starts the server first, from the process that holds
/// the inheritable lease (as `tasks __command` does), with descriptors closed
/// on exec and its own session. The job's wrapper calls reuse that server, so
/// the slot is free as soon as the job and its admission end, while the
/// server keeps running for later jobs. A second job only touches it.
#[test]
fn server_started_outside_the_job_never_holds_the_lease() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut f = Fixture::new("cache-fix");
    let wrapper = fake_wrapper(&f);
    let env = job_env(&wrapper, &f);
    let job_dir = f.root.join("job");
    std::fs::create_dir(&job_dir).unwrap();

    let lease = f.compile_lease("synthetic-owner:job-155");
    // Exactly the real worker state: the lease is inheritable in this process.
    lease.inherit().unwrap();
    let outcome = ensure(wrapper.as_os_str(), &env, &job_dir);
    // Back to close-on-exec at once: parallel tests also spawn children.
    unsafe { libc::fcntl(lease.raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    assert_eq!(outcome, Outcome::Started, "{}", read_log(&job_dir));
    let server = f.read_pid(&wrapper.with_file_name("server.pid"));
    // The job's own wrapper call (lease inherited) finds the running server.
    run_job_with_lease(&lease, 155, "\"$1\" rustc -vV", &wrapper);
    assert_eq!(f.read_pid(&wrapper.with_file_name("server.pid")), server);
    drop(lease);
    assert!(
        f.wait_free(10),
        "{:?}",
        crate::admission::holders::holders(&f.lock())
    );
    assert!(
        crate::worker::identity(server as u32).is_ok(),
        "server still running"
    );
    // Its session is not the job's (cancelling the job cannot kill it), and
    // it got the job's cache environment but not the job's TASKS_JOB_ID.
    assert_ne!(unsafe { libc::getsid(server) }, unsafe { libc::getsid(0) });
    let server_env = std::fs::read_to_string(wrapper.with_file_name("server.env")).unwrap();
    assert!(server_env.contains("SCCACHE_DIR="), "{server_env}");
    assert!(!server_env.contains("TASKS_JOB_ID="), "{server_env}");

    // Next job: the server is running, so ensure only touches it.
    let lease = f.compile_lease("synthetic-owner:job-156");
    assert_eq!(
        ensure(wrapper.as_os_str(), &env, &job_dir),
        Outcome::AlreadyRunning
    );
    drop(lease);
    assert!(f.wait_free(10));
    assert!(read_log(&job_dir).contains("Address in use"));
}

#[test]
fn missing_wrapper_is_reported_not_fatal() {
    let f = Fixture::new("cache-missing");
    let outcome = ensure(OsStr::new("/nonexistent/sccache"), &[], &f.root);
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert!(outcome.note().contains("warning"));
}

fn read_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(LOG_NAME)).unwrap_or_default()
}
