//! The machine policy: one TOML file of limits for this machine, with environment overrides.
//!
//! `<config dir>/machine-policy.toml`. A missing file means the documented defaults; an
//! environment variable of the daemon wins over the file; a bad file (unknown key, wrong type,
//! out-of-range value, inconsistent pair) is refused as a whole, loudly, naming each field, and
//! the daemon runs on the safe defaults (R-218, R-219). Every value reports its source.
//!
//! The keys are a flat table ([`FIELDS`]); the TOML sections are just the dotted prefixes, so
//! `slots.compile = 2` is `[slots] compile = 2`.

use crate::api::{SettingView, Source};
use crate::peers_config::{self, PeerConfig};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use tasks_admission::{Gate, Layout, Thresholds};
use tasks_core::{DeviceId, SlotKind};
use tasks_ledger::RetentionPolicy;
use tasks_platform::{Level, PressureThresholds};
use tasks_policy::submit_rules::BroadTestMode;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// The type and range of one setting.
#[derive(Debug, Clone, Copy)]
enum Ty {
    Uint(u64, u64),
    Float(f64, f64),
    /// A float in range, or `off`.
    OptFloat(f64, f64),
    /// An integer in range, or `off`.
    OptUint(u64, u64),
    /// `normal`, `warn` or `critical`.
    Level,
    /// Comma-separated words (an array of strings in the file).
    List,
    /// One of a fixed set of words.
    Choice(&'static [&'static str]),
}

/// One setting: key, environment variable, type and default.
struct Field {
    key: &'static str,
    env: &'static str,
    ty: Ty,
    default: &'static str,
    /// Default on macOS when it differs (CPU and I/O pressure gates are off there).
    default_macos: Option<&'static str>,
}

const fn f(key: &'static str, env: &'static str, ty: Ty, default: &'static str) -> Field {
    Field {
        key,
        env,
        ty,
        default,
        default_macos: None,
    }
}

const fn fm(
    key: &'static str,
    env: &'static str,
    ty: Ty,
    default: &'static str,
    mac: &'static str,
) -> Field {
    Field {
        key,
        env,
        ty,
        default,
        default_macos: Some(mac),
    }
}

/// Every setting. The committed sample lists exactly these keys with their defaults.
const FIELDS: &[Field] = &[
    f("slots.compile", "TASKS_COMPILE_SLOTS", Ty::Uint(1, 8), "2"),
    f("slots.run", "TASKS_RUN_SLOTS", Ty::Uint(1, 16), "4"),
    f("slots.offload", "TASKS_OFFLOAD_SLOTS", Ty::Uint(1, 16), "4"),
    f(
        "slots.transfer",
        "TASKS_TRANSFER_SLOTS",
        Ty::Uint(1, 4),
        "1",
    ),
    f(
        "slots.short_lane",
        "TASKS_SHORT_LANE_SLOTS",
        Ty::Uint(0, 4),
        "1",
    ),
    f(
        "slots.short_job_max_secs",
        "TASKS_SHORT_JOB_MAX_SECS",
        Ty::Float(1.0, 600.0),
        "60",
    ),
    f(
        "slots.short_job_max_memory_mib",
        "TASKS_SHORT_JOB_MAX_MEMORY_MIB",
        Ty::Uint(16, 1_048_576),
        "1024",
    ),
    f(
        "queue.aging_mins",
        "TASKS_QUEUE_AGING_MINS",
        Ty::OptUint(1, 10_080),
        "20",
    ),
    fm(
        "pressure.compile.max_cpu",
        "TASKS_COMPILE_MAX_CPU",
        Ty::OptFloat(0.0, 100.0),
        "40",
        "off",
    ),
    fm(
        "pressure.compile.max_io",
        "TASKS_COMPILE_MAX_IO",
        Ty::OptFloat(0.0, 100.0),
        "40",
        "off",
    ),
    f(
        "pressure.compile.max_memory",
        "TASKS_COMPILE_MAX_MEMORY",
        Ty::Level,
        "normal",
    ),
    fm(
        "pressure.run.max_cpu",
        "TASKS_RUN_MAX_CPU",
        Ty::OptFloat(0.0, 100.0),
        "80",
        "off",
    ),
    fm(
        "pressure.run.max_io",
        "TASKS_RUN_MAX_IO",
        Ty::OptFloat(0.0, 100.0),
        "80",
        "off",
    ),
    f(
        "pressure.run.max_memory",
        "TASKS_RUN_MAX_MEMORY",
        Ty::Level,
        "normal",
    ),
    f(
        "pressure.offload.max_memory",
        "TASKS_OFFLOAD_MAX_MEMORY",
        Ty::Level,
        "normal",
    ),
    fm(
        "pressure.transfer.max_io",
        "TASKS_TRANSFER_MAX_IO",
        Ty::OptFloat(0.0, 100.0),
        "40",
        "off",
    ),
    fm(
        "attribution.trip_io_pct",
        "TASKS_TRIP_IO_PCT",
        Ty::OptFloat(0.0, 100.0),
        "10",
        "off",
    ),
    fm(
        "attribution.trip_cpu_pct",
        "TASKS_TRIP_CPU_PCT",
        Ty::OptFloat(0.0, 100.0),
        "60",
        "off",
    ),
    f(
        "attribution.trip_interval_secs",
        "TASKS_TRIP_INTERVAL_SECS",
        Ty::Uint(1, 86_400),
        "60",
    ),
    f(
        "attribution.sample_secs",
        "TASKS_ATTRIB_SAMPLE_SECS",
        Ty::Uint(1, 600),
        "5",
    ),
    f(
        "memory.min_available_gib",
        "TASKS_MIN_AVAILABLE_GIB",
        Ty::Float(0.0, 4096.0),
        "2",
    ),
    f(
        "memory.some_warn",
        "TASKS_MEMORY_SOME_WARN",
        Ty::Float(0.0, 100.0),
        "10",
    ),
    f(
        "memory.some_critical",
        "TASKS_MEMORY_SOME_CRITICAL",
        Ty::Float(0.0, 100.0),
        "40",
    ),
    f(
        "memory.full_warn",
        "TASKS_MEMORY_FULL_WARN",
        Ty::Float(0.0, 100.0),
        "5",
    ),
    f(
        "memory.full_critical",
        "TASKS_MEMORY_FULL_CRITICAL",
        Ty::Float(0.0, 100.0),
        "20",
    ),
    f(
        "disk.cache_reserve_gib",
        "TASKS_CACHE_RESERVE_GIB",
        Ty::Float(0.0, 100_000.0),
        "20",
    ),
    f(
        "disk.state_reserve_gib",
        "TASKS_STATE_RESERVE_GIB",
        Ty::Float(0.0, 100_000.0),
        "1",
    ),
    f(
        "cache.sccache",
        "TASKS_SCCACHE",
        Ty::Choice(&["auto", "off"]),
        "auto",
    ),
    f(
        "cache.sccache_size_gib",
        "TASKS_SCCACHE_SIZE_GIB",
        Ty::Uint(1, 100_000),
        "30",
    ),
    f(
        "runner.cancel_grace_ms",
        "TASKS_CANCEL_GRACE_MS",
        Ty::Uint(100, 30_000),
        "300",
    ),
    f(
        "runner.wall_timeout_secs",
        "TASKS_WALL_TIMEOUT_SECS",
        Ty::OptUint(1, 604_800),
        "off",
    ),
    f(
        "runner.idle_timeout_secs",
        "TASKS_IDLE_TIMEOUT_SECS",
        Ty::OptUint(1, 604_800),
        "off",
    ),
    f(
        "logs.soft_cap_mib",
        "TASKS_LOG_SOFT_CAP_MIB",
        Ty::Uint(1, 65_536),
        "64",
    ),
    f(
        "logs.hard_cap_mib",
        "TASKS_LOG_HARD_CAP_MIB",
        Ty::Uint(1, 1_048_576),
        "1024",
    ),
    f(
        "retention.max_finished_jobs",
        "TASKS_RETAIN_JOBS",
        Ty::OptUint(10, 10_000_000),
        "20000",
    ),
    f(
        "retention.max_job_age_days",
        "TASKS_RETAIN_JOB_DAYS",
        Ty::OptUint(1, 3650),
        "60",
    ),
    f(
        "retention.max_sample_age_days",
        "TASKS_RETAIN_SAMPLE_DAYS",
        Ty::OptUint(1, 3650),
        "365",
    ),
    f(
        "retention.max_samples_per_key",
        "TASKS_RETAIN_SAMPLES_PER_KEY",
        Ty::OptUint(1, 100_000),
        "200",
    ),
    f(
        "sync.idle_days",
        "TASKS_SYNC_IDLE_DAYS",
        Ty::Uint(1, 3650),
        "7",
    ),
    f("sync.gc_days", "TASKS_SYNC_GC_DAYS", Ty::Uint(1, 365), "7"),
    f(
        "audit.retention_days",
        "TASKS_AUDIT_RETENTION_DAYS",
        Ty::OptUint(1, 3650),
        "90",
    ),
    f(
        "audit.checkpoint_every",
        "TASKS_AUDIT_CHECKPOINT_EVERY",
        Ty::Uint(0, 1_000_000),
        "100",
    ),
    f(
        "cargo.max_jobs",
        "TASKS_MAX_CARGO_JOBS",
        Ty::Uint(1, 16),
        "2",
    ),
    f(
        "cargo.jobs_when_busy",
        "TASKS_CARGO_JOBS_WHEN_BUSY",
        Ty::Uint(1, 16),
        "1",
    ),
    f(
        "stuck.seconds",
        "TASKS_STUCK_JOB_SECONDS",
        Ty::Uint(30, 3600),
        "300",
    ),
    f(
        "alerts.slot_grace_secs",
        "TASKS_ALERT_SLOT_GRACE_SECS",
        Ty::Uint(0, 86_400),
        "120",
    ),
    f(
        "alerts.memory_secs",
        "TASKS_ALERT_MEMORY_SECS",
        Ty::Uint(0, 86_400),
        "60",
    ),
    f(
        "alerts.peer_grace_secs",
        "TASKS_ALERT_PEER_GRACE_SECS",
        Ty::Uint(0, 86_400),
        "120",
    ),
    f(
        "alerts.audit_secs",
        "TASKS_AUDIT_SECS",
        Ty::Uint(5, 86_400),
        "300",
    ),
    f(
        "daemon.tick_ms",
        "TASKS_TICK_MS",
        Ty::Uint(50, 60_000),
        "1000",
    ),
    f(
        "daemon.nofile_target",
        "TASKS_NOFILE_TARGET",
        Ty::Uint(256, 1_048_576),
        "65536",
    ),
    f(
        "submit.broad_test",
        "TASKS_BROAD_TEST",
        Ty::Choice(&["refuse", "warn"]),
        "refuse",
    ),
    f(
        "placement.peer_wait_secs",
        "TASKS_PEER_WAIT_SECS",
        Ty::Uint(0, 86_400),
        "600",
    ),
    f(
        "power.keep_awake",
        "TASKS_KEEP_AWAKE",
        Ty::Choice(&["on", "off"]),
        "on",
    ),
    f(
        "power.keep_awake_linger_secs",
        "TASKS_KEEP_AWAKE_LINGER_SECS",
        Ty::Uint(0, 3600),
        "120",
    ),
    f(
        "power.peer_jobs_on_battery",
        "TASKS_PEER_JOBS_ON_BATTERY",
        Ty::Choice(&["false", "true"]),
        "false",
    ),
    f("machine.devices", "TASKS_DEVICES", Ty::List, ""),
    f("machine.tools", "TASKS_TOOLS", Ty::List, ""),
];

/// A parsed value.
#[derive(Debug, Clone, PartialEq)]
enum Val {
    U(u64),
    F(f64),
    OptF(Option<f64>),
    OptU(Option<u64>),
    Level(Level),
    List(Vec<String>),
    Choice(String),
}

impl Val {
    fn text(&self) -> String {
        match self {
            Val::U(v) => v.to_string(),
            Val::F(v) => v.to_string(),
            Val::OptF(Some(v)) => v.to_string(),
            Val::OptU(Some(v)) => v.to_string(),
            Val::OptF(None) | Val::OptU(None) => "off".to_string(),
            Val::Level(l) => level_name(*l).to_string(),
            Val::List(v) => v.join(","),
            Val::Choice(w) => w.clone(),
        }
    }
}

fn level_name(l: Level) -> &'static str {
    match l {
        Level::Normal => "normal",
        Level::Warn => "warn",
        Level::Critical => "critical",
    }
}

fn parse_val(ty: Ty, text: &str) -> Result<Val, String> {
    let t = text.trim();
    match ty {
        Ty::Uint(lo, hi) => {
            let v: u64 = t
                .parse()
                .map_err(|_| format!("{t:?} is not a whole number"))?;
            if (lo..=hi).contains(&v) {
                Ok(Val::U(v))
            } else {
                Err(format!("{v} is outside {lo}..={hi}"))
            }
        }
        Ty::Float(lo, hi) => parse_float(t, lo, hi).map(Val::F),
        Ty::OptFloat(lo, hi) => {
            if t == "off" {
                Ok(Val::OptF(None))
            } else {
                parse_float(t, lo, hi).map(|v| Val::OptF(Some(v)))
            }
        }
        Ty::OptUint(lo, hi) => {
            if t == "off" {
                return Ok(Val::OptU(None));
            }
            let v: u64 = t
                .parse()
                .map_err(|_| format!("{t:?} is not a whole number or `off`"))?;
            if (lo..=hi).contains(&v) {
                Ok(Val::OptU(Some(v)))
            } else {
                Err(format!("{v} is outside {lo}..={hi} (or `off`)"))
            }
        }
        Ty::Level => match t {
            "normal" => Ok(Val::Level(Level::Normal)),
            "warn" => Ok(Val::Level(Level::Warn)),
            "critical" => Ok(Val::Level(Level::Critical)),
            other => Err(format!("{other:?} is not normal, warn or critical")),
        },
        Ty::Choice(words) => {
            if words.contains(&t) {
                Ok(Val::Choice(t.to_string()))
            } else {
                Err(format!("{t:?} is not {}", words.join(" or ")))
            }
        }
        Ty::List => {
            let items: Vec<String> = t
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if items.iter().any(|i| i.contains(char::is_whitespace)) {
                return Err("list items cannot contain spaces".to_string());
            }
            Ok(Val::List(items))
        }
    }
}

fn parse_float(t: &str, lo: f64, hi: f64) -> Result<f64, String> {
    let v: f64 = t.parse().map_err(|_| format!("{t:?} is not a number"))?;
    if v.is_finite() && v >= lo && v <= hi {
        Ok(v)
    } else {
        Err(format!("{t} is outside {lo}..={hi}"))
    }
}

fn default_text(fl: &Field) -> &'static str {
    if cfg!(target_os = "macos") {
        fl.default_macos.unwrap_or(fl.default)
    } else {
        fl.default
    }
}

fn field(key: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.key == key)
}

/// Flattens a TOML table into dotted keys with their value as text.
fn flatten(prefix: &str, table: &toml::Table, out: &mut Vec<(String, Result<String, String>)>) {
    for (k, v) in table {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            toml::Value::Table(t) => flatten(&key, t, out),
            toml::Value::Integer(i) => out.push((key, Ok(i.to_string()))),
            toml::Value::Float(x) => out.push((key, Ok(x.to_string()))),
            toml::Value::String(s) => out.push((key, Ok(s.clone()))),
            toml::Value::Boolean(b) => out.push((key, Ok(b.to_string()))),
            toml::Value::Array(a) => {
                let items: Option<Vec<&str>> = a.iter().map(toml::Value::as_str).collect();
                match items {
                    Some(v) => out.push((key, Ok(v.join(",")))),
                    None => out.push((key, Err("must be an array of strings".to_string()))),
                }
            }
            other => out.push((key, Err(format!("unsupported {} value", other.type_str())))),
        }
    }
}

/// The effective machine policy.
#[derive(Debug, Clone)]
pub struct MachinePolicy {
    vals: BTreeMap<&'static str, Val>,
    sources: BTreeMap<&'static str, Source>,
    /// Problems found in the file or the environment, each naming its field.
    pub problems: Vec<String>,
    /// True when a file was found and used.
    pub file_loaded: bool,
    peers: PeerConfig,
}

impl MachinePolicy {
    /// The documented defaults.
    pub fn defaults() -> MachinePolicy {
        let mut vals = BTreeMap::new();
        let mut sources = BTreeMap::new();
        for fl in FIELDS {
            if let Ok(v) = parse_val(fl.ty, default_text(fl)) {
                vals.insert(fl.key, v);
                sources.insert(fl.key, Source::Default);
            }
        }
        MachinePolicy {
            vals,
            sources,
            problems: Vec::new(),
            file_loaded: false,
            peers: PeerConfig::default(),
        }
    }

    /// Reads `path` (a missing file is fine) and applies `env` over it.
    pub fn load(path: &Path, env: &dyn Fn(&str) -> Option<String>) -> MachinePolicy {
        match std::fs::read_to_string(path) {
            Ok(text) => MachinePolicy::from_text(Some(&text), env),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                MachinePolicy::from_text(None, env)
            }
            Err(e) => {
                let mut p = MachinePolicy::from_text(None, env);
                p.problems.push(format!(
                    "{}: cannot read: {e}; using defaults",
                    path.display()
                ));
                p
            }
        }
    }

    /// Builds the policy from file text and an environment.
    pub fn from_text(file: Option<&str>, env: &dyn Fn(&str) -> Option<String>) -> MachinePolicy {
        let mut policy = MachinePolicy::defaults();
        if let Some(text) = file {
            match policy.apply_file(text) {
                Ok(candidate) => {
                    policy = candidate;
                    policy.file_loaded = true;
                }
                Err(problems) => {
                    policy.problems.extend(problems);
                    policy
                        .problems
                        .push("machine policy file refused as a whole; using safe defaults".into());
                }
            }
        }
        policy.apply_env(env);
        policy
    }

    fn apply_file(&self, text: &str) -> Result<MachinePolicy, Vec<String>> {
        let mut table: toml::Table =
            toml::from_str(text).map_err(|e| vec![format!("syntax: {e}")])?;
        let mut next = self.clone();
        let mut problems = Vec::new();
        // `[peers]` holds tables of its own, so it is parsed apart from the flat keys
        if let Some(peers) = table.remove("peers") {
            let (cfg, p) = PeerConfig::parse(&peers);
            next.peers = cfg;
            problems.extend(p);
        }
        let mut flat = Vec::new();
        flatten("", &table, &mut flat);
        for (key, value) in flat {
            let Some(fl) = field(&key) else {
                problems.push(format!("{key}: unknown field"));
                continue;
            };
            let parsed = value.and_then(|t| parse_val(fl.ty, &t));
            match parsed {
                Ok(v) => {
                    next.vals.insert(fl.key, v);
                    next.sources.insert(fl.key, Source::File);
                }
                Err(e) => problems.push(format!("{key}: {e}")),
            }
        }
        problems.extend(next.cross_check());
        if problems.is_empty() {
            Ok(next)
        } else {
            Err(problems)
        }
    }

    fn apply_env(&mut self, env: &dyn Fn(&str) -> Option<String>) {
        let before = self.clone();
        let mut problems = Vec::new();
        for fl in FIELDS {
            let Some(raw) = env(fl.env) else { continue };
            match parse_val(fl.ty, &raw) {
                Ok(v) => {
                    self.vals.insert(fl.key, v);
                    self.sources.insert(fl.key, Source::Env);
                }
                Err(e) => problems.push(format!("{} ({}): {e}; ignored", fl.key, fl.env)),
            }
        }
        let cross = self.cross_check();
        if !cross.is_empty() {
            *self = before;
            problems.extend(
                cross
                    .into_iter()
                    .map(|c| format!("environment: {c}; ignored")),
            );
        }
        self.problems.extend(problems);
    }

    /// Checks the pairs that must agree.
    fn cross_check(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.psi().validate().is_err() {
            out.push("memory.*_warn must not exceed its *_critical".to_string());
        }
        if self.u("logs.hard_cap_mib") < self.u("logs.soft_cap_mib") {
            out.push("logs.hard_cap_mib must be at least logs.soft_cap_mib".to_string());
        }
        if self.u("cargo.jobs_when_busy") > self.u("cargo.max_jobs") {
            out.push("cargo.jobs_when_busy must not exceed cargo.max_jobs".to_string());
        }
        out
    }

    fn u(&self, key: &str) -> u64 {
        match self.vals.get(key) {
            Some(Val::U(v)) => *v,
            _ => 0,
        }
    }
    fn fl(&self, key: &str) -> f64 {
        match self.vals.get(key) {
            Some(Val::F(v)) => *v,
            _ => 0.0,
        }
    }
    fn of(&self, key: &str) -> Option<f64> {
        match self.vals.get(key) {
            Some(Val::OptF(v)) => *v,
            _ => None,
        }
    }
    fn ou(&self, key: &str) -> Option<u64> {
        match self.vals.get(key) {
            Some(Val::OptU(v)) => *v,
            _ => None,
        }
    }
    fn lvl(&self, key: &str) -> Level {
        match self.vals.get(key) {
            Some(Val::Level(l)) => *l,
            _ => Level::Normal,
        }
    }
    fn list(&self, key: &str) -> Vec<String> {
        match self.vals.get(key) {
            Some(Val::List(l)) => l.clone(),
            _ => Vec::new(),
        }
    }

    /// Where a key's value came from.
    pub fn source_of(&self, key: &str) -> Option<Source> {
        self.sources.get(key).copied()
    }

    /// Compile slots (1..=8, R-029).
    pub fn compile_slots(&self) -> u32 {
        u32::try_from(self.u("slots.compile")).unwrap_or(2)
    }
    /// Slots of a kind.
    pub fn slot_count(&self, kind: SlotKind) -> u32 {
        let key = match kind {
            SlotKind::Compile => "slots.compile",
            SlotKind::Run => "slots.run",
            SlotKind::Offload => "slots.offload",
            SlotKind::Transfer => "slots.transfer",
            SlotKind::Device => {
                return u32::try_from(self.list("machine.devices").len()).unwrap_or(0);
            }
        };
        u32::try_from(self.u(key)).unwrap_or(1)
    }
    /// Machine I/O pressure (percent, avg10) at or above which a pressure trip is recorded;
    /// `None` turns the trip off.
    pub fn trip_io_pct(&self) -> Option<f32> {
        self.of("attribution.trip_io_pct").map(|v| v as f32)
    }
    /// Machine CPU pressure (percent, avg10) at or above which a pressure trip is recorded.
    pub fn trip_cpu_pct(&self) -> Option<f32> {
        self.of("attribution.trip_cpu_pct").map(|v| v as f32)
    }
    /// Shortest time between two trips of the same kind; the rest are counted.
    pub fn trip_interval_secs(&self) -> u64 {
        self.u("attribution.trip_interval_secs")
    }
    /// How often the outside-process window is sampled.
    pub fn attrib_sample_secs(&self) -> u64 {
        self.u("attribution.sample_secs")
    }
    /// Device serials attached to this machine, in lock-index order.
    pub fn devices(&self) -> Vec<String> {
        self.list("machine.devices")
    }
    /// Extra tools this machine declares.
    pub fn tools(&self) -> Vec<String> {
        self.list("machine.tools")
    }
    /// The slot layout admission works on.
    pub fn layout(&self) -> Layout {
        let mut l = Layout::default();
        for k in [
            SlotKind::Compile,
            SlotKind::Run,
            SlotKind::Offload,
            SlotKind::Transfer,
        ] {
            l.counts.insert(k, self.slot_count(k));
        }
        for (i, d) in self.devices().into_iter().enumerate() {
            l.devices
                .insert(DeviceId(d), u32::try_from(i).unwrap_or(u32::MAX));
        }
        l
    }
    /// The admission thresholds. On macOS the CPU and I/O limits default to off.
    pub fn thresholds(&self) -> Thresholds {
        let gate = |p: &str| Gate {
            max_cpu: self.of(&format!("pressure.{p}.max_cpu")).map(|v| v as f32),
            max_io: self.of(&format!("pressure.{p}.max_io")).map(|v| v as f32),
            max_memory: self.lvl(&format!("pressure.{p}.max_memory")),
        };
        Thresholds {
            min_available_bytes: (self.fl("memory.min_available_gib") * GIB as f64) as u64,
            compile: gate("compile"),
            run: gate("run"),
            offload: Gate {
                max_cpu: None,
                max_io: None,
                max_memory: self.lvl("pressure.offload.max_memory"),
            },
            transfer: Gate {
                max_cpu: None,
                max_io: self.of("pressure.transfer.max_io").map(|v| v as f32),
                max_memory: self.lvl("pressure.run.max_memory"),
            },
            cache_reserve_bytes: (self.fl("disk.cache_reserve_gib") * GIB as f64) as u64,
            state_reserve_bytes: (self.fl("disk.state_reserve_gib") * GIB as f64) as u64,
            short_job_max_secs: self.fl("slots.short_job_max_secs"),
            short_job_max_memory_bytes: self.u("slots.short_job_max_memory_mib") * MIB,
            short_job_lane_slots: u32::try_from(self.u("slots.short_lane")).unwrap_or(0),
            aging_ms: self.ou("queue.aging_mins").map_or(0, |m| m * 60_000),
        }
    }
    /// PSI memory thresholds.
    pub fn psi(&self) -> PressureThresholds {
        PressureThresholds {
            memory_some_warn: self.fl("memory.some_warn") as f32,
            memory_some_critical: self.fl("memory.some_critical") as f32,
            memory_full_warn: self.fl("memory.full_warn") as f32,
            memory_full_critical: self.fl("memory.full_critical") as f32,
        }
    }
    /// Cancel grace.
    pub fn cancel_grace(&self) -> Duration {
        Duration::from_millis(self.u("runner.cancel_grace_ms"))
    }
    /// Default wall-clock timeout of a job.
    pub fn wall_timeout(&self) -> Option<Duration> {
        self.ou("runner.wall_timeout_secs").map(Duration::from_secs)
    }
    /// Default output-idle timeout of a job.
    pub fn idle_timeout(&self) -> Option<Duration> {
        self.ou("runner.idle_timeout_secs").map(Duration::from_secs)
    }
    /// Log size at which the finished log is elided to head and tail.
    pub fn log_soft_cap_bytes(&self) -> u64 {
        self.u("logs.soft_cap_mib") * MIB
    }
    /// Log size at which a running job is killed.
    pub fn log_hard_cap_bytes(&self) -> u64 {
        self.u("logs.hard_cap_mib") * MIB
    }
    /// Retention rules.
    pub fn retention(&self) -> RetentionPolicy {
        let day = |d: u64| Duration::from_secs(d * 86_400);
        RetentionPolicy {
            max_job_age: self.ou("retention.max_job_age_days").map(day),
            max_finished_jobs: self
                .ou("retention.max_finished_jobs")
                .and_then(|v| usize::try_from(v).ok()),
            max_sample_age: self.ou("retention.max_sample_age_days").map(day),
            max_samples_per_key: self
                .ou("retention.max_samples_per_key")
                .and_then(|v| usize::try_from(v).ok()),
        }
    }
    /// How long a synced working copy may sit unused, and how often mirrors are collected
    /// (design §6 retention).
    pub fn sync_retention(&self) -> tasks_peer::sync::WorkerRetention {
        let day = |d: u64| Duration::from_secs(d.saturating_mul(86_400));
        tasks_peer::sync::WorkerRetention {
            idle: day(self.u("sync.idle_days")),
            gc_every: day(self.u("sync.gc_days")),
            ..tasks_peer::sync::WorkerRetention::default()
        }
    }
    /// How long audit rows are kept (design D5); `None` keeps them for good.
    pub fn audit_retention(&self) -> Option<Duration> {
        self.ou("audit.retention_days")
            .map(|d| Duration::from_secs(d.saturating_mul(86_400)))
    }
    /// Rows between signed audit checkpoints; 0 writes them only on demand.
    pub fn audit_checkpoint_every(&self) -> u64 {
        self.u("audit.checkpoint_every")
    }
    /// What to do with a broad `cargo test` without the escape (R-191): `refuse` or `warn`.
    pub fn broad_test_mode(&self) -> BroadTestMode {
        match self.vals.get("submit.broad_test") {
            Some(Val::Choice(w)) => BroadTestMode::parse(w).unwrap_or_default(),
            _ => BroadTestMode::default(),
        }
    }
    /// The `cargo -j` ceiling (R-092).
    pub fn cargo_max_jobs(&self) -> u32 {
        u32::try_from(self.u("cargo.max_jobs")).unwrap_or(2)
    }
    /// The `cargo -j` a new job gets while another compile job runs (R-093).
    pub fn cargo_jobs_when_busy(&self) -> u32 {
        u32::try_from(self.u("cargo.jobs_when_busy")).unwrap_or(1)
    }
    /// How long a job placed on a sleeping or unreachable peer waits for it (R-600).
    pub fn peer_wait_secs(&self) -> u64 {
        self.u("placement.peer_wait_secs")
    }
    /// Whether the daemon holds the machine awake while it has work (R-607).
    pub fn keep_awake(&self) -> bool {
        !matches!(self.vals.get("power.keep_awake"), Some(Val::Choice(w)) if w == "off")
    }
    /// How long the hold outlasts the last job, so back-to-back jobs do not flap (R-607).
    pub fn keep_awake_linger_secs(&self) -> u64 {
        self.u("power.keep_awake_linger_secs")
    }
    /// Whether jobs received from peers run while this machine is on battery (R-614).
    pub fn peer_jobs_on_battery(&self) -> bool {
        matches!(self.vals.get("power.peer_jobs_on_battery"), Some(Val::Choice(w)) if w == "true")
    }
    /// The stuck-job window in seconds (R-095).
    pub fn stuck_window_secs(&self) -> u64 {
        self.u("stuck.seconds")
    }
    /// Grace before a held slot with no live job alerts (R-222).
    pub fn slot_grace_secs(&self) -> u64 {
        self.u("alerts.slot_grace_secs")
    }
    /// How long memory pressure must last before it alerts (R-222).
    pub fn memory_alert_secs(&self) -> u64 {
        self.u("alerts.memory_secs")
    }
    /// Grace before a stale or offline peer alerts (R-222).
    pub fn peer_grace_secs(&self) -> u64 {
        self.u("alerts.peer_grace_secs")
    }
    /// The configured peers and this daemon's listen address.
    pub fn peers(&self) -> &PeerConfig {
        &self.peers
    }
    /// Seconds between holder audits (R-049).
    pub fn audit_secs(&self) -> u64 {
        self.u("alerts.audit_secs")
    }
    /// Scheduler tick period.
    pub fn tick(&self) -> Duration {
        Duration::from_millis(self.u("daemon.tick_ms"))
    }
    /// Open-file soft limit to ask for (R-094).
    pub fn nofile_target(&self) -> u64 {
        self.u("daemon.nofile_target")
    }
    /// Disk reserve in bytes for the cache filesystem.
    pub fn cache_reserve_bytes(&self) -> u64 {
        (self.fl("disk.cache_reserve_gib") * GIB as f64) as u64
    }
    /// Whether the machine wraps rustc in sccache when it finds one (`cache.sccache`).
    pub fn sccache_enabled(&self) -> bool {
        !matches!(self.vals.get("cache.sccache"), Some(Val::Choice(w)) if w == "off")
    }
    /// The sccache size limit, as sccache spells it (`30G`).
    pub fn sccache_size(&self) -> String {
        format!("{}G", self.u("cache.sccache_size_gib"))
    }
    /// Disk reserve in bytes for the state filesystem.
    pub fn state_reserve_bytes(&self) -> u64 {
        (self.fl("disk.state_reserve_gib") * GIB as f64) as u64
    }

    /// Every setting with its effective value and source (R-219).
    pub fn settings(&self) -> Vec<SettingView> {
        FIELDS
            .iter()
            .map(|fl| SettingView {
                key: fl.key.to_string(),
                value: self.vals.get(fl.key).map(Val::text).unwrap_or_default(),
                source: self.source_of(fl.key).unwrap_or(Source::Default),
            })
            .collect()
    }

    /// The text of a sample file listing every key at its default.
    pub fn sample_text() -> String {
        let mut out = String::from(
            "# tasks2 machine policy. Every line shows the default; uncomment to change.\n\
             # Environment variables of the daemon win over this file.\n\
             # A bad value refuses the whole file and the daemon runs on the defaults.\n",
        );
        let mut section = "";
        for fl in FIELDS {
            let (sec, name) = fl.key.rsplit_once('.').unwrap_or(("", fl.key));
            if sec != section {
                out.push_str(&format!("\n[{sec}]\n"));
                section = sec;
            }
            let d = default_text(fl);
            let rendered = match fl.ty {
                Ty::Level | Ty::OptFloat(..) | Ty::OptUint(..) if d == "off" => {
                    format!("\"{d}\"")
                }
                Ty::Level | Ty::Choice(_) => format!("\"{d}\""),
                Ty::List => "[]".to_string(),
                _ => d.to_string(),
            };
            out.push_str(&format!("# {name} = {rendered}   # env {}\n", fl.env));
        }
        out.push_str(peers_config::SAMPLE);
        out
    }
}

/// A job's wall-clock limit: its own (or its step's) limit, capped by the machine's.
/// A job may ask for less time than the machine allows, never more.
pub fn effective_wall_timeout(
    job: Option<Duration>,
    machine: Option<Duration>,
) -> Option<Duration> {
    match (job, machine) {
        (Some(j), Some(m)) => Some(j.min(m)),
        (j, m) => j.or(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r095_the_machine_wall_timeout_is_a_ceiling_for_a_jobs_own_limit() {
        let s = Duration::from_secs;
        assert_eq!(
            effective_wall_timeout(Some(s(60)), Some(s(3600))),
            Some(s(60))
        );
        assert_eq!(
            effective_wall_timeout(Some(s(7200)), Some(s(3600))),
            Some(s(3600))
        );
        assert_eq!(effective_wall_timeout(Some(s(60)), None), Some(s(60)));
        assert_eq!(effective_wall_timeout(None, Some(s(3600))), Some(s(3600)));
        assert_eq!(effective_wall_timeout(None, None), None);
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn env1(k: &'static str, v: &'static str) -> impl Fn(&str) -> Option<String> {
        move |name| (name == k).then(|| v.to_string())
    }

    // R-218
    #[test]
    fn r218_missing_file_gives_defaults_and_env_wins_over_the_file() {
        let d = MachinePolicy::from_text(None, &no_env);
        assert_eq!(d.compile_slots(), 2);
        assert!(d.problems.is_empty());
        assert_eq!(d.source_of("slots.compile"), Some(Source::Default));
        let file = "[slots]\ncompile = 3\nrun = 6\n";
        let p = MachinePolicy::from_text(Some(file), &no_env);
        assert_eq!(p.compile_slots(), 3);
        assert_eq!(p.source_of("slots.compile"), Some(Source::File));
        assert!(p.file_loaded);
        let p = MachinePolicy::from_text(Some(file), &env1("TASKS_COMPILE_SLOTS", "5"));
        assert_eq!(p.compile_slots(), 5);
        assert_eq!(p.source_of("slots.compile"), Some(Source::Env));
        assert_eq!(p.source_of("slots.run"), Some(Source::File));
    }

    // R-218, R-219
    #[test]
    fn r218_a_bad_file_falls_back_to_defaults_loudly_naming_the_field() {
        let bad = "[slots]\ncompile = 99\nrun = 6\n[pressure.compile]\nmax_cpu = \"high\"\n[nope]\nx = 1\n";
        let p = MachinePolicy::from_text(Some(bad), &no_env);
        // the whole file is refused: even the valid `run = 6` is not applied
        assert_eq!(p.slot_count(SlotKind::Run), 4);
        assert!(!p.file_loaded);
        let all = p.problems.join("\n");
        assert!(all.contains("slots.compile"), "{all}");
        assert!(all.contains("pressure.compile.max_cpu"), "{all}");
        assert!(all.contains("nope.x: unknown field"), "{all}");
        assert!(all.contains("refused as a whole"), "{all}");
        // a syntax error is refused too
        let p = MachinePolicy::from_text(Some("slots = ["), &no_env);
        assert!(p.problems.iter().any(|m| m.starts_with("syntax")));
        // a bad environment value is ignored, named, and the file value stays
        let p = MachinePolicy::from_text(
            Some("[slots]\ncompile = 3\n"),
            &env1("TASKS_COMPILE_SLOTS", "0"),
        );
        assert_eq!(p.compile_slots(), 3);
        assert!(p.problems[0].contains("TASKS_COMPILE_SLOTS"));
    }

    // R-219
    #[test]
    fn r219_values_are_bounded_and_inconsistent_pairs_are_refused() {
        let inverted = "[memory]\nsome_warn = 50\nsome_critical = 10\n";
        let p = MachinePolicy::from_text(Some(inverted), &no_env);
        assert!(p.problems.iter().any(|m| m.contains("memory.")));
        let caps = "[logs]\nsoft_cap_mib = 100\nhard_cap_mib = 10\n";
        assert!(
            MachinePolicy::from_text(Some(caps), &no_env)
                .problems
                .iter()
                .any(|m| m.contains("logs.hard_cap_mib"))
        );
        let float_as_int = "[slots]\ncompile = 2.5\n";
        assert!(
            !MachinePolicy::from_text(Some(float_as_int), &no_env)
                .problems
                .is_empty()
        );
        let grace = MachinePolicy::from_text(Some("[runner]\ncancel_grace_ms = 99\n"), &no_env);
        assert!(!grace.problems.is_empty());
        let settings = MachinePolicy::defaults().settings();
        assert_eq!(settings.len(), FIELDS.len());
        assert!(settings.iter().all(|s| s.source == Source::Default));
    }

    // R-218: the committed sample parses, and its commented-out defaults equal the defaults.
    #[test]
    fn r218_the_committed_sample_parses() {
        let committed = include_str!("../machine-policy.sample.toml");
        let p = MachinePolicy::from_text(Some(committed), &no_env);
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        assert!(p.file_loaded);
        // uncommenting every line changes nothing
        let uncommented: String = MachinePolicy::sample_text()
            .lines()
            .map(|l| {
                l.strip_prefix("# ")
                    .filter(|r| r.contains(" = "))
                    .unwrap_or(l)
            })
            .map(|l| l.split("   # env").next().unwrap_or(l).to_string() + "\n")
            .collect();
        let p = MachinePolicy::from_text(Some(&uncommented), &no_env);
        assert!(p.problems.is_empty(), "{:?}\n{uncommented}", p.problems);
        let d = MachinePolicy::defaults();
        for s in p.settings().iter().zip(d.settings().iter()) {
            assert_eq!(s.0.value, s.1.value, "{}", s.0.key);
        }
        for fl in FIELDS {
            let name = fl.key.rsplit_once('.').map_or(fl.key, |(_, n)| n);
            assert!(committed.contains(&format!("# {name} = ")), "{}", fl.key);
            assert!(committed.contains(fl.env), "{}", fl.env);
        }
    }

    #[test]
    fn macos_style_policy_can_switch_cpu_and_io_gates_off() {
        let p = MachinePolicy::from_text(
            Some("[pressure.compile]\nmax_cpu = \"off\"\nmax_io = \"off\"\n"),
            &no_env,
        );
        let t = p.thresholds();
        assert_eq!(t.compile.max_cpu, None);
        assert_eq!(t.compile.max_io, None);
        assert_eq!(t.compile.max_memory, Level::Normal);
    }

    // R-286
    #[test]
    fn r286_audit_retention_is_ninety_days_by_default_and_can_be_changed_or_turned_off() {
        let d = MachinePolicy::defaults();
        assert_eq!(d.audit_retention(), Some(Duration::from_secs(90 * 86_400)));
        assert_eq!(d.audit_checkpoint_every(), 100);
        let p = MachinePolicy::from_text(
            Some("[audit]\nretention_days = 7\ncheckpoint_every = 5\n"),
            &no_env,
        );
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        assert_eq!(p.audit_retention(), Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(p.audit_checkpoint_every(), 5);
        let off = MachinePolicy::from_text(Some("[audit]\nretention_days = \"off\"\n"), &no_env);
        assert!(off.problems.is_empty(), "{:?}", off.problems);
        assert_eq!(off.audit_retention(), None);
        let env = MachinePolicy::from_text(None, &env1("TASKS_AUDIT_RETENTION_DAYS", "30"));
        assert_eq!(
            env.audit_retention(),
            Some(Duration::from_secs(30 * 86_400))
        );
        let bad = MachinePolicy::from_text(Some("[audit]\nretention_days = 0\n"), &no_env);
        assert!(!bad.problems.is_empty());
    }

    // R-321
    #[test]
    fn r321_synced_copies_idle_for_seven_days_go_and_mirrors_are_collected_weekly() {
        let d = MachinePolicy::defaults().sync_retention();
        assert_eq!(d.idle, Duration::from_secs(7 * 86_400));
        assert_eq!(d.gc_every, Duration::from_secs(7 * 86_400));
        let p = MachinePolicy::from_text(Some("[sync]\nidle_days = 2\ngc_days = 30\n"), &no_env);
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        let r = p.sync_retention();
        assert_eq!(
            (r.idle, r.gc_every),
            (
                Duration::from_secs(2 * 86_400),
                Duration::from_secs(30 * 86_400)
            )
        );
        let env = MachinePolicy::from_text(None, &env1("TASKS_SYNC_IDLE_DAYS", "3"));
        assert_eq!(env.sync_retention().idle, Duration::from_secs(3 * 86_400));
        let bad = MachinePolicy::from_text(Some("[sync]\nidle_days = 0\n"), &no_env);
        assert!(!bad.problems.is_empty());
    }

    #[test]
    fn typed_views_follow_the_values() {
        let p = MachinePolicy::from_text(
            Some(
                "[disk]\ncache_reserve_gib = 30\n[cargo]\nmax_jobs = 8\njobs_when_busy = 2\n\
                 [machine]\ndevices = [\"A1\", \"B2\"]\n",
            ),
            &no_env,
        );
        assert_eq!(p.thresholds().cache_reserve_bytes, 30 * GIB);
        assert_eq!(p.cargo_max_jobs(), 8);
        assert_eq!(p.cargo_jobs_when_busy(), 2);
        let l = p.layout();
        assert_eq!(l.devices[&DeviceId("B2".into())], 1);
        assert_eq!(l.counts[&SlotKind::Compile], 2);
        assert_eq!(
            p.retention().max_job_age,
            Some(Duration::from_secs(60 * 86_400))
        );
        // R-092: busy lowering cannot exceed the ceiling
        let bad =
            MachinePolicy::from_text(Some("[cargo]\nmax_jobs = 2\njobs_when_busy = 4\n"), &no_env);
        assert!(bad.problems.iter().any(|m| m.contains("jobs_when_busy")));
    }

    // R-174 (superseded by R-257): the `[peers]` section is strict transport settings; a bad
    // section refuses the whole file and old per-peer tables are pointed at pairing.
    #[test]
    fn r174_the_peers_section_is_strict_and_a_bad_one_refuses_the_whole_file() {
        let good = "[slots]\nrun = 6\n[peers]\nrelay = \"off\"\n";
        let p = MachinePolicy::from_text(Some(good), &no_env);
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        assert_eq!(p.peers().relay, crate::peers_config::RelaySetting::Off);
        assert_eq!(p.slot_count(SlotKind::Run), 6);
        // an old per-peer table: refused as a whole, valid keys beside it are not applied
        let bad = "[slots]\nrun = 6\n[peers.nixos]\naddr = \"192.168.1.7:7843\"\n";
        let p = MachinePolicy::from_text(Some(bad), &no_env);
        assert_eq!(p.peers().relay, crate::peers_config::RelaySetting::Default);
        assert_eq!(p.slot_count(SlotKind::Run), 4);
        let all = p.problems.join("\n");
        assert!(
            all.contains("peers.nixos: peers are no longer configured here"),
            "{all}"
        );
        assert!(all.contains("refused as a whole"), "{all}");
        // the grace is an ordinary setting with an environment override
        let p = MachinePolicy::from_text(None, &env1("TASKS_ALERT_PEER_GRACE_SECS", "5"));
        assert_eq!(p.peer_grace_secs(), 5);
        assert_eq!(MachinePolicy::defaults().peer_grace_secs(), 120);
    }
}
