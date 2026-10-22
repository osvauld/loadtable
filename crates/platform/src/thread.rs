//! Native accounting/priority for the calling thread, not the whole daemon.

pub fn cpu_seconds() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: valid output buffer, read only on success.
        if unsafe { libc::getrusage(libc::RUSAGE_THREAD, usage.as_mut_ptr()) } != 0 {
            return None;
        }
        let usage = unsafe { usage.assume_init() };
        let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        Some(secs(usage.ru_utime) + secs(usage.ru_stime))
    }
    #[cfg(target_os = "macos")]
    {
        let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: CLOCK_THREAD_CPUTIME_ID measures just the caller; buffer read on success.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) } != 0 {
            return None;
        }
        let time = unsafe { time.assume_init() };
        Some(time.tv_sec as f64 + time.tv_nsec as f64 / 1e9)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    None
}

/// Best effort, affects only the calling scan thread. A priority failure never relaxes
/// scan bounds. Linux retains its existing nice/idle-I/O calls; macOS uses thread BG mode.
pub fn lower_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 19);
        libc::syscall(libc::SYS_ioprio_set, 1, tid, 3 << 13);
    }
    #[cfg(target_os = "macos")]
    unsafe {
        libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_thread_cpu_clock_is_monotonic() {
        let before = cpu_seconds().unwrap();
        for i in 0..10_000_u64 {
            std::hint::black_box(i.wrapping_mul(i));
        }
        let after = cpu_seconds().unwrap();
        assert!(before.is_finite() && before >= 0.0);
        assert!(after.is_finite() && after >= before);
    }
}
