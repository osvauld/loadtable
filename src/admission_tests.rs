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
// ---- Configurable thresholds (worker portability) ----
fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
    move |name| {
        pairs
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| std::ffi::OsString::from(v))
    }
}
const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
#[test]
fn thresholds_default_exactly_to_the_previous_constants() {
    let t = Thresholds::from_lookup(lookup(&[])).unwrap();
    assert_eq!(t, Thresholds::default());
    assert_eq!(t.min_available_memory_bytes, 8 * GIB);
    assert_eq!(t.min_cache_free_bytes, 20 * GIB);
    assert_eq!(t.min_state_free_bytes, 256 * MIB);
    assert_eq!(MIN_MEMORY, 8 * GIB);
    assert_eq!(crate::cache::MIN_FREE_BYTES, 20 * GIB);
    assert_eq!(crate::cache::MIN_STATE_FREE_BYTES, 256 * MIB);
    assert!(!Thresholds::overridden(lookup(&[])));
    assert!(Thresholds::overridden(lookup(&[(MEMORY_ENV, "8192")])));
    // Default blocker text is unchanged for existing readers.
    let low = Pressure {
        available_bytes: 8 * GIB - 1,
        ..free().unwrap()
    };
    assert_eq!(
        low.blocker_with(&t).as_deref(),
        Some("available memory below 8 GiB")
    );
}
#[test]
fn thresholds_accept_explicit_whole_units() {
    let t = Thresholds::from_lookup(lookup(&[
        (MEMORY_ENV, "3072"),
        (CACHE_FREE_ENV, "10"),
        (STATE_FREE_ENV, "128"),
    ]))
    .unwrap();
    assert_eq!(t.min_available_memory_bytes, 3072 * MIB);
    assert_eq!(t.min_cache_free_bytes, 10 * GIB);
    assert_eq!(t.min_state_free_bytes, 128 * MIB);
    // Floors are inclusive.
    let floor = Thresholds::from_lookup(lookup(&[
        (MEMORY_ENV, "1024"),
        (CACHE_FREE_ENV, "1"),
        (STATE_FREE_ENV, "64"),
    ]))
    .unwrap();
    assert_eq!(floor.min_available_memory_bytes, GIB);
    assert_eq!(floor.min_cache_free_bytes, GIB);
    assert_eq!(floor.min_state_free_bytes, 64 * MIB);
}
#[test]
fn thresholds_reject_garbage_and_values_below_the_floor() {
    for (name, value) in [
        (MEMORY_ENV, ""),
        (MEMORY_ENV, " 4096"),
        (MEMORY_ENV, "4096 "),
        (MEMORY_ENV, "4096MiB"),
        (MEMORY_ENV, "4G"),
        (MEMORY_ENV, "-4096"),
        (MEMORY_ENV, "+4096"),
        (MEMORY_ENV, "4096.0"),
        (MEMORY_ENV, "0x1000"),
        (MEMORY_ENV, "1023"),
        (MEMORY_ENV, "0"),
        (MEMORY_ENV, "99999999999999999999999"),
        (MEMORY_ENV, "1048577"),
        (CACHE_FREE_ENV, "0"),
        (CACHE_FREE_ENV, "abc"),
        (CACHE_FREE_ENV, "65537"),
        (STATE_FREE_ENV, "63"),
        (STATE_FREE_ENV, "1e3"),
    ] {
        let error = Thresholds::from_lookup(lookup(&[(name, value)]))
            .expect_err(&format!("{name}={value:?} must be rejected"))
            .to_string();
        assert!(error.contains(name), "{error}");
    }
    use std::os::unix::ffi::OsStringExt;
    let invalid_utf8 = std::ffi::OsString::from_vec(vec![0xff, b'1']);
    assert!(Thresholds::from_lookup(|n| (n == MEMORY_ENV).then(|| invalid_utf8.clone())).is_err());
}
#[test]
fn blocker_names_the_effective_memory_threshold() {
    let t = Thresholds::from_lookup(lookup(&[(MEMORY_ENV, "3072")])).unwrap();
    let mut p = Pressure {
        available_bytes: 3072 * MIB,
        ..free().unwrap()
    };
    assert_eq!(p.blocker_with(&t), None);
    p.available_bytes -= 1;
    assert_eq!(
        p.blocker_with(&t).as_deref(),
        Some("available memory below 3 GiB")
    );
    let odd = Thresholds::from_lookup(lookup(&[(MEMORY_ENV, "1536")])).unwrap();
    p.available_bytes = 1536 * MIB - 1;
    assert_eq!(
        p.blocker_with(&odd).as_deref(),
        Some("available memory below 1536 MiB")
    );
    let four = Thresholds::from_lookup(lookup(&[(MEMORY_ENV, "4096")])).unwrap();
    assert_eq!(
        p.blocker_with(&four).as_deref(),
        Some("available memory below 4 GiB")
    );
    // A lower memory threshold never relaxes the PSI gates.
    p.available_bytes = 64 * GIB;
    p.cpu_some_avg10 = 40.01;
    assert_eq!(
        p.blocker_with(&t).as_deref(),
        Some("CPU PSI some avg10 above 40%")
    );
}
#[test]
fn worker_record_drives_reported_thresholds_and_flags_mismatches() {
    let f = Fixture::new();
    let defaults = Thresholds::default();
    // No record: this process's own configuration is reported.
    let (report, warnings) = report_thresholds(&f.0, Ok(defaults), false).unwrap();
    assert_eq!(report.source, "default");
    assert_eq!(report.effective, defaults);
    assert!(warnings.is_empty());
    let low = Thresholds::from_lookup(lookup(&[(MEMORY_ENV, "3072")])).unwrap();
    let (report, _) = report_thresholds(&f.0, Ok(low), true).unwrap();
    assert_eq!(report.source, "environment");
    // Invalid local configuration without a worker record fails clearly.
    assert!(report_thresholds(&f.0, Err("bad TASKS value".into()), true).is_err());
    // The worker's recorded values win, and a differing shell is flagged.
    record_worker_thresholds(&f.0, &low).unwrap();
    let mode = fs::metadata(f.0.join(THRESHOLD_RECORD)).unwrap().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let (report, warnings) = report_thresholds(&f.0, Ok(defaults), false).unwrap();
    assert_eq!(report.source, "worker-record");
    assert_eq!(report.effective, low);
    assert_eq!(report.recorded_by_pid, Some(std::process::id()));
    assert!(warnings.is_empty());
    let (_, warnings) = report_thresholds(&f.0, Ok(defaults), true).unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let (_, warnings) = report_thresholds(&f.0, Err("bad".into()), true).unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    // A corrupt or symlinked record is a warning, never silently trusted.
    fs::write(f.0.join(THRESHOLD_RECORD), b"{not json").unwrap();
    let (report, warnings) = report_thresholds(&f.0, Ok(defaults), false).unwrap();
    assert_eq!(report.source, "default");
    assert_eq!(warnings.len(), 1);
    fs::remove_file(f.0.join(THRESHOLD_RECORD)).unwrap();
    let elsewhere = f.0.join("elsewhere.json");
    fs::write(&elsewhere, b"{}").unwrap();
    std::os::unix::fs::symlink(&elsewhere, f.0.join(THRESHOLD_RECORD)).unwrap();
    let (report, warnings) = report_thresholds(&f.0, Ok(defaults), false).unwrap();
    assert_eq!(report.source, "default");
    assert_eq!(warnings.len(), 1);
    // A recorded value below the floor (tampered) is rejected too.
    fs::remove_file(f.0.join(THRESHOLD_RECORD)).unwrap();
    let mut tampered = low;
    tampered.min_available_memory_bytes = 1;
    record_worker_thresholds(&f.0, &tampered).unwrap();
    let (report, warnings) = report_thresholds(&f.0, Ok(defaults), false).unwrap();
    assert_eq!(report.source, "default");
    assert_eq!(warnings.len(), 1);
}
#[test]
fn status_reports_effective_thresholds_and_blocker() {
    let f = Fixture::new();
    let low = Thresholds::from_lookup(lookup(&[(MEMORY_ENV, "3072")])).unwrap();
    let report = ThresholdReport::new("environment", low, None);
    let pressure = Pressure {
        available_bytes: 2 * GIB,
        ..free().unwrap()
    };
    let status = status_with(&f.0.join("missing"), pressure, report, vec![]).unwrap();
    assert_eq!(
        status.pressure_blocker.as_deref(),
        Some("available memory below 3 GiB")
    );
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["thresholds"]["min_available_memory_bytes"], 3072 * MIB);
    assert_eq!(json["thresholds"]["source"], "environment");
    assert_eq!(json["thresholds"]["cpu_some_avg10_max"], 40.0);
    assert_eq!(json["thresholds"]["io_full_avg10_max"], 10.0);
}
#[test]
fn status_reports_live_disks_and_disk_blocker_additively() {
    let f = Fixture::new();
    let pressure = free().unwrap();
    let report = ThresholdReport::new("default", Thresholds::default(), None);
    let mut status = status_with(&f.0.join("missing"), pressure, report, vec![]).unwrap();
    let disk = |path: &str, available_bytes| crate::cache::DiskSpace {
        path: path.into(),
        available_bytes,
        reserve_bytes: crate::cache::MIN_FREE_BYTES,
    };
    with_disks(
        &mut status,
        Ok(disk(
            "/fixture/build-cache",
            crate::cache::MIN_FREE_BYTES - 1,
        )),
        Err("synthetic statvfs failure".into()),
    );
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["cache_disk"]["path"], "/fixture/build-cache");
    assert_eq!(
        json["cache_disk"]["available_bytes"],
        crate::cache::MIN_FREE_BYTES - 1
    );
    assert!(json["state_disk"].is_null());
    let blocker = json["disk_blocker"].as_str().unwrap();
    assert!(blocker.contains("cache disk below reserve"), "{blocker}");
    assert!(blocker.contains("/fixture/build-cache"), "{blocker}");
    assert!(status.warnings.iter().any(|w| w.contains("state disk")));
    // Existing fields are unchanged.
    for field in [
        "backend",
        "pressure",
        "pressure_blocker",
        "thresholds",
        "resources",
        "waiting",
        "warnings",
    ] {
        assert!(json.get(field).is_some(), "{field}");
    }
    let report = ThresholdReport::new("default", Thresholds::default(), None);
    let mut ok = status_with(&f.0.join("missing"), free().unwrap(), report, vec![]).unwrap();
    let state = crate::cache::DiskSpace {
        path: "/fixture/state".into(),
        available_bytes: crate::cache::MIN_STATE_FREE_BYTES,
        reserve_bytes: crate::cache::MIN_STATE_FREE_BYTES,
    };
    with_disks(
        &mut ok,
        Ok(disk("/c", crate::cache::MIN_FREE_BYTES)),
        Ok(state),
    );
    assert_eq!(ok.disk_blocker, None);
    assert!(ok.warnings.is_empty());
}
// ---- Configurable Cargo jobs cap (dedicated compute worker) ----
#[test]
fn max_cargo_jobs_defaults_to_two_and_is_strictly_parsed() {
    assert_eq!(MAX_CARGO_JOBS_ENV, "TASKS_MAX_CARGO_JOBS");
    assert_eq!(DEFAULT_MAX_CARGO_JOBS, 2);
    assert_eq!(Thresholds::default().max_cargo_jobs, 2);
    assert_eq!(
        Thresholds::from_lookup(lookup(&[])).unwrap().max_cargo_jobs,
        2
    );
    for (value, expected) in [("1", 1), ("8", 8), ("64", 64)] {
        let t = Thresholds::from_lookup(lookup(&[(MAX_CARGO_JOBS_ENV, value)])).unwrap();
        assert_eq!(t.max_cargo_jobs, expected);
        // Only the cap changes; the memory/disk reserves keep their defaults.
        assert_eq!(
            Thresholds {
                max_cargo_jobs: DEFAULT_MAX_CARGO_JOBS,
                ..t
            },
            Thresholds::default()
        );
    }
    assert!(Thresholds::overridden(lookup(&[(MAX_CARGO_JOBS_ENV, "8")])));
    for value in [
        "",
        "0",
        "65",
        "garbage",
        "8 ",
        " 8",
        "+8",
        "-8",
        "8.0",
        "0x8",
        "1e1",
        "99999999999999999999999",
    ] {
        let error = Thresholds::from_lookup(lookup(&[(MAX_CARGO_JOBS_ENV, value)]))
            .expect_err(&format!("{value:?} must be rejected"))
            .to_string();
        assert!(error.contains(MAX_CARGO_JOBS_ENV), "{error}");
    }
}
#[test]
fn max_cargo_jobs_is_recorded_reported_and_old_records_default_to_two() {
    let f = Fixture::new();
    let eight = Thresholds::from_lookup(lookup(&[(MAX_CARGO_JOBS_ENV, "8")])).unwrap();
    record_worker_thresholds(&f.0, &eight).unwrap();
    let (report, warnings) = report_thresholds(&f.0, Ok(Thresholds::default()), false).unwrap();
    assert_eq!(report.source, "worker-record");
    assert_eq!(report.effective.max_cargo_jobs, 8);
    assert!(warnings.is_empty());
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["max_cargo_jobs"], 8);
    // A shell whose own cap differs from the worker's is flagged.
    let (_, warnings) = report_thresholds(&f.0, Ok(Thresholds::default()), true).unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(MAX_CARGO_JOBS_ENV), "{warnings:?}");
    // A tampered out-of-range cap is rejected like a tampered reserve.
    let mut tampered = eight;
    tampered.max_cargo_jobs = 65;
    record_worker_thresholds(&f.0, &tampered).unwrap();
    let (report, warnings) = report_thresholds(&f.0, Ok(Thresholds::default()), false).unwrap();
    assert_eq!(report.source, "default");
    assert_eq!(warnings.len(), 1);
    // Records written by earlier releases lack the field: they mean the default.
    let d = Thresholds::default();
    fs::write(
        f.0.join(THRESHOLD_RECORD),
        serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "pid": 1,
            "recorded_at": 1,
            "thresholds": {
                "min_available_memory_bytes": d.min_available_memory_bytes,
                "min_cache_free_bytes": d.min_cache_free_bytes,
                "min_state_free_bytes": d.min_state_free_bytes,
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let (report, warnings) = report_thresholds(&f.0, Ok(d), false).unwrap();
    assert_eq!(report.source, "worker-record");
    assert_eq!(report.effective.max_cargo_jobs, 2);
    assert!(warnings.is_empty());
}
