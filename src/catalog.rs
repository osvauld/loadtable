//! Commands are local policy, never supplied by a job. This is not a sandbox:
//! approved local Rust source and Cargo build dependencies execute with the user's rights.
use crate::{Cost, JobSpec, Result};
use serde::Serialize;
use std::path::Path;

pub const SUITE: &str = "task-manager.tests";
pub const CONFIGURATION: &str = "debug/offline";
pub const ANDROID_SUITE: &str = "android.assemble.debug";
// Keep false until real x86_64 and dual-ABI validation, host policy review and
// adequate disk admission. No environment variable silently enables execution.
pub const ANDROID_ENABLED: bool = false;

pub fn android_contract(spec: &JobSpec) -> Result<&'static str> {
    if spec.suite != ANDROID_SUITE || spec.cost != Cost::CpuSimulation {
        return Err(
            "Android build contract requires android.assemble.debug / cpu_simulation".into(),
        );
    }
    match spec.configuration.as_str() {
        "debug/x86_64/rust-debug/offline" => Ok("x86_64"),
        "debug/arm64-v8a/rust-debug/offline" => Ok("arm64-v8a"),
        "debug/x86_64,arm64-v8a/rust-debug/offline" => Ok("x86_64,arm64-v8a"),
        _ => Err("unsupported Android ABI/profile/network configuration".into()),
    }
}

/// Client-side: maps `phone`/`emulator`/an ABI list to the canonical configuration.
/// Daemons keep accepting only canonical strings, so older daemons stay compatible.
pub fn android_configuration(value: &str) -> Result<String> {
    let abis = value
        .strip_prefix("debug/")
        .and_then(|v| v.strip_suffix("/rust-debug/offline"))
        .unwrap_or(value);
    let abis = match abis {
        "x86_64" | "emulator" => "x86_64",
        "arm64-v8a" | "phone" => "arm64-v8a",
        "x86_64,arm64-v8a" => "x86_64,arm64-v8a",
        _ => {
            return Err(
                "Android target must be phone (arm64-v8a), emulator (x86_64) or x86_64,arm64-v8a"
                    .into(),
            );
        }
    };
    Ok(format!("debug/{abis}/rust-debug/offline"))
}

#[derive(Debug, Serialize)]
pub struct Suite {
    pub name: &'static str,
    pub executable: bool,
    pub blocked_on: &'static str,
    pub configuration: &'static str,
    pub build: &'static str,
    pub run: &'static str,
    pub resources: &'static str,
    pub permissions: &'static str,
}

pub fn catalog() -> [Suite; 4] {
    [
        Suite {
            name: SUITE,
            executable: true,
            blocked_on: "",
            configuration: CONFIGURATION,
            build: "cargo test --locked --offline --lib --bins --tests --no-run --message-format=json",
            run: "execute freshly compiled test harnesses directly; no Cargo under a run-only lock",
            resources: "shared build/run admission; exclusive target lease through cleanup; 20 GiB free-space reserve; worker-owned bounded sccache policy",
            permissions: "explicit approval of trusted local source; no phone/network-fetch/stress jobs; Rust code is not sandboxed",
        },
        Suite {
            name: "core.tests.compile",
            executable: false,
            blocked_on: "review complete workspace/build-script/native-input capture; implement compile-only result contract",
            configuration: "debug/offline",
            build: "cargo test --locked --offline -p p2pcore --lib --no-run",
            run: "none; compilation only, not evidence that tests pass",
            resources: "compile slot; Rust workspace target lease; disk reserve; shared sccache",
            permissions: "trusted local build scripts; no tests, devices or application launch; dependencies must already be downloaded",
        },
        Suite {
            name: "desktop.build",
            executable: false,
            blocked_on: "review workspace/native-library/runtime-asset capture; implement build-only artifact contract",
            configuration: "debug/offline",
            build: "cargo build --locked --offline -p desktop --bin p2p-desktop",
            run: "none; never opens the desktop application",
            resources: "compile slot; desktop target lease; disk reserve; shared sccache; installed native toolchain",
            permissions: "trusted local build scripts; no GUI/device access requested; dependencies must already be downloaded",
        },
        Suite {
            name: ANDROID_SUITE,
            executable: ANDROID_ENABLED,
            blocked_on: "managed worker/lane/APK completion implemented; real x86_64 and both-ABI builds, host policy review and sufficient disk reserve still required",
            configuration: "debug/x86_64/rust-debug/offline",
            build: "./gradlew --no-daemon --offline :app:assembleDebug -Pabis=x86_64 -PrustRelease=false -PdevTools=true -PvideoHdCapture=false (from isolated android/)",
            run: "none; APK build only, never install or wipe app data",
            resources: "compile slot; Gradle and Cargo target leases; disk reserve; SDK/NDK; Rust sccache plus separate Gradle cache",
            permissions: "trusted Gradle/build scripts; dependency network policy unresolved; phone/emulator operations forbidden",
        },
    ]
}

pub fn validate(spec: &JobSpec) -> Result<()> {
    if spec.suite != SUITE || spec.configuration != CONFIGURATION || spec.cost != Cost::Quick {
        android_contract(spec)?;
        if !ANDROID_ENABLED {
            return Err(
                "Android execution remains gated pending real APK validation and disk reserve"
                    .into(),
            );
        }
    }
    validate_shape(spec)
}
/// Explicit experimental approval is a separate action, never a catalog/default
/// approval or environment toggle. It exists to validate APKs before rollout.
pub(crate) fn validate_android_trial(spec: &JobSpec) -> Result<()> {
    android_contract(spec)?;
    validate_shape(spec)
}
pub(crate) fn validate_claimed(job: &crate::Job) -> Result<()> {
    if job.execution.as_ref().is_some_and(|e| e.local_command) {
        crate::local_command::Recipe::from_spec(&job.spec)?;
        Ok(())
    } else if job.execution.as_ref().is_some_and(|e| e.android_trial) {
        validate_android_trial(&job.spec)
    } else {
        validate(&job.spec)
    }
}
fn validate_shape(spec: &JobSpec) -> Result<()> {
    if !Path::new(&spec.source).is_absolute() {
        return Err("execution source must be an absolute repository path".into());
    }
    if spec.revision.len() != 40 || !spec.revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(
            "execution revision must be the full 40-character Git HEAD, not a branch or label"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> JobSpec {
        JobSpec {
            suite: SUITE.into(),
            configuration: CONFIGURATION.into(),
            cost: Cost::Quick,
            source: "/fixture/repo".into(),
            revision: "a".repeat(40),
            owner: "fixture".into(),
        }
    }
    #[test]
    fn executable_catalog_is_bounded() {
        assert!(validate(&spec()).is_ok());
        assert_eq!(catalog().iter().filter(|s| s.executable).count(), 1);
        for suite in catalog().iter().filter(|s| !s.executable) {
            let mut proposal = spec();
            proposal.suite = suite.name.into();
            proposal.configuration = suite.configuration.into();
            assert!(validate(&proposal).is_err());
            assert!(!suite.blocked_on.is_empty());
        }
        let mut bad = spec();
        bad.suite = "sh -c arbitrary".into();
        assert!(validate(&bad).is_err());
        let mut bad = spec();
        bad.configuration = "release".into();
        assert!(validate(&bad).is_err());
        let mut bad = spec();
        bad.cost = Cost::Device;
        assert!(validate(&bad).is_err());
        let mut bad = spec();
        bad.source = "dev".into();
        assert!(validate(&bad).is_err());
        let mut bad = spec();
        bad.revision = "HEAD".into();
        assert!(validate(&bad).is_err());
    }
    #[test]
    fn android_target_aliases_canonicalize_before_submission() {
        for (input, abis) in [
            ("phone", "arm64-v8a"),
            ("emulator", "x86_64"),
            ("arm64-v8a", "arm64-v8a"),
            ("x86_64,arm64-v8a", "x86_64,arm64-v8a"),
            ("debug/phone/rust-debug/offline", "arm64-v8a"),
            ("debug/x86_64/rust-debug/offline", "x86_64"),
        ] {
            let mut job = spec();
            job.suite = ANDROID_SUITE.into();
            job.cost = Cost::CpuSimulation;
            job.configuration = android_configuration(input).unwrap();
            assert_eq!(android_contract(&job).unwrap(), abis);
        }
        for bad in [
            "",
            "both",
            "phone,emulator",
            "release/phone/rust-debug/offline",
        ] {
            assert!(android_configuration(bad).is_err(), "{bad}");
        }
        // Aliases never reach stored state: the daemon contract stays canonical-only.
        let mut job = spec();
        job.suite = ANDROID_SUITE.into();
        job.cost = Cost::CpuSimulation;
        job.configuration = "debug/phone/rust-debug/offline".into();
        assert!(android_contract(&job).is_err());
    }
}
