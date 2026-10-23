//! Power facts shared by both operating systems: where the machine's power comes from, and the
//! handle that keeps it awake. The OS readers live in `linux/power.rs` and `macos/power.rs`.

use std::fmt;

/// Where the machine's power comes from now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerReading {
    /// Running on a battery (no mains power). A machine without a battery is never on battery.
    pub on_battery: bool,
    /// Battery charge, when there is a battery.
    pub percent: Option<u8>,
}

impl PowerReading {
    /// Mains power and no battery: a desktop.
    pub const AC: PowerReading = PowerReading {
        on_battery: false,
        percent: None,
    };
}

impl fmt::Display for PowerReading {
    /// `on battery 80%`, `on AC 100%` or `on AC`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.on_battery {
            "on battery"
        } else {
            "on AC"
        })?;
        if let Some(p) = self.percent {
            write!(f, " {p}%")?;
        }
        Ok(())
    }
}

/// Proof that the machine is being kept awake; dropping it lets the machine sleep again.
pub struct InhibitGuard {
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl InhibitGuard {
    /// A guard that runs `release` once, when dropped.
    pub fn new(release: impl FnOnce() + Send + 'static) -> InhibitGuard {
        InhibitGuard {
            release: Some(Box::new(release)),
        }
    }
}

impl Drop for InhibitGuard {
    fn drop(&mut self) {
        if let Some(r) = self.release.take() {
            r();
        }
    }
}

impl fmt::Debug for InhibitGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InhibitGuard")
    }
}

/// One entry of `/sys/class/power_supply`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supply {
    /// The `type` file (`Mains`, `Battery`, `USB`, ...).
    pub kind: String,
    /// The `online` file, for supplies that have one.
    pub online: Option<bool>,
    /// The `capacity` file, for batteries.
    pub capacity: Option<u8>,
}

/// Mains is on when any mains-type supply is online; with a battery and no mains the machine is
/// on battery. USB supplies are not counted as mains (a phone charging from the machine reports
/// itself there). No supply at all means a desktop on AC.
pub fn reading_from_supplies(supplies: &[Supply]) -> PowerReading {
    let has_battery = supplies.iter().any(|s| s.kind == "Battery");
    let mains_online = supplies
        .iter()
        .any(|s| s.kind == "Mains" && s.online == Some(true));
    let percent = supplies
        .iter()
        .find(|s| s.kind == "Battery")
        .and_then(|s| s.capacity)
        .map(|c| c.min(100));
    PowerReading {
        on_battery: has_battery && !mains_online,
        percent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(kind: &str, online: Option<bool>, capacity: Option<u8>) -> Supply {
        Supply {
            kind: kind.into(),
            online,
            capacity,
        }
    }

    #[test]
    fn r610_no_supplies_is_a_desktop_on_ac() {
        assert_eq!(reading_from_supplies(&[]), PowerReading::AC);
    }

    #[test]
    fn r610_battery_with_mains_online_is_on_ac() {
        let r =
            reading_from_supplies(&[s("Mains", Some(true), None), s("Battery", None, Some(80))]);
        assert_eq!(
            r,
            PowerReading {
                on_battery: false,
                percent: Some(80)
            }
        );
    }

    #[test]
    fn r610_battery_with_mains_offline_is_on_battery() {
        let r =
            reading_from_supplies(&[s("Mains", Some(false), None), s("Battery", None, Some(80))]);
        assert!(r.on_battery);
        assert_eq!(r.to_string(), "on battery 80%");
    }

    #[test]
    fn r610_a_usb_supply_is_not_mains() {
        let r = reading_from_supplies(&[s("USB", Some(true), None), s("Battery", None, Some(5))]);
        assert!(r.on_battery);
    }

    #[test]
    fn r610_display_without_a_percentage() {
        assert_eq!(PowerReading::AC.to_string(), "on AC");
    }

    #[test]
    fn r610_the_guard_releases_once_on_drop() {
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n2 = n.clone();
        let g = InhibitGuard::new(move || {
            n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        drop(g);
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
