//! Managed Android writable lanes. Cooperative leases, not a sandbox.
//! No legacy adoption, cache deletion, wrapper downloads or device operations.
use crate::{Result, android::Abis, cache, execution::BuildArtifact, workspace_snapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
pub const MAX_APK: u64 = 512 * 1024 * 1024;

pub fn lane_path(abis: Abis, source: &Path) -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME required")?);
    let checkout = fs::canonicalize(source)?;
    let identity = format!(
        "{:x}",
        Sha256::digest(checkout.as_os_str().as_encoded_bytes())
    );
    Ok(home
        .join("build-cache/p2p_chat/tasks/android-lanes")
        .join(abis.names())
        .join(&identity[..32]))
}
fn real_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return Err("Android lane directory must be real and owned by this user".into());
    }
    Ok(())
}
/// Open/create the root only. Acquire its lease and begin_android_use BEFORE
/// populating outputs; a nonempty unmarked root must be refused, never adopted.
pub fn verify_lease(root: &Path, held: &File) -> Result<()> {
    let expected = fs::symlink_metadata(root.join(".tasks-target.lock"))?;
    let actual = held.metadata()?;
    if !expected.is_file()
        || expected.nlink() != 1
        || expected.uid() != unsafe { libc::geteuid() }
        || (expected.dev(), expected.ino()) != (actual.dev(), actual.ino())
    {
        return Err("Android lane/inherited lease identity mismatch".into());
    }
    Ok(())
}
pub fn root(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err("Android lane must be absolute".into());
    }
    fs::create_dir_all(path)?;
    real_directory(path)?;
    Ok(fs::canonicalize(path)?)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Layout {
    pub root: PathBuf,
    pub tree: PathBuf,
    pub native_key: String,
}
impl Layout {
    /// Caller retains the root lease through all commands, result publication and cleanup.
    pub fn prepare(root: &Path, capsule: &Path, digest: &str, native_key: &str) -> Result<Self> {
        if native_key.len() != 64 || !native_key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid native cache identity".into());
        }
        real_directory(root)?;
        for name in [
            "trees",
            "target",
            "android-build",
            "app-build",
            "native",
            "project-cache",
        ] {
            let path = root.join(name);
            if !path.try_exists()? {
                fs::create_dir(&path)?;
            }
            real_directory(&path)?;
        }
        let tree = root.join("trees").join(format!(
            "{}-{}-{}",
            std::process::id(),
            crate::execution::now(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        workspace_snapshot::materialize(capsule, digest, &tree)?;
        let result = Self {
            root: root.to_owned(),
            tree,
            native_key: native_key.into(),
        };
        fs::create_dir(result.app_dir())?;
        if !result.native().try_exists()? {
            fs::create_dir(result.native())?;
        }
        for (relative, output) in result.mappings() {
            symlink(output, result.tree.join(relative))?;
        }
        result.verify(digest)?;
        Ok(result)
    }
    fn mappings(&self) -> [(&'static str, PathBuf); 3] {
        [
            ("target", self.root.join("target")),
            ("android/build", self.root.join("android-build")),
            ("android/app/build", self.app_dir()),
        ]
    }
    pub fn verify(&self, digest: &str) -> Result<()> {
        real_directory(&self.root)?;
        real_directory(&self.root.join("trees"))?;
        real_directory(&self.tree)?;
        if !self.tree.is_absolute() || self.tree.parent() != Some(self.root.join("trees").as_path())
        {
            return Err("working tree outside Android lane".into());
        }
        for (relative, output) in self.mappings() {
            real_directory(&output)?;
            if fs::read_link(self.tree.join(relative))? != output {
                return Err("Android output mapping changed".into());
            }
        }
        if self.native_key.len() != 64 || !self.native_key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid native cache identity".into());
        }
        for name in ["native", "project-cache"] {
            real_directory(&self.root.join(name))?;
        }
        real_directory(&self.native())?;
        workspace_snapshot::verify_inputs(&self.tree, digest)
    }
    pub fn native(&self) -> PathBuf {
        self.root.join("native").join(&self.native_key)
    }
    pub fn app_dir(&self) -> PathBuf {
        self.root
            .join("app-build")
            .join(self.tree.file_name().unwrap_or_default())
    }
    pub fn apk(&self) -> PathBuf {
        self.app_dir().join("outputs/apk/debug/app-debug.apk")
    }
}

pub fn file_hash(path: &Path, limit: u64) -> Result<(String, u64)> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > limit {
        return Err("unbounded or non-regular Android input/artifact".into());
    }
    let mut hash = Sha256::new();
    let mut total = 0;
    let mut bytes = [0; 65536];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > limit {
            return Err("Android input/artifact grew beyond bound".into());
        }
        hash.update(&bytes[..count]);
    }
    Ok((format!("{:x}", hash.finalize()), total))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Host {
    pub sdk: PathBuf,
    pub java: PathBuf,
    pub gradle: PathBuf,
    pub rust: PathBuf,
    pub toolchain: String,
    pub absent_config: Vec<PathBuf>,
    pub init_dir: PathBuf,
    pub identities: BTreeMap<PathBuf, String>,
}
impl Host {
    /// Uses an already installed distribution, NOT gradlew: --offline alone
    /// would still let the wrapper download its distribution before Gradle starts.
    pub fn discover(source: &Path, abis: Abis) -> Result<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME required")?);
        let sdk = std::env::var_os("ANDROID_HOME")
            .or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("Android/Sdk"));
        // Same JDK default as build_android.py; caller's JAVA_HOME is recorded if set.
        let java = std::env::var_os("JAVA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/lib/jvm/java-21-openjdk"));
        let sdk = fs::canonicalize(sdk)?;
        let java = fs::canonicalize(java)?;
        let properties =
            fs::read_to_string(source.join("android/gradle/wrapper/gradle-wrapper.properties"))?;
        if !properties.lines().any(|l| {
            l == "distributionUrl=https\\://services.gradle.org/distributions/gradle-9.8.1-bin.zip"
        }) {
            return Err(
                "Android runner currently requires reviewed Gradle 9.8.1-bin distribution".into(),
            );
        }
        let cache = home.join("build-cache/gradle/wrapper/dists/gradle-9.8.1-bin");
        let mut candidates = Vec::new();
        for (index, entry) in fs::read_dir(cache)?.take(33).enumerate() {
            if index == 32 {
                return Err("Gradle distribution inventory exceeds bound".into());
            }
            let path = entry?.path().join("gradle-9.8.1/bin/gradle");
            if path.is_file() {
                candidates.push(path);
            }
        }
        if candidates.len() != 1 {
            return Err(
                "exactly one installed Gradle 9.8.1 distribution required; no download performed"
                    .into(),
            );
        }
        let gradle = candidates.remove(0);
        let rustup = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".rustup"));
        let descriptor: toml::Value =
            toml::from_str(&fs::read_to_string(source.join("rust-toolchain.toml"))?)?;
        let channel = descriptor
            .get("toolchain")
            .and_then(|v| v.get("channel"))
            .and_then(toml::Value::as_str)
            .ok_or("explicit Rust channel required")?;
        if channel.is_empty() || !channel.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            return Err("only pinned numeric Rust toolchains supported".into());
        }
        let toolchain = format!("{channel}-{}-unknown-linux-gnu", std::env::consts::ARCH);
        let rust = rustup.join("toolchains").join(&toolchain);
        for triple in abis.triples() {
            let libs = rust.join("lib/rustlib").join(triple).join("lib");
            if !fs::read_dir(libs)?
                .any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().starts_with("libstd-")))
            {
                return Err(
                    "required Rust Android standard library absent; no rustup install performed"
                        .into(),
                );
            }
        }
        let mut identities = BTreeMap::new();
        for path in [
            rust.join("bin/rustc"),
            rust.join("bin/cargo"),
            home.join(".cargo/bin/cargo-ndk"),
        ] {
            identities.insert(path.clone(), file_hash(&path, 1024 * 1024 * 1024)?.0);
        }
        let lib = gradle
            .parent()
            .and_then(Path::parent)
            .ok_or("invalid Gradle installation")?
            .join("lib");
        for path in [
            sdk.join("ndk/28.2.13676358/source.properties"),
            sdk.join("platforms/android-37.2/package.xml"),
            java.join("release"),
            gradle.clone(),
            lib.join("gradle-launcher-9.8.1.jar"),
        ] {
            identities.insert(path.clone(), file_hash(&path, 64 * 1024 * 1024)?.0);
        }
        identities.insert(
            java.join("bin/java"),
            file_hash(&java.join("bin/java"), 64 * 1024 * 1024)?.0,
        );
        for name in ["clang", "ld.lld"] {
            let ndk = sdk.join("ndk/28.2.13676358");
            let path = fs::canonicalize(
                ndk.join("toolchains/llvm/prebuilt/linux-x86_64/bin")
                    .join(name),
            )?;
            if !path.starts_with(fs::canonicalize(ndk)?) {
                return Err("NDK executable outside pinned installation".into());
            }
            identities.insert(path.clone(), file_hash(&path, 1024 * 1024 * 1024)?.0);
        }
        // Fingerprint configuration, never copy/log its credential values.
        let mut absent_config = Vec::new();
        for path in [
            home.join(".cargo/config.toml"),
            home.join(".cargo/config"),
            home.join("build-cache/gradle/gradle.properties"),
        ] {
            if path.try_exists()? {
                identities.insert(path.clone(), file_hash(&path, 1024 * 1024)?.0);
            } else {
                absent_config.push(path);
            }
        }
        for path in [
            home.join("build-cache/gradle/init.gradle"),
            home.join("build-cache/gradle/init.gradle.kts"),
        ] {
            if path.try_exists()? {
                return Err("global Gradle init script requires separate review".into());
            }
            absent_config.push(path);
        }
        let init = home.join("build-cache/gradle/init.d");
        if init.try_exists()? && fs::read_dir(&init)?.next().is_some() {
            return Err("nonempty Gradle init.d requires separate review".into());
        }
        Ok(Self {
            sdk,
            java,
            gradle,
            rust,
            toolchain,
            identities,
            absent_config,
            init_dir: init,
        })
    }
    pub fn verify(&self) -> Result<()> {
        for path in &self.absent_config {
            match fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
                Ok(_) => {
                    return Err(
                        "new global Android build configuration appeared while queued".into(),
                    );
                }
            }
        }
        if self.init_dir.try_exists()? && fs::read_dir(&self.init_dir)?.next().is_some() {
            return Err("Gradle init.d changed while queued".into());
        }
        for (path, expected) in &self.identities {
            if file_hash(path, 1024 * 1024 * 1024)?.0 != *expected {
                return Err("Android host/config identity changed while queued".into());
            }
        }
        Ok(())
    }
}
/// Seed only an absent lane-local archive. Never repair/delete a corrupt archive
/// or mutate the shared native install. The upstream builder rechecks this pin.
pub fn seed_archive(source: &Path, destination: &Path, expected: &str) -> Result<()> {
    if file_hash(source, 32 * 1024 * 1024)?.0 != expected {
        return Err("pinned native archive changed".into());
    }
    if !destination.try_exists()? {
        let mut input = File::open(source)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(destination)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
    }
    if file_hash(destination, 32 * 1024 * 1024)?.0 != expected {
        return Err("lane native archive mismatched; no repair/deletion performed".into());
    }
    Ok(())
}
/// Validate central directory, CRCs and requested 64-bit JNI ELF architectures.
/// No extraction or adb. Fixed code, not a submitted command.
pub const CHECK_APK: &str = r#"
import sys, zipfile, pathlib, struct
p, requested = sys.argv[1], set(sys.argv[2].split(','))
with zipfile.ZipFile(p) as z:
    items=z.infolist(); names=[i.filename for i in items]
    assert len(items)<=10000 and len(set(names))==len(names)
    assert sum(i.file_size for i in items)<=1024*1024*1024
    assert all(i.file_size<=512*1024*1024 and not i.flag_bits&1 for i in items)
    assert all(not pathlib.PurePosixPath(n).is_absolute() and '..' not in pathlib.PurePosixPath(n).parts for n in names)
    assert 'AndroidManifest.xml' in names and z.testzip() is None
    actual={n.split('/')[1] for n in names if n.startswith('lib/') and n.endswith('.so')}
    assert actual==requested, (actual, requested)
    for abi in requested:
        with z.open('lib/'+abi+'/libp2pcore.so') as f: head=f.read(20)
        assert head[:6]==b'\x7fELF\x02\x01'
        assert struct.unpack('<H',head[18:20])[0]=={'x86_64':62,'arm64-v8a':183}[abi]
"#;
/// Called after CHECK_APK succeeds, before releasing the lane. Copy, don't link.
/// The worker revalidates source/mappings and supplies provenance separately.
pub fn publish(
    layout: &Layout,
    dir: &Path,
    source_digest: &str,
    configuration: &str,
    provenance: &serde_json::Value,
) -> Result<BuildArtifact> {
    let source = fs::canonicalize(layout.apk())?;
    if !source.starts_with(layout.app_dir()) {
        return Err("APK outside owned output".into());
    }
    let (expected, bytes) = file_hash(&source, MAX_APK)?;
    if bytes == 0 {
        return Err("empty APK".into());
    }
    let space = cache::state_space(dir)?;
    if space.available_bytes.saturating_sub(bytes) < space.reserve_bytes {
        return Err("insufficient durable APK result space".into());
    }
    let output = dir.join("apk");
    fs::create_dir(&output)?;
    let destination = output.join("app-debug.apk");
    let mut input = File::open(&source)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&destination)?;
    let copied = std::io::copy(&mut (&mut input).take(MAX_APK + 1), &mut file)?;
    file.sync_all()?;
    if copied != bytes
        || file_hash(&destination, MAX_APK)?.0 != expected
        || file_hash(&source, MAX_APK)?.0 != expected
    {
        return Err("APK changed during durable publication".into());
    }
    let path = output.join("provenance.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(&serde_json::to_vec_pretty(provenance)?)?;
    file.sync_all()?;
    let provenance_sha256 = file_hash(&path, 4 * 1024 * 1024)?.0;
    File::open(&output)?.sync_all()?;
    File::open(dir)?.sync_all()?;
    Ok(BuildArtifact {
        path: "apk/app-debug.apk".into(),
        sha256: expected,
        bytes,
        source_digest: source_digest.into(),
        configuration: configuration.into(),
        provenance_sha256,
    })
}

#[cfg(test)]
#[path = "android_build_tests.rs"]
mod tests;
