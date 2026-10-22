use std::io;

/// Waited-for descendants, from getrusage(RUSAGE_CHILDREN). This is not a live
/// process-tree measurement and max RSS is the largest individual descendant,
/// not the sum of all simultaneously resident children.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChildUsage {
    pub cpu_user_s: f64,
    pub cpu_sys_s: f64,
    pub max_rss_bytes: u64,
}

pub fn children_usage() -> io::Result<ChildUsage> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, usage.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let usage = unsafe { usage.assume_init() };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    Ok(ChildUsage {
        cpu_user_s: secs(usage.ru_utime),
        cpu_sys_s: secs(usage.ru_stime),
        max_rss_bytes: rss_bytes(usage.ru_maxrss.max(0) as u64, cfg!(target_os = "macos")),
    })
}

fn rss_bytes(native: u64, macos: bool) -> u64 {
    // Darwin returns bytes; Linux returns KiB. Do not multiply Darwin's bytes.
    if macos {
        native
    } else {
        native.saturating_mul(1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_units_are_native_not_assumed_linux() {
        assert_eq!(rss_bytes(123, false), 123 * 1024);
        assert_eq!(rss_bytes(123, true), 123);
        assert_eq!(rss_bytes(u64::MAX, false), u64::MAX);
    }

    #[test]
    fn live_children_usage_is_readable() {
        let usage = children_usage().unwrap();
        assert!(usage.cpu_user_s.is_finite() && usage.cpu_user_s >= 0.0);
        assert!(usage.cpu_sys_s.is_finite() && usage.cpu_sys_s >= 0.0);
    }
}
