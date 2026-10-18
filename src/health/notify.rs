//! Desktop notifications for alerts that need a person, rate-limited.
//!
//! The notifier is injected: the daemon uses [`NotifySend`] (`notify-send`, if on PATH) only
//! where `HealthConfig::notify_mode` allows it, [`FileNotifier`] in hermetic tests, and unit
//! tests use their own recorder. Nothing here runs against a session unless the daemon chose
//! the desktop mode.
use super::alert::{Alert, Severity};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub trait Notifier: Send {
    fn send(&mut self, title: &str, body: &str, critical: bool) -> Result<(), String>;
}

/// No notifications.
pub struct Silent;
impl Notifier for Silent {
    fn send(&mut self, _: &str, _: &str, _: bool) -> Result<(), String> {
        Ok(())
    }
}

/// `notify-send -a tasks -u normal|critical TITLE BODY`, bounded to a few seconds.
pub struct NotifySend {
    program: PathBuf,
}

impl NotifySend {
    /// None when `notify-send` is not on PATH.
    pub fn find() -> Option<Self> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join("notify-send"))
            .find(|p| p.is_file())
            .map(|program| Self { program })
    }
}

impl Notifier for NotifySend {
    fn send(&mut self, title: &str, body: &str, critical: bool) -> Result<(), String> {
        let mut child = std::process::Command::new(&self.program)
            .args([
                "-a",
                "tasks",
                "-u",
                if critical { "critical" } else { "normal" },
            ])
            .arg(title)
            .arg(body)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(status) if status.success() => return Ok(()),
                Some(status) => return Err(format!("notify-send exited {status}")),
                None if Instant::now() >= end => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("notify-send timed out".into());
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

/// Appends one JSON line per notification (hermetic tests).
pub struct FileNotifier(pub PathBuf);
impl Notifier for FileNotifier {
    fn send(&mut self, title: &str, body: &str, critical: bool) -> Result<(), String> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)
            .map_err(|e| e.to_string())?;
        let line = serde_json::json!({"title": title, "body": body, "critical": critical});
        writeln!(f, "{line}").map_err(|e| e.to_string())
    }
}

/// Per alert id at most one notification per `per_alert_s`; overall at most `burst` per
/// `window_s` (the rest are counted as suppressed and summarized in the next one sent).
#[derive(Clone, Debug)]
pub struct RateLimiter {
    pub per_alert_s: f64,
    pub burst: usize,
    pub window_s: f64,
    sent: HashMap<String, f64>,
    recent: VecDeque<f64>,
    pub suppressed: u64,
}

impl RateLimiter {
    pub fn new(per_alert_s: f64, burst: usize, window_s: f64) -> Self {
        Self {
            per_alert_s,
            burst,
            window_s,
            sent: HashMap::new(),
            recent: VecDeque::new(),
            suppressed: 0,
        }
    }

    /// May a notification for `id` go out at `now`? Records it when allowed.
    pub fn allow(&mut self, id: &str, now: f64) -> bool {
        while self
            .recent
            .front()
            .is_some_and(|t| now - t >= self.window_s)
        {
            self.recent.pop_front();
        }
        if self
            .sent
            .get(id)
            .is_some_and(|t| now - t < self.per_alert_s)
        {
            return false;
        }
        if self.recent.len() >= self.burst {
            self.suppressed += 1;
            return false;
        }
        self.sent.insert(id.to_string(), now);
        self.recent.push_back(now);
        if self.sent.len() > 1024 {
            let per = self.per_alert_s;
            self.sent.retain(|_, t| now - *t < per);
        }
        true
    }
}

/// Which alerts are worth a desktop notification: warn/critical marked `needs_human`.
pub fn wants(alert: &Alert) -> bool {
    alert.needs_human && alert.severity >= Severity::Warn
}

/// Sends what the limiter allows. Returns how many were sent.
pub fn deliver(
    notifier: &mut dyn Notifier,
    limiter: &mut RateLimiter,
    alerts: &[Alert],
    now: f64,
) -> usize {
    let mut sent = 0;
    for alert in alerts.iter().filter(|a| wants(a)) {
        if !limiter.allow(&alert.id, now) {
            continue;
        }
        let title = format!("tasks: {} on {}", alert.severity.as_str(), alert.node);
        let mut body = alert.summary.clone();
        if !alert.fix.is_empty() {
            body.push_str(&format!("\nFix: {}", alert.fix));
        }
        if limiter.suppressed > 0 {
            body.push_str(&format!(
                "\n({} more alerts not shown; see `tasks alerts`)",
                limiter.suppressed
            ));
            limiter.suppressed = 0;
        }
        match notifier.send(&title, &body, alert.severity == Severity::Critical) {
            Ok(()) => sent += 1,
            Err(e) => eprintln!("health: notification failed: {e}"),
        }
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::alert::{AlertBook, Observation};

    #[derive(Default)]
    struct Recorder(Vec<(String, String, bool)>);
    impl Notifier for Recorder {
        fn send(&mut self, t: &str, b: &str, c: bool) -> Result<(), String> {
            self.0.push((t.into(), b.into(), c));
            Ok(())
        }
    }

    #[test]
    fn rate_limit_per_alert_and_burst() {
        let mut l = RateLimiter::new(3600.0, 3, 600.0);
        assert!(l.allow("a", 0.0));
        assert!(!l.allow("a", 10.0), "same alert within the hour");
        assert!(l.allow("b", 11.0));
        assert!(l.allow("c", 12.0));
        assert!(!l.allow("d", 13.0), "burst of 3 per 10 min");
        assert_eq!(l.suppressed, 1);
        assert!(l.allow("d", 600.5), "window passed");
        assert!(l.allow("a", 3600.0), "hour passed");
    }

    #[test]
    fn deliver_sends_only_human_warn_and_critical_once() {
        let mut book = AlertBook::new("laptop");
        let raised = book.observe(
            "disk",
            vec![
                Observation::new("disk_reserve", Severity::Critical, "laptop")
                    .summary("cache disk below reserve")
                    .fix("free space")
                    .human(),
                Observation::new("high_cpu", Severity::Info, "laptop")
                    .summary("5 CPU-hours")
                    .human(),
                Observation::new("job_waiting", Severity::Warn, "laptop").summary("job waits"),
            ],
            0.0,
        );
        let mut r = Recorder::default();
        let mut l = RateLimiter::new(3600.0, 3, 600.0);
        assert_eq!(deliver(&mut r, &mut l, &raised, 0.0), 1);
        assert_eq!(r.0[0].0, "tasks: critical on laptop");
        assert!(r.0[0].1.contains("Fix: free space"));
        assert!(r.0[0].2);
        assert_eq!(
            deliver(&mut r, &mut l, &raised, 5.0),
            0,
            "rate-limited repeat"
        );
    }
}
