use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    LinuxMemAvailable,
    /// Readily reclaimable pages only, NOT an estimate of Linux MemAvailable:
    /// (free - speculative + purgeable) * the host's actual page size.
    MacosFreeAndPurgeable,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Memory {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub source: MemorySource,
}

/// Native admission headroom. macOS deliberately uses a conservative floor:
/// speculative, inactive, compressed and swap-backed pages are not added.
/// Apple Silicon's page size is queried, never assumed to be 4 KiB.
/// PSI is not reported here: it does not exist on macOS.
pub fn memory() -> io::Result<Memory> {
    native_memory()
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(target_os = "linux")]
fn native_memory() -> io::Result<Memory> {
    parse_linux(&std::fs::read_to_string("/proc/meminfo")?)
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux(text: &str) -> io::Result<Memory> {
    let field = |name| -> io::Result<u64> {
        let mut words = text
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .ok_or_else(|| invalid("missing meminfo field"))?
            .split_whitespace();
        let kb: u64 = words
            .next()
            .ok_or_else(|| invalid("missing memory value"))?
            .parse()
            .map_err(|_| invalid("invalid memory value"))?;
        if words.next() != Some("kB") || words.next().is_some() {
            return Err(invalid("invalid meminfo unit"));
        }
        kb.checked_mul(1024)
            .ok_or_else(|| invalid("memory value overflow"))
    };
    let total_bytes = field("MemTotal:")?;
    let available_bytes = field("MemAvailable:")?;
    validate(total_bytes, available_bytes)?;
    Ok(Memory {
        total_bytes,
        available_bytes,
        source: MemorySource::LinuxMemAvailable,
    })
}

fn validate(total: u64, available: u64) -> io::Result<()> {
    if total == 0 || available > total {
        Err(invalid("invalid native memory totals"))
    } else {
        Ok(())
    }
}

#[cfg(any(target_os = "macos", test))]
fn macos_memory(
    total: u64,
    page_size: u64,
    free: u64,
    speculative: u64,
    purgeable: u64,
) -> io::Result<Memory> {
    if page_size == 0 || !page_size.is_power_of_two() {
        return Err(invalid("invalid native page size"));
    }
    let available_bytes = free
        .checked_sub(speculative)
        .and_then(|p| p.checked_add(purgeable))
        .and_then(|p| p.checked_mul(page_size))
        .ok_or_else(|| invalid("invalid native memory page counters"))?;
    validate(total, available_bytes)?;
    Ok(Memory {
        total_bytes: total,
        available_bytes,
        source: MemorySource::MacosFreeAndPurgeable,
    })
}

#[cfg(any(target_os = "macos", test))]
fn vm_count_covers_prefix(count: u32, capacity: u32, required: u32) -> bool {
    count >= required && count <= capacity
}

// libc exposes host_statistics64 but not this libSystem Mach symbol. Keep the
// small ABI declaration here rather than hand-rolling the VM statistics layout.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc's existing Mach bindings; no native SDK needed to type-check.
fn native_memory() -> io::Result<Memory> {
    let mut total: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: hw.memsize returns a uint64_t; the length is checked below.
    if unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut total as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if size != std::mem::size_of::<u64>() {
        return Err(invalid("short hw.memsize result"));
    }
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return Err(invalid("native page size unavailable"));
    }
    // libc may describe a newer VM-statistics revision than the running kernel.
    // Zero the buffer, validate the returned prefix, and read ONLY the fields in it.
    let mut vm = std::mem::MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let required_count = (std::mem::offset_of!(libc::vm_statistics64, speculative_count)
        + std::mem::size_of::<libc::natural_t>())
    .div_ceil(std::mem::size_of::<libc::integer_t>()) as u32;
    // mach_host_self creates a send right. Release it on both success/failure.
    let host = unsafe { libc::mach_host_self() };
    let rc = unsafe {
        libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            vm.as_mut_ptr().cast(),
            &mut count,
        )
    };
    let released = unsafe { mach_port_deallocate(libc::mach_task_self(), host) };
    if released != libc::KERN_SUCCESS {
        return Err(io::Error::other(format!(
            "mach_port_deallocate failed: kern_return={released}"
        )));
    }
    if rc != libc::KERN_SUCCESS
        || !vm_count_covers_prefix(count, libc::HOST_VM_INFO64_COUNT, required_count)
    {
        return Err(io::Error::other(format!(
            "host_statistics64 failed: kern_return={rc}, count={count}"
        )));
    }
    let vm = unsafe { vm.assume_init() };
    macos_memory(
        total,
        page_size as u64,
        u64::from(vm.free_count),
        u64::from(vm.speculative_count),
        u64::from(vm.purgeable_count),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_reply_must_cover_used_fields_but_need_not_match_latest_revision() {
        assert!(vm_count_covers_prefix(40, 100, 26));
        assert!(vm_count_covers_prefix(26, 100, 26));
        assert!(!vm_count_covers_prefix(25, 100, 26));
        assert!(!vm_count_covers_prefix(101, 100, 26));
        assert!(!vm_count_covers_prefix(0, 100, 26));
    }

    #[test]
    fn live_memory_is_bounded_and_has_native_source() {
        let m = memory().unwrap();
        assert!(m.total_bytes > 0);
        assert!(m.available_bytes <= m.total_bytes);
        assert_eq!(
            m.source,
            if cfg!(target_os = "macos") {
                MemorySource::MacosFreeAndPurgeable
            } else {
                MemorySource::LinuxMemAvailable
            }
        );
    }

    #[test]
    fn linux_memavailable_is_not_free_or_swap() {
        let m =
            parse_linux("MemTotal: 100 kB\nMemFree: 1 kB\nMemAvailable: 50 kB\nSwapFree: 100 kB\n")
                .unwrap();
        assert_eq!(m.total_bytes, 100 * 1024);
        assert_eq!(m.available_bytes, 50 * 1024);
        for bad in [
            "",
            "MemTotal: 100 kB\nMemAvailable: 101 kB",
            "MemTotal: 100 MB\nMemAvailable: 1 kB",
            "MemTotal: 18446744073709551615 kB\nMemAvailable: 1 kB",
        ] {
            assert!(parse_linux(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn macos_uses_actual_page_size_and_excludes_speculative() {
        for page in [4096, 16384] {
            let m = macos_memory(1000 * page, page, 100, 20, 30).unwrap();
            assert_eq!(m.available_bytes, 110 * page);
            assert_eq!(m.source, MemorySource::MacosFreeAndPurgeable);
        }
    }

    #[test]
    fn macos_bad_samples_fail_closed() {
        assert!(macos_memory(1000, 0, 0, 0, 0).is_err());
        assert!(macos_memory(1000, 3, 0, 0, 0).is_err());
        assert!(macos_memory(1000, 4096, 0, 1, 0).is_err());
        assert!(macos_memory(u64::MAX, 16384, u64::MAX, 0, 1).is_err());
        assert!(macos_memory(1, 16384, 1, 0, 0).is_err());
    }
}
