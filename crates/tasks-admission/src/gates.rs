//! The pressure, memory and disk gates, as pure functions of values.

use crate::reason::DiskKind;
use crate::{Lane, OFFLOAD_FLOOR_CAP_BYTES, Thresholds, WaitReason};
use tasks_core::SlotKind;
use tasks_platform::PressureSnapshot;

/// Measurements taken once per decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Readings {
    /// Pressure.
    pub pressure: PressureSnapshot,
    /// Available memory in bytes.
    pub available_memory: u64,
    /// Free bytes on the cache filesystem.
    pub cache_free: u64,
    /// Free bytes on the state filesystem.
    pub state_free: u64,
}

/// Returns the first gate that blocks, or `None` when the job may start.
///
/// - Device slots skip every gate (R-035).
/// - Offload skips CPU and I/O, keeps the memory floor min(reserve, 2 GiB), the memory
///   level and the state reserve; with `new_output` it also needs the 2 GiB fetch floor on
///   the cache disk, never the full cache reserve (R-060, R-065).
/// - The short lane skips only the CPU gate: it keeps the memory floor, the memory level, the
///   I/O gate and the disk reserve. Whether a job gets the lane at all is decided by history
///   (`Waiter::is_short`), never here.
/// - The light pool skips the CPU gate in the same way (a light job barely uses CPU, so a busy
///   machine is no reason to hold it) and keeps every other gate.
/// - Compile and run need the cache reserve only when `new_output` (R-061).
pub fn check_gates(
    kind: SlotKind,
    lane: Lane,
    new_output: bool,
    r: &Readings,
    t: &Thresholds,
) -> Option<WaitReason> {
    if kind == SlotKind::Device {
        return None;
    }
    let offload = kind == SlotKind::Offload;
    let gate = t.gate(kind);
    if r.state_free < t.state_reserve_bytes {
        return Some(WaitReason::Disk {
            which: DiskKind::State,
            free: r.state_free,
            reserve: t.state_reserve_bytes,
        });
    }
    if new_output {
        let reserve = if offload {
            t.cache_reserve_bytes.min(OFFLOAD_FLOOR_CAP_BYTES)
        } else {
            t.cache_reserve_bytes
        };
        if r.cache_free < reserve {
            return Some(WaitReason::Disk {
                which: DiskKind::Cache,
                free: r.cache_free,
                reserve,
            });
        }
    }
    let floor = if offload {
        t.min_available_bytes.min(OFFLOAD_FLOOR_CAP_BYTES)
    } else {
        t.min_available_bytes
    };
    if r.available_memory < floor {
        return Some(WaitReason::MemoryLow {
            available: r.available_memory,
            floor,
        });
    }
    if r.pressure.memory > gate.max_memory {
        return Some(WaitReason::MemoryStalls {
            kind,
            level: r.pressure.memory,
            max: gate.max_memory,
        });
    }
    if offload {
        return None;
    }
    // the short lane and the light pool exist because the CPU gate is the wrong question for them
    if lane == Lane::Normal
        && let Some(max) = gate.max_cpu
    {
        match r.pressure.cpu {
            None => return Some(WaitReason::CpuUnavailable(kind)),
            Some(m) if m > max => {
                return Some(WaitReason::Cpu {
                    kind,
                    measured: m,
                    max,
                });
            }
            Some(_) => {}
        }
    }
    if let Some(max) = gate.max_io {
        match r.pressure.io {
            None => return Some(WaitReason::IoUnavailable(kind)),
            Some(m) if m > max => {
                return Some(WaitReason::Io {
                    kind,
                    measured: m,
                    max,
                });
            }
            Some(_) => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{readings, thresholds};
    use tasks_platform::Level;

    const GIB: u64 = 1 << 30;

    #[test]
    fn healthy_machine_admits_everything() {
        let (r, t) = (readings(), thresholds());
        for k in [
            SlotKind::Compile,
            SlotKind::Run,
            SlotKind::Offload,
            SlotKind::Transfer,
            SlotKind::Device,
        ] {
            assert_eq!(check_gates(k, Lane::Normal, true, &r, &t), None);
        }
    }

    #[test]
    fn cpu_boundary_is_strict_r053() {
        let t = thresholds();
        let mut r = readings();
        r.pressure.cpu = Some(40.0);
        assert_eq!(
            check_gates(SlotKind::Compile, Lane::Normal, false, &r, &t),
            None
        );
        r.pressure.cpu = Some(63.0);
        let w = check_gates(SlotKind::Compile, Lane::Normal, false, &r, &t);
        assert_eq!(
            w.map(|w| w.to_string()).as_deref(),
            Some("CPU contended: pressure 63% > 40% for compile")
        );
    }

    #[test]
    fn missing_reading_with_a_limit_fails_closed_r053() {
        let t = thresholds();
        let mut r = readings();
        r.pressure.cpu = None;
        assert_eq!(
            check_gates(SlotKind::Compile, Lane::Normal, false, &r, &t),
            Some(WaitReason::CpuUnavailable(SlotKind::Compile))
        );
        r.pressure.cpu = Some(0.0);
        r.pressure.io = None;
        assert_eq!(
            check_gates(SlotKind::Run, Lane::Normal, false, &r, &t),
            Some(WaitReason::IoUnavailable(SlotKind::Run))
        );
        // macOS style policy: no cpu/io limits, no readings needed.
        let mut mac = thresholds();
        mac.compile.max_cpu = None;
        mac.compile.max_io = None;
        assert_eq!(
            check_gates(SlotKind::Compile, Lane::Normal, false, &r, &mac),
            None
        );
    }

    #[test]
    fn run_cpu_limit_relaxes_only_run_cpu_r059() {
        let mut t = thresholds();
        t.run.max_cpu = Some(90.0);
        let mut r = readings();
        r.pressure.cpu = Some(70.0);
        assert_eq!(
            check_gates(SlotKind::Run, Lane::Normal, false, &r, &t),
            None
        );
        assert!(check_gates(SlotKind::Compile, Lane::Normal, false, &r, &t).is_some());
        r.pressure.io = Some(99.0);
        assert!(check_gates(SlotKind::Run, Lane::Normal, false, &r, &t).is_some());
        r.pressure.io = Some(0.0);
        r.pressure.memory = Level::Warn;
        assert!(check_gates(SlotKind::Run, Lane::Normal, false, &r, &t).is_some());
    }

    #[test]
    fn memory_floor_names_the_threshold_r058() {
        let t = thresholds();
        let mut r = readings();
        r.available_memory = GIB;
        let w = check_gates(SlotKind::Compile, Lane::Normal, false, &r, &t).map(|w| w.to_string());
        assert_eq!(w.as_deref(), Some("memory: 1 GiB available, floor 4 GiB"));
    }

    #[test]
    fn native_levels_block_with_abundant_resources_r070() {
        let t = thresholds();
        let mut r = readings();
        r.available_memory = 64 * GIB;
        r.cache_free = 900 * GIB;
        for lvl in [Level::Warn, Level::Critical] {
            r.pressure.memory = lvl;
            assert!(matches!(
                check_gates(SlotKind::Compile, Lane::Normal, true, &r, &t),
                Some(WaitReason::MemoryStalls { .. })
            ));
        }
    }

    #[test]
    fn each_floor_blocks_independently_r071() {
        let t = thresholds();
        let mut r = readings();
        r.state_free = 0;
        assert!(matches!(
            check_gates(SlotKind::Run, Lane::Normal, false, &r, &t),
            Some(WaitReason::Disk {
                which: DiskKind::State,
                ..
            })
        ));
        r = readings();
        r.cache_free = 0;
        assert!(matches!(
            check_gates(SlotKind::Run, Lane::Normal, true, &r, &t),
            Some(WaitReason::Disk {
                which: DiskKind::Cache,
                ..
            })
        ));
        r = readings();
        r.available_memory = 0;
        assert!(matches!(
            check_gates(SlotKind::Run, Lane::Normal, false, &r, &t),
            Some(WaitReason::MemoryLow { .. })
        ));
    }

    #[test]
    fn disk_reserve_is_precise_and_named_r066() {
        let t = thresholds();
        let mut r = readings();
        r.cache_free = t.cache_reserve_bytes;
        assert_eq!(
            check_gates(SlotKind::Compile, Lane::Normal, true, &r, &t),
            None
        );
        r.cache_free = t.cache_reserve_bytes - 1;
        let w = check_gates(SlotKind::Compile, Lane::Normal, true, &r, &t).map(|w| w.to_string());
        assert_eq!(
            w.as_deref(),
            Some("disk: cache has 19.9 GiB free, reserve 20 GiB")
        );
    }

    #[test]
    fn existing_links_proceed_with_low_cache_disk_r061() {
        let t = thresholds();
        let mut r = readings();
        r.cache_free = 0;
        assert_eq!(
            check_gates(SlotKind::Compile, Lane::Normal, false, &r, &t),
            None
        );
    }

    #[test]
    fn offload_ignores_cpu_io_and_the_cache_reserve_r060_r065() {
        let t = thresholds();
        let mut r = readings();
        r.pressure.cpu = Some(100.0);
        r.pressure.io = Some(100.0);
        r.cache_free = 3 * GIB;
        r.available_memory = 3 * GIB;
        assert_eq!(
            check_gates(SlotKind::Offload, Lane::Normal, true, &r, &t),
            None
        );
        assert!(check_gates(SlotKind::Compile, Lane::Normal, true, &r, &t).is_some());
        // fetch floor 2 GiB, memory floor min(4, 2) GiB
        r.cache_free = GIB;
        assert!(
            matches!(check_gates(SlotKind::Offload, Lane::Normal, true, &r, &t), Some(WaitReason::Disk { reserve, .. }) if reserve == 2 * GIB)
        );
        assert_eq!(
            check_gates(SlotKind::Offload, Lane::Normal, false, &r, &t),
            None
        );
        r.available_memory = GIB;
        assert!(
            matches!(check_gates(SlotKind::Offload, Lane::Normal, false, &r, &t), Some(WaitReason::MemoryLow { floor, .. }) if floor == 2 * GIB)
        );
    }

    #[test]
    fn offload_floor_never_exceeds_the_configured_reserve_r060() {
        let mut t = thresholds();
        t.min_available_bytes = GIB;
        let mut r = readings();
        r.available_memory = GIB;
        assert_eq!(
            check_gates(SlotKind::Offload, Lane::Normal, false, &r, &t),
            None
        );
    }

    // R-332
    #[test]
    fn r332_transfer_waits_for_the_io_gate_unlike_offload() {
        let t = thresholds();
        let mut r = readings();
        r.pressure.io = Some(40.0);
        assert_eq!(
            check_gates(SlotKind::Transfer, Lane::Normal, false, &r, &t),
            None,
            "at the limit is still admitted"
        );
        r.pressure.io = Some(41.0);
        assert!(matches!(
            check_gates(SlotKind::Transfer, Lane::Normal, false, &r, &t),
            Some(WaitReason::Io {
                kind: SlotKind::Transfer,
                ..
            })
        ));
        assert_eq!(
            check_gates(SlotKind::Offload, Lane::Normal, false, &r, &t),
            None,
            "the offload slot ignores I/O"
        );
        // a missing reading fails closed, like every other I/O gate
        r.pressure.io = None;
        assert_eq!(
            check_gates(SlotKind::Transfer, Lane::Normal, false, &r, &t),
            Some(WaitReason::IoUnavailable(SlotKind::Transfer))
        );
    }

    // R-332
    #[test]
    fn r332_transfer_ignores_cpu_but_keeps_memory_and_the_state_reserve() {
        let t = thresholds();
        let mut r = readings();
        r.pressure.cpu = Some(99.0);
        assert_eq!(
            check_gates(SlotKind::Transfer, Lane::Normal, false, &r, &t),
            None
        );
        r.available_memory = GIB;
        assert!(matches!(
            check_gates(SlotKind::Transfer, Lane::Normal, false, &r, &t),
            Some(WaitReason::MemoryLow { .. })
        ));
        r.available_memory = 8 * GIB;
        r.state_free = 0;
        assert!(matches!(
            check_gates(SlotKind::Transfer, Lane::Normal, false, &r, &t),
            Some(WaitReason::Disk { .. })
        ));
    }

    #[test]
    fn short_lane_skips_only_the_cpu_gate() {
        let t = thresholds();
        let mut r = readings();
        r.pressure.cpu = Some(95.0);
        assert!(check_gates(SlotKind::Compile, Lane::Normal, false, &r, &t).is_some());
        assert_eq!(
            check_gates(SlotKind::Compile, Lane::Short, false, &r, &t),
            None
        );
        r.available_memory = 0;
        assert!(check_gates(SlotKind::Compile, Lane::Short, false, &r, &t).is_some());
    }
}
