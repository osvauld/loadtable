//! Offload admission class: its own lock pool and a lightweight-client gate
//! (no CPU, memory-some or I/O PSI). Synthetic pressure samples and lock dirs only.
use super::*;
use std::sync::atomic::AtomicUsize;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "tasks-offload-admission-{}-{}",
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
/// A laptop busy compiling: every shared gate is closed except memory.
fn compiling() -> Pressure {
    Pressure {
        available_bytes: 3 * GIB,
        cpu_some_avg10: 100.0,
        memory_some_avg10: 30.0,
        memory_full_avg10: 0.5,
        io_some_avg10: 90.0,
        io_full_avg10: 60.0,
    }
}

#[test]
fn offload_pool_has_its_own_lock_files() {
    let names = Kind::Offload.names().unwrap();
    assert_eq!(
        names,
        [
            "p2p-offload-0.lock",
            "p2p-offload-1.lock",
            "p2p-offload-2.lock",
            "p2p-offload-3.lock"
        ]
    );
    assert_eq!(names.len(), OFFLOAD_SLOTS);
    assert_eq!(Kind::Offload.label(), "offload");
    // Legacy names are unchanged and disjoint from the offload pool.
    assert_eq!(Kind::Compile.names().unwrap(), ["p2p-build.lock"]);
    let run = Kind::Run.names().unwrap();
    assert_eq!(
        run,
        [
            "p2p-run-0.lock",
            "p2p-run-1.lock",
            "p2p-run-2.lock",
            "p2p-run-3.lock"
        ]
    );
    assert!(names.iter().all(|n| !run.contains(n)));
}

#[test]
fn offload_gate_ignores_cpu_memory_some_and_io_psi() {
    let t = Thresholds::default();
    let p = compiling();
    assert_eq!(p.blocker_for(&t, &Kind::Offload), None);
    // The same sample blocks every other class.
    for kind in [Kind::Compile, Kind::Run, Kind::Device("s".into())] {
        assert!(p.blocker_for(&t, &kind).is_some(), "{kind:?}");
    }
}

#[test]
fn offload_gate_keeps_a_memory_floor_and_memory_thrash_gate() {
    let t = Thresholds::default();
    assert_eq!(offload_min_memory(&t), OFFLOAD_MIN_MEMORY);
    assert_eq!(OFFLOAD_MIN_MEMORY, 2 * GIB);
    let low = Pressure {
        available_bytes: OFFLOAD_MIN_MEMORY - 1,
        ..quiet()
    };
    assert_eq!(
        low.blocker_for(&t, &Kind::Offload).as_deref(),
        Some("available memory below 2 GiB (offload slots)")
    );
    let at = Pressure {
        available_bytes: OFFLOAD_MIN_MEMORY,
        ..quiet()
    };
    assert_eq!(at.blocker_for(&t, &Kind::Offload), None);
    let thrash = Pressure {
        memory_full_avg10: 0.51,
        ..quiet()
    };
    assert_eq!(
        thrash.blocker_for(&t, &Kind::Offload).as_deref(),
        Some("memory PSI full avg10 above 0.5%")
    );
    for bad in [f64::NAN, -1.0, 100.5] {
        let p = Pressure {
            cpu_some_avg10: bad,
            ..quiet()
        };
        assert_eq!(
            p.blocker_for(&t, &Kind::Offload).as_deref(),
            Some("invalid pressure metric")
        );
    }
}

#[test]
fn offload_memory_floor_never_exceeds_the_configured_reserve() {
    let t = Thresholds::from_lookup(|name| (name == MEMORY_ENV).then(|| "1024".into())).unwrap();
    assert_eq!(offload_min_memory(&t), GIB);
    let p = Pressure {
        available_bytes: GIB,
        ..quiet()
    };
    assert_eq!(p.blocker_for(&t, &Kind::Offload), None);
    // The run-slot CPU override does not affect offload either way.
    let t = Thresholds::from_lookup(|name| (name == RUN_CPU_ENV).then(|| "100".into())).unwrap();
    assert_eq!(compiling().blocker_for(&t, &Kind::Offload), None);
}

fn cache_space(available_bytes: u64) -> Result<crate::cache::DiskSpace> {
    Ok(crate::cache::DiskSpace {
        path: "/synthetic/build-cache".into(),
        available_bytes,
        reserve_bytes: 0,
    })
}
fn state_space(available_bytes: u64) -> Result<crate::cache::DiskSpace> {
    Ok(crate::cache::DiskSpace {
        path: "/synthetic/state".into(),
        available_bytes,
        reserve_bytes: crate::cache::MIN_STATE_FREE_BYTES,
    })
}

#[test]
fn offload_job_is_admitted_below_the_cache_reserve_but_above_the_fetch_floor() {
    let t = Thresholds::default();
    assert_eq!(t.min_cache_free_bytes, 20 * GIB);
    let floor = OFFLOAD_FETCH_FLOOR + t.min_state_free_bytes;
    assert_eq!(OFFLOAD_FETCH_FLOOR, 2 * GIB);
    // 10 GiB free: a local compile job would wait (20 GiB reserve); offload does not.
    for fetches in [true, false] {
        assert_eq!(
            offload_disk_blocker(fetches, cache_space(10 * GIB), state_space(GIB), &t),
            None,
            "fetches={fetches}"
        );
    }
    assert_eq!(
        offload_disk_blocker(true, cache_space(floor), state_space(GIB), &t),
        None
    );
}

#[test]
fn fetching_offload_job_waits_below_the_floor_non_fetching_only_on_the_state_reserve() {
    let t = Thresholds::default();
    let floor = OFFLOAD_FETCH_FLOOR + t.min_state_free_bytes;
    let text = offload_disk_blocker(true, cache_space(floor - 1), state_space(GIB), &t).unwrap();
    assert!(text.contains("offload fetch floor"), "{text}");
    assert!(text.contains("/synthetic/build-cache"), "{text}");
    assert!(text.contains("2.2 GiB reserve"), "{text}");
    // Without a fetch the cache disk is irrelevant, even when full or unmeasurable.
    assert_eq!(
        offload_disk_blocker(false, cache_space(0), state_space(GIB), &t),
        None
    );
    assert_eq!(
        offload_disk_blocker(false, Err("HOME is required".into()), state_space(GIB), &t),
        None
    );
    // The state reserve always applies; unmeasurable fails closed.
    let text = offload_disk_blocker(false, cache_space(0), state_space(1), &t).unwrap();
    assert!(text.contains("state disk below reserve"), "{text}");
    let text = offload_disk_blocker(false, cache_space(0), Err("gone".into()), &t).unwrap();
    assert!(text.contains("state disk not measured"), "{text}");
    let text =
        offload_disk_blocker(true, Err("HOME is required".into()), state_space(GIB), &t).unwrap();
    assert!(text.contains("cache disk not measured"), "{text}");
}

#[test]
fn local_compile_disk_gate_still_needs_the_full_cache_reserve() {
    let t = Thresholds::default();
    let low = crate::cache::DiskSpace {
        path: "/synthetic/build-cache".into(),
        available_bytes: 10 * GIB,
        reserve_bytes: t.min_cache_free_bytes,
    };
    // The same 10 GiB that admits an offload job blocks local output placement.
    assert!(!low.admitted());
    let text = crate::cache::disk_blocker(Some(&low), None).unwrap();
    assert!(text.contains("cache disk below reserve"), "{text}");
}

#[test]
fn offload_acquires_while_compile_and_run_slots_are_full_and_cpu_is_saturated() {
    let f = Fixture::new();
    let free = || {
        Ok(Pressure {
            available_bytes: MIN_MEMORY,
            ..quiet()
        })
    };
    let mut held = vec![
        acquire_at(&f.0, Kind::Compile, "model", "c", free, || false)
            .unwrap()
            .unwrap(),
    ];
    for _ in 0..4 {
        held.push(
            acquire_at(&f.0, Kind::Run, "model", "r", free, || false)
                .unwrap()
                .unwrap(),
        );
    }
    let busy = || Ok(compiling());
    let mut offloads = vec![];
    for _ in 0..OFFLOAD_SLOTS {
        offloads.push(
            acquire_gated(&f.0, Kind::Offload, "model", "o", busy, || None, || false)
                .unwrap()
                .unwrap(),
        );
    }
    // A fifth offload waits for the pool; it never borrows a run slot.
    let n = AtomicUsize::new(0);
    assert!(
        acquire_gated(
            &f.0,
            Kind::Offload,
            "model",
            "o",
            busy,
            || None,
            || n.fetch_add(1, Ordering::Relaxed) > 3
        )
        .unwrap()
        .is_none()
    );
    // Offloads holding the pool never block run or compile slots.
    drop(held.pop());
    held.push(
        acquire_at(&f.0, Kind::Run, "model", "r", free, || false)
            .unwrap()
            .unwrap(),
    );
    let status = status_at(&f.0, compiling()).unwrap();
    let busy_names: Vec<String> = status
        .resources
        .iter()
        .filter(|r| r.busy)
        .map(|r| r.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    for name in Kind::Offload.names().unwrap() {
        assert!(busy_names.contains(&name), "{busy_names:?}");
    }
    let holder = status
        .resources
        .iter()
        .find(|r| r.path.ends_with("p2p-offload-0.lock"))
        .and_then(|r| r.holder.as_ref())
        .expect("offload holder note");
    assert_eq!(holder.slot.as_deref(), Some("offload"));
    assert_eq!(status.offload_pressure_blocker, None);
    assert!(status.pressure_blocker.is_some());
}

#[test]
fn offload_disk_blocker_makes_it_wait_and_can_be_stopped() {
    let f = Fixture::new();
    let n = AtomicUsize::new(0);
    let result = acquire_gated(
        &f.0,
        Kind::Offload,
        "model",
        "o",
        || Ok(quiet()),
        || Some("cache disk below reserve".into()),
        || n.fetch_add(1, Ordering::Relaxed) > 2,
    )
    .unwrap();
    assert!(result.is_none());
    // Nothing was taken while it waited.
    let lease = acquire_gated(
        &f.0,
        Kind::Offload,
        "m",
        "o",
        || Ok(quiet()),
        || None,
        || false,
    )
    .unwrap()
    .unwrap();
    assert!(lease.holder.ends_with("p2p-offload-0.holder"));
}

#[test]
fn status_reports_the_offload_blocker_and_floor() {
    let f = Fixture::new();
    let low = Pressure {
        available_bytes: GIB,
        ..quiet()
    };
    let status = status_at(&f.0, low).unwrap();
    assert_eq!(
        status.offload_pressure_blocker.as_deref(),
        Some("available memory below 2 GiB (offload slots)")
    );
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(
        json["thresholds"]["offload_min_available_memory_bytes"],
        2 * GIB
    );
    assert_eq!(json["thresholds"]["offload_slots"], OFFLOAD_SLOTS);
    // The disk reserve joins the offload blocker when it is the only blocker.
    let mut status = status_at(&f.0, quiet()).unwrap();
    let low_cache = crate::cache::DiskSpace {
        path: "/synthetic/build-cache".into(),
        available_bytes: 0,
        reserve_bytes: 20 * GIB,
    };
    with_disks(&mut status, Ok(low_cache), Err("synthetic".into()));
    let text = status.offload_pressure_blocker.unwrap();
    assert!(text.contains("offload fetch floor"), "{text}");
    let mut status = status_at(&f.0, quiet()).unwrap();
    with_disks(
        &mut status,
        Err("synthetic".into()),
        Err("synthetic".into()),
    );
    let text = status.offload_pressure_blocker.unwrap();
    assert!(text.contains("cache disk not measured"), "{text}");
}

#[test]
fn service_slots_have_their_own_names_and_ignore_cpu_pressure() {
    let names = Kind::Service.names().unwrap();
    assert_eq!(names.len(), SERVICE_SLOTS);
    assert!(names.iter().all(|n| n.starts_with("p2p-service-")));
    assert_eq!(Kind::Service.label(), "service");
    let t = Thresholds::default();
    assert_eq!(compiling().blocker_for(&t, &Kind::Service), None);
    let f = Fixture::new();
    let busy = || Ok(compiling());
    let held: Vec<_> = (0..SERVICE_SLOTS)
        .map(|_| {
            acquire_gated(&f.0, Kind::Service, "model", "s", busy, || None, || false)
                .unwrap()
                .unwrap()
        })
        .collect();
    assert_eq!(held.len(), SERVICE_SLOTS);
}
