//! macOS power through IOKit: the providing power source, the battery charge, and the
//! `PreventUserIdleSystemSleep` assertion (what `caffeinate -i` takes). Changes no setting: the
//! assertion ends when released or when the process exits.
#![allow(unsafe_code)]

use crate::{InhibitGuard, PlatformError, PowerReading};
use std::ffi::{CStr, CString, c_char, c_void};

type CfRef = *const c_void;

const UTF8: u32 = 0x0800_0100;
const NUMBER_SINT32: isize = 3;
const ASSERTION_LEVEL_ON: u32 = 255;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(alloc: CfRef, c: *const c_char, encoding: u32) -> CfRef;
    fn CFStringGetCString(s: CfRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFRelease(cf: CfRef);
    fn CFArrayGetCount(a: CfRef) -> isize;
    fn CFArrayGetValueAtIndex(a: CfRef, i: isize) -> CfRef;
    fn CFDictionaryGetValue(d: CfRef, key: CfRef) -> CfRef;
    fn CFNumberGetValue(n: CfRef, ty: isize, out: *mut c_void) -> u8;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CfRef,
        level: u32,
        name: CfRef,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
    fn IOPSCopyPowerSourcesInfo() -> CfRef;
    fn IOPSGetProvidingPowerSourceType(info: CfRef) -> CfRef;
    fn IOPSCopyPowerSourcesList(info: CfRef) -> CfRef;
    fn IOPSGetPowerSourceDescription(info: CfRef, source: CfRef) -> CfRef;
}

/// An owned CoreFoundation object, released on drop.
struct Cf(CfRef);

impl Cf {
    /// Wraps a +1 reference; `None` for null.
    fn owned(r: CfRef) -> Option<Cf> {
        (!r.is_null()).then_some(Cf(r))
    }

    fn string(s: &str) -> Option<Cf> {
        let c = CString::new(s).ok()?;
        // SAFETY: `c` is a valid NUL-terminated string; a null allocator means the default.
        Cf::owned(unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) })
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a Create/Copy call and is released exactly once.
        unsafe { CFRelease(self.0) }
    }
}

/// A CFString as text.
fn text_of(s: CfRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let mut buf = [0 as c_char; 256];
    // SAFETY: `s` is a CFString; `buf` is writable for the given size.
    let ok = unsafe { CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, UTF8) };
    // SAFETY: on success the buffer holds a NUL-terminated string.
    (ok != 0).then(|| {
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    })
}

fn dict_int(dict: CfRef, key: &str) -> Option<i32> {
    let k = Cf::string(key)?;
    // SAFETY: `dict` is a CFDictionary and `k` a CFString; the value is borrowed.
    let v = unsafe { CFDictionaryGetValue(dict, k.0) };
    if v.is_null() {
        return None;
    }
    let mut out = 0i32;
    // SAFETY: `v` is a CFNumber in the power-source dictionary; SInt32 is the type asked for.
    let ok = unsafe { CFNumberGetValue(v, NUMBER_SINT32, (&raw mut out).cast()) };
    (ok != 0).then_some(out)
}

fn dict_text(dict: CfRef, key: &str) -> Option<String> {
    let k = Cf::string(key)?;
    // SAFETY: as in `dict_int`.
    text_of(unsafe { CFDictionaryGetValue(dict, k.0) })
}

pub(super) fn read() -> Result<PowerReading, PlatformError> {
    let err = |d: &str| PlatformError::parse("IOPowerSources", d);
    // SAFETY: no arguments; returns a +1 blob or null.
    let info = Cf::owned(unsafe { IOPSCopyPowerSourcesInfo() }).ok_or_else(|| err("no info"))?;
    // SAFETY: `info` is a power-sources blob; the result is borrowed from it.
    let providing = text_of(unsafe { IOPSGetProvidingPowerSourceType(info.0) });
    // a desktop reports "AC Power" and has no internal battery in the list
    let on_battery = providing.as_deref() == Some("Battery Power");
    let mut percent = None;
    // SAFETY: `info` is a power-sources blob; returns a +1 array or null.
    if let Some(list) = Cf::owned(unsafe { IOPSCopyPowerSourcesList(info.0) }) {
        // SAFETY: `list` is a CFArray.
        let n = unsafe { CFArrayGetCount(list.0) };
        for i in 0..n {
            // SAFETY: `i` is in range; the element is borrowed from the array.
            let src = unsafe { CFArrayGetValueAtIndex(list.0, i) };
            // SAFETY: `info` and `src` belong together; the description is borrowed.
            let desc = unsafe { IOPSGetPowerSourceDescription(info.0, src) };
            if desc.is_null() || dict_text(desc, "Type").as_deref() != Some("InternalBattery") {
                continue;
            }
            let cur = dict_int(desc, "Current Capacity");
            let max = dict_int(desc, "Max Capacity")
                .filter(|m| *m > 0)
                .unwrap_or(100);
            percent = cur.map(|c| (c.saturating_mul(100) / max).clamp(0, 100) as u8);
            break;
        }
    }
    Ok(PowerReading {
        on_battery,
        percent,
    })
}

/// Takes the assertion; the guard releases it.
pub(super) fn inhibit(why: &str) -> Result<InhibitGuard, PlatformError> {
    let fail = |detail: String| PlatformError::Command {
        command: "IOPMAssertionCreateWithName".into(),
        detail,
    };
    let kind = Cf::string("PreventUserIdleSystemSleep").ok_or_else(|| fail("no string".into()))?;
    let name = Cf::string(why).ok_or_else(|| fail("no string".into()))?;
    let mut id = 0u32;
    // SAFETY: both strings are valid CFStrings and `id` is writable.
    let rc = unsafe { IOPMAssertionCreateWithName(kind.0, ASSERTION_LEVEL_ON, name.0, &mut id) };
    if rc != 0 {
        return Err(fail(format!("IOReturn {rc:#x}")));
    }
    Ok(InhibitGuard::new(move || {
        // SAFETY: `id` came from a successful create and is released once.
        unsafe {
            IOPMAssertionRelease(id);
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn r612_the_power_source_reads() {
        let r = read().unwrap();
        println!("power: {r}");
    }

    #[test]
    fn r612_the_assertion_shows_in_pmset_and_goes_away() {
        let name = "tasks2d test assertion (R-612)";
        let g = inhibit(name).unwrap();
        let held = Command::new("/usr/bin/pmset")
            .args(["-g", "assertions"])
            .output()
            .unwrap();
        let held = String::from_utf8_lossy(&held.stdout).into_owned();
        println!("--- while held ---\n{held}");
        assert!(held.contains(name), "assertion missing:\n{held}");
        drop(g);
        let after = Command::new("/usr/bin/pmset")
            .args(["-g", "assertions"])
            .output()
            .unwrap();
        let after = String::from_utf8_lossy(&after.stdout).into_owned();
        assert!(!after.contains(name), "assertion still held:\n{after}");
    }
}
