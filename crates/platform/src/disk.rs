use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[derive(Clone, Debug, serde::Serialize)]
pub struct DiskSpace {
    /// Space available to the calling user, NOT f_bfree (which includes root's
    /// reserve), and not APFS's hypothetical capacity after purging snapshots.
    pub available_bytes: u64,
}

/// Inspect the nearest existing ancestor without creating anything. This uses
/// statvfs on Linux and macOS. Reserve policy remains the caller's responsibility.
pub fn disk_space(path: &Path) -> io::Result<DiskSpace> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache/disk paths must be absolute",
        ));
    }
    let mut existing = path;
    while !existing.try_exists()? {
        existing = existing
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no existing disk ancestor"))?;
    }
    let name = CString::new(existing.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in disk path"))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(name.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.f_frsize == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "statvfs returned zero fragment size",
        ));
    }
    // Preserve the scheduler's saturating byte conversion on large filesystems.
    let available_bytes =
        ((stat.f_bavail as u128) * (stat.f_frsize as u128)).min(u64::MAX as u128) as u64;
    Ok(DiskSpace { available_bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_descendants_use_existing_filesystem_without_creation() {
        let root = std::env::temp_dir();
        let absent = root
            .join(format!("tasks-platform-absent-{}", std::process::id()))
            .join("child");
        assert!(!absent.exists());
        let measured = disk_space(&absent).unwrap();
        assert!(measured.available_bytes > 0);
        assert!(!absent.exists());
        // Avoid equality assertions: unrelated jobs may write between samples.
        assert!(disk_space(&root).unwrap().available_bytes > 0);
    }

    #[test]
    fn relative_and_nul_paths_are_refused() {
        assert_eq!(
            disk_space(Path::new("relative")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(disk_space(Path::new("/bad\0path")).is_err());
    }
}
