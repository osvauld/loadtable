//! Health thresholds and intervals: constants with environment overrides.
//!
//! TODO(policy file): feature/log-search adds a HUML policy file loader. Once it lands, read
//! these values from it (this struct is the seam: `HealthConfig::from_lookup` is the only
//! place they are resolved). Until then: environment variables of the daemon, else defaults.
//! Health is advisory, so an invalid value is reported and its default used (admission
//! thresholds, by contrast, fail closed).
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

const GIB: u64 = 1024 * 1024 * 1024;

/// How often the checks run (seconds, 1..=3600). Default 30.
pub const INTERVAL_ENV: &str = "TASKS_HEALTH_INTERVAL_S";
/// How often the bounded disk-usage scan runs (seconds, 10..=86400). Default 600.
pub const DISK_SCAN_ENV: &str = "TASKS_HEALTH_DISK_SCAN_S";
/// Entry budget of one disk-usage scan (1000..=50000000). Default 300000.
pub const DISK_SCAN_FILES_ENV: &str = "TASKS_HEALTH_DISK_SCAN_FILES";
/// Time budget of one disk-usage scan (seconds, 1..=600). Default 15.
pub const DISK_SCAN_SECONDS_ENV: &str = "TASKS_HEALTH_DISK_SCAN_MAX_S";
/// Warn when a disk is estimated to fill within this many hours (1..=720). Default 6.
pub const TTF_ENV: &str = "TASKS_HEALTH_TIME_TO_FULL_H";
/// Warn when one checkout's build output exceeds this many GiB (1..=65536). Default 60.
pub const TARGET_ENV: &str = "TASKS_HEALTH_TARGET_MAX_GIB";
/// A slot held with no live job for this long raises an alert (seconds, 10..=86400). Default 120.
pub const SLOT_ENV: &str = "TASKS_HEALTH_SLOT_ORPHAN_S";
/// Memory pressure above the gate for this long raises an alert (seconds, 10..=86400). Default 300.
pub const PRESSURE_ENV: &str = "TASKS_HEALTH_PRESSURE_S";
/// A job waiting longer than this raises an alert (minutes, 1..=10080). Default 30.
pub const JOB_WAIT_ENV: &str = "TASKS_HEALTH_JOB_WAIT_MIN";
/// A compile job queued this long while a peer is idle raises an alert (seconds, 10..=86400).
/// Default 120.
pub const IDLE_PEER_ENV: &str = "TASKS_HEALTH_IDLE_PEER_S";
/// A peer offline/stale for this long raises an alert (seconds, 5..=86400). Default 60.
pub const PEER_ENV: &str = "TASKS_HEALTH_PEER_S";
/// A finished job above this many CPU-hours raises an info alert (1..=1000). Default 2.
pub const HIGH_CPU_ENV: &str = "TASKS_HEALTH_HIGH_CPU_H";
/// A missing-capability mark expires after this many hours (1..=8760). Default 24.
pub const CAPABILITY_TTL_ENV: &str = "TASKS_HEALTH_CAPABILITY_TTL_H";
/// Desktop notifier: `on`, `off`, or `file:/ABS/PATH` (JSON lines; tests). Default: on only
/// on a controller (role host) running on the shared default state with a desktop session.
pub const NOTIFY_ENV: &str = "TASKS_HEALTH_NOTIFY";

#[derive(Clone, Debug, PartialEq)]
pub enum NotifyMode {
    Off,
    /// `notify-send`, when it is on PATH.
    Desktop,
    /// Append JSON lines to this file (hermetic tests; never touches a desktop session).
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct HealthConfig {
    pub interval: Duration,
    pub disk_scan_interval: Duration,
    pub disk_scan_max_entries: u64,
    pub disk_scan_max_time: Duration,
    pub time_to_full_warn_s: f64,
    pub target_max_bytes: u64,
    pub slot_orphan_s: f64,
    pub pressure_s: f64,
    pub job_wait_s: f64,
    pub idle_peer_s: f64,
    pub peer_s: f64,
    pub high_cpu_s: f64,
    pub capability_ttl_s: f64,
    /// Explicit notifier choice; `None` = the default rule (see [`NOTIFY_ENV`]).
    pub notify: Option<NotifyMode>,
    /// Per alert id: at most one notification per this many seconds.
    pub notify_per_alert_s: f64,
    /// At most `notify_burst` notifications per `notify_window_s` overall.
    pub notify_burst: usize,
    pub notify_window_s: f64,
    /// Invalid settings, reported once in the daemon log.
    pub warnings: Vec<String>,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            disk_scan_interval: Duration::from_secs(600),
            disk_scan_max_entries: 300_000,
            disk_scan_max_time: Duration::from_secs(15),
            time_to_full_warn_s: 6.0 * 3600.0,
            target_max_bytes: 60 * GIB,
            slot_orphan_s: 120.0,
            pressure_s: 300.0,
            job_wait_s: 30.0 * 60.0,
            idle_peer_s: 120.0,
            peer_s: 60.0,
            high_cpu_s: 2.0 * 3600.0,
            capability_ttl_s: 24.0 * 3600.0,
            notify: None,
            notify_per_alert_s: 3600.0,
            notify_burst: 3,
            notify_window_s: 600.0,
            warnings: vec![],
        }
    }
}

impl HealthConfig {
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<OsString>) -> Self {
        let mut c = Self::default();
        let mut warnings = vec![];
        let mut num = |env: &str, min: u64, max: u64| -> Option<u64> {
            let raw = get(env)?;
            match raw.to_str().and_then(|t| t.parse::<u64>().ok()) {
                Some(v) if (min..=max).contains(&v) => Some(v),
                _ => {
                    warnings.push(format!(
                        "{env} must be a whole number from {min} to {max}; got {:?}, default used",
                        raw.to_string_lossy()
                    ));
                    None
                }
            }
        };
        if let Some(v) = num(INTERVAL_ENV, 1, 3600) {
            c.interval = Duration::from_secs(v);
        }
        if let Some(v) = num(DISK_SCAN_ENV, 10, 86_400) {
            c.disk_scan_interval = Duration::from_secs(v);
        }
        if let Some(v) = num(DISK_SCAN_FILES_ENV, 1000, 50_000_000) {
            c.disk_scan_max_entries = v;
        }
        if let Some(v) = num(DISK_SCAN_SECONDS_ENV, 1, 600) {
            c.disk_scan_max_time = Duration::from_secs(v);
        }
        if let Some(v) = num(TTF_ENV, 1, 720) {
            c.time_to_full_warn_s = v as f64 * 3600.0;
        }
        if let Some(v) = num(TARGET_ENV, 1, 65_536) {
            c.target_max_bytes = v * GIB;
        }
        if let Some(v) = num(SLOT_ENV, 10, 86_400) {
            c.slot_orphan_s = v as f64;
        }
        if let Some(v) = num(PRESSURE_ENV, 10, 86_400) {
            c.pressure_s = v as f64;
        }
        if let Some(v) = num(JOB_WAIT_ENV, 1, 10_080) {
            c.job_wait_s = v as f64 * 60.0;
        }
        if let Some(v) = num(IDLE_PEER_ENV, 10, 86_400) {
            c.idle_peer_s = v as f64;
        }
        if let Some(v) = num(PEER_ENV, 5, 86_400) {
            c.peer_s = v as f64;
        }
        if let Some(v) = num(HIGH_CPU_ENV, 1, 1000) {
            c.high_cpu_s = v as f64 * 3600.0;
        }
        if let Some(v) = num(CAPABILITY_TTL_ENV, 1, 8760) {
            c.capability_ttl_s = v as f64 * 3600.0;
        }
        if let Some(raw) = get(NOTIFY_ENV) {
            let text = raw.to_string_lossy();
            c.notify = match text.as_ref() {
                "on" | "1" | "true" => Some(NotifyMode::Desktop),
                "off" | "0" | "false" => Some(NotifyMode::Off),
                t if t.starts_with("file:/") => Some(NotifyMode::File(PathBuf::from(&t[5..]))),
                other => {
                    warnings.push(format!(
                        "{NOTIFY_ENV} must be on, off or file:/ABS/PATH; got {other:?}, notifier off"
                    ));
                    Some(NotifyMode::Off)
                }
            };
        }
        c.warnings = warnings;
        c
    }

    /// The notifier this daemon uses. Unless set explicitly, desktop notifications are on only
    /// for a controller (role host) on the shared default state (no `TASK_MANAGER_STATE_DIR`:
    /// test and isolated daemons never notify) with a desktop session in its environment.
    pub fn notify_mode(
        &self,
        role_host: bool,
        get: impl Fn(&str) -> Option<OsString>,
    ) -> NotifyMode {
        if let Some(mode) = &self.notify {
            return mode.clone();
        }
        let isolated = get("TASK_MANAGER_STATE_DIR").is_some();
        let session = ["DBUS_SESSION_BUS_ADDRESS", "WAYLAND_DISPLAY", "DISPLAY"]
            .iter()
            .any(|name| get(name).is_some_and(|v| !v.is_empty()));
        if role_host && !isolated && session {
            NotifyMode::Desktop
        } else {
            NotifyMode::Off
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).map(OsString::from)
    }

    #[test]
    fn defaults_overrides_and_invalid_values() {
        let c = HealthConfig::from_lookup(lookup(&[]));
        assert_eq!(c, HealthConfig::default());
        assert_eq!(c.interval, Duration::from_secs(30));
        assert_eq!(c.disk_scan_interval, Duration::from_secs(600));
        let c = HealthConfig::from_lookup(lookup(&[
            (INTERVAL_ENV, "2"),
            (JOB_WAIT_ENV, "5"),
            (TARGET_ENV, "0"),
            (NOTIFY_ENV, "file:/tmp/x.jsonl"),
        ]));
        assert_eq!(c.interval, Duration::from_secs(2));
        assert_eq!(c.job_wait_s, 300.0);
        assert_eq!(
            c.target_max_bytes,
            60 * GIB,
            "invalid value keeps the default"
        );
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        assert_eq!(c.notify, Some(NotifyMode::File("/tmp/x.jsonl".into())));
    }

    #[test]
    fn desktop_notifier_defaults_on_only_for_a_shared_controller_with_a_session() {
        let c = HealthConfig::default();
        let session = lookup(&[("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1/bus")]);
        assert_eq!(c.notify_mode(true, &session), NotifyMode::Desktop);
        assert_eq!(c.notify_mode(false, &session), NotifyMode::Off, "worker");
        let isolated = lookup(&[
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1/bus"),
            ("TASK_MANAGER_STATE_DIR", "/tmp/fixture"),
        ]);
        assert_eq!(
            c.notify_mode(true, &isolated),
            NotifyMode::Off,
            "test daemon"
        );
        assert_eq!(
            c.notify_mode(true, lookup(&[])),
            NotifyMode::Off,
            "no session"
        );
        let forced = HealthConfig::from_lookup(lookup(&[(NOTIFY_ENV, "off")]));
        assert_eq!(forced.notify_mode(true, &session), NotifyMode::Off);
    }
}
