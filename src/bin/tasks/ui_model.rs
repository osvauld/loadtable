//! Pure view model for the dashboard window. It reads cluster snapshots
//! (docs/design/cluster-snapshot.md, v1) and turns them into the cards and rows the
//! window draws: machine cards, running jobs, the queue with start estimates and
//! recent results. No egui and no I/O here; everything is unit-tested in
//! `ui_model_tests.rs`.
//!
//! Reader rules from the contract: unknown fields are ignored, `null` means
//! "unknown" and is shown as such (never as 0), and one malformed job never hides
//! the rest of a snapshot.
#[cfg(test)]
#[path = "ui_model_tests.rs"]
mod tests;

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

/// The snapshot version this reader was written against.
pub const SNAPSHOT_VERSION: u64 = 1;
/// A machine whose newest snapshot is older than this is shown offline.
pub const OFFLINE_AFTER_S: f64 = 15.0;
/// Default slot counts (README "Limits") when a snapshot lists no slots of a kind.
const DEFAULT_SLOTS: [(&str, usize); 3] = [("compile", 1), ("run", 4), ("offload", 4)];
/// argv[0] basenames of offload clients; their own command is the part after `--`.
const OFFLOAD_CLIENTS: [&str; 2] = ["tasks-offload", "offload-portable-desktop.sh"];

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Snapshot reader types (lenient)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Pressure {
    pub cpu_some_avg10: Option<f64>,
    pub memory_some_avg10: Option<f64>,
    pub memory_full_avg10: Option<f64>,
    pub io_some_avg10: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Slot {
    pub kind: Option<String>,
    pub name: Option<String>,
    pub busy: Option<bool>,
    pub holder_job: Option<i64>,
    pub holder_who: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct MachineHistory {
    pub interval_s: Option<f64>,
    #[serde(deserialize_with = "null_default")]
    pub cpu_busy_cores: Vec<Option<f64>>,
    #[serde(deserialize_with = "null_default")]
    pub mem_used_bytes: Vec<Option<f64>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Machine {
    pub id: Option<String>,
    pub role: Option<String>,
    pub logical_cpus: Option<u32>,
    pub cpu_busy_cores: Option<f64>,
    pub mem_total_bytes: Option<u64>,
    pub mem_available_bytes: Option<u64>,
    pub swap_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub pressure: Option<Pressure>,
    pub blocker: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub slots: Vec<Slot>,
    pub history: Option<MachineHistory>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Queue {
    pub position: Option<u64>,
    pub waiting_for: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct JobHistory {
    pub interval_s: Option<f64>,
    #[serde(deserialize_with = "null_default")]
    pub cpu_cores: Vec<Option<f64>>,
    #[serde(deserialize_with = "null_default")]
    pub rss_bytes: Vec<Option<f64>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Live {
    pub cpu_cores: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub processes: Option<u64>,
    pub history: Option<JobHistory>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Progress {
    pub crates_compiled: Option<u64>,
    pub crates_expected: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Estimate {
    pub total_s: Option<f64>,
    pub remaining_s: Option<f64>,
    pub samples: Option<u64>,
    pub basis: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct JobResult {
    pub exit_code: Option<i64>,
    pub wall_s: Option<f64>,
    pub cpu_user_s: Option<f64>,
    pub cpu_sys_s: Option<f64>,
    pub peak_rss_bytes: Option<u64>,
    pub crates_compiled: Option<u64>,
    pub warmth: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Offload {
    pub worker: Option<String>,
    pub remote_job_id: Option<i64>,
    pub transport: Option<String>,
    pub sync_s: Option<f64>,
    pub fetch_s: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Job {
    pub id: i64,
    pub owner: Option<String>,
    pub phase: Option<String>,
    pub resource: Option<String>,
    pub device: Option<String>,
    pub checkout: Option<String>,
    pub checkout_name: Option<String>,
    pub command: Option<String>,
    pub kind_key: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub argv: Vec<String>,
    pub submitted_at: Option<f64>,
    pub started_at: Option<f64>,
    pub finished_at: Option<f64>,
    pub queue: Option<Queue>,
    pub live: Option<Live>,
    pub progress: Option<Progress>,
    pub warmth: Option<String>,
    pub warmth_reason: Option<String>,
    pub estimate: Option<Estimate>,
    pub result: Option<JobResult>,
    pub offload: Option<Offload>,
    /// The job exactly as received, for the details view.
    #[serde(skip)]
    pub raw: Value,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub version: Option<u64>,
    pub taken_at: Option<f64>,
    pub machine: Machine,
    pub jobs: Vec<Job>,
    /// Reader problems (skipped jobs, unreadable machine, newer version).
    pub warnings: Vec<String>,
}

impl Snapshot {
    pub fn parse_str(text: &str) -> Result<Self, String> {
        Self::parse(serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?)
    }

    /// Reads one snapshot document. Only a non-object is an error; anything else
    /// degrades to "unknown" with a warning.
    pub fn parse(value: Value) -> Result<Self, String> {
        let Value::Object(mut object) = value else {
            return Err("snapshot is not a JSON object".into());
        };
        let mut warnings = Vec::new();
        let version = object.get("version").and_then(Value::as_u64);
        if let Some(v) = version.filter(|v| *v > SNAPSHOT_VERSION) {
            warnings.push(format!(
                "snapshot version {v} is newer than this window ({SNAPSHOT_VERSION}); unknown fields ignored"
            ));
        }
        let taken_at = object.get("taken_at").and_then(Value::as_f64);
        let machine = match object.remove("machine") {
            None | Some(Value::Null) => Machine::default(),
            Some(value) => serde_json::from_value(value).unwrap_or_else(|e| {
                warnings.push(format!("machine section unreadable: {e}"));
                Machine::default()
            }),
        };
        let mut jobs = Vec::new();
        if let Some(Value::Array(list)) = object.remove("jobs") {
            for value in list {
                if !value.get("id").is_some_and(Value::is_i64) {
                    warnings.push("skipped a job without an integer id".into());
                    continue;
                }
                match serde_json::from_value::<Job>(value.clone()) {
                    Ok(mut job) => {
                        job.raw = value;
                        jobs.push(job);
                    }
                    Err(e) => warnings.push(format!(
                        "skipped job {}: {e}",
                        value.get("id").and_then(Value::as_i64).unwrap_or_default()
                    )),
                }
            }
        }
        Ok(Self {
            version,
            taken_at,
            machine,
            jobs,
            warnings,
        })
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Memory in binary gigabytes labelled "GB" (as `free -h` users read it): one
/// decimal below 10, whole numbers above.
pub fn gb(bytes: f64) -> String {
    format!("{} GB", gb_number(bytes / GIB, bytes / GIB))
}

fn gb_number(value: f64, scale: f64) -> String {
    if scale < 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    }
}

/// `21/39 GB` or `6.1/7.6 GB`: both numbers use the total's precision.
pub fn used_of_total(used: f64, total: f64) -> String {
    let scale = total / GIB;
    format!(
        "{}/{} GB",
        gb_number(used / GIB, scale),
        gb_number(total / GIB, scale)
    )
}

/// Elapsed time as a clock: `12:04`, or `1:02:05` past an hour.
pub fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Rounded minutes for estimates: `4 min`, `1 h 05 min`.
pub fn minutes(seconds: f64) -> String {
    let m = ((seconds.max(0.0) / 60.0).round() as u64).max(1);
    if m >= 60 {
        format!("{} h {:02} min", m / 60, m % 60)
    } else {
        format!("{m} min")
    }
}

/// Remaining time of a running job.
pub fn left(remaining: f64) -> String {
    if remaining <= 0.0 {
        "taking longer than estimated".into()
    } else if remaining < 45.0 {
        "<1 min left".into()
    } else {
        format!("~{} left", minutes(remaining))
    }
}

pub fn ago(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    match s {
        0..60 => format!("{s} s ago"),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86400),
    }
}

pub fn cores(value: f64) -> String {
    format!("{value:.1} cores")
}

pub fn percent(fraction: f64) -> String {
    format!("{:.0}%", (fraction * 100.0).clamp(0.0, 999.0))
}

/// Short display command: drops `env` and `NAME=value` prefixes, `-j N`,
/// `--locked`/`--offline`/`--frozen`, reduces paths to their last component and
/// shows an offload client as the command it offloads. Display only.
pub fn short_command(argv: &[String]) -> String {
    let mut args: Vec<&str> = argv.iter().map(String::as_str).collect();
    loop {
        while args
            .first()
            .is_some_and(|a| *a == "env" || is_assignment(a))
        {
            args.remove(0);
        }
        let Some(first) = args.first() else {
            return String::new();
        };
        if OFFLOAD_CLIENTS.contains(&basename(first)) {
            match args.iter().position(|a| *a == "--") {
                Some(i) if i + 1 < args.len() => {
                    args.drain(..=i);
                    continue;
                }
                _ => {}
            }
        }
        break;
    }
    let mut out = Vec::new();
    let mut skip_next = false;
    for (i, arg) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        match *arg {
            "--locked" | "--offline" | "--frozen" => continue,
            "-j" | "--jobs" => {
                skip_next = true;
                continue;
            }
            a if a.starts_with("--jobs=")
                || (a.starts_with("-j")
                    && a.len() > 2
                    && a[2..].bytes().all(|b| b.is_ascii_digit())) =>
            {
                continue;
            }
            a if i == 0 || a.starts_with('/') => out.push(basename(a).to_string()),
            a => out.push(a.to_string()),
        }
    }
    out.join(" ")
}

fn is_assignment(arg: &str) -> bool {
    arg.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
}

fn owner_name(owner: &str) -> &str {
    owner.split_once('@').map(|(who, _)| who).unwrap_or(owner)
}

/// The agent running a job: its owner up to the `@` (`metrics@kurukkan` ->
/// (`metrics`, `kurukkan`)); an owner without `@` is all agent, no host.
pub fn agent_of(owner: &str) -> (String, Option<String>) {
    let owner = owner.trim();
    let (agent, host) = match owner.split_once('@') {
        Some((agent, host)) => (agent, Some(host.to_string()).filter(|h| !h.is_empty())),
        None => (owner, None),
    };
    let agent = if agent.is_empty() { "unknown" } else { agent };
    (agent.to_string(), host)
}

/// A stable hue (0..360) per agent name, so one agent has one colour everywhere.
pub fn agent_hue(name: &str) -> u32 {
    name.chars()
        .fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32))
        % 360
}

/// The agent's dot colour: its hue at fixed 60 % saturation, 55 % lightness.
pub fn agent_rgb(name: &str) -> [u8; 3] {
    hsl_to_rgb(agent_hue(name) as f32, 0.60, 0.55)
}

pub fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> [u8; 3] {
    let c = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let h = (hue.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = lightness - c / 2.0;
    let byte = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [byte(r), byte(g), byte(b)]
}

// ---------------------------------------------------------------------------
// Inputs and view types
// ---------------------------------------------------------------------------

/// What the window knows about one machine's source.
#[derive(Clone, Debug, Default)]
pub struct MachineState {
    /// Shown when the snapshot has no machine id yet ("laptop", "worker").
    pub fallback_name: String,
    pub snapshot: Option<Snapshot>,
    /// The local daemon's machine: its jobs can be cancelled and their logs read.
    pub local: bool,
    /// Seconds since `snapshot.taken_at`, as measured on the receiving side.
    pub seen_ago_s: Option<f64>,
    pub error: Option<String>,
    /// False when no source is configured for this card (e.g. no remote command).
    pub configured: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Presence {
    Online,
    Connecting,
    NotConfigured(String),
    Offline {
        last_seen: Option<String>,
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Level {
    Ok,
    Warn,
    Bad,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Warmth {
    Warm,
    Cold,
    Unknown,
}

impl Warmth {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("warm") => Self::Warm,
            Some("cold") => Self::Cold,
            _ => Self::Unknown,
        }
    }
    pub fn badge(self) -> &'static str {
        match self {
            Self::Warm => "WARM",
            Self::Cold => "COLD",
            Self::Unknown => "?",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobKey {
    pub machine: String,
    pub id: i64,
}

/// What a button does: talk to the local daemon about job N, or stay disabled.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Local { id: i64, hover: String },
    Disabled(String),
}

#[derive(Clone, Debug)]
pub struct MachineCard {
    pub title: String,
    pub status: Presence,
    pub cpu_fraction: Option<f32>,
    pub cpu_text: String,
    pub ram_fraction: Option<f32>,
    pub ram_text: String,
    pub swap_text: String,
    pub pressure_text: String,
    pub pressure_level: Level,
    pub pressure_detail: String,
    pub slots_text: String,
    pub slots_detail: String,
    /// One chip per slot: `build #320 metrics` when held, `run-1` when free.
    pub chips: Vec<SlotChip>,
    /// Distinct agents with running or waiting jobs on this machine.
    pub agents: Vec<String>,
    pub cpu_history: Vec<Option<f32>>,
    pub mem_history: Vec<Option<f32>>,
    pub history_span: String,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SlotChip {
    pub text: String,
    pub busy: bool,
    pub agent: Option<String>,
    pub hover: String,
}

#[derive(Clone, Debug)]
pub struct RunningCard {
    pub key: JobKey,
    /// The laptop offload client shown folded into this worker job.
    pub via: Option<i64>,
    pub title: String,
    pub agent: String,
    pub owner: String,
    pub command: String,
    pub machine: String,
    pub warmth: Warmth,
    pub warmth_reason: String,
    pub cpu_text: String,
    pub rss_text: String,
    pub cpu_history: Vec<Option<f32>>,
    pub rss_history: Vec<Option<f32>>,
    /// (fraction when the total is known, text).
    pub progress: Option<(Option<f32>, String)>,
    pub elapsed_text: String,
    pub eta_text: String,
    pub eta_basis: String,
    pub offload_note: Option<String>,
    pub stale: Option<String>,
    pub cancel: Target,
    pub logs: Target,
    pub details: Details,
}

#[derive(Clone, Debug)]
pub struct QueuedRow {
    pub key: JobKey,
    pub via: Option<i64>,
    pub position: usize,
    pub title: String,
    pub agent: String,
    pub owner: String,
    pub warmth: Warmth,
    pub warmth_reason: String,
    pub command: String,
    pub machine: String,
    pub waiting_text: String,
    pub start_text: String,
    pub stale: Option<String>,
    pub proposal: bool,
    pub local: bool,
    pub cancel: Target,
    pub logs: Target,
    pub details: Details,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    Passed,
    Failed,
    Cancelled,
    Interrupted,
}

impl Outcome {
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Passed => "✔",
            Self::Failed => "✖",
            Self::Cancelled => "⏹",
            Self::Interrupted => "⚠",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RecentRow {
    pub key: JobKey,
    pub via: Option<i64>,
    pub outcome: Outcome,
    pub title: String,
    pub agent: String,
    pub owner: String,
    pub warmth: Warmth,
    pub wall_text: String,
    pub finished_text: String,
    pub command: String,
    pub machine: String,
    pub summary: String,
    pub finished_at: Option<f64>,
    pub logs: Target,
    pub details: Details,
}

#[derive(Clone, Debug, Default)]
pub struct Details {
    /// The full owner, `agent@host`.
    pub owner: String,
    pub argv: String,
    pub checkout: String,
    pub raw_json: String,
}

#[derive(Clone, Debug, Default)]
pub struct View {
    /// `2 agents active · 3 running · 2 queued · 13.2 cores busy across 2 machines`.
    pub summary: String,
    pub machines: Vec<MachineCard>,
    pub running: Vec<RunningCard>,
    pub queued: Vec<QueuedRow>,
    pub recent: Vec<RecentRow>,
}

// ---------------------------------------------------------------------------
// Machine cards
// ---------------------------------------------------------------------------

fn role_label(machine: &Machine, fallback: &str) -> String {
    match machine.role.as_deref() {
        Some("host") => "laptop".into(),
        Some("worker") => "worker".into(),
        Some(other) => other.into(),
        None => fallback.into(),
    }
}

pub fn online(state: &MachineState) -> Presence {
    if !state.configured {
        return Presence::NotConfigured(
            state
                .error
                .clone()
                .unwrap_or_else(|| "no source configured".into()),
        );
    }
    match (&state.snapshot, state.seen_ago_s) {
        (None, _) => match &state.error {
            Some(error) => Presence::Offline {
                last_seen: None,
                reason: error.clone(),
            },
            None => Presence::Connecting,
        },
        (Some(_), Some(age)) if age <= OFFLINE_AFTER_S => Presence::Online,
        (Some(_), age) => Presence::Offline {
            last_seen: age.map(ago),
            reason: state
                .error
                .clone()
                .unwrap_or_else(|| "no update received recently".into()),
        },
    }
}

/// Pressure in words: the admission blocker if any, else "ok" when the PSI
/// values are known, else "unknown".
pub fn pressure_words(machine: &Machine) -> (String, Level) {
    if let Some(blocker) = &machine.blocker {
        let lower = blocker.to_lowercase();
        let (text, level) = if lower.contains("available memory") {
            ("low free memory", Level::Bad)
        } else if lower.contains("memory") {
            ("memory high", Level::Bad)
        } else if lower.contains("i/o") {
            ("I/O high", Level::Bad)
        } else if lower.contains("cpu") {
            ("CPU high", Level::Warn)
        } else if lower.contains("disk") {
            ("disk low", Level::Bad)
        } else {
            (blocker.as_str(), Level::Warn)
        };
        return (text.into(), level);
    }
    match &machine.pressure {
        Some(p) if p.memory_some_avg10.is_some() || p.cpu_some_avg10.is_some() => {
            ("ok".into(), Level::Ok)
        }
        _ => ("unknown".into(), Level::Unknown),
    }
}

fn pressure_detail(machine: &Machine) -> String {
    let value = |v: Option<f64>| v.map(|v| format!("{v:.2}%")).unwrap_or("unknown".into());
    let mut text = match &machine.blocker {
        Some(b) => format!("Admission blocker: {b}\n"),
        None => "No admission pressure blocker reported.\n".into(),
    };
    match &machine.pressure {
        Some(p) => {
            text += &format!(
                "PSI avg10: CPU some {}, memory some {}, memory full {}, I/O some {}",
                value(p.cpu_some_avg10),
                value(p.memory_some_avg10),
                value(p.memory_full_avg10),
                value(p.io_some_avg10)
            )
        }
        None => text += "PSI: unknown",
    }
    text
}

fn slot_kind(slot: &Slot) -> &str {
    slot.kind.as_deref().unwrap_or("?")
}

/// `compile 1/1 · run 1/4 · offload 2/4 · phone: none`.
pub fn slots_text(machine: &Machine) -> (String, String) {
    let mut counts: BTreeMap<usize, (&str, usize, usize)> = BTreeMap::new();
    let order = |kind: &str| match kind {
        "compile" => 0,
        "run" => 1,
        "offload" => 2,
        _ => 3,
    };
    let mut devices = Vec::new();
    let mut detail = Vec::new();
    for slot in &machine.slots {
        let kind = slot_kind(slot);
        let busy = slot.busy.unwrap_or(false);
        let holder = match (slot.holder_job, &slot.holder_who) {
            (Some(job), Some(who)) => format!(" by #{job} {who}"),
            (Some(job), None) => format!(" by #{job}"),
            (None, Some(who)) => format!(" by {who}"),
            (None, None) => String::new(),
        };
        detail.push(format!(
            "{kind} {}: {}{holder}",
            slot.name.as_deref().unwrap_or("?"),
            match slot.busy {
                Some(true) => "busy",
                Some(false) => "free",
                None => "unknown",
            }
        ));
        if kind == "device" {
            let name = slot.name.as_deref().unwrap_or("device");
            let name = name.strip_prefix("p2p-device-").unwrap_or(name);
            devices.push(format!("{name} {}", if busy { "busy" } else { "free" }));
            continue;
        }
        let entry = counts.entry(order(kind)).or_insert((kind, 0, 0));
        entry.1 += busy as usize;
        entry.2 += 1;
    }
    let mut parts: Vec<String> = counts
        .values()
        .map(|(kind, busy, total)| format!("{kind} {busy}/{total}"))
        .collect();
    if machine.slots.is_empty() {
        parts.push("slots unknown".into());
    }
    parts.push(if devices.is_empty() {
        "phone: none".into()
    } else {
        format!("phone: {}", devices.join(", "))
    });
    let detail = if detail.is_empty() {
        "No slot information in the snapshot.".into()
    } else {
        detail.join("\n")
    };
    (parts.join(" · "), detail)
}

pub fn slot_chip(slot: &Slot) -> SlotChip {
    let name = slot
        .name
        .as_deref()
        .map(|n| n.strip_prefix("p2p-").unwrap_or(n).to_string())
        .unwrap_or_else(|| slot_kind(slot).to_string());
    let busy = slot.busy.unwrap_or(false);
    let agent = slot
        .holder_who
        .as_deref()
        .filter(|_| busy)
        .map(|who| agent_of(who).0);
    let mut text = name.clone();
    if busy {
        if let Some(job) = slot.holder_job {
            text += &format!(" #{job}");
        }
        if let Some(agent) = &agent {
            text += &format!(" {agent}");
        }
    }
    let hover = match (busy, &slot.holder_who, slot.busy) {
        (true, Some(who), _) => format!("{name}: held by {who}"),
        (true, None, _) => format!("{name}: busy"),
        (false, _, Some(false)) => format!("{name}: free"),
        _ => format!("{name}: state unknown"),
    };
    SlotChip {
        text,
        busy,
        agent,
        hover,
    }
}

fn normalise(values: &[Option<f64>], scale: Option<f64>) -> Vec<Option<f32>> {
    let scale = scale.filter(|s| *s > 0.0).or_else(|| {
        values
            .iter()
            .flatten()
            .copied()
            .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))))
            .filter(|m| *m > 0.0)
    });
    values
        .iter()
        .map(|v| match (v, scale) {
            (Some(v), Some(s)) if v.is_finite() => Some((v / s).clamp(0.0, 1.0) as f32),
            (Some(_), None) => Some(0.0),
            _ => None,
        })
        .collect()
}

pub fn machine_card(state: &MachineState) -> MachineCard {
    let empty = Machine::default();
    let snapshot = state.snapshot.as_ref();
    let machine = snapshot.map(|s| &s.machine).unwrap_or(&empty);
    let role = role_label(machine, &state.fallback_name);
    let title = match &machine.id {
        Some(id) if role == "worker" => format!("{id} worker"),
        Some(id) => format!("{id} ({role})"),
        None => role.clone(),
    };
    let cpu_fraction = match (machine.cpu_busy_cores, machine.logical_cpus) {
        (Some(busy), Some(n)) if n > 0 => Some((busy / n as f64).clamp(0.0, 1.0)),
        _ => None,
    };
    let cpu_text = match (cpu_fraction, machine.cpu_busy_cores) {
        (Some(f), _) => format!("CPU {}", percent(f)),
        (None, Some(busy)) => format!("CPU {}", cores(busy)),
        (None, None) => "CPU unknown".into(),
    };
    let (ram_fraction, ram_text) = match (machine.mem_total_bytes, machine.mem_available_bytes) {
        (Some(total), Some(available)) if total > 0 => {
            let used = total.saturating_sub(available) as f64;
            (
                Some(used / total as f64),
                format!("RAM {}", used_of_total(used, total as f64)),
            )
        }
        _ => (None, "RAM unknown".into()),
    };
    let swap_text = match (machine.swap_total_bytes, machine.swap_used_bytes) {
        (Some(0), _) => "swap none".into(),
        (_, Some(used)) => format!("swap {}", gb(used as f64)),
        _ => "swap unknown".into(),
    };
    let (pressure_text, pressure_level) = pressure_words(machine);
    let (slots_text, slots_detail) = slots_text(machine);
    let chips = machine.slots.iter().map(slot_chip).collect();
    let mut agents: Vec<String> = Vec::new();
    for job in snapshot.map(|s| s.jobs.as_slice()).unwrap_or(&[]) {
        if matches!(class(job), Class::Running | Class::Waiting) {
            let (agent, _) = agent_of(job.owner.as_deref().unwrap_or(""));
            if !agents.contains(&agent) {
                agents.push(agent);
            }
        }
    }
    let history = machine.history.clone().unwrap_or_default();
    let points = history
        .cpu_busy_cores
        .len()
        .max(history.mem_used_bytes.len());
    let history_span = match history.interval_s {
        Some(i) if points > 1 => format!(
            "last {} (one point every {i:.0} s)",
            minutes(i * (points - 1) as f64)
        ),
        _ => "no history yet".into(),
    };
    let mut warnings = snapshot.map(|s| s.warnings.clone()).unwrap_or_default();
    if let Some(s) = snapshot
        && s.version.is_none()
    {
        warnings.push("snapshot has no version; read as v1".into());
    }
    if let Some(error) = &state.error
        && snapshot.is_some()
    {
        warnings.push(error.clone());
    }
    MachineCard {
        title,
        status: online(state),
        cpu_fraction: cpu_fraction.map(|f| f as f32),
        cpu_text,
        ram_fraction: ram_fraction.map(|f| f as f32),
        ram_text,
        swap_text,
        pressure_text,
        pressure_level,
        pressure_detail: pressure_detail(machine),
        slots_text,
        slots_detail,
        chips,
        agents,
        cpu_history: normalise(
            &history.cpu_busy_cores,
            machine.logical_cpus.map(|n| n as f64),
        ),
        mem_history: normalise(
            &history.mem_used_bytes,
            machine.mem_total_bytes.map(|t| t as f64),
        ),
        history_span,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// Jobs: classification, pairing, queue math
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Class {
    /// Submitted, awaiting approval (`queued` phase).
    Proposal,
    Waiting,
    Running,
    Finished(Outcome),
}

pub fn class(job: &Job) -> Class {
    match job.phase.as_deref() {
        Some("queued") => Class::Proposal,
        Some("approved" | "preparing") => Class::Waiting,
        Some(p) if p.starts_with("waiting") => Class::Waiting,
        Some("building" | "running") => Class::Running,
        Some("passed") => Class::Finished(Outcome::Passed),
        Some("failed") => Class::Finished(Outcome::Failed),
        Some("cancelled") => Class::Finished(Outcome::Cancelled),
        Some("interrupted") => Class::Finished(Outcome::Interrupted),
        _ if job.finished_at.is_some() => {
            Class::Finished(match job.result.as_ref().and_then(|r| r.exit_code) {
                Some(0) => Outcome::Passed,
                _ => Outcome::Failed,
            })
        }
        _ if job.live.is_some() || job.started_at.is_some() => Class::Running,
        _ => Class::Waiting,
    }
}

fn is_offload(job: &Job) -> bool {
    job.offload.is_some() || job.resource.as_deref() == Some("offload")
}

/// Slot queue a job waits in: `compile`, `run`, `offload` or `device:SERIAL`.
pub fn slot_queue(job: &Job) -> String {
    match job.resource.as_deref() {
        Some("device") => format!("device:{}", job.device.as_deref().unwrap_or("?")),
        Some(kind) => kind.into(),
        None => "?".into(),
    }
}

/// Simulates when each queued job gets a slot. `busy` is the remaining time of
/// every job holding a slot (None = no estimate), `capacity` the slot count and
/// `queued` each waiting job's expected runtime in queue order. A start is None
/// as soon as it depends on an unknown duration; every later one is then None too.
pub fn queue_starts(
    busy: &[Option<f64>],
    capacity: usize,
    queued: &[Option<f64>],
) -> Vec<Option<f64>> {
    let mut slots: Vec<Option<f64>> = busy.iter().map(|r| r.map(|r| r.max(0.0))).collect();
    while slots.len() < capacity.max(1) {
        slots.push(Some(0.0));
    }
    let mut lost = false;
    let mut starts = Vec::with_capacity(queued.len());
    for total in queued {
        if lost {
            starts.push(None);
            continue;
        }
        let earliest = slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|s| (i, s)))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        let unknown = slots.iter().any(Option::is_none);
        match earliest {
            Some((i, start)) if !unknown || start <= 0.0 => {
                starts.push(Some(start));
                slots[i] = total.map(|t| start + t.max(0.0));
            }
            _ => {
                lost = true;
                starts.push(None);
            }
        }
    }
    starts
}

struct Entry<'a> {
    machine: usize,
    job: &'a Job,
}

fn entries(states: &[MachineState]) -> Vec<Entry<'_>> {
    states
        .iter()
        .enumerate()
        .filter_map(|(machine, s)| s.snapshot.as_ref().map(|snap| (machine, snap)))
        .flat_map(|(machine, snap)| snap.jobs.iter().map(move |job| Entry { machine, job }))
        .collect()
}

/// Laptop offload job index -> worker job index (indices into `entries`). The
/// pair is linked by `offload.remote_job_id`, else by owner `WHO@laptop` and the
/// same checkout (contract "Rules"). Each worker job pairs at most once.
fn pair_offloads(states: &[MachineState], list: &[Entry]) -> HashMap<usize, usize> {
    let mut pairs = HashMap::new();
    let mut used = HashSet::new();
    let machine_id = |m: usize| {
        states[m]
            .snapshot
            .as_ref()
            .and_then(|s| s.machine.id.clone())
            .unwrap_or_default()
    };
    for (l, laptop) in list.iter().enumerate() {
        if !states[laptop.machine].local || !is_offload(laptop.job) {
            continue;
        }
        let remote = |w: &Entry| !states[w.machine].local;
        let by_id = laptop
            .job
            .offload
            .as_ref()
            .and_then(|o| o.remote_job_id)
            .and_then(|rid| {
                list.iter()
                    .position(|w| remote(w) && w.job.id == rid)
                    .filter(|w| !used.contains(w))
            });
        let found = by_id.or_else(|| {
            let owner = laptop.job.owner.as_deref()?;
            let tagged = format!("{}@{}", owner_name(owner), machine_id(laptop.machine));
            let laptop_done = matches!(class(laptop.job), Class::Finished(_));
            let submitted = laptop.job.submitted_at.unwrap_or(0.0);
            list.iter()
                .enumerate()
                .filter(|(w, entry)| {
                    remote(entry)
                        && !used.contains(w)
                        && entry.job.offload.is_none()
                        && entry
                            .job
                            .owner
                            .as_deref()
                            .is_some_and(|o| o == tagged || o == owner)
                        && entry.job.checkout.is_some()
                        && entry.job.checkout == laptop.job.checkout
                        && matches!(class(entry.job), Class::Finished(_)) == laptop_done
                        && entry.job.submitted_at.unwrap_or(submitted) >= submitted - 60.0
                })
                .min_by(|a, b| {
                    let d = |e: &Entry| (e.job.submitted_at.unwrap_or(submitted) - submitted).abs();
                    d(a.1).total_cmp(&d(b.1))
                })
                .map(|(w, _)| w)
        });
        if let Some(w) = found {
            used.insert(w);
            pairs.insert(l, w);
        }
    }
    pairs
}

fn details(job: &Job) -> Details {
    Details {
        owner: job.owner.clone().unwrap_or_else(|| "unknown".into()),
        argv: if job.argv.is_empty() {
            "unknown".into()
        } else {
            job.argv.join(" ")
        },
        checkout: job.checkout.clone().unwrap_or_else(|| "unknown".into()),
        raw_json: serde_json::to_string_pretty(&job.raw).unwrap_or_default(),
    }
}

fn title(job: &Job) -> String {
    job.checkout_name
        .clone()
        .or_else(|| job.checkout.as_deref().map(|c| basename(c).to_string()))
        .or_else(|| job.owner.as_deref().map(|o| owner_name(o).to_string()))
        .unwrap_or_else(|| "unknown".into())
}

fn command(job: &Job) -> String {
    job.command
        .clone()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| {
            let short = short_command(&job.argv);
            if short.is_empty() {
                "unknown command".into()
            } else {
                short
            }
        })
}

struct Ctx<'a> {
    states: &'a [MachineState],
}

impl Ctx<'_> {
    fn label(&self, m: usize) -> String {
        let state = &self.states[m];
        state
            .snapshot
            .as_ref()
            .map(|s| role_label(&s.machine, &state.fallback_name))
            .unwrap_or_else(|| state.fallback_name.clone())
    }
    /// Seconds to advance the machine's clock past its snapshot. Offline
    /// machines are shown as of their last snapshot (0), not extrapolated.
    fn age(&self, m: usize) -> f64 {
        if online(&self.states[m]) != Presence::Online {
            return 0.0;
        }
        self.states[m].seen_ago_s.unwrap_or(0.0).max(0.0)
    }
    /// The machine's own clock now: snapshot time plus the time since receipt.
    fn now(&self, m: usize) -> Option<f64> {
        self.states[m]
            .snapshot
            .as_ref()
            .and_then(|s| s.taken_at)
            .map(|t| t + self.age(m))
    }
    fn stale(&self, m: usize) -> Option<String> {
        match online(&self.states[m]) {
            Presence::Online => None,
            Presence::Offline {
                last_seen: Some(seen),
                ..
            } => Some(format!("machine offline; last seen {seen}")),
            _ => Some("machine offline; state may be out of date".into()),
        }
    }
    fn remaining(&self, m: usize, job: &Job) -> Option<f64> {
        job.estimate
            .as_ref()
            .and_then(|e| e.remaining_s)
            .map(|r| r - self.age(m))
    }
    fn local_target(&self, m: usize, job: &Job, what: &str) -> Target {
        if self.states[m].local {
            Target::Local {
                id: job.id,
                hover: format!("{what} job #{}", job.id),
            }
        } else {
            Target::Disabled(match what {
                "Cancel" => "cancel on the worker: later".into(),
                _ => format!(
                    "logs on the worker: run `tasks logs {}` there (in-window remote logs: later)",
                    job.id
                ),
            })
        }
    }
}

fn agent(job: &Job) -> String {
    agent_of(job.owner.as_deref().unwrap_or("")).0
}

fn warmth_reason(job: &Job) -> String {
    job.warmth_reason
        .clone()
        .unwrap_or_else(|| match job.warmth.as_deref() {
            Some("warm") | Some("cold") => "no reason given".into(),
            _ => "warmth unknown".into(),
        })
}

/// Builds the whole view. Pure: the same states always give the same view.
pub fn build(states: &[MachineState]) -> View {
    let ctx = Ctx { states };
    let list = entries(states);
    let pairs = pair_offloads(states, &list);
    let hidden: HashSet<usize> = pairs.keys().copied().collect();
    let laptop_of: HashMap<usize, usize> = pairs.iter().map(|(l, w)| (*w, *l)).collect();
    let key = |e: &Entry| JobKey {
        machine: states[e.machine]
            .snapshot
            .as_ref()
            .and_then(|s| s.machine.id.clone())
            .unwrap_or_else(|| states[e.machine].fallback_name.clone()),
        id: e.job.id,
    };

    // Queue start estimates per (machine, slot queue), over every job including
    // hidden laptop offload clients (they do hold laptop offload slots).
    let mut starts: HashMap<usize, Option<f64>> = HashMap::new();
    let mut positions: HashMap<usize, usize> = HashMap::new();
    let mut groups: BTreeMap<(usize, String), (Vec<usize>, Vec<usize>)> = BTreeMap::new();
    for (i, e) in list.iter().enumerate() {
        let group = groups.entry((e.machine, slot_queue(e.job))).or_default();
        match class(e.job) {
            Class::Running => group.0.push(i),
            Class::Waiting => group.1.push(i),
            _ => {}
        }
    }
    for ((machine, queue), (running, mut waiting)) in groups {
        waiting.sort_by(|a, b| {
            let pos = |i: &usize| list[*i].job.queue.as_ref().and_then(|q| q.position);
            let sub = |i: &usize| list[*i].job.submitted_at.unwrap_or(f64::MAX);
            pos(a)
                .unwrap_or(u64::MAX)
                .cmp(&pos(b).unwrap_or(u64::MAX))
                .then(sub(a).total_cmp(&sub(b)))
                .then(list[*a].job.id.cmp(&list[*b].job.id))
        });
        let capacity = if queue.starts_with("device:") {
            1
        } else {
            let listed = states[machine]
                .snapshot
                .as_ref()
                .map(|s| {
                    s.machine
                        .slots
                        .iter()
                        .filter(|slot| slot_kind(slot) == queue)
                        .count()
                })
                .unwrap_or(0);
            if listed > 0 {
                listed
            } else {
                DEFAULT_SLOTS
                    .iter()
                    .find(|(k, _)| *k == queue)
                    .map(|(_, n)| *n)
                    .unwrap_or(1)
            }
        };
        let busy: Vec<_> = running
            .iter()
            .map(|i| ctx.remaining(machine, list[*i].job))
            .collect();
        let totals: Vec<_> = waiting
            .iter()
            .map(|i| list[*i].job.estimate.as_ref().and_then(|e| e.total_s))
            .collect();
        let capacity = capacity.max(running.len());
        for (n, (i, start)) in waiting
            .iter()
            .zip(queue_starts(&busy, capacity, &totals))
            .enumerate()
        {
            starts.insert(*i, start);
            positions.insert(*i, n + 1);
        }
    }

    let mut view = View {
        machines: states.iter().map(machine_card).collect(),
        ..View::default()
    };
    for (i, e) in list.iter().enumerate() {
        if hidden.contains(&i) {
            continue;
        }
        let job = e.job;
        let m = e.machine;
        let laptop = laptop_of.get(&i).map(|l| &list[*l]);
        let offload_note = match laptop {
            Some(l) => Some(offload_note(l.job, &ctx.label(l.machine))),
            None if is_offload(job) && states[m].local => {
                Some("offload client; the worker's job is not visible (worker offline or not linked yet)".into())
            }
            None => None,
        };
        let machine = match laptop {
            None if is_offload(job) && states[m].local => format!("{} ➡ worker", ctx.label(m)),
            _ => ctx.label(m),
        };
        // For a pair, the buttons act on the laptop's offload client job.
        let (cancel, logs) = match laptop {
            Some(l) => (
                Target::Local {
                    id: l.job.id,
                    hover: format!(
                        "Cancels the laptop offload client #{} (worker job #{} stops only if the client stops it)",
                        l.job.id, job.id
                    ),
                },
                Target::Local {
                    id: l.job.id,
                    hover: format!("Log of the laptop offload client #{}", l.job.id),
                },
            ),
            None => (
                ctx.local_target(m, job, "Cancel"),
                ctx.local_target(m, job, "Log of"),
            ),
        };
        match class(job) {
            Class::Running => {
                let mut card =
                    running_card(&ctx, m, job, key(e), machine, offload_note, cancel, logs);
                card.via = laptop.map(|l| l.job.id);
                view.running.push(card);
            }
            c @ (Class::Waiting | Class::Proposal) => {
                let proposal = c == Class::Proposal;
                let waiting_for = job.queue.as_ref().and_then(|q| q.waiting_for.clone());
                let waiting_text = if proposal {
                    format!("awaiting approval on {}", ctx.label(m))
                } else {
                    match (&waiting_for, job.phase.as_deref()) {
                        (Some(w), _) => format!("waits for {w}"),
                        (None, Some("approved")) => {
                            "approved; waits for the worker to start".into()
                        }
                        (None, Some("preparing")) => "preparing".into(),
                        (None, _) => "waiting".into(),
                    }
                };
                let start_text = if proposal {
                    "starts after approval".into()
                } else {
                    match starts.get(&i).copied().flatten() {
                        Some(s) if s > 30.0 => format!("starts in ~{}", minutes(s)),
                        Some(_) => match &waiting_for {
                            Some(w) if !w.contains("slot") => format!("starts when {w} clears"),
                            _ => "starts soon".into(),
                        },
                        None => "start time unknown".into(),
                    }
                };
                view.queued.push(QueuedRow {
                    key: key(e),
                    via: laptop.map(|l| l.job.id),
                    position: positions.get(&i).copied().unwrap_or(0),
                    title: title(job),
                    agent: agent(job),
                    warmth: Warmth::parse(job.warmth.as_deref()),
                    warmth_reason: warmth_reason(job),
                    owner: job.owner.clone().unwrap_or_else(|| "unknown".into()),
                    command: command(job),
                    machine,
                    waiting_text,
                    start_text,
                    stale: ctx.stale(m),
                    proposal,
                    local: states[m].local,
                    cancel,
                    logs,
                    details: details(job),
                });
            }
            Class::Finished(outcome) => {
                let mut row =
                    recent_row(&ctx, m, job, key(e), machine, outcome, offload_note, logs);
                row.via = laptop.map(|l| l.job.id);
                view.recent.push(row);
            }
        }
    }
    view.running.sort_by(|a, b| a.key.cmp(&b.key));
    view.queued.sort_by(|a, b| {
        (a.proposal, &a.machine, a.position, a.key.id)
            .cmp(&(b.proposal, &b.machine, b.position, b.key.id))
    });
    view.recent.sort_by(|a, b| {
        b.finished_at
            .unwrap_or(0.0)
            .total_cmp(&a.finished_at.unwrap_or(0.0))
            .then(b.key.id.cmp(&a.key.id))
    });
    view.summary = summary(states, &view);
    view
}

/// The line above the machine cards.
pub fn summary(states: &[MachineState], view: &View) -> String {
    let mut agents: Vec<&str> = view
        .running
        .iter()
        .map(|r| r.agent.as_str())
        .chain(
            view.queued
                .iter()
                .filter(|q| !q.proposal)
                .map(|q| q.agent.as_str()),
        )
        .collect();
    agents.sort();
    agents.dedup();
    let online: Vec<_> = states
        .iter()
        .filter(|s| online(s) == Presence::Online)
        .filter_map(|s| s.snapshot.as_ref())
        .collect();
    let busy: Option<f64> = online.iter().map(|s| s.machine.cpu_busy_cores).sum();
    let waiting = view.queued.iter().filter(|q| !q.proposal).count();
    let cores = match busy {
        Some(b) if !online.is_empty() => format!(
            "{b:.1} cores busy across {} machine{}",
            online.len(),
            if online.len() == 1 { "" } else { "s" }
        ),
        _ if online.is_empty() => "no machine online".into(),
        _ => "cores busy unknown".into(),
    };
    format!(
        "{} agent{} active · {} running · {waiting} queued · {cores}",
        agents.len(),
        if agents.len() == 1 { "" } else { "s" },
        view.running.len()
    )
}

fn offload_note(laptop: &Job, laptop_label: &str) -> String {
    let o = laptop.offload.clone().unwrap_or_default();
    let secs = |v: Option<f64>| {
        v.map(|v| format!("{v:.1} s"))
            .unwrap_or_else(|| "pending".into())
    };
    format!(
        "offloaded from {laptop_label} #{}{} · sync {} · fetch {}",
        laptop.id,
        o.transport.map(|t| format!(" via {t}")).unwrap_or_default(),
        secs(o.sync_s),
        secs(o.fetch_s)
    )
}

#[allow(clippy::too_many_arguments)]
fn running_card(
    ctx: &Ctx,
    m: usize,
    job: &Job,
    key: JobKey,
    machine: String,
    offload_note: Option<String>,
    cancel: Target,
    logs: Target,
) -> RunningCard {
    let snapshot = ctx.states[m].snapshot.as_ref();
    let cpus = snapshot
        .and_then(|s| s.machine.logical_cpus)
        .map(|n| n as f64);
    let live = job.live.clone().unwrap_or_default();
    let history = live.history.clone().unwrap_or_default();
    let progress =
        job.progress
            .as_ref()
            .and_then(|p| match (p.crates_compiled, p.crates_expected) {
                (Some(done), Some(total)) if total > 0 => Some((
                    Some((done as f32 / total as f32).clamp(0.0, 1.0)),
                    format!("{done}/{total} crates"),
                )),
                (Some(done), _) => Some((None, format!("{done} crates (total unknown)"))),
                _ => None,
            });
    let elapsed_text = match (ctx.now(m), job.started_at) {
        (Some(now), Some(start)) => format!("{} elapsed", clock(now - start)),
        _ => "elapsed unknown".into(),
    };
    let estimate = job.estimate.as_ref();
    let samples = estimate.and_then(|e| e.samples);
    let (eta_text, eta_basis) = match ctx.remaining(m, job) {
        Some(r) => (
            match samples {
                Some(n) => format!(
                    "{} ({n} similar run{})",
                    left(r),
                    if n == 1 { "" } else { "s" }
                ),
                None => left(r),
            },
            estimate
                .and_then(|e| e.basis.clone())
                .unwrap_or_else(|| "basis not reported".into()),
        ),
        None => (
            "ETA unknown".into(),
            "No finished runs of this kind on this machine yet.".into(),
        ),
    };
    RunningCard {
        key,
        via: None,
        title: title(job),
        agent: agent(job),
        owner: job.owner.clone().unwrap_or_else(|| "unknown".into()),
        command: command(job),
        machine,
        warmth: Warmth::parse(job.warmth.as_deref()),
        warmth_reason: warmth_reason(job),
        cpu_text: live
            .cpu_cores
            .map(cores)
            .unwrap_or_else(|| "CPU unknown".into()),
        rss_text: live
            .rss_bytes
            .map(|b| gb(b as f64))
            .unwrap_or_else(|| "RAM unknown".into()),
        cpu_history: normalise(&history.cpu_cores, cpus),
        rss_history: normalise(&history.rss_bytes, None),
        progress,
        elapsed_text,
        eta_text: match ctx.stale(m) {
            Some(_) => format!("{eta_text} (at last contact)"),
            None => eta_text,
        },
        eta_basis,
        offload_note,
        stale: ctx.stale(m),
        cancel,
        logs,
        details: details(job),
    }
}

#[allow(clippy::too_many_arguments)]
fn recent_row(
    ctx: &Ctx,
    m: usize,
    job: &Job,
    key: JobKey,
    machine: String,
    outcome: Outcome,
    offload_note: Option<String>,
    logs: Target,
) -> RecentRow {
    let result = job.result.clone().unwrap_or_default();
    let wall = result.wall_s.or(match (job.started_at, job.finished_at) {
        (Some(s), Some(f)) => Some(f - s),
        _ => None,
    });
    let warmth = result.warmth.as_deref().or(job.warmth.as_deref());
    let wall_text = wall.map(clock).unwrap_or_else(|| "unknown".into());
    let finished_text = match (ctx.now(m), job.finished_at) {
        (Some(now), Some(f)) => ago(now - f),
        _ => "unknown".into(),
    };
    let mut parts = vec![wall.map(clock).unwrap_or_else(|| "time unknown".into())];
    if let Some(w @ ("warm" | "cold")) = warmth {
        parts.push(w.into());
    }
    if outcome == Outcome::Failed
        && let Some(code) = result.exit_code
    {
        parts.push(format!("exit {code}"));
    }
    if let (Some(now), Some(f)) = (ctx.now(m), job.finished_at) {
        parts.push(ago(now - f));
    }
    if let Some(note) = offload_note {
        parts.push(note);
    }
    RecentRow {
        key,
        via: None,
        outcome,
        title: title(job),
        agent: agent(job),
        warmth: Warmth::parse(warmth),
        wall_text,
        finished_text,
        owner: job.owner.clone().unwrap_or_else(|| "unknown".into()),
        command: command(job),
        machine,
        summary: parts.join(" · "),
        finished_at: job.finished_at,
        logs,
        details: details(job),
    }
}

/// Recent rows matching the owner substring (case-insensitive) and machine
/// label (`None` = all); cancelled jobs only when asked for.
pub fn filter_recent<'a>(
    rows: &'a [RecentRow],
    owner: &str,
    machine: Option<&str>,
    show_cancelled: bool,
) -> Vec<&'a RecentRow> {
    let owner = owner.trim().to_lowercase();
    rows.iter()
        .filter(|r| show_cancelled || r.outcome != Outcome::Cancelled)
        .filter(|r| owner.is_empty() || r.owner.to_lowercase().contains(&owner))
        .filter(|r| machine.is_none_or(|m| r.machine == m))
        .collect()
}
