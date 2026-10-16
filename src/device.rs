//! adb device discovery for "test on phone". Listing is read-only (`adb devices`,
//! `getprop`): it never installs, launches or wipes. Install/launch runs only inside
//! a device-locked job and keeps app data (`install -r`; no uninstall/downgrade/clear).
use crate::{Result, admission};
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Only the phone ABI is built for phone tests; emulators use x86_64 builds.
pub const PHONE_ABI: &str = "arm64-v8a";

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Device {
    /// Real adb serial, e.g. `ZD22257P6G` or `192.168.1.3:42917`.
    pub serial: String,
    /// adb state: `device` (ready), `unauthorized`, `offline`, ...
    pub state: String,
    pub model: Option<String>,
    /// `phone` or `emulator`.
    pub kind: &'static str,
    pub abis: Vec<String>,
    /// Coordinator lock identity (`p2p-device-LOCK.lock`).
    pub lock: String,
    pub busy: bool,
    pub holder: Option<String>,
}
impl Device {
    pub fn ready(&self) -> bool {
        self.state == "device"
    }
    /// Why this device cannot take an `abi` APK right now; lock contention is not a
    /// refusal, the install job waits for the device lock instead.
    pub fn check_ready(&self, abi: &str) -> Result<()> {
        match self.state.as_str() {
            "device" => {}
            "unauthorized" => {
                return Err(format!(
                    "{} is unauthorized: accept the USB debugging prompt on the phone",
                    self.serial
                )
                .into());
            }
            state => return Err(format!("{} is {state}, not ready", self.serial).into()),
        }
        if !self.abis.iter().any(|a| a == abi) {
            return Err(format!(
                "{} supports [{}], not {abi}; use an emulator build instead",
                self.serial,
                self.abis.join(",")
            )
            .into());
        }
        Ok(())
    }
}

fn sdk() -> Option<PathBuf> {
    std::env::var_os("ANDROID_HOME")
        .or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Android/Sdk")))
}
pub fn adb() -> PathBuf {
    sdk()
        .map(|s| s.join("platform-tools/adb"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| "adb".into())
}

/// Lock identity for a real adb serial. Wireless `host:port` serials map `:` to `_`,
/// matching the lock names already in use (e.g. `p2p-device-192.168.1.3_42917`).
pub fn lock_serial(serial: &str) -> Result<String> {
    let lock = serial.replace(':', "_");
    if lock.is_empty()
        || lock.len() > 128
        || !lock
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err("invalid device serial".into());
    }
    Ok(lock)
}

/// Bounded so a wedged adb server cannot hang `tasks devices` or the window.
fn adb_output(args: &[&str], timeout: Duration) -> Result<String> {
    let mut child = Command::new(adb())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("adb unavailable: {e}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("adb {} timed out", args.join(" ")).into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut text = String::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_string(&mut text)?;
    }
    if !status.success() {
        return Err(format!("adb {} failed ({status})", args.join(" ")).into());
    }
    Ok(text)
}
fn getprop(serial: &str, prop: &str) -> Result<String> {
    Ok(adb_output(
        &["-s", serial, "shell", "getprop", prop],
        Duration::from_secs(5),
    )?
    .trim()
    .to_owned())
}

/// (serial, state, model) from `adb devices -l`, skipping adb's own startup chatter.
pub(crate) fn parse_devices(text: &str) -> Vec<(String, String, Option<String>)> {
    text.lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let serial = fields.next()?;
            let state = match fields.next()? {
                "no" => "no permissions",
                state => state,
            };
            let model = fields
                .find_map(|f| f.strip_prefix("model:"))
                .map(|m| m.replace('_', " "));
            Some((serial.into(), state.into(), model))
        })
        .collect()
}

fn describe(
    serial: String,
    state: String,
    model: Option<String>,
    locks: &[admission::ResourceStatus],
) -> Result<Device> {
    let lock = lock_serial(&serial)?;
    let ready = state == "device";
    let abis = if ready {
        getprop(&serial, "ro.product.cpu.abilist")
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .map(Into::into)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };
    let emulator = serial.starts_with("emulator-")
        || (ready && getprop(&serial, "ro.kernel.qemu").is_ok_and(|v| v == "1"));
    let file = format!("p2p-device-{lock}.lock");
    let held = locks
        .iter()
        .find(|r| r.path.file_name().and_then(|n| n.to_str()) == Some(file.as_str()));
    Ok(Device {
        kind: if emulator { "emulator" } else { "phone" },
        busy: held.is_some_and(|r| r.busy),
        holder: held.and_then(|r| r.holder.as_ref()).map(|n| n.who.clone()),
        serial,
        state,
        model,
        abis,
        lock,
    })
}

/// Connected devices with readiness, ABIs and coordinator lock availability.
pub fn list() -> Result<Vec<Device>> {
    let text = adb_output(&["devices", "-l"], Duration::from_secs(10))?;
    let locks = admission::status().map(|s| s.resources).unwrap_or_default();
    parse_devices(&text)
        .into_iter()
        .map(|(serial, state, model)| describe(serial, state, model, &locks))
        .collect()
}

/// `--device` given: that device. Otherwise exactly one ready phone.
pub fn select_phone(devices: &[Device], serial: Option<&str>) -> Result<Device> {
    let device = match serial {
        Some(s) => devices
            .iter()
            .find(|d| d.serial == s)
            .ok_or_else(|| format!("{s} is not connected; see tasks devices"))?,
        None => {
            let phones: Vec<_> = devices
                .iter()
                .filter(|d| d.kind == "phone" && d.ready())
                .collect();
            match phones.as_slice() {
                [one] => *one,
                [] => return Err("no ready phone connected; see tasks devices".into()),
                _ => return Err("several phones ready; pass --device SERIAL".into()),
            }
        }
    };
    device.check_ready(PHONE_ABI)?;
    Ok(device.clone())
}

fn package_name(apk: &Path) -> Result<String> {
    let tools = sdk().ok_or("Android SDK not found")?.join("build-tools");
    let version = |p: &PathBuf| -> Vec<u64> {
        p.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .split('.')
            .map(|n| n.parse().unwrap_or(0))
            .collect()
    };
    let aapt2 = std::fs::read_dir(&tools)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("aapt2").is_file())
        .max_by_key(version)
        .ok_or("aapt2 not found in SDK build-tools")?
        .join("aapt2");
    let output = Command::new(aapt2)
        .args(["dump", "packagename"])
        .arg(apk)
        .stdin(Stdio::null())
        .output()?;
    let package = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    // The name reaches the device shell; accept only a plain Java package name.
    if !output.status.success()
        || package.is_empty()
        || !package
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._".contains(&b))
    {
        return Err("could not read the APK package name".into());
    }
    Ok(package)
}

fn adb_step(args: &[&str]) -> Result<()> {
    eprintln!("tasks: adb {}", args.join(" "));
    let status = Command::new(adb())
        .args(args)
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(format!("adb {} failed ({status})", args.join(" ")).into());
    }
    Ok(())
}

/// Runs inside a device-locked job. Rechecks readiness, then installs keeping app
/// data and opens the launcher activity. Never uninstalls, downgrades or clears data.
pub fn install_and_launch(serial: &str, apk: &Path) -> Result<()> {
    let text = adb_output(&["devices", "-l"], Duration::from_secs(10))?;
    let (serial, state, model) = parse_devices(&text)
        .into_iter()
        .find(|(s, _, _)| s == serial)
        .ok_or_else(|| format!("{serial} disconnected before install"))?;
    describe(serial.clone(), state, model, &[])?.check_ready(PHONE_ABI)?;
    let package = package_name(apk)?;
    let apk = apk.to_str().ok_or("UTF-8 APK path required")?;
    adb_step(&["-s", &serial, "install", "-r", apk])?;
    adb_step(&[
        "-s",
        &serial,
        "shell",
        "monkey",
        "-p",
        &package,
        "-c",
        "android.intent.category.LAUNCHER",
        "1",
    ])?;
    eprintln!("tasks: {package} installed and launched on {serial}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(serial: &str, state: &str, kind: &'static str, abis: &[&str]) -> Device {
        Device {
            serial: serial.into(),
            state: state.into(),
            model: None,
            kind,
            abis: abis.iter().map(|a| a.to_string()).collect(),
            lock: lock_serial(serial).unwrap(),
            busy: false,
            holder: None,
        }
    }

    #[test]
    fn parses_adb_listing_including_wireless_and_unauthorized() {
        let text = "* daemon not running; starting now at tcp:5037\n\
                    * daemon started successfully\n\
                    List of devices attached\n\
                    ZD22257P6G             device usb:1-2 product:foo model:moto_g_power device:bar transport_id:3\n\
                    192.168.1.3:42917      device product:p model:Pixel_7 device:d transport_id:4\n\
                    9BWAY1LSDC             unauthorized usb:1-3 transport_id:5\n\
                    emulator-5554          offline transport_id:6\n\
                    XYZ                    no permissions (user in plugdev group); see [http://developer.android.com/tools/device.html]\n\n";
        let devices = parse_devices(text);
        assert_eq!(devices.len(), 5);
        assert_eq!(
            devices[0],
            (
                "ZD22257P6G".into(),
                "device".into(),
                Some("moto g power".into())
            )
        );
        assert_eq!(devices[1].0, "192.168.1.3:42917");
        assert_eq!(devices[2].1, "unauthorized");
        assert_eq!(devices[3].1, "offline");
        assert_eq!(devices[4].1, "no permissions");
        assert!(parse_devices("List of devices attached\n\n").is_empty());
    }

    #[test]
    fn wireless_serials_share_the_existing_lock_names() {
        assert_eq!(
            lock_serial("192.168.1.3:42917").unwrap(),
            "192.168.1.3_42917"
        );
        assert_eq!(lock_serial("ZD22257P6G").unwrap(), "ZD22257P6G");
        for bad in ["", "a b", "x;rm", "../d"] {
            assert!(lock_serial(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn phone_selection_needs_one_ready_arm64_phone() {
        let phone = device(
            "ZD22257P6G",
            "device",
            "phone",
            &["arm64-v8a", "armeabi-v7a"],
        );
        let emulator = device("emulator-5554", "device", "emulator", &["x86_64"]);
        let locked = device("9BWAY1LSDC", "unauthorized", "phone", &[]);
        let all = [phone.clone(), emulator.clone(), locked.clone()];
        assert_eq!(select_phone(&all, None).unwrap(), phone);
        assert_eq!(select_phone(&all, Some("ZD22257P6G")).unwrap(), phone);
        assert!(select_phone(&all, Some("9BWAY1LSDC")).is_err());
        assert!(select_phone(&all, Some("emulator-5554")).is_err());
        assert!(select_phone(&all, Some("missing")).is_err());
        assert!(select_phone(std::slice::from_ref(&emulator), None).is_err());
        let second = device("192.168.1.3:42917", "device", "phone", &["arm64-v8a"]);
        assert!(select_phone(&[phone.clone(), second.clone()], None).is_err());
        assert_eq!(
            select_phone(&[phone, second.clone()], Some("192.168.1.3:42917")).unwrap(),
            second
        );
        // A busy lock is not a refusal: the install job queues for it.
        let mut busy = device("ZD22257P6G", "device", "phone", &["arm64-v8a"]);
        busy.busy = true;
        assert!(select_phone(&[busy], None).is_ok());
    }
}
