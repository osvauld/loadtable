//! Read-only, opt-in Apple Silicon onboarding. No daemon, network or installer calls.
//!
//! Requirements and plans are untrusted data. Parsing, validation, assessment and
//! rendering never start a process. Only `run_local_preflight` runs tools, using a
//! closed list of local probes independent of any requirements/plan. This is not
//! a sandbox: the locally installed Rust tools and Apple tools must be trusted.
//! Manual commands are display data, NOT an execution or approval API.

pub mod cli;
#[cfg(feature = "ui")]
pub mod ui;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use std::io::Read;
use std::time::Duration;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
pub const MAX_PROBE_BYTES: usize = 32 * 1024;
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_STEPS: usize = 32;
const MAX_TARGETS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupError(pub String);
impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SetupError {}
type Result<T> = std::result::Result<T, SetupError>;

fn invalid(message: &str) -> SetupError {
    SetupError(message.into())
}

fn text(value: &str, max: usize, multiline: bool) -> Result<()> {
    if value.is_empty() || value.len() > max {
        return Err(invalid("empty or oversized text field"));
    }
    if value.chars().any(|c| {
        (c.is_control() && !(multiline && c == '\n'))
            || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }) {
        return Err(invalid(
            "control or bidirectional formatting character in text",
        ));
    }
    Ok(())
}

fn version(value: &str) -> Option<Vec<u32>> {
    if value.len() > 20 {
        return None;
    }
    let parts: Vec<_> = value.split('.').collect();
    if !(1..=3).contains(&parts.len()) {
        return None;
    }
    parts
        .iter()
        .map(|p| {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                None
            } else {
                p.parse().ok()
            }
        })
        .collect()
}

/// No command/probe/URL fields. Unsupported keys, versions and target names fail closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirements {
    pub schema_version: u32,
    pub label: String,
    pub minimum_macos: String,
    pub rust_targets: Vec<String>,
    pub full_xcode: bool,
    pub ios_sdk: bool,
    pub simulator_runtime: bool,
}

impl Default for Requirements {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            label: "Apple Silicon Rust worker".into(),
            minimum_macos: "13.0".into(),
            rust_targets: vec!["aarch64-apple-darwin".into()],
            full_xcode: true,
            ios_sdk: false,
            simulator_runtime: false,
        }
    }
}

impl Requirements {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(invalid("unsupported requirements schema version"));
        }
        text(&self.label, 160, false)?;
        if version(&self.minimum_macos).is_none() {
            return Err(invalid("minimum_macos must have 1-3 numeric components"));
        }
        if self.rust_targets.len() > MAX_TARGETS {
            return Err(invalid("too many Rust targets"));
        }
        let mut seen = BTreeSet::new();
        for target in &self.rust_targets {
            if !matches!(
                target.as_str(),
                "aarch64-apple-darwin"
                    | "x86_64-apple-darwin"
                    | "aarch64-apple-ios"
                    | "aarch64-apple-ios-sim"
                    | "x86_64-apple-ios"
            ) || !seen.insert(target)
            {
                return Err(invalid("unsupported or duplicate Rust target"));
            }
        }
        if (self.ios_sdk || self.simulator_runtime) && !self.full_xcode {
            return Err(invalid("iOS SDK/runtime requirements require full_xcode"));
        }
        Ok(())
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let value: Self = decode(bytes)?;
        value.validate()?;
        Ok(value)
    }
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(invalid("setup document exceeds 256 KiB"));
    }
    serde_json::from_slice(bytes).map_err(|e| SetupError(format!("invalid setup JSON: {e}")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Present,
    Missing,
    Unknown,
    Unsupported,
    NotRequested,
}

/// Human-inspection-only argv. Even a loaded plan containing malicious argv
/// cannot execute it: there is intentionally no method converting this to Command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualCommand {
    pub program: String,
    pub args: Vec<String>,
}
impl ManualCommand {
    /// POSIX single-quoted display, suitable for manual copying, never shell-evaluated here.
    pub fn display(&self) -> String {
        std::iter::once(&self.program)
            .chain(self.args.iter())
            .map(|s| format!("'{}'", s.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ")
    }
    fn validate(&self) -> Result<()> {
        text(&self.program, 512, false)?;
        if self.args.len() > 16 {
            return Err(invalid("too many manual command arguments"));
        }
        for arg in &self.args {
            // Empty arguments are legitimate argv, unlike empty labels/programs.
            if !arg.is_empty() {
                text(arg, 1024, false)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupStep {
    pub capability: String,
    pub state: CapabilityState,
    pub evidence: String,
    pub instructions: String,
    pub manual_commands: Vec<ManualCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupPlan {
    pub schema_version: u32,
    pub requirements: Requirements,
    pub steps: Vec<SetupStep>,
}
impl SetupPlan {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(invalid("unsupported plan schema version"));
        }
        self.requirements.validate()?;
        if self.steps.len() > MAX_STEPS {
            return Err(invalid("too many setup steps"));
        }
        let mut seen = BTreeSet::new();
        for step in &self.steps {
            text(&step.capability, 160, false)?;
            if !seen.insert(&step.capability) {
                return Err(invalid("duplicate capability"));
            }
            text(&step.evidence, 4096, true)?;
            text(&step.instructions, 4096, true)?;
            if step.manual_commands.len() > 8 {
                return Err(invalid("too many manual commands"));
            }
            for command in &step.manual_commands {
                command.validate()?;
            }
        }
        Ok(())
    }

    /// Validate and serialize before computing a document digest. No probes run.
    pub fn document(&self) -> Result<PlanDocument> {
        self.validate()?;
        PlanDocument::from_json(
            &serde_json::to_vec_pretty(self).map_err(|e| SetupError(e.to_string()))?,
        )
    }

    pub fn render_text(&self) -> Result<String> {
        self.validate()?;
        let mut out = format!(
            "{}\nUNTRUSTED DISPLAY DATA. No remote automatic setup implemented.\nCommands are for manual review only; nothing is executed.\n",
            self.requirements.label
        );
        for step in &self.steps {
            out.push_str(&format!(
                "\n{}: {:?}\nEvidence: {}\nFix: {}\n",
                step.capability, step.state, step.evidence, step.instructions
            ));
            for command in &step.manual_commands {
                out.push_str(&format!("  {}\n", command.display()));
            }
        }
        Ok(out)
    }
}

/// Immutable parsed document plus SHA-256 of the EXACT received UTF-8 JSON bytes
/// (including whitespace/order). It binds contents only, not identity, freshness,
/// safety, consent or authorization. No approval enforcement is implemented.
#[derive(Debug, Clone)]
pub struct PlanDocument {
    plan: SetupPlan,
    bytes: Vec<u8>,
    sha256: String,
}
impl PlanDocument {
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let plan: SetupPlan = decode(bytes)?;
        plan.validate()?;
        Ok(Self {
            plan,
            bytes: bytes.to_vec(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
        })
    }
    /// Bounded input read; callers own file/network timeout policy. No process calls.
    pub fn from_reader(reader: impl Read) -> Result<Self> {
        let mut bytes = Vec::new();
        reader
            .take((MAX_DOCUMENT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| SetupError(e.to_string()))?;
        Self::from_json(&bytes)
    }
    pub fn plan(&self) -> &SetupPlan {
        &self.plan
    }
    pub fn exact_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Closed local probe identifiers. Remote data cannot specify argv or probe IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Probe {
    Architecture,
    MacosVersion,
    RustcVersion,
    CargoVersion,
    RustupShow,
    RustTargets,
    DeveloperDirectory,
    XcodeVersion,
    XcodeSdks,
    SimulatorRuntimes,
}
impl Probe {
    pub const ALL: [Self; 10] = [
        Self::Architecture,
        Self::MacosVersion,
        Self::RustcVersion,
        Self::CargoVersion,
        Self::RustupShow,
        Self::RustTargets,
        Self::DeveloperDirectory,
        Self::XcodeVersion,
        Self::XcodeSdks,
        Self::SimulatorRuntimes,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Architecture => "OS architecture",
            Self::MacosVersion => "macOS version",
            Self::RustcVersion => "rustc --version",
            Self::CargoVersion => "cargo --version",
            Self::RustupShow => "rustup show",
            Self::RustTargets => "rustup target list --installed",
            Self::DeveloperDirectory => "xcode-select -p",
            Self::XcodeVersion => "xcodebuild -version",
            Self::XcodeSdks => "xcodebuild -showsdks",
            Self::SimulatorRuntimes => "xcrun simctl list runtimes --json",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    Success,
    Missing,
    Failed,
    TimedOut,
    OutputLimit,
    Skipped,
    Unsupported,
}
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub probe: Probe,
    pub status: ProbeStatus,
    pub stdout: String,
    pub stderr: String,
    pub detail: String,
}
impl ProbeResult {
    fn empty(probe: Probe, status: ProbeStatus, detail: &str) -> Self {
        Self {
            probe,
            status,
            stdout: String::new(),
            stderr: String::new(),
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProbeReport {
    pub results: Vec<ProbeResult>,
}
impl ProbeReport {
    fn get(&self, probe: Probe) -> Option<&ProbeResult> {
        // Ambiguous evidence must not imply readiness.
        let mut results = self.results.iter().filter(|r| r.probe == probe);
        let first = results.next()?;
        if results.next().is_some() {
            None
        } else {
            Some(first)
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProbeOptions {
    /// simctl can start CoreSimulator services. Opt-in independently of remote requirements.
    pub include_simulator_runtime: bool,
}

/// EXPLICIT LOCAL ACTION ONLY. At most 10 fixed commands, 5s and 32 KiB per
/// stdout/stderr each. On non-macOS no commands run. Rust tools are resolved only
/// from the local user's ~/.cargo/bin, not PATH or a received checkout. No shell,
/// sudo, installer, licence acceptance, arbitrary cwd or remote argv.
pub fn run_local_preflight(options: ProbeOptions) -> ProbeReport {
    let results = Probe::ALL
        .into_iter()
        .map(|probe| {
            if !cfg!(target_os = "macos") {
                return ProbeResult::empty(probe, ProbeStatus::Unsupported, "host is not macOS");
            }
            if probe == Probe::SimulatorRuntimes && !options.include_simulator_runtime {
                return ProbeResult::empty(
                    probe,
                    ProbeStatus::Skipped,
                    "simctl not opted in locally",
                );
            }
            #[cfg(unix)]
            {
                local::run(probe)
            }
            #[cfg(not(unix))]
            {
                ProbeResult::empty(
                    probe,
                    ProbeStatus::Unsupported,
                    "Unix process supervision required",
                )
            }
        })
        .collect();
    ProbeReport { results }
}

#[cfg(unix)]
mod local {
    use super::*;
    use std::io;
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;

    fn command(probe: Probe) -> Option<(PathBuf, &'static [&'static str])> {
        let (program, args): (&str, &[&str]) = match probe {
            Probe::Architecture => ("/usr/bin/uname", &["-m"]),
            Probe::MacosVersion => ("/usr/bin/sw_vers", &["-productVersion"]),
            Probe::RustcVersion => ("rustc", &["--version"]),
            Probe::CargoVersion => ("cargo", &["--version"]),
            Probe::RustupShow => ("rustup", &["show"]),
            Probe::RustTargets => ("rustup", &["target", "list", "--installed"]),
            Probe::DeveloperDirectory => ("/usr/bin/xcode-select", &["-p"]),
            Probe::XcodeVersion => ("/usr/bin/xcodebuild", &["-version"]),
            Probe::XcodeSdks => ("/usr/bin/xcodebuild", &["-showsdks"]),
            Probe::SimulatorRuntimes => {
                ("/usr/bin/xcrun", &["simctl", "list", "runtimes", "--json"])
            }
        };
        let path = if program.starts_with('/') {
            PathBuf::from(program)
        } else {
            let home = PathBuf::from(std::env::var_os("HOME")?);
            if !home.is_absolute() {
                return None;
            }
            home.join(".cargo/bin").join(program)
        };
        Some((path, args))
    }

    fn nonblocking(fd: RawFd) -> io::Result<()> {
        // SAFETY: borrowed open pipe fd; flags only, ownership is unchanged.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn read_chunk(reader: &mut impl Read, bytes: &mut Vec<u8>) -> io::Result<bool> {
        let mut chunk = [0; 4096];
        match reader.read(&mut chunk) {
            Ok(n) => {
                let remaining = MAX_PROBE_BYTES.saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..n.min(remaining)]);
                Ok(n > remaining)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    fn stop(child: &mut Child) {
        // SAFETY: fresh owned process group, never a caller-supplied pid/group.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        // No blocking pipe joins or waits. SIGKILL normally reaps on the first
        // poll; if the OS delays exit, a reaper owns the Child off the caller thread.
        for _ in 0..20 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) fn run(probe: Probe) -> ProbeResult {
        let Some((program, args)) = command(probe) else {
            return ProbeResult::empty(
                probe,
                ProbeStatus::Missing,
                "absolute HOME required for ~/.cargo/bin",
            );
        };
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .env("LC_ALL", "C")
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("DEVELOPER_DIR")
            .env_remove("SDKROOT");
        // Probes describe the default user toolchain and system-selected Xcode,
        // not repository overrides. No shell or cwd toolchain file is consulted.
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                return ProbeResult::empty(
                    probe,
                    if e.kind() == io::ErrorKind::NotFound {
                        ProbeStatus::Missing
                    } else {
                        ProbeStatus::Failed
                    },
                    &e.to_string(),
                );
            }
        };
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let started = Instant::now();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut status = ProbeStatus::Success;
        let detail: String;
        let mut exited = false;
        if let Err(e) =
            nonblocking(stdout.as_raw_fd()).and_then(|_| nonblocking(stderr.as_raw_fd()))
        {
            status = ProbeStatus::Failed;
            detail = e.to_string();
        } else {
            loop {
                if started.elapsed() >= PROBE_TIMEOUT {
                    status = ProbeStatus::TimedOut;
                    detail = "5 second deadline exceeded".into();
                    break;
                }
                match read_chunk(&mut stdout, &mut out)
                    .and_then(|a| read_chunk(&mut stderr, &mut err).map(|b| a || b))
                {
                    Ok(true) => {
                        status = ProbeStatus::OutputLimit;
                        detail = "32 KiB stream limit exceeded".into();
                        break;
                    }
                    Err(e) => {
                        status = ProbeStatus::Failed;
                        detail = e.to_string();
                        break;
                    }
                    Ok(false) => {}
                }
                match child.try_wait() {
                    Ok(Some(exit)) => {
                        exited = true;
                        // Drain at most 9 chunks per stream after exit (32 KiB + 1),
                        // including when a descendant keeps the pipe open.
                        for _ in 0..9 {
                            match read_chunk(&mut stdout, &mut out)
                                .and_then(|a| read_chunk(&mut stderr, &mut err).map(|b| a || b))
                            {
                                Ok(true) => {
                                    status = ProbeStatus::OutputLimit;
                                    break;
                                }
                                Err(_) => {
                                    status = ProbeStatus::Failed;
                                    break;
                                }
                                Ok(false) => {}
                            }
                        }
                        if !exit.success() && status == ProbeStatus::Success {
                            status = ProbeStatus::Failed;
                        }
                        detail = format!("exit {exit}");
                        break;
                    }
                    Err(e) => {
                        status = ProbeStatus::Failed;
                        detail = e.to_string();
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        }
        // Never signal a group after reaping its leader: the PID could be reused.
        // Successful Apple tools may intentionally leave system services running.
        if !exited {
            stop(&mut child);
            // A delayed kernel exit must not make the preflight/UI wait forever.
            if !matches!(child.try_wait(), Ok(Some(_))) {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
        }
        ProbeResult {
            probe,
            status,
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            detail,
        }
    }
}

fn state(result: Option<&ProbeResult>) -> CapabilityState {
    match result.map(|r| r.status) {
        Some(ProbeStatus::Success) => CapabilityState::Present,
        Some(ProbeStatus::Missing) => CapabilityState::Missing,
        Some(ProbeStatus::Unsupported) => CapabilityState::Unsupported,
        _ => CapabilityState::Unknown,
    }
}

// Local tool output is also untrusted text. Escape controls and limit UI evidence.
fn evidence(result: Option<&ProbeResult>) -> String {
    let Some(r) = result else {
        return "Not probed (or duplicate evidence)".into();
    };
    let raw = format!(
        "{:?}: {}\n{}\n{}",
        r.status,
        r.detail,
        r.stdout.trim(),
        r.stderr.trim()
    );
    let mut out = String::new();
    for c in raw.chars() {
        let piece = if (c.is_control() && c != '\n')
            || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            format!("\\u{{{:x}}}", c as u32)
        } else {
            c.to_string()
        };
        if out.len() + piece.len() > 3900 {
            out.push_str(" [truncated]");
            break;
        }
        out.push_str(&piece);
    }
    out
}

fn successful(report: &ProbeReport, probe: Probe) -> Option<&str> {
    report
        .get(probe)
        .filter(|r| r.status == ProbeStatus::Success)
        .map(|r| r.stdout.trim())
}

/// Pure assessment of synthetic or locally collected evidence. No processes run.
/// Unknown/failing/timed-out probes never become "missing" or "ready" by guesswork.
pub fn assess(requirements: &Requirements, report: &ProbeReport) -> Result<SetupPlan> {
    requirements.validate()?;
    let mut steps = Vec::new();
    let mut add = |name: &str, state, probe, instructions: &str, manual_commands| {
        steps.push(SetupStep {
            capability: name.into(),
            state,
            evidence: evidence(report.get(probe)),
            instructions: instructions.into(),
            manual_commands,
        });
    };
    let arch = match successful(report, Probe::Architecture) {
        Some("arm64") => CapabilityState::Present,
        Some("x86_64") => CapabilityState::Unsupported,
        Some(_) => CapabilityState::Unknown,
        None => state(report.get(Probe::Architecture)),
    };
    add(
        "Apple Silicon native host",
        arch,
        Probe::Architecture,
        "Use a native arm64 macOS session. x86_64 may be Intel or Rosetta; neither proves native Apple Silicon readiness.",
        vec![],
    );
    let os = match successful(report, Probe::MacosVersion).and_then(version) {
        Some(mut actual) => {
            let mut minimum = version(&requirements.minimum_macos).expect("validated version");
            actual.resize(3, 0);
            minimum.resize(3, 0);
            if actual >= minimum {
                CapabilityState::Present
            } else {
                CapabilityState::Unsupported
            }
        }
        None if successful(report, Probe::MacosVersion).is_some() => CapabilityState::Unknown,
        None => state(report.get(Probe::MacosVersion)),
    };
    add(
        "macOS minimum version",
        os,
        Probe::MacosVersion,
        &format!(
            "Requires macOS {} or later. Review updates manually in System Settings; no update is performed here.",
            requirements.minimum_macos
        ),
        vec![],
    );
    for (probe, prefix) in [
        (Probe::RustcVersion, "rustc "),
        (Probe::CargoVersion, "cargo "),
    ] {
        let s = match successful(report, probe) {
            Some(output)
                if output
                    .strip_prefix(prefix)
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.split('-').next())
                    .and_then(version)
                    .is_some() =>
            {
                CapabilityState::Present
            }
            Some(_) => CapabilityState::Unknown,
            None => state(report.get(probe)),
        };
        add(
            probe.label(),
            s,
            probe,
            "Install/review Rust manually using rustup.rs. Probes look only in ~/.cargo/bin; custom installations are not searched. No download or install is performed.",
            vec![],
        );
    }
    let rustup = match successful(report, Probe::RustupShow) {
        Some(output) if output.contains("no active toolchain") => CapabilityState::Missing,
        Some(output)
            if output.contains("installed toolchains") && output.contains("active toolchain") =>
        {
            CapabilityState::Present
        }
        Some(_) => CapabilityState::Unknown,
        None => state(report.get(Probe::RustupShow)),
    };
    add(
        "Rust toolchain selection",
        rustup,
        Probe::RustupShow,
        "Review the default rustup toolchain in ~/.cargo/bin. Project-specific overrides and compiler usability are not tested.",
        vec![],
    );
    for target in &requirements.rust_targets {
        let s = match successful(report, Probe::RustTargets) {
            Some(output) if output.lines().any(|line| line.trim() == target) => {
                CapabilityState::Present
            }
            Some(output)
                if output.lines().all(|line| {
                    line.is_empty()
                        || (line.split('-').count() >= 3
                            && line
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
                }) =>
            {
                CapabilityState::Missing
            }
            Some(_) => CapabilityState::Unknown,
            None => state(report.get(Probe::RustTargets)),
        };
        let commands = if s == CapabilityState::Missing {
            vec![ManualCommand {
                program: "rustup".into(),
                args: vec!["target".into(), "add".into(), target.clone()],
            }]
        } else {
            vec![]
        };
        add(
            &format!("Rust target {target}"),
            s,
            Probe::RustTargets,
            "If missing, inspect this command and run manually for the intended toolchain (requires a download). Installed std does not prove linker/SDK readiness. Ensure rustup resolves to the trusted ~/.cargo/bin tool.",
            commands,
        );
    }
    let directory = successful(report, Probe::DeveloperDirectory);
    let full =
        directory.is_some_and(|p| p.starts_with('/') && p.ends_with(".app/Contents/Developer"));
    let clt = directory.is_some_and(|p| p == "/Library/Developer/CommandLineTools");
    let selected = if full || clt {
        CapabilityState::Present
    } else if directory.is_some() {
        CapabilityState::Unknown
    } else {
        state(report.get(Probe::DeveloperDirectory))
    };
    add(
        "Selected developer tools",
        selected,
        Probe::DeveloperDirectory,
        "Command Line Tools (CLT) alone do not provide iOS SDKs. Review the selected directory; custom Xcode paths may need manual verification. DEVELOPER_DIR/SDKROOT overrides are ignored by these probes.",
        vec![],
    );
    let xcode_version = successful(report, Probe::XcodeVersion).is_some_and(|s| {
        s.strip_prefix("Xcode ")
            .and_then(|s| s.lines().next())
            .and_then(version)
            .is_some()
            && s.lines().any(|line| {
                line.strip_prefix("Build version ")
                    .is_some_and(|v| !v.is_empty())
            })
    });
    let xcode = if !requirements.full_xcode {
        CapabilityState::NotRequested
    } else if full && xcode_version {
        CapabilityState::Present
    } else if clt {
        CapabilityState::Missing
    } else if matches!(
        report.get(Probe::XcodeVersion).map(|r| r.status),
        Some(ProbeStatus::Missing | ProbeStatus::Unsupported)
    ) {
        state(report.get(Probe::XcodeVersion))
    } else {
        CapabilityState::Unknown
    };
    add(
        "Full Xcode (not CLT only)",
        xcode,
        Probe::XcodeVersion,
        "Install full Xcode manually from Apple if required. Open it yourself to review any first-launch components/licence and select its developer directory. No installation, privileged selection or licence acceptance is implemented.",
        vec![],
    );
    let sdks = successful(report, Probe::XcodeSdks);
    let sdk_state = |requested: bool, prefix: &str| {
        if !requested {
            CapabilityState::NotRequested
        } else if let Some(output) = sdks {
            let words: Vec<_> = output.split_whitespace().collect();
            if words.windows(2).any(|pair| {
                pair[0] == "-sdk" && pair[1].strip_prefix(prefix).and_then(version).is_some()
            }) {
                CapabilityState::Present
            } else if output.contains("-sdk ") {
                CapabilityState::Missing
            } else {
                CapabilityState::Unknown
            }
        } else {
            state(report.get(Probe::XcodeSdks))
        }
    };
    add(
        "macOS SDK",
        sdk_state(true, "macosx"),
        Probe::XcodeSdks,
        "Review SDKs in the selected developer tools. A listed SDK is evidence only, not a successful compile/link test.",
        vec![],
    );
    add(
        "iOS device SDK",
        sdk_state(requirements.ios_sdk, "iphoneos"),
        Probe::XcodeSdks,
        "Use full Xcode with the iOS platform installed through Xcode Settings. No platform download is started.",
        vec![],
    );
    add(
        "iOS simulator SDK",
        sdk_state(requirements.simulator_runtime, "iphonesimulator"),
        Probe::XcodeSdks,
        "A simulator SDK and a bootable runtime are separate requirements. Review Xcode Settings manually.",
        vec![],
    );
    let runtime = if !requirements.simulator_runtime {
        CapabilityState::NotRequested
    } else if let Some(output) = successful(report, Probe::SimulatorRuntimes) {
        match serde_json::from_str::<serde_json::Value>(output)
            .ok()
            .and_then(|v| v.get("runtimes").and_then(|r| r.as_array()).cloned())
        {
            Some(runtimes)
                if runtimes.iter().any(|r| {
                    r.get("isAvailable").and_then(|v| v.as_bool()) == Some(true)
                        && r.get("identifier")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| {
                                s.starts_with("com.apple.CoreSimulator.SimRuntime.iOS-")
                            })
                }) =>
            {
                CapabilityState::Present
            }
            Some(runtimes)
                if runtimes.is_empty()
                    || runtimes.iter().all(|r| {
                        r.get("isAvailable").and_then(|v| v.as_bool()).is_some()
                            && r.get("identifier").and_then(|v| v.as_str()).is_some()
                    }) =>
            {
                CapabilityState::Missing
            }
            _ => CapabilityState::Unknown,
        }
    } else {
        state(report.get(Probe::SimulatorRuntimes))
    };
    add(
        "Available iOS simulator runtime",
        runtime,
        Probe::SimulatorRuntimes,
        "Optionally opt in to the local simctl probe (may start CoreSimulator services). Install/manage runtimes manually in Xcode Settings. No simulator is booted here.",
        vec![],
    );
    let plan = SetupPlan {
        schema_version: SCHEMA_VERSION,
        requirements: requirements.clone(),
        steps,
    };
    plan.validate()?;
    Ok(plan)
}

#[cfg(test)]
mod tests;
