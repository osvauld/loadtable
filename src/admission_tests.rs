use super::*;
use std::{
    process::{Command, Stdio},
    sync::atomic::AtomicUsize,
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "tasks-native-admission-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn free() -> Result<Pressure> {
    Ok(Pressure {
        available_bytes: MIN_MEMORY,
        cpu_some_avg10: 0.0,
        memory_some_avg10: 0.0,
        memory_full_avg10: 0.0,
        io_some_avg10: 0.0,
        io_full_avg10: 0.0,
    })
}
#[test]
fn pressure_is_fail_closed_and_respects_boundaries() {
    let quiet = "some avg10=0\nfull avg10=0";
    assert!(
        Pressure::parse(
            "MemAvailable: 8388608 kB",
            "some avg10=40.00 avg60=1.0",
            quiet,
            quiet
        )
        .unwrap()
        .permits()
    );
    for text in [
        "some avg10=NaN",
        "some avg10=-1",
        "some avg10=101",
        "full avg10=0",
    ] {
        assert!(Pressure::parse("MemAvailable: 1 kB", text, quiet, quiet).is_err());
        assert!(Pressure::parse("MemAvailable: 1 kB", quiet, text, quiet).is_err());
        assert!(Pressure::parse("MemAvailable: 1 kB", quiet, quiet, text).is_err());
    }
    assert!(Pressure::parse("invalid", "some avg10=0", quiet, quiet).is_err());
    assert!(
        !Pressure {
            available_bytes: MIN_MEMORY - 1,
            ..free().unwrap()
        }
        .permits()
    );
    assert!(
        !Pressure {
            available_bytes: MIN_MEMORY,
            cpu_some_avg10: 40.01,
            ..free().unwrap()
        }
        .permits()
    );
}
#[test]
fn synthetic_stalls_queue_without_holding_compile_and_can_cancel() {
    for (metric, maximum) in [(0, 1.0), (1, 0.5), (2, 20.0), (3, 10.0)] {
        let f = Fixture::new();
        let mut pressure = free().unwrap();
        let field = match metric {
            0 => &mut pressure.memory_some_avg10,
            1 => &mut pressure.memory_full_avg10,
            2 => &mut pressure.io_some_avg10,
            _ => &mut pressure.io_full_avg10,
        };
        *field = maximum;
        assert!(pressure.permits());
        *match metric {
            0 => &mut pressure.memory_some_avg10,
            1 => &mut pressure.memory_full_avg10,
            2 => &mut pressure.io_some_avg10,
            _ => &mut pressure.io_full_avg10,
        } = maximum + 0.01;
        let attempts = AtomicUsize::new(0);
        assert!(
            acquire_at(
                &f.0,
                Kind::Compile,
                "synthetic",
                "no workload",
                || Ok(pressure.clone()),
                || attempts.fetch_add(1, Ordering::Relaxed) > 0
            )
            .unwrap()
            .is_none()
        );
        let lock = open_lock(&f.0.join("p2p-build.lock"), false).unwrap();
        assert!(take(&lock).unwrap());
    }
}
#[test]
fn pressure_is_rechecked_after_lock_and_recovers_without_job_replay() {
    let f = Fixture::new();
    let samples = AtomicUsize::new(0);
    let lease = acquire_at(
        &f.0,
        Kind::Compile,
        "synthetic",
        "no workload",
        || {
            let mut p = free()?;
            if samples.fetch_add(1, Ordering::Relaxed) == 1 {
                p.io_full_avg10 = 99.0;
            }
            Ok(p)
        },
        || false,
    )
    .unwrap()
    .unwrap();
    assert!(samples.load(Ordering::Relaxed) >= 4);
    drop(lease);
}
#[test]
fn rust_lock_excludes_python_and_python_excludes_rust() {
    let f = Fixture::new();
    let lease = acquire_at(&f.0, Kind::Compile, "model", "test", free, || false)
        .unwrap()
        .unwrap();
    let probe = "import fcntl,sys; f=open(sys.argv[1],'r+');\ntry: fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB)\nexcept BlockingIOError: sys.exit(0)\nsys.exit(1)";
    assert!(
        Command::new("python3")
            .args(["-c", probe])
            .arg(f.0.join("p2p-build.lock"))
            .status()
            .unwrap()
            .success()
    );
    drop(lease);
    // Exercise the exact legacy Python protocol in a separately owned process.
    let script = "import fcntl,sys; f=open(sys.argv[1],'r+'); fcntl.flock(f,fcntl.LOCK_EX); print('ready',flush=True); sys.stdin.read()";
    let mut child = Command::new("python3")
        .args(["-c", script])
        .arg(f.0.join("p2p-build.lock"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::BufRead;
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "ready");
    let n = AtomicUsize::new(0);
    assert!(
        acquire_at(&f.0, Kind::Compile, "model", "test", free, || n
            .fetch_add(1, Ordering::Relaxed)
            > 0)
        .unwrap()
        .is_none()
    );
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}
#[test]
fn four_run_slots_are_bounded_and_separate_from_compile() {
    let f = Fixture::new();
    let mut leases = Vec::new();
    for _ in 0..4 {
        leases.push(
            acquire_at(&f.0, Kind::Run, "model", "test", free, || false)
                .unwrap()
                .unwrap(),
        );
    }
    let n = AtomicUsize::new(0);
    assert!(
        acquire_at(&f.0, Kind::Run, "model", "test", free, || n
            .fetch_add(1, Ordering::Relaxed)
            > 0)
        .unwrap()
        .is_none()
    );
    let _compile = acquire_at(&f.0, Kind::Compile, "model", "test", free, || false)
        .unwrap()
        .unwrap();
    assert_eq!(
        status_at(&f.0, free().unwrap())
            .unwrap()
            .resources
            .iter()
            .filter(|s| s.busy)
            .count(),
        5
    );
}
#[test]
fn device_is_exclusive_and_never_aliases_invalid_serials() {
    let f = Fixture::new();
    let _a = acquire_at(
        &f.0,
        Kind::Device("synthetic-1".into()),
        "model",
        "test",
        free,
        || false,
    )
    .unwrap()
    .unwrap();
    let _b = acquire_at(
        &f.0,
        Kind::Device("synthetic-2".into()),
        "model",
        "test",
        free,
        || false,
    )
    .unwrap()
    .unwrap();
    let n = AtomicUsize::new(0);
    assert!(
        acquire_at(
            &f.0,
            Kind::Device("synthetic-1".into()),
            "model",
            "test",
            free,
            || n.fetch_add(1, Ordering::Relaxed) > 0
        )
        .unwrap()
        .is_none()
    );
    for s in ["", "../escape", "serial:port", "a/b"] {
        assert!(Kind::Device(s.into()).names().is_err());
    }
}
#[test]
fn inherited_admission_lease_survives_parent_copy_drop() {
    let f = Fixture::new();
    let lease = acquire_at(&f.0, Kind::Compile, "model", "test", free, || false)
        .unwrap()
        .unwrap();
    lease.inherit().unwrap();
    let mut child = Command::new("sleep").arg("0.3").spawn().unwrap();
    drop(lease);
    let lock = open_lock(&f.0.join("p2p-build.lock"), false).unwrap();
    assert!(!take(&lock).unwrap());
    assert!(child.wait().unwrap().success());
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !take(&lock).unwrap() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn status_is_read_only_and_does_not_trust_stale_holder_notes() {
    let f = Fixture::new();
    let missing = f.0.join("missing");
    assert!(
        status_at(&missing, free().unwrap())
            .unwrap()
            .resources
            .is_empty()
    );
    assert!(!missing.exists());
    let lease = acquire_at(&f.0, Kind::Compile, "model", "test", free, || false)
        .unwrap()
        .unwrap();
    let live = status_at(&f.0, free().unwrap()).unwrap();
    assert!(live.resources[0].busy);
    assert_eq!(live.resources[0].holder.as_ref().unwrap().who, "model");
    drop(lease);
    fs::write(
        f.0.join("p2p-build.holder"),
        br#"{"who":"legacy","cmd":"test","since":1.25,"pid":1}"#,
    )
    .unwrap();
    // Parallel fork/exec fixtures can briefly retain a CLOEXEC copy. Only the
    // idle expectation retries; active/exclusive assertions remain immediate.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let idle = loop {
        let status = status_at(&f.0, free().unwrap()).unwrap();
        if !status.resources[0].busy {
            break status;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!idle.resources[0].busy);
    assert!(idle.resources[0].holder.is_none());
    assert!(f.0.join("p2p-build.holder").exists());
}
#[test]
fn fifo_notes_are_scoped_and_stale_notes_do_not_block() {
    let f = Fixture::new();
    let id = worker::identity(std::process::id()).unwrap();
    let n = Note {
        who: "model".into(),
        cmd: "test".into(),
        since: now() as f64,
        pid: id.pid,
        start_ticks: Some(id.start_ticks),
        slot: Some("compile".into()),
    };
    let first = Wait::new(&f.0, &n).unwrap();
    assert!(
        first
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&format!("-{}-", id.start_ticks))
    );
    let mut unidentified = n.clone();
    unidentified.start_ticks = None;
    assert!(Wait::new(&f.0, &unidentified).is_err());
    std::thread::sleep(Duration::from_millis(2));
    let second = Wait::new(&f.0, &n).unwrap();
    assert!(older_waiter(&f.0, &second, "compile").unwrap());
    assert!(!older_waiter(&f.0, &first, "compile").unwrap());
    assert!(!older_waiter(&f.0, &second, "run").unwrap());
    // Simulate a reused PID, then a legacy PID-only note. Neither may block.
    let mut stale = n.clone();
    stale.start_ticks = Some(id.start_ticks + 1);
    fs::write(&first.path, serde_json::to_vec(&stale).unwrap()).unwrap();
    assert!(!older_waiter(&f.0, &second, "compile").unwrap());
    stale.start_ticks = None;
    fs::write(&first.path, serde_json::to_vec(&stale).unwrap()).unwrap();
    assert!(!older_waiter(&f.0, &second, "compile").unwrap());
    drop(first);
    assert!(!older_waiter(&f.0, &second, "compile").unwrap());
}
#[test]
fn unsafe_lock_and_pressure_errors_refuse_admission() {
    let f = Fixture::new();
    let other = f.0.join("preserve");
    fs::write(&other, "preserve").unwrap();
    std::os::unix::fs::symlink(&other, f.0.join("p2p-build.lock")).unwrap();
    assert!(acquire_at(&f.0, Kind::Compile, "model", "test", free, || false).is_err());
    assert_eq!(fs::read_to_string(other).unwrap(), "preserve");
    let g = Fixture::new();
    assert!(
        acquire_at(
            &g.0,
            Kind::Compile,
            "model",
            "test",
            || Err("pressure unavailable".into()),
            || false
        )
        .is_err()
    );
    assert!(fs::read_dir(&g.0).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("p2p-wait-")
    }));
}
