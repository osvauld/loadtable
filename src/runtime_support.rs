//! Compilation boundaries are distinct from runtime support.
//!
//! macOS currently has tested host primitives but not admission, descriptor-lease
//! inspection, or parent-death supervision. Reject local runtime entry points before
//! opening shared state, spawning a daemon/worker, or approving/submitting any job.
use std::io;

pub const MACOS_RUNTIME_UNAVAILABLE: &str = "macOS local runtime is not enabled yet: native admission, lock/lease inspection and \
     supervisor recovery are still being integrated; no job was submitted or started";

pub fn require_local_runtime() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    return Ok(());
    #[cfg(not(target_os = "linux"))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        MACOS_RUNTIME_UNAVAILABLE,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compilation_does_not_implicitly_enable_local_runtime() {
        let result = require_local_runtime();
        #[cfg(target_os = "linux")]
        assert!(result.is_ok());
        #[cfg(not(target_os = "linux"))]
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
    }
}
