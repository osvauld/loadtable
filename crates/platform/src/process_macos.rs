use super::{Identity, Observation};
use std::{
    io,
    mem::{MaybeUninit, size_of},
};

pub(super) fn observe(pid: u32) -> io::Result<Observation> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    // SAFETY: the output buffer has exactly the documented PROC_PIDTBSDINFO size.
    // It is read only after proc_pidinfo confirms that the entire struct was written.
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    if bytes <= 0 {
        return Err(io::Error::last_os_error());
    }
    if bytes as usize != size_of::<libc::proc_bsdinfo>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short proc_pidinfo result",
        ));
    }
    // SAFETY: a full struct was returned above.
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid || info.pbi_start_tvusec >= 1_000_000 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid proc_pidinfo identity",
        ));
    }
    let start_ticks = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)
        .and_then(|seconds| seconds.checked_add(info.pbi_start_tvusec))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "process start time overflow"))?;
    Ok(Observation {
        identity: Identity { pid, start_ticks },
        zombie: info.pbi_status == libc::SZOMB,
    })
}
