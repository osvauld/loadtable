//! Human-readable wait reasons that name the measured number and the threshold.

use crate::Hold;
use std::fmt;
use tasks_core::{DeviceId, JobId, SlotKind};
use tasks_platform::Level;

/// Which filesystem a disk wait is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskKind {
    /// Where cache output lands.
    Cache,
    /// Where state lives.
    State,
}

/// Why a waiter is not admitted yet.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitReason {
    /// An extra hold on the waiter.
    Held(Hold),
    /// An earlier waiter on the same checkout goes first.
    BehindCheckout(JobId),
    /// Every slot of the kind is busy.
    SlotsBusy {
        /// Slot kind.
        kind: SlotKind,
        /// Number of slots of this kind.
        total: usize,
        /// Holders; `None` is a holder outside the ledger.
        holders: Vec<Option<JobId>>,
        /// Waiters of the same kind ahead of this one.
        ahead: Vec<JobId>,
    },
    /// The slot was taken between planning and locking.
    SlotTaken(SlotKind, u32),
    /// Another job holds the device.
    DeviceBusy {
        /// The device.
        device: DeviceId,
        /// Holder, when known.
        holder: Option<JobId>,
    },
    /// The device is not attached.
    DeviceUnknown(DeviceId),
    /// A device job that names no device.
    NoDevice,
    /// CPU pressure over the limit.
    Cpu {
        /// Slot kind.
        kind: SlotKind,
        /// Measured percent.
        measured: f32,
        /// Limit percent.
        max: f32,
    },
    /// A CPU limit is set but the reading is missing (fail closed, R-053).
    CpuUnavailable(SlotKind),
    /// I/O pressure over the limit.
    Io {
        /// Slot kind.
        kind: SlotKind,
        /// Measured percent.
        measured: f32,
        /// Limit percent.
        max: f32,
    },
    /// An I/O limit is set but the reading is missing (fail closed).
    IoUnavailable(SlotKind),
    /// Memory pressure level above the allowed level.
    MemoryStalls {
        /// Slot kind.
        kind: SlotKind,
        /// Measured level.
        level: Level,
        /// Highest allowed level.
        max: Level,
    },
    /// Available memory below the floor.
    MemoryLow {
        /// Available bytes.
        available: u64,
        /// Required floor in bytes.
        floor: u64,
    },
    /// Free disk below the reserve.
    Disk {
        /// Which filesystem.
        which: DiskKind,
        /// Free bytes.
        free: u64,
        /// Reserve in bytes.
        reserve: u64,
    },
}

/// The lower-case name of a slot kind.
pub fn kind_name(k: SlotKind) -> &'static str {
    match k {
        SlotKind::Compile => "compile",
        SlotKind::Run => "run",
        SlotKind::Offload => "offload",
        SlotKind::Device => "device",
        SlotKind::Transfer => "transfer",
    }
}

/// Formats bytes as GiB with at most one decimal (rounded down): `12.1 GiB`, `20 GiB`.
pub fn gib(bytes: u64) -> String {
    let tenths = u128::from(bytes) * 10 / (1u128 << 30);
    if tenths.is_multiple_of(10) {
        format!("{} GiB", tenths / 10)
    } else {
        format!("{}.{} GiB", tenths / 10, tenths % 10)
    }
}

fn ordinal(n: usize) -> String {
    let suffix = match (n % 100, n % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

impl fmt::Display for Hold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Hold::SameCheckoutBusy(j) => write!(f, "checkout busy with running #{}", j.0),
            Hold::Dependency(j) => write!(f, "waiting for #{} to finish", j.0),
            Hold::Drain => write!(f, "operator drain: no new admissions"),
            Hold::Other(s) => write!(f, "{s}"),
        }
    }
}

impl fmt::Display for WaitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaitReason::Held(h) => write!(f, "held: {h}"),
            WaitReason::BehindCheckout(j) => write!(f, "behind #{} (same checkout)", j.0),
            WaitReason::SlotsBusy {
                kind,
                total,
                holders,
                ahead,
            } => {
                let k = kind_name(*kind);
                if *total == 0 {
                    return write!(f, "no {k} slots configured");
                }
                let who: Vec<String> = holders
                    .iter()
                    .map(|h| h.map_or("external".to_string(), |j| format!("#{}", j.0)))
                    .collect();
                let all = if *total == 1 {
                    format!("the only {k} slot is busy")
                } else {
                    format!("all {total} {k} slots busy")
                };
                write!(
                    f,
                    "{all} ({}); you are {} in line",
                    who.join(", "),
                    ordinal(ahead.len() + 1)
                )
            }
            WaitReason::SlotTaken(k, i) => {
                write!(f, "{} slot {i} was just taken; retrying", kind_name(*k))
            }
            WaitReason::DeviceBusy { device, holder } => match holder {
                Some(j) => write!(f, "device {} held by #{}", device.0, j.0),
                None => write!(f, "device {} held by an external process", device.0),
            },
            WaitReason::DeviceUnknown(d) => write!(f, "device {} is not attached", d.0),
            WaitReason::NoDevice => write!(f, "device job names no device"),
            WaitReason::Cpu {
                kind,
                measured,
                max,
            } => write!(
                f,
                "CPU contended: pressure {measured:.0}% > {max:.0}% for {}",
                kind_name(*kind)
            ),
            WaitReason::CpuUnavailable(k) => write!(
                f,
                "CPU pressure unreadable for {} (refusing, fail closed)",
                kind_name(*k)
            ),
            WaitReason::Io {
                kind,
                measured,
                max,
            } => write!(
                f,
                "I/O stalls: pressure {measured:.0}% > {max:.0}% for {}",
                kind_name(*kind)
            ),
            WaitReason::IoUnavailable(k) => write!(
                f,
                "I/O pressure unreadable for {} (refusing, fail closed)",
                kind_name(*k)
            ),
            WaitReason::MemoryStalls { kind, level, max } => write!(
                f,
                "memory stalls: level {} > {} for {}",
                format!("{level:?}").to_lowercase(),
                format!("{max:?}").to_lowercase(),
                kind_name(*kind)
            ),
            WaitReason::MemoryLow { available, floor } => write!(
                f,
                "memory: {} available, floor {}",
                gib(*available),
                gib(*floor)
            ),
            WaitReason::Disk {
                which,
                free,
                reserve,
            } => write!(
                f,
                "disk: {} has {} free, reserve {}",
                match which {
                    DiskKind::Cache => "cache",
                    DiskKind::State => "state",
                },
                gib(*free),
                gib(*reserve)
            ),
        }
    }
}

impl WaitReason {
    /// Maps to the coarse phase reason stored in the ledger; the text goes with it.
    pub fn to_core(&self) -> tasks_core::WaitReason {
        use tasks_core::WaitReason as C;
        match self {
            WaitReason::SlotsBusy { kind, .. } | WaitReason::SlotTaken(kind, _) => C::Slot(*kind),
            WaitReason::Held(Hold::Dependency(j)) => C::Dependency(*j),
            WaitReason::Disk { .. } => C::Disk,
            WaitReason::DeviceBusy { device, .. } | WaitReason::DeviceUnknown(device) => {
                C::Device(device.clone())
            }
            other => C::Pressure(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gib_rounds_down_to_one_decimal() {
        assert_eq!(gib(20 << 30), "20 GiB");
        assert_eq!(gib((12 << 30) + (1 << 30) / 8), "12.1 GiB");
        assert_eq!(gib(0), "0 GiB");
    }

    #[test]
    fn slots_busy_names_holders_and_position() {
        let r = WaitReason::SlotsBusy {
            kind: SlotKind::Compile,
            total: 2,
            holders: vec![Some(JobId(4278)), Some(JobId(4280))],
            ahead: vec![JobId(1)],
        };
        assert_eq!(
            r.to_string(),
            "all 2 compile slots busy (#4278, #4280); you are 2nd in line"
        );
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(23), "23rd");
    }

    #[test]
    fn labels_never_say_memory_high_r073() {
        let m = WaitReason::MemoryStalls {
            kind: SlotKind::Run,
            level: Level::Warn,
            max: Level::Normal,
        };
        let text = m.to_string();
        assert!(text.contains("memory stalls") && !text.contains("memory high"));
        let c = WaitReason::Cpu {
            kind: SlotKind::Compile,
            measured: 63.4,
            max: 40.0,
        };
        assert_eq!(
            c.to_string(),
            "CPU contended: pressure 63% > 40% for compile"
        );
        let i = WaitReason::Io {
            kind: SlotKind::Run,
            measured: 30.0,
            max: 20.0,
        };
        assert!(i.to_string().starts_with("I/O stalls"));
    }

    #[test]
    fn to_core_maps_the_coarse_reason() {
        let d = WaitReason::Disk {
            which: DiskKind::Cache,
            free: 0,
            reserve: 1,
        };
        assert_eq!(d.to_core(), tasks_core::WaitReason::Disk);
        let dep = WaitReason::Held(Hold::Dependency(JobId(3)));
        assert_eq!(dep.to_core(), tasks_core::WaitReason::Dependency(JobId(3)));
    }
}
