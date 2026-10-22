//! Size limits for one sync, on both sides.

/// Limits applied by the origin (snapshot) and the worker (receipt).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncLimits {
    /// Largest file that is synced; a larger one is refused by name.
    pub max_file_bytes: u64,
    /// Most files in one snapshot.
    pub max_files: usize,
    /// Most bytes of pack streamed for one sync.
    pub max_pack_bytes: u64,
    /// Most allowlisted extra files.
    pub max_extra_files: usize,
}

impl Default for SyncLimits {
    fn default() -> Self {
        SyncLimits {
            max_file_bytes: 128 * 1024 * 1024,
            max_files: 200_000,
            max_pack_bytes: 2 * 1024 * 1024 * 1024,
            max_extra_files: 256,
        }
    }
}
