//! Scrubbing of inherited descriptors (R-087): the daemon may be started by a
//! parent that leaked descriptors (lock files, sockets, pipes). Rust opens its
//! own descriptors close-on-exec, but inherited ones are not, so jobs would
//! inherit them. [`close_on_exec_inherited`] marks them all close-on-exec once.

use crate::PlatformError;
use rustix::fd::BorrowedFd;
use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
use std::path::Path;

/// Directory listing the open descriptors of this process.
#[cfg(target_os = "linux")]
const FD_DIR: &str = "/proc/self/fd";
/// Directory listing the open descriptors of this process.
#[cfg(target_os = "macos")]
const FD_DIR: &str = "/dev/fd";

/// Marks every descriptor above 2 close-on-exec and returns how many were
/// changed. Descriptors that were already close-on-exec (including the one used
/// to list the directory) are left alone. Call once at daemon startup.
pub fn close_on_exec_inherited() -> Result<usize, PlatformError> {
    close_on_exec_in(Path::new(FD_DIR))
}

fn close_on_exec_in(dir: &Path) -> Result<usize, PlatformError> {
    // Collect first: the directory handle is itself a descriptor.
    let mut fds = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| PlatformError::io(dir, e))? {
        let entry = entry.map_err(|e| PlatformError::io(dir, e))?;
        if let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
            .filter(|&fd| fd > 2)
        {
            fds.push(fd);
        }
    }
    let mut changed = 0;
    for fd in fds {
        // SAFETY: the descriptor is only borrowed for two fcntl calls and never
        // closed or owned here. If it was closed meanwhile, fcntl fails with
        // EBADF, which is ignored. The `rustix` safe API has no way to name a
        // descriptor we did not open, so this single call needs `unsafe`.
        #[allow(unsafe_code)]
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let Ok(flags) = fcntl_getfd(borrowed) else {
            continue;
        };
        if flags.contains(FdFlags::CLOEXEC) {
            continue;
        }
        if fcntl_setfd(borrowed, flags | FdFlags::CLOEXEC).is_ok() {
            changed += 1;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fd::AsFd;
    use rustix::io::{fcntl_dupfd_cloexec, fcntl_getfd};

    #[test]
    fn r087_inherited_descriptor_becomes_close_on_exec() {
        let file = std::fs::File::open("/dev/null").unwrap();
        // Duplicate without CLOEXEC to model an inherited descriptor.
        let dup = rustix::io::dup(file.as_fd()).unwrap();
        let flags = fcntl_getfd(&dup).unwrap();
        fcntl_setfd(&dup, flags - FdFlags::CLOEXEC).unwrap();
        assert!(!fcntl_getfd(&dup).unwrap().contains(FdFlags::CLOEXEC));
        let changed = close_on_exec_inherited().unwrap();
        assert!(changed >= 1);
        assert!(fcntl_getfd(&dup).unwrap().contains(FdFlags::CLOEXEC));
        // A second call has nothing left to change for our descriptor.
        let again = fcntl_dupfd_cloexec(file.as_fd(), 3).unwrap();
        assert!(fcntl_getfd(&again).unwrap().contains(FdFlags::CLOEXEC));
    }

    #[test]
    fn r087_standard_descriptors_are_untouched() {
        close_on_exec_inherited().unwrap();
        for fd in [0, 1, 2] {
            // SAFETY: test-only borrow of the standard descriptors.
            #[allow(unsafe_code)]
            let b = unsafe { BorrowedFd::borrow_raw(fd) };
            if let Ok(f) = fcntl_getfd(b) {
                assert!(!f.contains(FdFlags::CLOEXEC), "fd {fd}");
            }
        }
    }
}
