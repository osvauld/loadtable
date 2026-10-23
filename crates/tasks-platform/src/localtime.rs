//! The machine's local time zone offset, for times shown to people (std has no time zones).
#![allow(unsafe_code)]

/// Seconds east of UTC that the system time zone has at `unix_secs`; 0 when it cannot be read.
pub fn local_offset_secs(unix_secs: i64) -> i32 {
    let t = unix_secs as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: `t` is a valid time_t and `tm` a writable tm; localtime_r fills it and returns
    // null on failure.
    let filled = unsafe { libc::localtime_r(&t, tm.as_mut_ptr()) };
    if filled.is_null() {
        return 0;
    }
    // SAFETY: localtime_r succeeded, so `tm` is initialised.
    let tm = unsafe { tm.assume_init() };
    i32::try_from(tm.tm_gmtoff).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r616_the_offset_is_a_sane_zone_offset() {
        let off = local_offset_secs(1_700_000_000);
        assert!((-14 * 3600..=14 * 3600).contains(&off), "{off}");
    }
}
