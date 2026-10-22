//! Shared Unix filesystem metadata identities. These are not lock-holder observations.
use std::os::unix::fs::MetadataExt;

/// (major, minor, inode), interpreted using the current OS's dev_t representation.
/// A key is local to this filesystem namespace; never compare keys from different hosts.
pub fn file_key(metadata: &std::fs::Metadata) -> (u32, u32, u64) {
    #[cfg(target_os = "linux")]
    let (major, minor) = (libc::major(metadata.dev()), libc::minor(metadata.dev()));
    #[cfg(target_os = "macos")]
    let (major, minor) = {
        let device = metadata.dev() as libc::dev_t;
        (libc::major(device) as u32, libc::minor(device) as u32)
    };
    (major, minor, metadata.ino())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_metadata_key_is_stable() {
        let exe = std::env::current_exe().unwrap();
        let first = file_key(&std::fs::metadata(&exe).unwrap());
        let second = file_key(&std::fs::metadata(&exe).unwrap());
        assert_eq!(first, second);
        assert_ne!(first.2, 0);
    }
}
