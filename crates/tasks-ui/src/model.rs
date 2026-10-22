//! The view model: the daemon's snapshot turned into cards and table rows, with every piece
//! of text decided here. Nothing in this module draws, so it is tested on fixed inputs.

use crate::actions::{Actions, actions_for, queue_banner};
use crate::feed::Dashboard;
use crate::format::{self, UNKNOWN};
use std::collections::BTreeMap;
use tasks_core::{Phase, WaitReason};
use tasks_daemon::api::{JobView, PeerView, RunningView, StatusSnapshot, WaiterView};

/// Tabs of the window. `Audit` and `PressureEvents` are planned (see
/// `docs/DESIGN-peers-sync-audit.md`) and have no content yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    /// Machine cards and the job tables.
    Dashboard,
    /// Per-recipe and per-target timing.
    Stats,
    /// Planned: the audit log.
    Audit,
    /// Planned: pressure and stall events.
    PressureEvents,
}

impl Tab {
    /// Tabs with content, in order.
    pub const SHOWN: [Tab; 2] = [Tab::Dashboard, Tab::Stats];
    /// Tabs that are reserved but not built.
    pub const PLANNED: [Tab; 2] = [Tab::Audit, Tab::PressureEvents];

    /// The tab's title.
    pub fn label(self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Stats => "Stats",
            Tab::Audit => "Audit",
            Tab::PressureEvents => "Pressure events",
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
    /// The table's heading.
    pub fn title(self) -> &'static str {
        match self {
            Section::Running => "Running",
            Section::Queued => "Queued",
            Section::Recent => "Recent",
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
    /// The agent's colour.
    pub color: [u8; 3],
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
    /// What the ETA rests on.
    pub eta_basis: String,
    /// 1-based queue position, when the daemon listed this waiter (R-129).
    pub position: Option<usize>,
    /// What the job waits for.
    pub wait: Option<String>,
    /// When the daemon expects it to start (R-129).
    pub start_estimate: Option<String>,
    /// Running time, waiting time or time since finish, by section.
    pub age: String,
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
    /// Its colour.
    pub color: Option<[u8; 3]>,
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
    /// True for the machine this daemon runs on.
    pub local: bool,
    /// Reachability.
    pub online: Online,
    /// `online`, or `offline, last seen 4m05s ago`.
    pub status_text: String,
    /// Draw dimmed: the numbers are old or absent.
    pub greyed: bool,
    /// CPU line.
    pub cpu: String,
    /// RAM line.
    pub ram: String,
    /// Swap line.
    pub swap: String,
    /// Pressure in words.
    pub pressure: String,
    /// What a new compile job would wait for there, if anything (R-208).
    pub blocker: Option<String>,
    /// Slots and holders.
    pub slots: Vec<SlotLine>,
    /// Agents with running jobs here, with counts (R-208).
    pub agents: Vec<(String, [u8; 3], usize)>,
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
    /// The local card first, then one per peer.
    pub machines: Vec<MachineCard>,
    /// Running table.
    pub running: Vec<JobRow>,
    /// Queued table.
    pub queued: Vec<JobRow>,
    /// Recent table, newest first.
    pub recent: Vec<JobRow>,
    /// The queue-held banner (R-189).
    pub banner: Option<String>,
    /// Alerts, one line each.
    pub alerts: Vec<String>,
    /// True while the daemon holds the queue.
    pub draining: bool,
    /// Policy problems the daemon names.
    pub problems: Vec<String>,
    /// True when the job list was cut.
    pub truncated: bool,
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

fn section_of(p: &Phase) -> Section {
    match p {
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

fn job_row(
    j: &JobView,
    st: &StatusSnapshot,
    running: &BTreeMap<i64, &RunningView>,
    waiting: &BTreeMap<i64, &WaiterView>,
    now_ms: i64,
) -> JobRow {
    let local = st.daemon.machine.as_str();
    let section = section_of(&j.phase);
    let (phase, class) = phase_text(&j.phase);
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
            Some(e) => (format!("~{}", format::duration(e)), "history".to_string()),
            None => (UNKNOWN.to_string(), "no history".to_string()),
        },
        None => (UNKNOWN.to_string(), String::new()),
    };

    let wait = match (&j.phase, wv) {
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

    JobRow {
        id: j.id.0,
        section,
        owner: j.owner.clone(),
        agent: agent.clone(),
        color: format::agent_color(&agent),
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
        eta_basis,
        position: wv.map(|w| w.position),
        wait,
        start_estimate: wv.map(|w| start_estimate_text(w)),
        age,
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
                color: Some(format::agent_color(&a)),
                agent: Some(a),
            }
        }
        None => Holder {
            label: format!("#{}", id.0),
            agent: None,
            color: None,
        },
    }
}

fn agents_of(rows: &[&JobRow]) -> Vec<(String, [u8; 3], usize)> {
    let mut m: BTreeMap<&str, ([u8; 3], usize)> = BTreeMap::new();
    for r in rows {
        let e = m.entry(r.agent.as_str()).or_insert((r.color, 0));
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
        local: true,
        online: Online::Online,
        status_text: "online (this machine)".into(),
        greyed: false,
        cpu: format!(
            "CPU pressure {} ({})",
            format::percent(p.cpu),
            format::pressure_word(p.cpu)
        ),
        ram,
        swap: format!("swap {UNKNOWN} (not reported by the daemon)"),
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
                            color: None,
                        },
                        (Some(id), None) => holder_for_job(id, jobs, running_views),
                        (None, None) => Holder {
                            label: "unknown holder".into(),
                            agent: None,
                            color: None,
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
                "{old}CPU pressure {} ({}), {} cores",
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
    MachineCard {
        name: peer.name.clone(),
        local: false,
        online,
        status_text,
        greyed,
        cpu,
        ram,
        swap: format!("swap {UNKNOWN} (not reported)"),
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
        machines,
        running: running_rows,
        queued: queued_rows,
        recent: recent_rows,
        banner: queue_banner(&st.daemon),
        alerts: st
            .alerts
            .iter()
            .map(|a| format!("[{}] {}", a.level, a.message))
            .collect(),
        draining: st.daemon.draining,
        problems: st.daemon.policy_problems.clone(),
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
        let fwd = vm.running.iter().find(|r| r.remote).expect("forward");
        assert!(fwd.stale);
        assert!(!vm.running.iter().filter(|r| !r.remote).any(|r| r.stale));
        // an online peer is not stale
        let busy = build(&fixtures::busy(), 0);
        assert!(busy.running.iter().all(|r| !r.stale));
        assert!(!busy.machines.iter().any(|m| m.greyed));
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
            machine: None,
        };
        let c = peer_card(&p, &[]);
        assert_eq!(c.online, Online::Unknown);
        assert!(c.greyed);
        assert!(c.cpu.contains(UNKNOWN) && !c.cpu.contains(" 0"));
        assert_eq!(phase_text(&Phase::Interrupted).1, PhaseClass::Neutral);
    }

    #[test]
    fn r128_a_machine_without_readings_shows_dashes_not_zeros() {
        let mut d = fixtures::idle();
        d.status.pressure.cpu = None;
        d.status.pressure.available_memory = None;
        d.status.pressure.memory_total = None;
        d.status.pressure.memory_some = None;
        let vm = build(&d, 0);
        let m = &vm.machines[0];
        assert_eq!(m.cpu, "CPU pressure - (unknown)");
        assert_eq!(m.ram, "RAM -");
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
            assert_eq!(r.color, format::agent_color(&r.agent));
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
                .any(|(a, _, n)| a == "claude" && *n >= 1)
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
        assert_eq!(ids, vec![92, 91, 90], "{ids:?}");
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
    fn the_banner_and_alerts_reach_the_model() {
        let mut d = fixtures::busy();
        d.status.daemon.draining = true;
        d.status.daemon.drained_by = Some("lead".into());
        d.status.daemon.drained_at_ms = Some(d.status.daemon.now_ms - 60_000);
        let vm = build(&d, 0);
        assert!(vm.draining);
        assert!(vm.banner.unwrap_or_default().contains("drained by lead"));
        assert_eq!(vm.alerts.len(), 1);
        assert!(vm.alerts[0].starts_with("[warn]"));
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
