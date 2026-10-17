//! Fresh-host portability (missing PSI, lock directory) and the opt-in run-slot
//! CPU PSI threshold. Synthetic /proc trees and pressure samples only.
use super::*;
use std::os::unix::fs::PermissionsExt;

struct Proc(PathBuf);
impl Proc {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "tasks-ops-proc-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(p.join("pressure")).unwrap();
        fs::write(p.join("meminfo"), "MemAvailable: 16777216 kB\n").unwrap();
        for name in ["cpu", "memory", "io"] {
            fs::write(
                p.join("pressure").join(name),
                "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
            )
            .unwrap();
        }
        Self(p)
    }
}
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn quiet() -> Pressure {
    Pressure {
        available_bytes: 64 * GIB,
        cpu_some_avg10: 0.0,
        memory_some_avg10: 0.0,
        memory_full_avg10: 0.0,
        io_some_avg10: 0.0,
        io_full_avg10: 0.0,
    }
}
fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
    move |name| {
        pairs
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| OsString::from(v))
    }
}

// ---- #29b: missing PSI fails closed with an exact message ----
#[test]
fn complete_synthetic_proc_tree_is_read() {
    let p = Proc::new();
    let read = Pressure::read_from(&p.0).unwrap();
    assert_eq!(read.available_bytes, 16 * GIB);
}
#[test]
fn missing_psi_file_is_named_and_explains_kernel_psi() {
    for missing in ["cpu", "memory", "io"] {
        let p = Proc::new();
        fs::remove_file(p.0.join("pressure").join(missing)).unwrap();
        let message = Pressure::read_from(&p.0).unwrap_err().to_string();
        let file = p.0.join("pressure").join(missing);
        assert!(message.contains(&file.display().to_string()), "{message}");
        assert!(message.contains("psi=1"), "{message}");
        assert!(message.contains("CONFIG_PSI"), "{message}");
        assert!(message.contains("fails closed"), "{message}");
    }
    let p = Proc::new();
    fs::remove_dir_all(p.0.join("pressure")).unwrap();
    let message = Pressure::read_from(&p.0).unwrap_err().to_string();
    assert!(message.contains("pressure/cpu"), "{message}");
    // Non-PSI files keep a plain message that still names the path.
    let p = Proc::new();
    fs::remove_file(p.0.join("meminfo")).unwrap();
    let message = Pressure::read_from(&p.0).unwrap_err().to_string();
    assert!(
        message.contains("meminfo") && !message.contains("psi=1"),
        "{message}"
    );
}

// ---- #29: lock directory wiring into admission ----
#[test]
fn admission_refuses_a_world_writable_lock_dir_without_creating_locks() {
    let d = Proc::new();
    let open = d.0.join("open-locks");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
    let free = || Ok(quiet());
    let error = acquire_at(&open, Kind::Compile, "model", "test", free, || false)
        .err()
        .expect("must refuse")
        .to_string();
    assert!(error.contains("world-writable"), "{error}");
    assert_eq!(fs::read_dir(&open).unwrap().count(), 0);
    fs::set_permissions(&open, fs::Permissions::from_mode(0o700)).unwrap();
}
#[test]
fn admission_creates_a_missing_lock_dir_private() {
    let d = Proc::new();
    let root = d.0.join("fresh-locks");
    let free = || Ok(quiet());
    let lease = acquire_at(&root, Kind::Compile, "model", "test", free, || false)
        .unwrap()
        .unwrap();
    assert_eq!(fs::metadata(&root).unwrap().mode() & 0o7777, 0o700);
    assert!(root.join("p2p-build.lock").is_file());
    drop(lease);
}
#[test]
fn status_reports_lock_dir_and_run_blocker_fields() {
    let d = Proc::new();
    let status = status_at(&d.0.join("missing"), quiet()).unwrap();
    let json = serde_json::to_value(&status).unwrap();
    assert!(json.get("lock_dir").is_some(), "{json}");
    assert!(json["run_pressure_blocker"].is_null(), "{json}");
}

// ---- #27: opt-in, more lenient run-slot CPU PSI threshold ----
#[test]
fn run_cpu_threshold_defaults_to_the_shared_cpu_gate() {
    assert_eq!(RUN_CPU_ENV, "TASKS_RUN_CPU_SOME_MAX_PCT");
    let t = Thresholds::default();
    assert_eq!(t.run_cpu_some_avg10_max_pct, 40);
    assert_eq!(Thresholds::from_lookup(lookup(&[])).unwrap(), t);
    let busy = Pressure {
        cpu_some_avg10: 40.01,
        ..quiet()
    };
    // Unset: run, compile and device stages keep the historical text and gate.
    for kind in [Kind::Run, Kind::Compile, Kind::Device("serial".into())] {
        assert_eq!(
            busy.blocker_for(&t, &kind).as_deref(),
            Some("CPU PSI some avg10 above 40%"),
            "{kind:?}"
        );
    }
    assert_eq!(busy.blocker_with(&t), busy.blocker_for(&t, &Kind::Compile));
}
#[test]
fn run_cpu_threshold_is_strict_and_cannot_tighten_or_exceed_bounds() {
    let ok = Thresholds::from_lookup(lookup(&[(RUN_CPU_ENV, "75")])).unwrap();
    assert_eq!(ok.run_cpu_some_avg10_max_pct, 75);
    assert!(Thresholds::overridden(lookup(&[(RUN_CPU_ENV, "75")])));
    for bad in ["39", "101", "0", "", "75%", " 75", "+75", "7.5", "abc"] {
        let error = Thresholds::from_lookup(lookup(&[(RUN_CPU_ENV, bad)]))
            .unwrap_err()
            .to_string();
        assert!(error.contains(RUN_CPU_ENV), "{bad:?}: {error}");
    }
}
#[test]
fn raised_run_cpu_threshold_relaxes_only_run_cpu_psi() {
    let t = Thresholds::from_lookup(lookup(&[(RUN_CPU_ENV, "80")])).unwrap();
    let cpu = Pressure {
        cpu_some_avg10: 60.0,
        ..quiet()
    };
    assert_eq!(cpu.blocker_for(&t, &Kind::Run), None);
    assert_eq!(
        cpu.blocker_for(&t, &Kind::Compile).as_deref(),
        Some("CPU PSI some avg10 above 40%")
    );
    let hot = Pressure {
        cpu_some_avg10: 80.5,
        ..quiet()
    };
    assert_eq!(
        hot.blocker_for(&t, &Kind::Run).as_deref(),
        Some("CPU PSI some avg10 above 80% (run slots, TASKS_RUN_CPU_SOME_MAX_PCT)")
    );
    // Memory, memory PSI and I/O PSI still gate run slots unchanged.
    for p in [
        Pressure {
            available_bytes: MIN_MEMORY - 1,
            ..cpu.clone()
        },
        Pressure {
            memory_some_avg10: 1.5,
            ..cpu.clone()
        },
        Pressure {
            memory_full_avg10: 0.6,
            ..cpu.clone()
        },
        Pressure {
            io_some_avg10: 25.0,
            ..cpu.clone()
        },
        Pressure {
            io_full_avg10: 11.0,
            ..cpu.clone()
        },
    ] {
        assert!(p.blocker_for(&t, &Kind::Run).is_some(), "{p:?}");
    }
}
#[test]
fn run_cpu_threshold_is_recorded_and_old_records_mean_default() {
    let d = Proc::new();
    let t = Thresholds::from_lookup(lookup(&[(RUN_CPU_ENV, "90")])).unwrap();
    record_worker_thresholds(&d.0, &t).unwrap();
    let (report, _) = report_thresholds(&d.0, Ok(Thresholds::default()), false).unwrap();
    assert_eq!(report.effective.run_cpu_some_avg10_max_pct, 90);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["run_cpu_some_avg10_max_pct"], 90);
    // A record from an earlier release (no field) means the default.
    let old = r#"{"schema":1,"pid":1,"recorded_at":0,"thresholds":{"min_available_memory_bytes":8589934592,"min_cache_free_bytes":21474836480,"min_state_free_bytes":268435456,"max_cargo_jobs":2}}"#;
    fs::write(d.0.join(THRESHOLD_RECORD), old).unwrap();
    let (report, warnings) = report_thresholds(&d.0, Ok(Thresholds::default()), false).unwrap();
    assert_eq!(report.source, "worker-record", "{warnings:?}");
    assert_eq!(report.effective.run_cpu_some_avg10_max_pct, 40);
    // A tampered record below the floor is not trusted.
    let mut tampered = t;
    tampered.run_cpu_some_avg10_max_pct = 10;
    record_worker_thresholds(&d.0, &tampered).unwrap();
    let (report, warnings) = report_thresholds(&d.0, Ok(Thresholds::default()), false).unwrap();
    assert_eq!(report.source, "default");
    assert_eq!(warnings.len(), 1);
}
