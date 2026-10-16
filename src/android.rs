//! Android build-only preparation. Plans are NOT approval or execution; the worker
//! keeps normal catalog approval gated; explicit experimental trials are separate.
use crate::{Result, snapshot::Snapshot, workspace_snapshot};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Abis {
    X86,
    Arm64,
    Both,
}
impl Abis {
    /// `emulator`/`phone` are input aliases only; stored configurations keep ABI names.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "x86_64" | "emulator" => Ok(Self::X86),
            "arm64-v8a" | "phone" => Ok(Self::Arm64),
            "x86_64,arm64-v8a" => Ok(Self::Both),
            _ => Err("ABI must be emulator (x86_64), phone (arm64-v8a) or x86_64,arm64-v8a".into()),
        }
    }
    /// Canonical job configuration; history and admission key on this exact string.
    pub fn configuration(self) -> String {
        format!("debug/{}/rust-debug/offline", self.names())
    }
    pub(crate) fn names(self) -> &'static str {
        match self {
            Self::X86 => "x86_64",
            Self::Arm64 => "arm64-v8a",
            Self::Both => "x86_64,arm64-v8a",
        }
    }
    pub(crate) fn triples(self) -> &'static [&'static str] {
        match self {
            Self::X86 => &["x86_64-linux-android"],
            Self::Arm64 => &["aarch64-linux-android"],
            Self::Both => &["x86_64-linux-android", "aarch64-linux-android"],
        }
    }
}
#[derive(Debug, Serialize)]
pub struct Step {
    pub name: String,
    pub directory: &'static str,
    pub argv: Vec<String>,
    pub admission: &'static str,
}
#[derive(Debug, Serialize)]
pub struct Identity {
    pub path: PathBuf,
    pub present: bool,
    pub sha256: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct Plan {
    pub schema: u32,
    pub executable: bool,
    pub configuration: String,
    pub snapshot: Snapshot,
    pub steps: Vec<Step>,
    pub implicit_gradle_steps: [&'static str; 2],
    pub expected_apk: &'static str,
    pub artifact_contract: &'static str,
    pub toolchain_markers: Vec<Identity>,
    pub libaom_version: String,
    pub libaom_tarball: Identity,
    pub libaom_tarball_verified: bool,
    pub gradle_user_home: PathBuf,
    pub output_contract: &'static str,
    pub blockers: Vec<String>,
    pub installs: bool,
    pub publishes: bool,
}
fn bytes(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > limit {
        return Err("preflight input is not a bounded regular file".into());
    }
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return Err("preflight input grew past its size bound".into());
    }
    Ok(data)
}
fn identity(path: PathBuf, limit: u64) -> Result<Identity> {
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Identity {
            path,
            present: false,
            sha256: None,
        }),
        Err(e) => Err(e.into()),
        Ok(_) => Ok(Identity {
            sha256: Some(format!("{:x}", Sha256::digest(bytes(&path, limit)?))),
            path,
            present: true,
        }),
    }
}
fn pin(script: &str, key: &str) -> Result<String> {
    let prefix = format!("{key} = \"");
    let value = script
        .lines()
        .find_map(|line| {
            line.strip_prefix(&prefix)
                .and_then(|value| value.strip_suffix('"'))
        })
        .ok_or("libaom source pin missing or unsupported")?;
    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_hexdigit() || c == b'.') {
        return Err("invalid libaom source pin".into());
    }
    Ok(value.into())
}
/// Audits source and local installation markers, without invoking Gradle, Cargo,
/// source scripts, downloads, daemons, adb or any device operation.
pub fn preflight(source: &Path, revision: &str, abis: Abis) -> Result<Plan> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME required")?);
    let sdk = std::env::var_os("ANDROID_HOME")
        .or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("Android/Sdk"));
    let java = std::env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/lib/jvm/java-21-openjdk"));
    preflight_at(source, revision, abis, &home, &sdk, &java)
}
fn preflight_at(
    source: &Path,
    revision: &str,
    abis: Abis,
    home: &Path,
    sdk: &Path,
    java: &Path,
) -> Result<Plan> {
    let snapshot = workspace_snapshot::inspect(source, revision)?;
    let script = String::from_utf8(bytes(&source.join("scripts/build_libaom.py"), 1024 * 1024)?)?;
    let version = pin(&script, "VERSION")?;
    let expected = pin(&script, "SHA256")?;
    if expected.len() != 64 || !expected.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("libaom SHA256 must be 64 hexadecimal characters".into());
    }
    let tarball = identity(
        home.join(format!(
            "build-cache/p2p_chat/libaom/libaom-{version}.tar.gz"
        )),
        32 * 1024 * 1024,
    )?;
    let verified = tarball.sha256.as_deref() == Some(expected.as_str());
    let mut steps: Vec<Step> = abis.triples().iter().map(|triple|Step {
        name:format!("libaom:{triple}"), directory:".",
        argv:vec!["python3".into(),"scripts/build_libaom.py".into(),"--target".into(),(*triple).into()],
        admission:"compile; must acquire native-output lease before executing; no outer nested buildlock",
    }).collect();
    steps.push(Step {
        name: "assemble-debug".into(),
        directory: "android",
        argv: vec![
            "./gradlew".into(),
            "--no-daemon".into(),
            "--offline".into(),
            "--console=plain".into(),
            ":app:assembleDebug".into(),
            format!("-Pabis={}", abis.names()),
            "-PrustRelease=false".into(),
            "-PdevTools=true".into(),
            "-PvideoHdCapture=false".into(),
        ],
        admission: "compile; Gradle itself runs cargo-ndk and UniFFI, do not nest build_android.py",
    });
    // Installation markers are partial identities, NOT a hash of entire toolchains.
    let markers = [
        sdk.join("ndk/28.2.13676358/source.properties"),
        sdk.join("platforms/android-37.2/package.xml"),
        java.join("release"),
        source.join("android/gradle/wrapper/gradle-wrapper.properties"),
    ];
    let mut toolchain_markers = Vec::new();
    let mut blockers = vec![
        "normal Android catalog approval remains gated pending real x86_64 and dual-ABI APK validation; explicit approve-android-trial is per-job experimental permission only".into(),
        "managed writable lanes and atomic APK results exist, but production native/Gradle integration is not validated; trial requires disk reserve and exclusive lane admission".into(),
        "trial preserves machine cache policy and maps repo/target for UniFFI; different source checkouts do not share Cargo targets".into(),
        "review trusted host/config policy: trial fingerprints partial installed Rust/JDK/SDK/NDK/Gradle identities, not entire hermetic toolchains".into(),
        "trial needs installed numeric Rust toolchain/Android stdlibs, installed Gradle 9.8.1 and verified native source; offline flags are not a network sandbox for arbitrary trusted build code".into(),
    ];
    for path in markers {
        let marker = identity(path, 1024 * 1024)?;
        if !marker.present {
            blockers.push(format!(
                "toolchain marker missing: {}",
                marker.path.display()
            ));
        }
        toolchain_markers.push(marker);
    }
    if !verified {
        blockers.push(
            "pinned libaom tarball missing or mismatched; no download/repair performed".into(),
        );
    }
    if workspace_snapshot::inspect(source, revision)?.digest != snapshot.digest {
        return Err("workspace changed during Android preflight".into());
    }
    Ok(Plan {
        schema: 1,
        executable: false,
        configuration: abis.configuration(),
        snapshot,
        steps,
        implicit_gradle_steps: [
            "cargoNdkBuild: p2pcore --lib (dev-tools enabled for debug)",
            "uniffiBindgen: host generator reads built target library; Kotlin compile depends on it",
        ],
        expected_apk: "android/app/build/outputs/apk/debug/app-debug.apk",
        artifact_contract: "copy freshly produced APK into owned durable job results; record SHA-256, byte length, source digest, ABI/profile and toolchain identities; build success is not tests/install success",
        toolchain_markers,
        libaom_version: version,
        libaom_tarball: tarball,
        libaom_tarball_verified: verified,
        gradle_user_home: home.join("build-cache/gradle"),
        output_contract: "per-lane target/, android/build and android/app/build; separate Gradle project cache; shared machine compiler/dependency caches are not exclusively leased or retired",
        blockers,
        installs: false,
        publishes: false,
    })
}

#[cfg(test)]
#[path = "android_tests.rs"]
mod tests;
