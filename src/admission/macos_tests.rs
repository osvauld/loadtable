use super::*;
use task_manager_platform::{DiskSpace, Memory};

fn healthy() -> Sample {
    Sample {
        resources: ResourceSample {
            memory: Memory {
                total_bytes: 32 << 30,
                available_bytes: 16 << 30,
                source: MemorySource::MacosFreeAndPurgeable,
            },
            cache_disk: DiskSpace {
                available_bytes: 100 << 30,
            },
            state_disk: DiskSpace {
                available_bytes: 100 << 30,
            },
        },
        memory_pressure: MemoryPressure::Normal,
        load_average_1m: 2.0,
        logical_cpus: 8,
    }
}

#[test]
fn native_pressure_levels_are_not_psi() {
    assert_eq!(MemoryPressure::decode(1).unwrap(), MemoryPressure::Normal);
    assert_eq!(MemoryPressure::decode(2).unwrap(), MemoryPressure::Warning);
    assert_eq!(MemoryPressure::decode(4).unwrap(), MemoryPressure::Critical);
    for raw in [-1, 0, 3, 5, 6, i32::MAX] {
        assert!(MemoryPressure::decode(raw).is_err());
    }
}

#[test]
fn passing_resources_never_enable_runtime_or_fabricate_psi() {
    let report = healthy().evaluate(&Thresholds::default()).unwrap();
    assert_eq!(report.resource_blocker, None);
    assert!(!report.runtime_enabled);
    let json = serde_json::to_string(&report).unwrap();
    assert!(json.contains("macos-compile-candidate-v1"));
    assert!(!json.contains("avg10"));
    assert!(!json.contains("psi"));
}

#[test]
fn pressure_warning_and_critical_block_even_with_abundant_resources() {
    for (pressure, expected) in [
        (MemoryPressure::Warning, "Darwin memory pressure is warning"),
        (
            MemoryPressure::Critical,
            "Darwin memory pressure is critical",
        ),
    ] {
        let mut sample = healthy();
        sample.memory_pressure = pressure;
        assert_eq!(
            sample
                .evaluate(&Thresholds::default())
                .unwrap()
                .resource_blocker,
            Some(expected)
        );
    }
}

#[test]
fn exact_reserves_and_load_limit_pass() {
    let t = Thresholds::default();
    let mut sample = healthy();
    sample.resources.memory.available_bytes = t.min_available_memory_bytes;
    sample.resources.cache_disk.available_bytes = t.min_cache_free_bytes;
    sample.resources.state_disk.available_bytes = t.min_state_free_bytes;
    sample.load_average_1m = sample.logical_cpus as f64 * MAX_LOAD_PER_LOGICAL_CPU;
    assert_eq!(sample.evaluate(&t).unwrap().resource_blocker, None);
}

#[test]
fn each_resource_floor_blocks_independently() {
    let t = Thresholds::default();
    for (index, expected) in [
        "available memory below reserve",
        "cache disk below reserve",
        "state disk below reserve",
    ]
    .into_iter()
    .enumerate()
    {
        let mut sample = healthy();
        match index {
            0 => sample.resources.memory.available_bytes = t.min_available_memory_bytes - 1,
            1 => sample.resources.cache_disk.available_bytes = t.min_cache_free_bytes - 1,
            _ => sample.resources.state_disk.available_bytes = t.min_state_free_bytes - 1,
        }
        assert_eq!(
            sample.evaluate(&t).unwrap().resource_blocker,
            Some(expected)
        );
    }
}

#[test]
fn load_above_limit_blocks() {
    let mut sample = healthy();
    sample.load_average_1m = 6.01;
    assert_eq!(
        sample
            .evaluate(&Thresholds::default())
            .unwrap()
            .resource_blocker,
        Some("Darwin one-minute load above 0.75 per logical CPU")
    );
}

#[test]
fn malformed_load_and_cpu_counts_fail_closed() {
    for load in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1] {
        let mut sample = healthy();
        sample.load_average_1m = load;
        assert!(sample.evaluate(&Thresholds::default()).is_err());
    }
    let mut sample = healthy();
    sample.logical_cpus = 0;
    assert!(sample.evaluate(&Thresholds::default()).is_err());
}

#[test]
fn invalid_or_non_native_memory_fails_closed() {
    for index in 0..3 {
        let mut sample = healthy();
        match index {
            0 => sample.resources.memory.total_bytes = 0,
            1 => sample.resources.memory.available_bytes = u64::MAX,
            _ => sample.resources.memory.source = MemorySource::LinuxMemAvailable,
        }
        assert!(sample.evaluate(&Thresholds::default()).is_err());
    }
}

#[test]
fn invalid_thresholds_cannot_pass_candidate_gate() {
    for index in 0..3 {
        let mut t = Thresholds::default();
        match index {
            0 => t.min_available_memory_bytes = 0,
            1 => t.min_cache_free_bytes = 0,
            _ => t.min_state_free_bytes = 0,
        }
        assert!(healthy().evaluate(&t).is_err());
    }
}

#[test]
fn explicit_reserves_are_honored_without_changing_defaults() {
    let t = Thresholds {
        min_available_memory_bytes: 20 << 30,
        ..Thresholds::default()
    };
    assert_eq!(
        healthy().evaluate(&t).unwrap().resource_blocker,
        Some("available memory below reserve")
    );
    assert_eq!(Thresholds::default().min_available_memory_bytes, 8 << 30);
    assert_eq!(Thresholds::default().max_cargo_jobs, 2);
}

#[test]
fn linux_run_psi_override_does_not_relax_native_load_limit() {
    let t = Thresholds {
        run_cpu_some_avg10_max_pct: 100,
        ..Thresholds::default()
    };
    let mut sample = healthy();
    sample.load_average_1m = 7.0;
    assert!(sample.evaluate(&t).unwrap().resource_blocker.is_some());
}

#[cfg(target_os = "macos")]
#[test]
fn native_probe_reads_real_resources_without_creating_paths() {
    let absent = std::env::temp_dir().join(format!(
        "tasks-macos-admission-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    assert!(!absent.exists());
    let report = Sample::read(&absent.join("cache"), &absent.join("state"))
        .unwrap()
        .evaluate(&Thresholds::default())
        .unwrap();
    assert!(!report.runtime_enabled);
    // A busy/low-memory host is allowed to block; do not lower its gates for this test.
    assert!(!absent.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn unmeasurable_disk_path_fails_closed() {
    assert!(Sample::read(std::path::Path::new("relative"), std::path::Path::new("/")).is_err());
    assert!(
        Sample::read(
            std::path::Path::new("/"),
            std::path::Path::new("/bad\0path")
        )
        .is_err()
    );
}
