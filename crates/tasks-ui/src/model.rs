//! The view model: the daemon's snapshot turned into cards and table rows, with every piece
//! of text decided here. Nothing in this module draws, so it is tested on fixed inputs.

use crate::actions::{Actions, actions_for, queue_banner};
use crate::feed::Dashboard;
use crate::format::{self, UNKNOWN};
use std::collections::BTreeMap;
use tasks_core::{Phase, WaitReason};
use tasks_daemon::api::{AlertView, JobView, PeerView, RunningView, StatusSnapshot, WaiterView};

/// Tabs of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    /// Machines and the one job list.
    Queue,
    /// Every finished job the window holds.
    History,
    /// Per-recipe and per-target timing.
    Timings,
    /// Pressure trips: what caused each stall.
    Stalls,
    /// The audit log.
    Audit,
}

impl Tab {
    /// Tabs with content, in order.
    pub const SHOWN: [Tab; 5] = [
        Tab::Queue,
        Tab::History,
        Tab::Timings,
        Tab::Stalls,
        Tab::Audit,
    ];
    /// Tabs that are reserved but not built (none now).
    pub const PLANNED: [Tab; 0] = [];

    /// The tab's title.
    pub fn label(self) -> &'static str {
        match self {
            Tab::Queue => "Queue",
            Tab::History => "History",
            Tab::Timings => "Timings",
            Tab::Stalls => "Stalls",
            Tab::Audit => "Audit",
        }
    }
}

/// Which table a job sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    /// Executing now.
    Running,
    /// Submitted, not started.
    Queued,
    /// Finished.
    Recent,
}

impl Section {
    /// The group's heading.
    pub fn title(self) -> &'static str {
        match self {
            Section::Running => "Running",
            Section::Queued => "Waiting",
            Section::Recent => "Finished",
        }
    }
}

/// How a phase is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseClass {
    /// Waiting for something.
    Waiting,
    /// Executing.
    Running,
    /// Finished well.
    Passed,
    /// Finished badly (failed, timed out, not started).
    Failed,
    /// Cancelled or interrupted.
    Neutral,
}

/// The state cell of a row: what the job is doing, in words, so no column of the list is a
/// column of dashes (R-581).
#[derive(Debug, Clone, PartialEq)]
pub struct StateCell {
    /// A progress bar, for a running job: 0..=1, 0 when nothing is known.
    pub bar: Option<f32>,
    /// The main line for a waiting or finished job; empty for a running one.
    pub head: String,
    /// Its colour class.
    pub tone: PhaseClass,
    /// Second line, left: `3 of 10 targets`, `after #101`, `1 file fetched`.
    pub left: String,
    /// Second line, right and stronger: `~1m20s left`, `starts ~2m`, `5 min ago`.
    pub right: String,
}

/// One table row.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRow {
    /// Job id.
    pub id: i64,
    /// The table it belongs to.
    pub section: Section,
    /// Full owner, for the tooltip and the details (R-204).
    pub owner: String,
    /// Owner up to `@`.
    pub agent: String,
    /// The agent's hue index (the theme's `a1..a4`), stable per name.
    pub hue: usize,
    /// The coding-agent product the name mentions, shown as small text (R-583).
    pub vendor: Option<&'static str>,
    /// The command with env values abbreviated.
    pub command: String,
    /// The machine badge: where the job runs or ran.
    pub machine: String,
    /// True when that machine is not this one.
    pub remote: bool,
    /// True when the job runs on a peer that is offline: the row is old news (R-203).
    pub stale: bool,
    /// Phase in words.
    pub phase: String,
    /// Its colour class.
    pub class: PhaseClass,
    /// The peer's last known phase of a forwarded job (R-186).
    pub remote_phase: Option<String>,
    /// Targets done over expected, as a fraction, when known.
    pub progress: Option<f32>,
    /// `3/10` or [`UNKNOWN`].
    pub progress_text: String,
    /// The running target.
    pub current: Option<String>,
    /// `~1m20s` or [`UNKNOWN`].
    pub eta: String,
    /// Seconds left by the estimate, for a running job that has one.
    pub eta_secs: Option<f64>,
    /// What the ETA rests on.
    pub eta_basis: String,
    /// 1-based queue position, when the daemon listed this waiter (R-129).
    pub position: Option<usize>,
    /// What the job waits for.
    pub wait: Option<String>,
    /// The class the queue orders a waiting job by (`blocking`, `normal`, `background`); empty
    /// for a job that is not waiting.
    pub queue_class: String,
    /// Why it is in that class when not plainly normal (the blocking reason, or how aging
    /// lifted it).
    pub queue_note: String,
    /// When the daemon expects it to start (R-129).
    pub start_estimate: Option<String>,
    /// Running time, waiting time or time since finish, by section.
    pub age: String,
    /// Seconds since the job finished, for a finished one.
    pub finished_ago_secs: Option<u64>,
    /// The cell that says what the job is doing (R-581).
    pub state: StateCell,
    /// Wall-clock limit.
    pub timeout: Option<String>,
    /// Why it was cancelled, when someone said.
    pub cancel_reason: Option<String>,
    /// One-line outcome.
    pub summary: Option<String>,
    /// Which buttons work (R-206).
    pub actions: Actions,
    /// The job as the daemon sent it, for the details view.
    pub job: JobView,
}

/// Reachability of a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Online {
    /// This machine, or a peer that answers.
    Online,
    /// A peer that answered a while ago.
    Stale,
    /// A peer that does not answer.
    Offline,
    /// A peer not probed yet.
    Unknown,
}

/// A holder of a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// `#12 claude` or the external holder's description.
    pub label: String,
    /// The agent, for a ledger job.
    pub agent: Option<String>,
    /// Its hue index.
    pub hue: Option<usize>,
}

/// One slot kind of a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotLine {
    /// `compile`, `run`, `offload`, `device`.
    pub kind: String,
    /// Free slots.
    pub free: u32,
    /// All slots.
    pub total: u32,
    /// Who holds the busy ones. Empty for a peer: its holders are not in the status it reports.
    pub holders: Vec<Holder>,
}

/// A machine card.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineCard {
    /// Machine name.
    pub name: String,
    /// `this machine` or `worker`.
    pub role: &'static str,
    /// True for the machine this daemon runs on.
    pub local: bool,
    /// Reachability.
    pub online: Online,
    /// `online`, or `offline, last seen 4m05s ago`.
    pub status_text: String,
    /// Draw dimmed: the numbers are old or absent.
    pub greyed: bool,
    /// For a dimmed row, the one line that replaces the gauges (R-582).
    pub away_note: Option<String>,
    /// Memory stall (PSI some, percent), when measured.
    pub stall: Option<f32>,
    /// The stall percentage above which a new compile waits.
    pub stall_limit: f32,
    /// CPU busy percent, when measured.
    pub cpu_busy: Option<f32>,
    /// `8.0 / 16 GiB` free of total, or `- / 64 GiB` when only the size is known.
    pub ram_free: String,
    /// CPU line, for the gauge's tooltip.
    pub cpu: String,
    /// RAM line, for the tooltip.
    pub ram: String,
    /// Swap line, for the tooltip.
    pub swap: String,
    /// Pressure in words, for the tooltip.
    pub pressure: String,
    /// What a new compile job would wait for there, if anything (R-208).
    pub blocker: Option<String>,
    /// Slots and holders.
    pub slots: Vec<SlotLine>,
    /// Agents with running jobs here: name, hue index, count (R-208).
    pub agents: Vec<(String, usize, usize)>,
    /// Jobs running here (from this ledger).
    pub running: usize,
    /// Jobs waiting here.
    pub waiting: usize,
    /// Extra lines (disk, link errors).
    pub notes: Vec<String>,
}

/// Everything the Dashboard tab shows.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewModel {
    /// This machine's name.
    pub local: String,
    /// The daemon's clock for this snapshot, ms since the epoch.
    pub now_ms: i64,
    /// The local card first, then one per peer.
    pub machines: Vec<MachineCard>,
    /// Running table.
    pub running: Vec<JobRow>,
    /// Queued table.
    pub queued: Vec<JobRow>,
    /// Recent table, newest first.
    pub recent: Vec<JobRow>,
    /// The queue-held banner (R-189), then the daemon's alerts and policy problems: one
    /// full-width row each, worst first (R-582).
    pub notices: Vec<Notice>,
    /// True while the daemon holds the queue.
    pub draining: bool,
    /// True when the job list was cut.
    pub truncated: bool,
}

/// How loud a [`Notice`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NoticeLevel {
    /// For the record.
    Info,
    /// Needs attention soon.
    Warn,
    /// Needs attention now.
    Critical,
}

/// A banner row: a bold title and a plain sentence, no bracketed tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// How loud.
    pub level: NoticeLevel,
    /// `Cache disk low on laptop`.
    pub title: String,
    /// `120 GiB free, reserve is 150 GiB. ...`
    pub text: String,
}

/// Finished jobs newer than this show in the Queue tab; all of them are in History.
pub const FINISHED_WINDOW_SECS: u64 = 3600;

impl ViewModel {
    /// The job with this id, wherever it is listed.
    pub fn find(&self, id: i64) -> Option<&JobRow> {
        self.running
            .iter()
            .chain(&self.queued)
            .chain(&self.recent)
            .find(|r| r.id == id)
    }

    /// Finished jobs the Queue tab lists: the last hour (R-581).
    pub fn finished_recently(&self) -> Vec<&JobRow> {
        self.recent
            .iter()
            .filter(|r| {
                r.finished_ago_secs
                    .is_some_and(|s| s <= FINISHED_WINDOW_SECS)
            })
            .collect()
    }

    /// Jobs running or waiting: the number on the Queue tab.
    pub fn open_jobs(&self) -> usize {
        self.running.len() + self.queued.len()
    }

    /// The ids of the Queue tab's rows from top to bottom, for the arrow keys.
    pub fn queue_ids(&self) -> Vec<i64> {
        self.running
            .iter()
            .chain(&self.queued)
            .chain(self.finished_recently())
            .map(|r| r.id)
            .collect()
    }
}

/// The id one step up (`-1`) or down (`1`) from `current` in `ids`; the first row when
/// nothing is selected or the selection left the list. Stops at the ends (R-587).
pub fn step_selection(ids: &[i64], current: Option<i64>, delta: i32) -> Option<i64> {
    let at = current.and_then(|c| ids.iter().position(|i| *i == c));
    let next = match at {
        None => 0,
        Some(i) if delta < 0 => i.saturating_sub(1),
        Some(i) => (i + 1).min(ids.len().saturating_sub(1)),
    };
    ids.get(next).copied()
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// `120.0 GiB` as `120 GiB`.
fn trim_gib(s: &str) -> String {
    s.replace(".0 GiB", " GiB")
}

/// An alert in a title and a sentence, from its id and the daemon's message (R-582).
pub fn alert_notice(a: &AlertView, machine: &str) -> Notice {
    let level = match a.level.as_str() {
        "critical" => NoticeLevel::Critical,
        "info" => NoticeLevel::Info,
        _ => NoticeLevel::Warn,
    };
    let msg = a.message.as_str();
    if let Some(kind) = a.id.strip_prefix("disk-")
        && let Some(free) = msg
            .split_once(" has ")
            .and_then(|(_, r)| r.split_once(" free"))
        && let Some((_, reserve)) = msg.split_once("reserve of ")
    {
        let (free, reserve) = (trim_gib(free.0), trim_gib(reserve));
        let what = match kind {
            "cache" => "Cache",
            "state" => "State",
            other => other,
        };
        let text = if msg.contains("below its reserve") {
            format!(
                "{free} free, reserve is {reserve}. New builds that write output here wait until space is freed."
            )
        } else {
            format!(
                "{free} free, reserve is {reserve}. Builds that write output here will wait if it drops below that."
            )
        };
        return Notice {
            level,
            title: format!("{what} disk low on {machine}"),
            text,
        };
    }
    if a.id == "memory-pressure" {
        return Notice {
            level,
            title: format!("Memory pressure on {machine}"),
            text: format!("{}.", capitalised(msg)),
        };
    }
    // anything else: the lead clause is the title, the rest the sentence
    let cut = [": ", " ("]
        .iter()
        .filter_map(|d| msg.find(d).map(|i| (i, d.len())))
        .min();
    match cut {
        Some((i, n)) => Notice {
            level,
            title: capitalised(msg.get(..i).unwrap_or(msg)),
            text: capitalised(&msg.get(i + n..).unwrap_or_default().replacen(')', "", 1)),
        },
        None => Notice {
            level,
            title: match level {
                NoticeLevel::Critical => "Needs attention now".to_string(),
                _ => "Heads up".to_string(),
            },
            text: capitalised(msg),
        },
    }
}

fn notices(st: &StatusSnapshot) -> Vec<Notice> {
    let mut out = Vec::new();
    if let Some(b) = queue_banner(&st.daemon) {
        let (title, text) = b.split_once(". ").unwrap_or((b.as_str(), ""));
        out.push(Notice {
            level: NoticeLevel::Warn,
            title: title.trim_end_matches('.').to_string(),
            text: text.to_string(),
        });
    }
    out.extend(
        st.alerts
            .iter()
            .map(|a| alert_notice(a, &st.daemon.machine)),
    );
    out.extend(st.daemon.policy_problems.iter().map(|p| Notice {
        level: NoticeLevel::Critical,
        title: "Machine policy problem".to_string(),
        text: capitalised(p),
    }));
    out.sort_by_key(|n| std::cmp::Reverse(n.level));
    out
}

/// Recent rows kept in the table.
pub const RECENT_ROWS: usize = 30;

/// A wait reason in words.
pub fn wait_reason_text(w: &WaitReason) -> String {
    match w {
        WaitReason::Slot(k) => format!("waiting for a {} slot", format!("{k:?}").to_lowercase()),
        WaitReason::Pressure(s) => format!("machine pressure: {s}"),
        WaitReason::Placement(s) => format!("placement: {s}"),
        WaitReason::Dependency(id) => format!("after job #{}", id.0),
        WaitReason::Disk => "low disk space".to_string(),
        WaitReason::Device(d) => format!("device {} busy", d.0),
    }
}

/// A phase in words and its colour class.
pub fn phase_text(p: &Phase) -> (String, PhaseClass) {
    match p {
        Phase::Queued => ("queued".into(), PhaseClass::Waiting),
        Phase::Waiting(_) => ("waiting".into(), PhaseClass::Waiting),
        Phase::Running => ("running".into(), PhaseClass::Running),
        Phase::Passed => ("passed".into(), PhaseClass::Passed),
        Phase::Failed { exit: Some(c) } => (format!("failed (exit {c})"), PhaseClass::Failed),
        Phase::Failed { exit: None } => ("failed".into(), PhaseClass::Failed),
        Phase::TimedOut { .. } => ("timed out".into(), PhaseClass::Failed),
        Phase::NotStarted { reason } => (
            format!("not started: {}", format::truncate(reason, 40)),
            PhaseClass::Failed,
        ),
        Phase::Cancelled => ("cancelled".into(), PhaseClass::Neutral),
        Phase::Interrupted => ("interrupted".into(), PhaseClass::Neutral),
    }
}

fn section_of(j: &JobView) -> Section {
    match &j.phase {
        // a forwarded job the peer holds back on its own gates is waiting, not running
        Phase::Running if j.remote_wait.is_some() => Section::Queued,
        Phase::Running => Section::Running,
        Phase::Queued | Phase::Waiting(_) => Section::Queued,
        _ => Section::Recent,
    }
}

fn start_estimate_text(w: &WaiterView) -> String {
    match w.start_estimate_secs {
        Some(s) if w.rough => format!("~{} (rough, no history)", format::duration(s)),
        Some(s) if s < 1.0 => "about now".to_string(),
        Some(s) => format!("~{}", format::duration(s)),
        None => "unknown".to_string(),
    }
}

fn machine_of<'a>(j: &'a JobView, local: &'a str) -> &'a str {
    j.machine
        .as_deref()
        .or(j.forwarded_to.as_ref().map(|f| f.0.as_str()))
        .unwrap_or(local)
}

fn peer_is_offline(peers: &[PeerView], name: &str) -> bool {
    peers
        .iter()
        .any(|p| p.name == name && matches!(p.state.as_str(), "offline"))
}

/// A wait text in the short form a row's state cell uses: `needs a compile slot`,
/// `memory stalls 12% over 10%`.
pub fn wait_phrase(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("waiting for ") {
        return format!("needs {rest}");
    }
    for prefix in ["machine pressure: ", "placement: "] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    text.to_string()
}

/// What a peer's hold-back says, without the `waiting on nixos: ` lead-in the daemon adds.
fn held_reason(text: &str, machine: &str) -> String {
    let lead = format!("waiting on {machine}: ");
    text.strip_prefix(&lead).unwrap_or(text).to_string()
}

/// `1 file fetched`, `2 files fetched, 1 failed`; empty when nothing was asked for.
fn fetch_summary(j: &JobView) -> String {
    let ok = j.fetch.iter().filter(|r| r.ok()).count();
    let bad = j.fetch.len() - ok;
    let files = |n: usize| if n == 1 { "file" } else { "files" };
    match (ok, bad) {
        (0, 0) => String::new(),
        (n, 0) => format!("{n} {} fetched", files(n)),
        (0, b) => format!("{b} {} not fetched", files(b)),
        (n, b) => format!("{n} {} fetched, {b} failed", files(n)),
    }
}

struct StateInput<'a> {
    j: &'a JobView,
    section: Section,
    class: PhaseClass,
    rv: Option<&'a RunningView>,
    wv: Option<&'a WaiterView>,
    wait: Option<&'a str>,
    machine: &'a str,
    age: &'a str,
    now_ms: i64,
}

/// The state cell of a row (R-581): progress and time left for a running job, the position
/// and reason for a waiting one, the result in words for a finished one.
fn state_cell(i: &StateInput) -> StateCell {
    let j = i.j;
    let secs = |from: i64, to: i64| ((to - from).max(0) as f64) / 1000.0;
    match i.section {
        Section::Running => {
            let elapsed = j
                .started_at_ms
                .map(|s| secs(s, i.now_ms))
                .or_else(|| i.rv.map(|r| r.elapsed_secs))
                .unwrap_or(0.0);
            // a bar is drawn only when something backs it: targets done of expected, or time
            // against an estimate from passing runs. Otherwise the elapsed time and the reason
            // (R-671); past the usual time it says so, never "0s left" (R-675).
            let left_text = |r: &RunningView, e: f64| match &r.eta_rough_from {
                Some(m) => format!("~{} left (rough, from {m}'s runs)", format::duration(e)),
                None => format!("~{} left", format::duration(e)),
            };
            let (bar, left, right) = match i.rv {
                Some(r) if r.over_by_secs.is_some() => (
                    None,
                    format!(
                        "over the usual by {}",
                        format::duration(r.over_by_secs.unwrap_or(0.0))
                    ),
                    format!("usual {}", format::duration(r.usual_secs.unwrap_or(0.0))),
                ),
                Some(r) if r.total_targets > 0 => (
                    Some(r.done_targets as f32 / r.total_targets as f32),
                    format!("{} of {} targets", r.done_targets, r.total_targets),
                    r.eta_secs
                        .map_or_else(|| i.age.to_string(), |e| left_text(r, e)),
                ),
                Some(r) if r.eta_secs.is_some() => {
                    let e = r.eta_secs.unwrap_or(0.0);
                    (
                        Some((elapsed / (elapsed + e).max(1.0)) as f32),
                        "estimated from passing runs".to_string(),
                        left_text(r, e),
                    )
                }
                Some(r) => (
                    None,
                    format!(
                        "no estimate: {}",
                        r.no_eta_why.as_deref().unwrap_or("no passing run yet")
                    ),
                    i.age.to_string(),
                ),
                // a forward: the peer's progress is not in this ledger
                None => (None, format!("on {}", i.machine), i.age.to_string()),
            };
            StateCell {
                bar: bar.map(|b| b.clamp(0.0, 1.0)),
                head: String::new(),
                tone: PhaseClass::Running,
                left,
                right,
            }
        }
        Section::Queued => {
            let class_word = |w: Option<&WaiterView>| match w.map(|w| w.class.as_str()) {
                Some(c) if !c.is_empty() && c != "normal" => c.to_string(),
                _ => String::new(),
            };
            let held = i.wv.is_some_and(|w| w.on_peer.is_some()) || j.remote_wait.is_some();
            if held {
                let why = i
                    .wait
                    .map(|w| held_reason(w, i.machine))
                    .unwrap_or_default();
                return StateCell {
                    bar: None,
                    head: format!("held by {} · {why}", i.machine),
                    tone: PhaseClass::Neutral,
                    left: class_word(i.wv),
                    right: String::new(),
                };
            }
            let why = i.wait.map(wait_phrase).unwrap_or_default();
            let head = match i.wv.map(|w| w.position).filter(|p| *p > 0) {
                Some(p) => format!("{} · {why}", format::ordinal(p)),
                None => why,
            };
            let left = match (class_word(i.wv), i.wv) {
                (c, _) if !c.is_empty() => c,
                (_, Some(w)) if w.rough && w.start_estimate_secs.is_some() => {
                    "rough estimate".to_string()
                }
                _ => String::new(),
            };
            let right = match i.wv.and_then(|w| w.start_estimate_secs) {
                Some(s) if s < 1.0 => "starts about now".to_string(),
                Some(s) => format!("starts ~{}", format::coarse(s)),
                None => String::new(),
            };
            StateCell {
                bar: None,
                head,
                tone: PhaseClass::Waiting,
                left,
                right,
            }
        }
        Section::Recent => {
            let ran = format::duration(secs(
                j.started_at_ms.unwrap_or(j.submitted_at_ms),
                j.finished_at_ms.unwrap_or(i.now_ms),
            ));
            let (head, left) = match &j.phase {
                Phase::Passed => (format!("Passed in {ran}"), fetch_summary(j)),
                Phase::Failed { exit: Some(c) } => (
                    format!("Failed (exit {c}) after {ran}"),
                    j.summary.clone().unwrap_or_default(),
                ),
                Phase::Failed { exit: None } => (
                    format!("Failed after {ran}"),
                    j.summary.clone().unwrap_or_default(),
                ),
                Phase::TimedOut { .. } => (
                    format!(
                        "Timed out after {}",
                        j.timeout_secs
                            .map_or_else(|| ran.clone(), |t| format::coarse(t as f64))
                    ),
                    String::new(),
                ),
                Phase::Cancelled => {
                    let head = match &j.cancel_reason {
                        Some(r) => format!("Cancelled · {r}"),
                        None => "Cancelled".to_string(),
                    };
                    let left = match &j.cancel_requested_by {
                        Some(by) if *by == j.owner => "by owner".to_string(),
                        Some(by) => format!("by {}", format::agent_of(by)),
                        None => String::new(),
                    };
                    (head, left)
                }
                Phase::Interrupted => ("Interrupted".to_string(), "the daemon restarted".into()),
                Phase::NotStarted { reason } => (
                    format!("Not started · {}", format::truncate(reason, 60)),
                    String::new(),
                ),
                // a job that is not finished never reaches this arm
                other => (phase_text(other).0, String::new()),
            };
            StateCell {
                bar: None,
                head,
                tone: i.class,
                left,
                right: j
                    .finished_at_ms
                    .map(|f| format::ago_words(secs(f, i.now_ms) as u64))
                    .unwrap_or_default(),
            }
        }
    }
}

fn job_row(
    j: &JobView,
    st: &StatusSnapshot,
    running: &BTreeMap<i64, &RunningView>,
    waiting: &BTreeMap<i64, &WaiterView>,
    now_ms: i64,
) -> JobRow {
    let local = st.daemon.machine.as_str();
    let section = section_of(j);
    let (phase, class) = if j.remote_wait.is_some() {
        ("waiting".to_string(), PhaseClass::Waiting)
    } else {
        phase_text(&j.phase)
    };
    let machine = machine_of(j, local).to_string();
    let remote = machine != local;
    let agent = format::agent_of(&j.owner).to_string();
    let rv = running.get(&j.id.0);
    let wv = waiting.get(&j.id.0);
    let secs = |from: i64, to: i64| ((to - from).max(0) as f64) / 1000.0;

    let (progress, progress_text, current) = match rv {
        Some(r) if r.total_targets > 0 => (
            Some((r.done_targets as f32 / r.total_targets as f32).clamp(0.0, 1.0)),
            format!("{}/{}", r.done_targets, r.total_targets),
            r.current.clone(),
        ),
        Some(r) => (None, UNKNOWN.to_string(), r.current.clone()),
        None => (None, UNKNOWN.to_string(), None),
    };
    let (eta, eta_basis) = match rv {
        Some(r) => match r.eta_secs {
            Some(e) if r.total_targets > 0 => (
                format!("~{}", format::duration(e)),
                format!("targets {}/{}", r.done_targets, r.total_targets),
            ),
            Some(e) => (
                format!("~{}", format::duration(e)),
                r.eta_rough_from
                    .as_ref()
                    .map_or("passing runs".to_string(), |m| {
                        format!("rough, from {m}'s runs")
                    }),
            ),
            None if r.over_by_secs.is_some() => (
                format!(
                    "over by {}",
                    format::duration(r.over_by_secs.unwrap_or(0.0))
                ),
                format!("usual {}", format::duration(r.usual_secs.unwrap_or(0.0))),
            ),
            None => (
                UNKNOWN.to_string(),
                r.no_eta_why
                    .clone()
                    .unwrap_or_else(|| "no passing run yet".to_string()),
            ),
        },
        None => (UNKNOWN.to_string(), String::new()),
    };

    let wait = match (&j.phase, wv) {
        (Phase::Running, _) if j.remote_wait.is_some() => j.remote_wait.clone(),
        (Phase::Queued | Phase::Waiting(_), Some(w)) if !w.wait_text.is_empty() => {
            Some(w.wait_text.clone())
        }
        (Phase::Queued | Phase::Waiting(_), _) => j
            .wait_text
            .clone()
            .or_else(|| match &j.phase {
                Phase::Waiting(r) => Some(wait_reason_text(r)),
                _ => None,
            })
            .or_else(|| Some("queued, not yet considered".to_string())),
        _ => None,
    };

    let age = match section {
        Section::Running => j
            .started_at_ms
            .map(|s| format::duration(secs(s, now_ms)))
            .unwrap_or_else(|| UNKNOWN.into()),
        Section::Queued => format::duration(secs(j.submitted_at_ms, now_ms)),
        Section::Recent => j
            .finished_at_ms
            .map(|f| format::ago(secs(f, now_ms) as u64))
            .unwrap_or_else(|| UNKNOWN.into()),
    };

    let state = state_cell(&StateInput {
        j,
        section,
        class,
        rv: rv.copied(),
        wv: wv.copied(),
        wait: wait.as_deref(),
        machine: &machine,
        age: &age,
        now_ms,
    });
    JobRow {
        id: j.id.0,
        section,
        owner: j.owner.clone(),
        agent: agent.clone(),
        hue: format::agent_hue(&agent),
        vendor: format::vendor_of(&agent),
        command: format::command_display(&j.argv),
        stale: remote && peer_is_offline(&st.peers, &machine) && !j.phase.is_terminal(),
        machine,
        remote,
        phase,
        class,
        remote_phase: j
            .remote_phase
            .as_ref()
            .filter(|_| j.forwarded_to.is_some())
            .map(|p| phase_text(p).0),
        progress,
        progress_text,
        current,
        eta,
        eta_secs: rv.and_then(|r| r.eta_secs),
        eta_basis,
        // a job waiting on a peer's gate is not in this machine's queue
        position: wv.filter(|w| w.on_peer.is_none()).map(|w| w.position),
        wait,
        queue_class: wv.map(|w| w.class.clone()).unwrap_or_default(),
        queue_note: wv.map(|w| w.class_note.clone()).unwrap_or_default(),
        start_estimate: wv
            .filter(|w| w.on_peer.is_none())
            .map(|w| start_estimate_text(w)),
        age,
        finished_ago_secs: j.finished_at_ms.map(|f| secs(f, now_ms).max(0.0) as u64),
        state,
        timeout: j
            .timeout_secs
            .map(|t| format!("limit {}", format::duration(t as f64))),
        cancel_reason: j.cancel_reason.clone(),
        summary: j.summary.clone(),
        actions: actions_for(j, local),
        job: j.clone(),
    }
}

fn holder_for_job(
    id: tasks_core::JobId,
    jobs: &BTreeMap<i64, &JobView>,
    running: &BTreeMap<i64, &RunningView>,
) -> Holder {
    let owner = jobs
        .get(&id.0)
        .map(|j| j.owner.as_str())
        .or_else(|| running.get(&id.0).map(|r| r.owner.as_str()));
    match owner {
        Some(o) => {
            let a = format::agent_of(o).to_string();
            Holder {
                label: format!("#{} {a}", id.0),
                hue: Some(format::agent_hue(&a)),
                agent: Some(a),
            }
        }
        None => Holder {
            label: format!("#{}", id.0),
            agent: None,
            hue: None,
        },
    }
}

fn agents_of(rows: &[&JobRow]) -> Vec<(String, usize, usize)> {
    let mut m: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for r in rows {
        let e = m.entry(r.agent.as_str()).or_insert((r.hue, 0));
        e.1 += 1;
    }
    m.into_iter()
        .map(|(a, (c, n))| (a.to_string(), c, n))
        .collect()
}

fn local_card(
    st: &StatusSnapshot,
    jobs: &BTreeMap<i64, &JobView>,
    running_views: &BTreeMap<i64, &RunningView>,
    rows: &[JobRow],
) -> MachineCard {
    let p = &st.pressure;
    let here: Vec<&JobRow> = rows
        .iter()
        .filter(|r| !r.remote && r.section == Section::Running)
        .collect();
    let ram = match (p.available_memory, p.memory_total) {
        (Some(a), Some(t)) => format!("RAM {} free of {}", format::bytes(a), format::bytes(t)),
        (Some(a), None) => format!("RAM {} available", format::bytes(a)),
        _ => format!("RAM {UNKNOWN}"),
    };
    let mut notes = Vec::new();
    if let Some(w) = &p.warning {
        notes.push(format!("warning: {w}"));
    }
    if let Some(c) = p.cache_free {
        notes.push(format!("cache disk {} free", format::bytes(c)));
    }
    MachineCard {
        name: st.daemon.machine.clone(),
        role: "this machine",
        local: true,
        online: Online::Online,
        status_text: "online (this machine)".into(),
        greyed: false,
        away_note: None,
        stall: p.memory_some,
        stall_limit: stall_limit(p.compile_blocker.as_deref()),
        cpu_busy: p.cpu_busy_pct,
        ram_free: match (p.available_memory, p.memory_total) {
            (Some(a), Some(t)) => format!("{} / {}", gib_free(a), gib_short(t)),
            (Some(a), None) => format!("{} / {UNKNOWN}", gib_free(a)),
            _ => UNKNOWN.to_string(),
        },
        cpu: format!(
            "{}CPU pressure {} ({})",
            busy_prefix(p.cpu_busy_pct),
            format::percent(p.cpu),
            format::pressure_word(p.cpu)
        ),
        ram,
        swap: swap_text(
            "",
            p.swap_total,
            p.swap_used,
            p.swap_in_per_s,
            p.swap_out_per_s,
            "not reported by the daemon",
        ),
        pressure: format!(
            "memory {} (some {}, full {}), io {}",
            p.memory,
            format::percent(p.memory_some),
            format::percent(p.memory_full),
            format::pressure_word(p.io)
        ),
        blocker: p.compile_blocker.clone(),
        slots: st
            .slots
            .iter()
            .map(|g| SlotLine {
                kind: g.kind.clone(),
                free: g.free,
                total: g.total,
                holders: g
                    .busy
                    .iter()
                    .map(|b| match (b.job, &b.holder) {
                        (Some(id), _) if !b.external => holder_for_job(id, jobs, running_views),
                        (_, Some(h)) => Holder {
                            label: format!("external: {h}"),
                            agent: None,
                            hue: None,
                        },
                        (Some(id), None) => holder_for_job(id, jobs, running_views),
                        (None, None) => Holder {
                            label: "unknown holder".into(),
                            agent: None,
                            hue: None,
                        },
                    })
                    .collect(),
            })
            .collect(),
        running: here.len(),
        waiting: rows
            .iter()
            .filter(|r| !r.remote && r.section == Section::Queued)
            .count(),
        agents: agents_of(&here),
        notes,
    }
}

/// The memory-stall percentage above which a new compile waits. The daemon does not report
/// its policy limit yet, so this is read from the blocker text (`... 12% over 10%`) and falls
/// back to the shipped default.
pub const STALL_LIMIT_DEFAULT: f32 = 10.0;

fn stall_limit(blocker: Option<&str>) -> f32 {
    blocker
        .and_then(|b| b.rsplit_once(" over "))
        .and_then(|(_, rest)| rest.strip_suffix('%'))
        .and_then(|n| n.trim().parse::<f32>().ok())
        .filter(|n| *n > 0.0)
        .unwrap_or(STALL_LIMIT_DEFAULT)
}

/// Memory in GiB for the RAM column: `8.0`, `16`, `64`; below 1 GiB in MiB.
fn gib_short(b: u64) -> String {
    let g = b as f64 / (1u64 << 30) as f64;
    if g < 1.0 {
        format!("{:.0} MiB", b as f64 / (1u64 << 20) as f64)
    } else if (g - g.round()).abs() < 0.05 {
        format!("{g:.0} GiB")
    } else {
        format!("{g:.1} GiB")
    }
}

/// The free-memory figure of the RAM column: `8.0`, or `512 MiB` when under a GiB.
fn gib_free(b: u64) -> String {
    if b < 1 << 30 {
        gib_short(b)
    } else {
        format!("{:.1}", b as f64 / (1u64 << 30) as f64)
    }
}

/// `CPU 23% busy, ` before the pressure text, or nothing when the busy figure is unknown.
fn busy_prefix(busy: Option<f32>) -> String {
    busy.map_or_else(String::new, |b| format!("CPU {b:.0}% busy, "))
}

/// The card's swap line: use against size, and the rates when known; `missing` says why when
/// the machine reported nothing.
fn swap_text(
    old: &str,
    total: Option<u64>,
    used: Option<u64>,
    in_s: Option<u64>,
    out_s: Option<u64>,
    missing: &str,
) -> String {
    match total {
        None => format!("swap {UNKNOWN} ({missing})"),
        Some(0) => format!("{old}no swap"),
        Some(t) => {
            let mut s = format!(
                "{old}swap {} of {} used",
                format::bytes(used.unwrap_or(0)),
                format::bytes(t)
            );
            if in_s.is_some() || out_s.is_some() {
                s.push_str(&format!(
                    " (in {}/s, out {}/s)",
                    format::bytes(in_s.unwrap_or(0)),
                    format::bytes(out_s.unwrap_or(0))
                ));
            }
            s
        }
    }
}

fn peer_card(peer: &PeerView, rows: &[JobRow]) -> MachineCard {
    let online = match peer.state.as_str() {
        "up" => Online::Online,
        "stale" => Online::Stale,
        "offline" => Online::Offline,
        _ => Online::Unknown,
    };
    let seen = peer.last_seen_ago_secs.map(format::ago);
    let status_text = match (online, &seen) {
        (Online::Online, _) => "online".to_string(),
        (Online::Stale, Some(s)) => format!("stale, last seen {s}"),
        (Online::Offline, Some(s)) => format!("offline, last seen {s}"),
        (Online::Offline, None) => "offline, never seen".to_string(),
        (Online::Stale, None) => "stale".to_string(),
        (Online::Unknown, _) => "not probed yet".to_string(),
    };
    let greyed = matches!(online, Online::Offline | Online::Unknown);
    let old = if matches!(online, Online::Online) {
        ""
    } else {
        "last known: "
    };
    let here: Vec<&JobRow> = rows
        .iter()
        .filter(|r| r.machine == peer.name && r.section == Section::Running)
        .collect();
    let (cpu, ram, pressure, blocker, slots, waiting) = match &peer.machine {
        Some(m) => (
            format!(
                "{old}{}CPU pressure {} ({}), {} cores",
                busy_prefix(m.cpu_busy_pct.map(f32::from)),
                format::percent(m.cpu_pct.map(f32::from)),
                format::pressure_word(m.cpu_pct.map(f32::from)),
                m.cores
            ),
            format!(
                "{old}RAM {} total, memory pressure {}",
                format::bytes(m.memory_bytes),
                format::percent(m.memory_pct.map(f32::from))
            ),
            format!(
                "{old}memory {}, io {}",
                format::pressure_word(m.memory_pct.map(f32::from)),
                format::pressure_word(m.io_pct.map(f32::from))
            ),
            Some(m.pressure_summary.clone()).filter(|s| !s.is_empty()),
            m.slots
                .iter()
                .map(|s| SlotLine {
                    kind: s.kind.clone(),
                    free: s.total.saturating_sub(s.used),
                    total: s.total,
                    holders: Vec::new(),
                })
                .collect(),
            m.waiting as usize,
        ),
        None => (
            format!("CPU {UNKNOWN}"),
            format!("RAM {UNKNOWN}"),
            format!("pressure {UNKNOWN}"),
            None,
            Vec::new(),
            rows.iter()
                .filter(|r| r.machine == peer.name && r.section == Section::Queued)
                .count(),
        ),
    };
    let mut notes = Vec::new();
    if !peer.detail.is_empty() {
        notes.push(format!("link: {}", peer.detail));
    }
    let away_note = match (online, peer.last_seen_ago_secs) {
        (Online::Offline, Some(secs)) => Some(format!(
            "Offline · last seen {} · its jobs wait or move",
            format::ago_words(secs)
        )),
        (Online::Offline, None) => Some("Offline · never seen".to_string()),
        (Online::Unknown, _) => Some("Not probed yet".to_string()),
        _ => None,
    };
    MachineCard {
        name: peer.name.clone(),
        role: "worker",
        local: false,
        online,
        status_text,
        greyed,
        away_note,
        stall: peer
            .machine
            .as_ref()
            .and_then(|m| m.memory_pct.map(f32::from)),
        stall_limit: stall_limit(blocker.as_deref()),
        cpu_busy: peer
            .machine
            .as_ref()
            .and_then(|m| m.cpu_busy_pct.map(f32::from)),
        ram_free: peer.machine.as_ref().map_or_else(
            || UNKNOWN.to_string(),
            |m| format!("{UNKNOWN} / {}", gib_short(m.memory_bytes)),
        ),
        cpu,
        ram,
        swap: swap_text(
            old,
            peer.machine.as_ref().and_then(|m| m.swap_total),
            peer.machine.as_ref().and_then(|m| m.swap_used),
            peer.machine.as_ref().and_then(|m| m.swap_in_per_s),
            peer.machine.as_ref().and_then(|m| m.swap_out_per_s),
            "not reported",
        ),
        pressure,
        blocker,
        slots,
        running: peer
            .machine
            .as_ref()
            .map_or(here.len(), |m| m.running as usize),
        waiting,
        agents: agents_of(&here),
        notes,
    }
}

/// Builds the view model of a refresh. `now_ms` is the daemon's clock when it gave one,
/// else the caller's.
pub fn build(d: &Dashboard, fallback_now_ms: i64) -> ViewModel {
    let st = &d.status;
    let now_ms = if st.daemon.now_ms > 0 {
        st.daemon.now_ms
    } else {
        fallback_now_ms
    };
    let running: BTreeMap<i64, &RunningView> = st.running.iter().map(|r| (r.id.0, r)).collect();
    let waiting: BTreeMap<i64, &WaiterView> = st.waiting.iter().map(|w| (w.id.0, w)).collect();
    let jobs: BTreeMap<i64, &JobView> = d.jobs.iter().map(|j| (j.id.0, j)).collect();

    let rows: Vec<JobRow> = d
        .jobs
        .iter()
        .map(|j| job_row(j, st, &running, &waiting, now_ms))
        .collect();
    let mut running_rows: Vec<JobRow> = Vec::new();
    let mut queued_rows: Vec<JobRow> = Vec::new();
    let mut recent_rows: Vec<JobRow> = Vec::new();
    for r in rows.iter().cloned() {
        match r.section {
            Section::Running => running_rows.push(r),
            Section::Queued => queued_rows.push(r),
            Section::Recent => recent_rows.push(r),
        }
    }
    running_rows.sort_by_key(|r| (r.job.started_at_ms.unwrap_or(i64::MAX), r.id));
    // Queue order is the daemon's: listed waiters by position, the rest after, by id.
    queued_rows.sort_by_key(|r| (r.position.unwrap_or(usize::MAX), r.id));
    recent_rows
        .sort_by_key(|r| std::cmp::Reverse(r.job.finished_at_ms.unwrap_or(r.job.submitted_at_ms)));
    recent_rows.truncate(RECENT_ROWS);

    let mut machines = vec![local_card(st, &jobs, &running, &rows)];
    machines.extend(st.peers.iter().map(|p| peer_card(p, &rows)));

    ViewModel {
        local: st.daemon.machine.clone(),
        now_ms,
        machines,
        running: running_rows,
        queued: queued_rows,
        recent: recent_rows,
        notices: notices(st),
        draining: st.daemon.draining,
        truncated: d.jobs_truncated,
    }
}

/// Keeps only the open-row ids that still exist, so widget memory does not grow with every
/// job that ever scrolled by (R-201).
pub fn prune_open(open: &mut std::collections::BTreeSet<i64>, vm: &ViewModel) {
    let live: std::collections::BTreeSet<i64> = vm
        .running
        .iter()
        .chain(&vm.queued)
        .chain(&vm.recent)
        .map(|r| r.id)
        .collect();
    open.retain(|id| live.contains(id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn r129_queued_rows_show_position_reason_and_start_estimate() {
        let vm = build(&fixtures::busy(), 0);
        assert!(vm.queued.len() >= 2);
        let first = &vm.queued[0];
        assert_eq!(first.position, Some(1));
        assert!(
            first
                .wait
                .as_deref()
                .unwrap_or_default()
                .contains("compile slot")
        );
        assert_eq!(first.start_estimate.as_deref(), Some("~2m05s"));
        // the list says it in the state cell: position, reason, start (R-581)
        assert_eq!(first.state.head, "1st · needs a compile slot");
        assert_eq!(first.state.right, "starts ~2m");
        let second = &vm.queued[1];
        assert_eq!(second.position, Some(2));
        assert!(
            second
                .start_estimate
                .as_deref()
                .unwrap_or_default()
                .contains("rough"),
            "{:?}",
            second.start_estimate
        );
        // a waiter the status cap cut off has no position, never position 0
        let capped = vm.queued.last().expect("rows");
        assert_eq!(capped.position, None);
        assert!(capped.wait.is_some());
    }

    #[test]
    fn r186_a_forwarded_job_shows_its_machine_and_remote_phase_once() {
        let vm = build(&fixtures::busy(), 0);
        let fwd: Vec<_> = vm.running.iter().filter(|r| r.remote).collect();
        assert_eq!(fwd.len(), 1, "one row for the forward, not a pair");
        assert_eq!(fwd[0].machine, "nixos");
        assert_eq!(fwd[0].remote_phase.as_deref(), Some("running"));
        let local: Vec<_> = vm.running.iter().filter(|r| !r.remote).collect();
        assert!(local.iter().all(|r| r.remote_phase.is_none()));
        assert_eq!(vm.running.iter().filter(|r| r.id == fwd[0].id).count(), 1);
    }

    // R-349
    #[test]
    fn r349_a_forwarded_job_the_peer_holds_back_is_a_queued_row_not_a_running_one() {
        let mut d = fixtures::busy();
        let why = "waiting on nixos: CPU contended: pressure 63% > 40% for compile";
        let j = d.jobs.iter_mut().find(|j| j.id.0 == 103).expect("forward");
        j.remote_phase = Some(Phase::Waiting(WaitReason::Pressure("CPU contended".into())));
        j.remote_wait = Some(why.to_string());
        d.status.running.retain(|r| r.id.0 != 103);
        d.status.waiting.push(WaiterView {
            id: tasks_core::JobId(103),
            owner: "hard-errors@laptop".into(),
            command: "cargo test".into(),
            wait_text: why.into(),
            position: 0,
            start_estimate_secs: None,
            rough: true,
            class: "normal".into(),
            class_note: String::new(),
            on_peer: Some("nixos".into()),
        });
        let vm = build(&d, 0);
        assert!(vm.running.iter().all(|r| r.id != 103));
        let row = vm.queued.iter().find(|r| r.id == 103).expect("queued row");
        assert_eq!(row.phase, "waiting");
        assert_eq!(row.class, PhaseClass::Waiting);
        assert_eq!(row.wait.as_deref(), Some(why));
        // it is not in this machine's queue: no position, no start estimate
        assert_eq!((row.position, row.start_estimate.clone()), (None, None));
        assert_eq!(row.machine, "nixos");
    }

    // R-340
    #[test]
    fn r340_a_waiting_row_shows_its_class_and_why() {
        let mut d = fixtures::busy();
        let w = d
            .status
            .waiting
            .iter_mut()
            .find(|w| w.id.0 == 110)
            .expect("waiter");
        w.class = "normal".into();
        w.class_note = "submitted background, rose to normal after 20m (aging); waited 25m".into();
        let vm = build(&d, 0);
        let row = vm.queued.iter().find(|r| r.id == 110).expect("row");
        assert_eq!(row.queue_class, "normal");
        assert!(
            row.queue_note.contains("rose to normal"),
            "{}",
            row.queue_note
        );
        // plainly normal says nothing in the list; the note is in the side panel (R-585)
        assert_eq!(row.state.left, "");
        let panel = crate::detail::detail(row, fixtures::NOW_MS, &jiff::tz::TimeZone::UTC, None);
        assert!(
            panel
                .facts
                .iter()
                .any(|(l, v)| *l == "Queue class" && v.contains("rose to normal")),
        );
        // any other class is named under the reason
        let mut d = fixtures::busy();
        for w in d.status.waiting.iter_mut().filter(|w| w.id.0 == 110) {
            w.class = "background".into();
        }
        let vm = build(&d, 0);
        let row = vm.queued.iter().find(|r| r.id == 110).expect("row");
        assert_eq!(row.state.left, "background");
        // a job that is not waiting has no class
        assert!(vm.running.iter().all(|r| r.queue_class.is_empty()));
    }

    #[test]
    fn r203_an_offline_worker_is_greyed_and_its_jobs_are_stale() {
        let vm = build(&fixtures::offline(), 0);
        let nixos = vm
            .machines
            .iter()
            .find(|m| m.name == "nixos")
            .expect("card");
        assert!(nixos.greyed);
        assert_eq!(nixos.online, Online::Offline);
        assert!(
            nixos.status_text.contains("last seen 7m00s ago"),
            "{}",
            nixos.status_text
        );
        assert!(nixos.cpu.starts_with("last known"));
        assert_eq!(
            nixos.away_note.as_deref(),
            Some("Offline · last seen 7 min ago · its jobs wait or move")
        );
        let fwd = vm.running.iter().find(|r| r.remote).expect("forward");
        assert!(fwd.stale);
        assert!(!vm.running.iter().filter(|r| !r.remote).any(|r| r.stale));
        // an online peer is not stale
        let busy = build(&fixtures::busy(), 0);
        assert!(busy.running.iter().all(|r| !r.stale));
        assert!(!busy.machines.iter().any(|m| m.greyed));
        assert!(busy.machines.iter().all(|m| m.away_note.is_none()));
    }

    #[test]
    fn r203_unknown_phases_and_peer_states_classify_conservatively() {
        let p = PeerView {
            name: "x".into(),
            node_id: "n".into(),
            state: "from-the-future".into(),
            last_seen_ago_secs: None,
            detail: String::new(),
            did: String::new(),
            roles_out: vec![],
            roles_in: vec![],
            grant_expires_at: None,
            path: "unknown".into(),
            rtt_ms: None,
            reply_ms: None,
            machine: None,
        };
        let c = peer_card(&p, &[]);
        assert_eq!(c.online, Online::Unknown);
        assert!(c.greyed);
        assert!(c.cpu.contains(UNKNOWN) && !c.cpu.contains(" 0"));
        assert_eq!(phase_text(&Phase::Interrupted).1, PhaseClass::Neutral);
    }

    // R-359
    #[test]
    fn r359_machine_cards_show_cpu_busy_and_swap_for_this_machine_and_for_peers() {
        let vm = build(&fixtures::idle(), 0);
        let local = &vm.machines[0];
        assert!(
            local.cpu.starts_with("CPU 23% busy, CPU pressure"),
            "{}",
            local.cpu
        );
        assert_eq!(
            local.swap,
            "swap 1.0 GiB of 8.0 GiB used (in 0 B/s, out 0 B/s)"
        );
        let mut d = fixtures::idle();
        d.status.pressure.swap_total = Some(0);
        assert_eq!(build(&d, 0).machines[0].swap, "no swap");
        // a peer shows what it reported
        let peer = vm
            .machines
            .iter()
            .find(|m| !m.local)
            .map(|m| (m.cpu.clone(), m.swap.clone()));
        assert_eq!(local.cpu_busy, Some(23.0));
        if let Some((cpu, swap)) = peer {
            assert!(cpu.contains("CPU 30% busy"), "{cpu}");
            assert!(swap.contains("swap 0 B of 4.0 GiB used"), "{swap}");
        }
        // unknown stays unknown, with the reason
        let mut d = fixtures::idle();
        d.status.pressure.swap_total = None;
        assert!(build(&d, 0).machines[0].swap.starts_with("swap -"));
    }

    #[test]
    fn r128_a_machine_without_readings_shows_dashes_not_zeros() {
        let mut d = fixtures::idle();
        d.status.pressure.cpu = None;
        d.status.pressure.available_memory = None;
        d.status.pressure.memory_total = None;
        d.status.pressure.memory_some = None;
        d.status.pressure.swap_total = None;
        d.status.pressure.cpu_busy_pct = None;
        let vm = build(&d, 0);
        let m = &vm.machines[0];
        assert_eq!(m.cpu, "CPU pressure - (unknown)");
        assert_eq!(m.ram, "RAM -");
        assert_eq!((m.stall, m.cpu_busy), (None, None));
        assert_eq!(m.ram_free, UNKNOWN);
        assert!(m.swap.starts_with("swap -"));
        assert!(m.pressure.contains("some -"));
    }

    #[test]
    fn r204_every_row_names_its_agent_and_details_the_full_owner() {
        let vm = build(&fixtures::busy(), 0);
        for r in vm.running.iter().chain(&vm.queued).chain(&vm.recent) {
            assert_eq!(r.agent, format::agent_of(&r.owner));
            assert!(!r.agent.contains('@'));
            assert_eq!(r.job.owner, r.owner, "the full owner stays for the tooltip");
            assert_eq!(r.hue, format::agent_hue(&r.agent));
        }
        assert!(vm.running.iter().any(|r| r.owner.contains('@')));
    }

    #[test]
    fn r208_pressure_waits_say_what_they_wait_for() {
        let vm = build(&fixtures::busy(), 0);
        let blocked = vm
            .queued
            .iter()
            .find(|r| r.wait.as_deref().is_some_and(|w| w.contains("pressure")))
            .expect("a pressure wait");
        assert!(
            blocked
                .wait
                .as_deref()
                .unwrap_or_default()
                .contains("memory stalls")
        );
        assert_eq!(
            vm.machines[0].blocker.as_deref(),
            Some("memory stalls 12% over 10%")
        );
        assert_eq!(
            wait_reason_text(&WaitReason::Dependency(tasks_core::JobId(4))),
            "after job #4"
        );
    }

    #[test]
    fn r208_machine_cards_list_slot_holders_and_agents_here() {
        let vm = build(&fixtures::busy(), 0);
        let laptop = &vm.machines[0];
        assert!(laptop.local);
        let compile = laptop
            .slots
            .iter()
            .find(|s| s.kind == "compile")
            .expect("compile");
        assert_eq!((compile.free, compile.total), (0, 2));
        let labels: Vec<_> = compile.holders.iter().map(|h| h.label.as_str()).collect();
        assert!(labels.iter().any(|l| l.contains("claude")), "{labels:?}");
        assert!(
            labels.iter().any(|l| l.starts_with("external:")),
            "{labels:?}"
        );
        assert!(
            laptop
                .agents
                .iter()
                .any(|(a, _, n)| a == "ui-claude" && *n >= 1)
        );
        assert_eq!(
            laptop.running,
            vm.running.iter().filter(|r| !r.remote).count()
        );
    }

    #[test]
    fn running_rows_show_progress_eta_and_basis_and_unknowns_stay_unknown() {
        let vm = build(&fixtures::busy(), 0);
        let r = vm.running.iter().find(|r| r.id == 101).expect("row");
        assert_eq!(r.progress_text, "3/10");
        assert_eq!(r.eta, "~1m20s");
        assert_eq!(r.eta_basis, "targets 3/10");
        assert!((r.progress.unwrap_or(0.0) - 0.3).abs() < 1e-6);
        let fwd = vm.running.iter().find(|r| r.remote).expect("fwd");
        assert_eq!(fwd.progress_text, UNKNOWN);
        assert_eq!(fwd.eta, UNKNOWN);
        assert_eq!(fwd.progress, None);
    }

    #[test]
    fn recent_rows_are_newest_first_with_cancel_reason_and_timeout() {
        let vm = build(&fixtures::busy(), 0);
        let ids: Vec<_> = vm.recent.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![92, 91, 90, 93, 85], "{ids:?}");
        let c = vm
            .recent
            .iter()
            .find(|r| r.phase == "cancelled")
            .expect("c");
        assert_eq!(c.cancel_reason.as_deref(), Some("wrong branch"));
        let t = vm
            .recent
            .iter()
            .find(|r| r.phase == "timed out")
            .expect("t");
        assert_eq!(t.timeout.as_deref(), Some("limit 30m00s"));
    }

    #[test]
    fn r201_widget_memory_stays_flat_over_many_refreshes_with_open_rows() {
        let mut open = std::collections::BTreeSet::new();
        let mut d = fixtures::busy();
        for round in 0..500_i64 {
            // every refresh brings a new job and drops an old one; the user opens each
            d.jobs.push(fixtures::job(
                1000 + round,
                "claude@laptop",
                fixtures::Phase::Passed,
            ));
            if d.jobs.len() > 40 {
                d.jobs.remove(0);
            }
            let vm = build(&d, 0);
            for r in vm.running.iter().chain(&vm.queued).chain(&vm.recent) {
                open.insert(r.id);
            }
            prune_open(&mut open, &vm);
            assert!(open.len() <= d.jobs.len(), "round {round}: {}", open.len());
        }
    }

    #[test]
    fn r582_the_banner_and_alerts_become_titles_and_sentences_without_tags() {
        let mut d = fixtures::busy();
        d.status.daemon.draining = true;
        d.status.daemon.drained_by = Some("lead".into());
        d.status.daemon.drained_at_ms = Some(d.status.daemon.now_ms - 60_000);
        let vm = build(&d, 0);
        assert!(vm.draining);
        assert_eq!(vm.notices.len(), 2);
        let q = &vm.notices[0];
        assert!(q.title.starts_with("Queue drained by lead"), "{}", q.title);
        assert!(q.text.contains("Resume"), "{}", q.text);
        let disk = &vm.notices[1];
        assert_eq!(disk.level, NoticeLevel::Warn);
        assert_eq!(disk.title, "Cache disk low on laptop");
        assert_eq!(
            disk.text,
            "280 GiB free, reserve is 150 GiB. Builds that write output here will wait if it drops below that."
        );
        for n in &vm.notices {
            assert!(
                !n.title.contains('[') && !n.text.contains("[warn]"),
                "{n:?}"
            );
        }
    }

    #[test]
    fn r582_each_kind_of_alert_reads_as_a_title_and_a_sentence() {
        let alert = |id: &str, level: &str, message: &str| AlertView {
            id: id.into(),
            level: level.into(),
            message: message.into(),
            since_ms: 0,
        };
        let below = alert_notice(
            &alert(
                "disk-cache",
                "critical",
                "/c has 120.0 GiB free, below its reserve of 150.0 GiB",
            ),
            "laptop",
        );
        assert_eq!(below.level, NoticeLevel::Critical);
        assert_eq!(below.title, "Cache disk low on laptop");
        assert_eq!(
            below.text,
            "120 GiB free, reserve is 150 GiB. New builds that write output here wait until space is freed."
        );
        let mem = alert_notice(
            &alert(
                "memory-pressure",
                "warn",
                "memory pressure has stayed at warn for 90 s",
            ),
            "laptop",
        );
        assert_eq!(mem.title, "Memory pressure on laptop");
        assert_eq!(mem.text, "Memory pressure has stayed at warn for 90 s.");
        let peer = alert_notice(
            &alert(
                "peer-nixos-offline",
                "warn",
                "peer nixos is offline (last answered 400 s ago): connect timed out; it has been so for 130 s",
            ),
            "laptop",
        );
        assert_eq!(peer.title, "Peer nixos is offline");
        assert!(
            peer.text.starts_with("Last answered 400 s ago"),
            "{}",
            peer.text
        );
        let plain = alert_notice(&alert("x", "critical", "something broke"), "laptop");
        assert_eq!(plain.title, "Needs attention now");
        assert_eq!(plain.text, "Something broke");
        // the worst comes first
        let mut d = fixtures::busy();
        d.status.alerts.push(alert("y", "critical", "it is bad"));
        let vm = build(&d, 0);
        assert_eq!(vm.notices[0].level, NoticeLevel::Critical);
        // a policy problem is critical and named as one
        let mut d = fixtures::busy();
        d.status.daemon.policy_problems = vec!["bad slot count".into()];
        let vm = build(&d, 0);
        assert!(
            vm.notices
                .iter()
                .any(|n| n.title == "Machine policy problem"
                    && n.text == "Bad slot count"
                    && n.level == NoticeLevel::Critical)
        );
    }

    #[test]
    fn r581_running_rows_say_progress_and_time_left_or_that_there_is_no_estimate() {
        let vm = build(&fixtures::busy(), 0);
        let s = |id: i64| {
            vm.running
                .iter()
                .find(|r| r.id == id)
                .expect("row")
                .state
                .clone()
        };
        let a = s(101);
        assert_eq!(
            (a.left.as_str(), a.right.as_str()),
            ("3 of 10 targets", "~1m20s left")
        );
        assert!((a.bar.unwrap_or(-1.0) - 0.3).abs() < 1e-6);
        assert_eq!(a.head, "");
        let b = s(102);
        assert_eq!(
            (b.left.as_str(), b.right.as_str()),
            ("no estimate: first run", "2m00s")
        );
        assert_eq!(b.bar, None, "no estimate, no bar (R-671)");
        let c = s(103);
        assert_eq!((c.left.as_str(), c.right.as_str()), ("on nixos", "2m00s"));
        // an estimate from history, without targets: the bar is elapsed over elapsed + left
        let mut d = fixtures::busy();
        for r in d.status.running.iter_mut().filter(|r| r.id.0 == 102) {
            r.eta_secs = Some(40.0);
        }
        let vm = build(&d, 0);
        let h = vm.running.iter().find(|r| r.id == 102).expect("row");
        assert_eq!(h.state.left, "estimated from passing runs");
        assert_eq!(h.state.right, "~40s left");
        assert!((h.state.bar.unwrap_or(-1.0) - 0.75).abs() < 1e-6);
    }

    #[test]
    fn r671_r672_r675_the_state_cell_draws_a_bar_only_for_a_real_estimate_and_never_says_zero_left()
    {
        let cell = |f: &dyn Fn(&mut RunningView)| {
            let mut d = fixtures::busy();
            for r in d.status.running.iter_mut().filter(|r| r.id.0 == 102) {
                f(r);
            }
            build(&d, 0)
                .running
                .iter()
                .find(|r| r.id == 102)
                .expect("row")
                .state
                .clone()
        };
        // no passing run yet: the reason, no bar
        let c = cell(&|r| r.no_eta_why = Some("no passing run yet".into()));
        assert_eq!(
            (c.bar, c.left.as_str()),
            (None, "no estimate: no passing run yet")
        );
        // another machine's runs: a bar, labelled rough
        let c = cell(&|r| {
            r.eta_secs = Some(240.0);
            r.eta_rough_from = Some("nixos".into());
        });
        assert!(c.bar.is_some());
        assert_eq!(c.right, "~4m00s left (rough, from nixos's runs)");
        // past the usual time: said plainly, no bar implying it is nearly done
        let c = cell(&|r| {
            r.eta_secs = None;
            r.usual_secs = Some(57.0);
            r.over_by_secs = Some(80.0);
        });
        assert_eq!(c.bar, None);
        assert_eq!(
            (c.left.as_str(), c.right.as_str()),
            ("over the usual by 1m20s", "usual 57s")
        );
    }

    #[test]
    fn r581_waiting_rows_say_position_reason_and_who_holds_them() {
        let vm = build(&fixtures::busy(), 0);
        let q = |id: i64| {
            vm.queued
                .iter()
                .find(|r| r.id == id)
                .expect("row")
                .state
                .clone()
        };
        let second = q(111);
        assert_eq!(second.head, "2nd · memory stalls 12% over 10%");
        assert_eq!(second.left, "rough estimate");
        assert_eq!(second.right, "starts ~5m");
        assert_eq!(second.tone, PhaseClass::Waiting);
        // the status cap cut this waiter off: the reason, no position, no estimate
        let capped = q(112);
        assert_eq!(capped.head, "needs a compile slot");
        assert_eq!(capped.right, "");
        // a peer's hold-back is named by whose it is, without the daemon's lead-in
        let held = q(113);
        assert_eq!(held.head, "held by nixos · its compile queue");
        assert_eq!(held.tone, PhaseClass::Neutral);
        assert_eq!(wait_phrase("waiting for a run slot"), "needs a run slot");
        assert_eq!(wait_phrase("low disk space"), "low disk space");
    }

    #[test]
    fn r581_finished_rows_say_the_result_in_words_and_how_long_ago() {
        let vm = build(&fixtures::busy(), 0);
        let f = |id: i64| {
            vm.recent
                .iter()
                .find(|r| r.id == id)
                .expect("row")
                .state
                .clone()
        };
        let pass = f(90);
        assert_eq!(pass.head, "Passed in 5m00s");
        assert_eq!(pass.tone, PhaseClass::Passed);
        assert_eq!(pass.right, "10 min ago");
        let timed = f(91);
        assert_eq!(timed.head, "Timed out after 30m");
        assert_eq!(timed.tone, PhaseClass::Failed);
        let cancelled = f(92);
        assert_eq!(cancelled.head, "Cancelled · wrong branch");
        assert_eq!(cancelled.left, "by owner");
        assert_eq!(cancelled.right, "1 min ago");
        assert_eq!(cancelled.tone, PhaseClass::Neutral);
        assert_eq!(f(93).head, "Failed (exit 101) after 5m00s");
        // a fetch is counted under a pass
        let mut d = fixtures::busy();
        let j = d.jobs.iter_mut().find(|j| j.id.0 == 90).expect("job");
        j.fetch = vec![tasks_core::FetchResult {
            path: "a".into(),
            status: tasks_core::FetchStatus::Fetched {
                bytes: 1,
                sha256: "x".into(),
                unchanged: false,
            },
        }];
        let vm = build(&d, 0);
        assert_eq!(
            vm.recent
                .iter()
                .find(|r| r.id == 90)
                .expect("row")
                .state
                .left,
            "1 file fetched"
        );
    }

    #[test]
    fn r581_the_queue_lists_the_last_hour_of_finished_jobs_and_history_lists_all() {
        let vm = build(&fixtures::busy(), 0);
        let hour: Vec<_> = vm.finished_recently().iter().map(|r| r.id).collect();
        assert_eq!(hour, vec![92, 91, 90, 93]);
        assert!(vm.recent.iter().any(|r| r.id == 85), "History keeps it");
        assert_eq!(vm.open_jobs(), vm.running.len() + vm.queued.len());
        // top to bottom: running, waiting, finished
        let ids = vm.queue_ids();
        assert_eq!(&ids[..3], &[101, 102, 103]);
        assert_eq!(ids.last(), Some(&93));
        assert_eq!(Section::Queued.title(), "Waiting");
    }

    #[test]
    fn r587_arrow_keys_step_through_the_rows_and_stop_at_the_ends() {
        let ids = [5, 6, 7];
        assert_eq!(step_selection(&ids, None, 1), Some(5));
        assert_eq!(step_selection(&ids, None, -1), Some(5));
        assert_eq!(step_selection(&ids, Some(5), 1), Some(6));
        assert_eq!(step_selection(&ids, Some(7), 1), Some(7));
        assert_eq!(step_selection(&ids, Some(5), -1), Some(5));
        assert_eq!(step_selection(&ids, Some(7), -1), Some(6));
        // the selected job left the list: start from the top
        assert_eq!(step_selection(&ids, Some(99), 1), Some(5));
        assert_eq!(step_selection(&[], Some(1), 1), None);
    }

    #[test]
    fn r582_a_machine_row_has_its_gauges_and_an_offline_one_a_note_instead() {
        let vm = build(&fixtures::three_machines(), 0);
        assert_eq!(vm.machines.len(), 3);
        let laptop = &vm.machines[0];
        assert_eq!((laptop.role, laptop.stall), ("this machine", Some(12.0)));
        assert_eq!(laptop.stall_limit, 10.0, "read from the blocker text");
        assert_eq!(laptop.ram_free, "8.0 / 16 GiB");
        let nixos = &vm.machines[1];
        assert_eq!((nixos.role, nixos.cpu_busy), ("worker", Some(30.0)));
        assert_eq!(nixos.ram_free, "- / 64 GiB");
        let air = &vm.machines[2];
        assert!(air.greyed);
        assert_eq!(
            air.away_note.as_deref(),
            Some("Offline · last seen 1 h ago · its jobs wait or move")
        );
        assert_eq!(stall_limit(None), STALL_LIMIT_DEFAULT);
        assert_eq!(stall_limit(Some("memory stalls 12% over 25%")), 25.0);
        assert_eq!(gib_short(512 << 20), "512 MiB");
        assert_eq!(gib_short(3 << 29), "1.5 GiB");
    }

    #[test]
    fn r583_tabs_are_named_for_what_they_hold() {
        let labels: Vec<_> = Tab::SHOWN.iter().map(|t| t.label()).collect();
        assert_eq!(
            labels,
            vec!["Queue", "History", "Timings", "Stalls", "Audit"]
        );
    }

    #[test]
    fn r202_the_reader_ignores_unknown_fields_and_keeps_nulls_unknown() {
        let job = fixtures::job(5, "a@b", fixtures::Phase::Running);
        let mut v = serde_json::to_value(&job).expect("json");
        v["from_the_future"] = serde_json::json!({"x": 1});
        v["timeout_secs"] = serde_json::Value::Null;
        let back: JobView = serde_json::from_value(v).expect("parses");
        assert_eq!(back.timeout_secs, None);
        let old = serde_json::json!({"state":"up","name":"n","node_id":"x","last_seen_ago_secs":null,"detail":""});
        let p: PeerView = serde_json::from_value(old).expect("old peer view");
        assert_eq!(p.machine, None);
        assert_eq!(p.last_seen_ago_secs, None);
    }
}
