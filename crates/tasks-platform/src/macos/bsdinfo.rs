//! The only `unsafe` in the crate: one `proc_pidinfo(PROC_PIDTBSDINFO)` call.
//! `libproc`'s build script needs the macOS SDK, so it cannot be type-checked
//! from Linux; `libc` can.
#![allow(unsafe_code)]

use crate::PlatformError;

/// The `proc_bsdinfo` fields `tasks` uses.
#[derive(Debug, Clone, Copy)]
pub(super) struct Bsd {
    pub pgid: u32,
    pub zombie: bool,
    pub start_sec: u64,
    pub start_usec: u64,
}

/// Reads BSD info for `pid`; `None` when the process does not exist.
pub(super) fn bsd_info(pid: i32) -> Result<Option<Bsd>, PlatformError> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let Ok(size_i32) = i32::try_from(size) else {
        return Err(PlatformError::parse("proc_pidinfo", "struct size overflow"));
    };
    // SAFETY: `info` is a writable zeroed buffer of exactly `size` bytes, which is
    // the size PROC_PIDTBSDINFO requires; the kernel writes at most `size` bytes.
    let count = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_i32,
        )
    };
    if count <= 0 {
        let err = std::io::Error::last_os_error();
        return if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(None)
        } else {
            Err(PlatformError::io(format!("proc_pidinfo({pid})"), err))
        };
    }
    if count != size_i32 {
        return Err(PlatformError::parse(
            "proc_pidinfo",
            "short PROC_PIDTBSDINFO result",
        ));
    }
    // SAFETY: the kernel reported that it filled the whole struct (count == size),
    // and the buffer was zero-initialised, so every field is a valid integer.
    let info = unsafe { info.assume_init() };
    if i64::from(info.pbi_pid) != i64::from(pid) {
        return Err(PlatformError::parse("proc_pidinfo", "pid mismatch"));
    }
    Ok(Some(Bsd {
        pgid: info.pbi_pgid,
        zombie: info.pbi_status == libc::SZOMB,
        start_sec: info.pbi_start_tvsec,
        start_usec: info.pbi_start_tvusec,
    }))
}
