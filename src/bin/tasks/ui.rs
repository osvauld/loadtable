//! The window: a live dashboard of both machines (laptop and LAN worker), built
//! from cluster snapshots (docs/design/cluster-snapshot.md). Data comes from
//! `ui_source` (background threads), the cards and rows from the pure view model
//! in `ui_model`; this file only draws them and sends the existing daemon actions
//! (approve, cancel, logs, submit, start worker). The job form and the devices
//! panel live on their own tabs.
#[cfg(test)]
#[path = "ui_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ui_raster.rs"]
mod raster;

use crate::ui_model::{
    self, JobKey, Level, MachineCard, MachineState, Presence, QueuedRow, RecentRow, RunningCard,
    Target, View, Warmth,
};
use crate::ui_source::{FixtureSource, LocalSource, RemoteSource, SnapshotSource, Update};
use eframe::egui::{self, Color32, RichText, Sense, Stroke};
use std::collections::HashSet;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tinline_task_manager::device::{self, Device};
use tinline_task_manager::{Action, Cost, JobSpec, Phase, Response, request_with_reconnect};

const POLL_INTERVAL: Duration = Duration::from_millis(2500);
/// Elapsed clocks tick once a second even without new snapshots.
const REPAINT_INTERVAL: Duration = Duration::from_secs(1);
const STATUS_FADE: Duration = Duration::from_secs(4);
/// adb is queried off the UI thread; a slow or missing adb never blocks the window.
const DEVICE_INTERVAL: Duration = Duration::from_secs(10);
/// Recent results shown before "show all".
const RECENT_COMPACT: usize = 6;
const DEMO_HOVER: &str = "fixture demo: no daemon is contacted, actions are disabled";
const COSTS: [(Cost, &str); 6] = [
    (Cost::Quick, "quick"),
    (Cost::CpuSimulation, "cpu simulation"),
    (Cost::RealTimeIntegration, "real-time integration"),
    (Cost::Stress, "stress"),
    (Cost::Measurement, "measurement"),
    (Cost::Device, "device"),
];

const GREEN: Color32 = Color32::from_rgb(64, 168, 96);
const AMBER: Color32 = Color32::from_rgb(214, 150, 38);
const RED: Color32 = Color32::from_rgb(214, 72, 64);
const CPU_LINE: Color32 = Color32::from_rgb(86, 140, 230);
const MEM_LINE: Color32 = Color32::from_rgb(176, 112, 214);
const LAPTOP_BADGE: Color32 = Color32::from_rgb(84, 104, 140);
const WORKER_BADGE: Color32 = Color32::from_rgb(122, 92, 156);
const WARM_BADGE: Color32 = Color32::from_rgb(52, 140, 80);
const COLD_BADGE: Color32 = Color32::from_rgb(56, 112, 196);
const UNKNOWN_BADGE: Color32 = Color32::from_rgb(120, 120, 120);

enum Kind {
    Poll,
    Submit,
    Cancel(i64),
    Approve(i64),
    StartWorker,
    Logs(i64),
}

/// Every reply carries the full job list; `seq` lets late poll replies be ignored.
struct Reply {
    seq: u64,
    kind: Kind,
    result: Result<Response, String>,
}

struct Form {
    suite: String,
    owner: String,
    source: String,
    revision: String,
    configuration: String,
    cost: Cost,
}

type DeviceScan = Result<Vec<Device>, String>;

/// Read-only device view plus the "Test on phone" launcher.
struct Devices {
    scan: Option<DeviceScan>,
    pending: Option<Receiver<DeviceScan>>,
    checked: Option<Instant>,
    /// App checkout to build for phone tests (absolute path).
    source: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tab {
    Dashboard,
    NewJob,
    Devices,
}

/// One snapshot source and what the window last heard from it.
struct Feed {
    source: Box<dyn SnapshotSource>,
    state: MachineState,
    received: Option<Instant>,
    base_age: f64,
}

impl Feed {
    fn new(source: Box<dyn SnapshotSource>) -> Self {
        let state = MachineState {
            fallback_name: source.label().to_string(),
            local: source.local(),
            configured: source.configured(),
            ..MachineState::default()
        };
        Self {
            source,
            state,
            received: None,
            base_age: 0.0,
        }
    }

    fn refresh(&mut self) {
        for update in self.source.poll() {
            match update {
                Update::Snapshot { snapshot, age_s } => {
                    self.state.snapshot = Some(*snapshot);
                    self.state.error = None;
                    self.received = Some(Instant::now());
                    self.base_age = age_s;
                }
                Update::Error(error) => self.state.error = Some(error),
            }
        }
        self.state.seen_ago_s = self
            .received
            .map(|at| self.base_age + at.elapsed().as_secs_f64());
    }
}

#[derive(Default)]
struct RecentFilter {
    owner: String,
    machine: Option<String>,
    show_cancelled: bool,
    expanded: bool,
}

/// What a click asked for; applied after drawing.
enum Click {
    Cancel(i64),
    Logs(i64),
    Approve(i64),
    StartWorker,
}

struct Dashboard {
    socket: PathBuf,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    next_seq: u64,
    applied_seq: u64,
    poll_in_flight: bool,
    last_poll: Option<Instant>,
    last_update: Option<Instant>,
    response: Option<Response>,
    /// (is_error, message, shown_at). Success messages fade back to the update time.
    status: Option<(bool, String, Instant)>,
    cancelling: Vec<i64>,
    submitting: bool,
    form: Form,
    log: Option<tinline_task_manager::execution::LogTail>,
    raw_log: bool,
    devices: Devices,
    /// `tasks ui --fixtures DIR`: never contacts the daemon, adb or the worker.
    demo: Option<String>,
    feeds: Vec<Feed>,
    view: View,
    tab: Tab,
    recent: RecentFilter,
    /// Rows whose details are expanded.
    open_rows: HashSet<JobKey>,
}

impl Dashboard {
    fn new(socket: PathBuf, feeds: Vec<Feed>, demo: Option<String>) -> Self {
        let default_source = self_suite_source(Path::new(env!("CARGO_MANIFEST_DIR")));
        let (tx, rx) = mpsc::channel();
        Self {
            socket,
            tx,
            rx,
            next_seq: 0,
            applied_seq: 0,
            poll_in_flight: false,
            last_poll: None,
            last_update: None,
            response: None,
            status: None,
            cancelling: Vec::new(),
            submitting: false,
            log: None,
            raw_log: false,
            devices: Devices {
                scan: None,
                pending: None,
                checked: None,
                source: String::new(),
            },
            form: Form {
                suite: tinline_task_manager::catalog::SUITE.into(),
                owner: std::env::var("USER").unwrap_or_default(),
                source: default_source
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                revision: default_source
                    .as_deref()
                    .and_then(head_revision)
                    .unwrap_or_default(),
                configuration: tinline_task_manager::catalog::CONFIGURATION.into(),
                cost: Cost::Quick,
            },
            demo,
            feeds,
            view: View::default(),
            tab: Tab::Dashboard,
            recent: RecentFilter::default(),
            open_rows: HashSet::new(),
        }
    }

    fn send(&mut self, context: &egui::Context, kind: Kind, action: Action) {
        if self.demo.is_some() {
            return;
        }
        self.next_seq += 1;
        let (seq, tx, socket, context) = (
            self.next_seq,
            self.tx.clone(),
            self.socket.clone(),
            context.clone(),
        );
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| {
                request_with_reconnect(&socket, action, || crate::ensure_daemon(&socket))
            })
            .map_err(|_| "request thread panicked".to_string())
            .and_then(|result| result.map_err(|e| e.to_string()));
            let _ = tx.send(Reply { seq, kind, result });
            context.request_repaint();
        });
    }

    fn poll(&mut self, context: &egui::Context) {
        if self.poll_in_flight || self.demo.is_some() {
            return;
        }
        self.poll_in_flight = true;
        self.last_poll = Some(Instant::now());
        self.send(context, Kind::Poll, Action::List);
    }

    fn receive(&mut self) {
        while let Ok(reply) = self.rx.try_recv() {
            match reply.kind {
                Kind::Poll => self.poll_in_flight = false,
                Kind::Submit => self.submitting = false,
                Kind::Cancel(id) => self.cancelling.retain(|&c| c != id),
                Kind::Approve(_) | Kind::StartWorker | Kind::Logs(_) => {}
            }
            let action = !matches!(reply.kind, Kind::Poll);
            match reply.result {
                Err(error) => {
                    self.status =
                        Some((true, format!("Daemon unreachable: {error}"), Instant::now()))
                }
                Ok(response) => {
                    if let Some(log) = &response.log {
                        self.log = Some(log.clone());
                    }
                    if let Some(error) = &response.error {
                        self.status = Some((true, error.clone(), Instant::now()));
                    } else if action {
                        self.status = Some((false, action_message(&reply.kind), Instant::now()));
                    } else if self
                        .status
                        .as_ref()
                        .is_some_and(|(is_error, _, _)| *is_error)
                    {
                        self.status = None;
                    }
                    // A failed request still returns the current list, unless it never reached
                    // the store (empty list with an error); keep the last good view then.
                    let usable = response.error.is_none() || !response.jobs.is_empty();
                    if reply.seq >= self.applied_seq && usable {
                        self.applied_seq = reply.seq;
                        self.last_update = Some(Instant::now());
                        self.response = Some(response);
                    }
                }
            }
        }
    }

    /// Pulls every source and rebuilds the view model.
    fn refresh_view(&mut self) {
        for feed in &mut self.feeds {
            feed.refresh();
        }
        let states: Vec<MachineState> = self.feeds.iter().map(|f| f.state.clone()).collect();
        self.view = ui_model::build(&states);
    }

    fn scan_devices(&mut self, context: &egui::Context) {
        if self.demo.is_some() {
            return;
        }
        if let Some(rx) = &self.devices.pending
            && let Ok(scan) = rx.try_recv()
        {
            self.devices.scan = Some(scan);
            self.devices.pending = None;
        }
        if self.devices.pending.is_some()
            || self
                .devices
                .checked
                .is_some_and(|at| at.elapsed() < DEVICE_INTERVAL)
        {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.devices.pending = Some(rx);
        self.devices.checked = Some(Instant::now());
        let context = context.clone();
        std::thread::spawn(move || {
            let scan = std::panic::catch_unwind(device::list)
                .map_err(|_| "device scan panicked".to_string())
                .and_then(|scan| scan.map_err(|e| e.to_string()));
            let _ = tx.send(scan);
            context.request_repaint();
        });
    }

    /// Runs `tasks phone-test` detached; its build/install jobs show up in the list.
    fn start_phone_test(&mut self, serial: &str) {
        if self.demo.is_some() {
            return;
        }
        let source = self.devices.source.trim().to_owned();
        let owner = self.form.owner.trim().to_owned();
        let started = (|| -> tinline_task_manager::Result<PathBuf> {
            let log = tinline_task_manager::state_dir().join(format!(
                "phone-test-{}-{}.log",
                unix_now(),
                std::process::id()
            ));
            let file = std::fs::File::create(&log)?;
            let mut child = std::process::Command::new(std::env::current_exe()?)
                .args(["phone-test", "--who", &owner, &source, "--device", serial])
                .stdin(std::process::Stdio::null())
                .stdout(file.try_clone()?)
                .stderr(file)
                .process_group(0)
                .spawn()?;
            std::thread::spawn(move || child.wait());
            Ok(log)
        })();
        self.status = Some(match started {
            Ok(log) => (
                false,
                format!(
                    "Phone test started on {serial}; output in {}",
                    log.display()
                ),
                Instant::now(),
            ),
            Err(e) => (true, format!("Phone test not started: {e}"), Instant::now()),
        });
    }

    fn devices(&mut self, ui: &mut egui::Ui) {
        ui.heading("Devices");
        ui.label(
            RichText::new("Phones and emulators connected to this machine (adb).")
                .weak()
                .small(),
        );
        ui.add_space(4.0);
        let mut test_on = None;
        match &self.devices.scan {
            None if self.demo.is_some() => {
                ui.label(RichText::new("Fixture demo: adb is not queried.").weak());
            }
            None => {
                ui.label(RichText::new("Checking adb…").weak());
            }
            Some(Err(error)) => {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            Some(Ok(list)) if list.is_empty() => {
                ui.label(RichText::new("No phone or emulator connected").weak());
            }
            Some(Ok(list)) => {
                let source_set = !self.devices.source.trim().is_empty();
                for d in list {
                    let (state, color) = if d.ready() {
                        ("ready", ui.visuals().text_color())
                    } else {
                        (d.state.as_str(), ui.visuals().error_fg_color)
                    };
                    let availability = match (&d.holder, d.busy) {
                        (Some(who), _) => format!("busy: {who}"),
                        (None, true) => "busy".into(),
                        (None, false) => "free".into(),
                    };
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(RichText::new(&d.serial).strong()).truncate());
                        ui.label(RichText::new(d.kind).weak());
                    });
                    ui.label(
                        RichText::new(format!(
                            "{} · {state} · {availability}",
                            d.model.as_deref().unwrap_or("unknown model")
                        ))
                        .color(color)
                        .small(),
                    );
                    if d.kind == "phone" {
                        let ok = d.check_ready(device::PHONE_ABI);
                        let hover = match (&ok, source_set) {
                            (Err(e), _) => e.to_string(),
                            (Ok(()), false) => "Set the app checkout first".into(),
                            (Ok(()), true) if d.busy => {
                                "Builds arm64-v8a now; install waits for the device".into()
                            }
                            (Ok(()), true) => {
                                "Build arm64-v8a only, install -r (keeps data), launch".into()
                            }
                        };
                        if ui
                            .add_enabled(
                                ok.is_ok() && source_set,
                                egui::Button::new("Test on phone"),
                            )
                            .on_hover_text(hover.clone())
                            .on_disabled_hover_text(hover)
                            .clicked()
                        {
                            test_on = Some(d.serial.clone());
                        }
                    }
                    ui.add_space(4.0);
                }
            }
        }
        ui.horizontal(|ui| {
            ui.label("App checkout");
            ui.add(egui::TextEdit::singleline(&mut self.devices.source).desired_width(260.0));
        });
        if let Some(serial) = test_on {
            self.start_phone_test(&serial);
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Task manager");
            ui.add_space(12.0);
            for (tab, label) in [
                (Tab::Dashboard, "Dashboard"),
                (Tab::NewJob, "New job"),
                (Tab::Devices, "Devices"),
            ] {
                ui.selectable_value(&mut self.tab, tab, label);
            }
            ui.add_space(12.0);
            let execution = match (&self.demo, &self.response) {
                (Some(dir), _) => format!("Fixture demo: {dir}"),
                (None, Some(r)) if r.execution_enabled => {
                    "Local worker active — approved jobs only".to_string()
                }
                (None, Some(_)) => "Worker stopped — proposals never run automatically".to_string(),
                (None, None) => "Waiting for daemon".to_string(),
            };
            ui.add(egui::Label::new(RichText::new(execution).weak()).truncate());
        });
        // Fixed-height status line: nothing below moves when messages come and go.
        let height = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        let width = ui.available_width();
        ui.allocate_ui_with_layout(
            egui::vec2(width, height),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_size(egui::vec2(width, height));
                if self
                    .status
                    .as_ref()
                    .is_some_and(|(is_error, _, at)| !is_error && at.elapsed() > STATUS_FADE)
                {
                    self.status = None;
                }
                let (text, color) = match &self.status {
                    Some((true, message, _)) => (message.clone(), ui.visuals().error_fg_color),
                    Some((false, message, _)) => (message.clone(), ui.visuals().text_color()),
                    None if self.demo.is_some() => {
                        (DEMO_HOVER.into(), ui.visuals().weak_text_color())
                    }
                    None => match self.last_update {
                        Some(at) => (
                            format!("Updated {}", ui_model::ago(at.elapsed().as_secs_f64())),
                            ui.visuals().weak_text_color(),
                        ),
                        None => ("Connecting…".into(), ui.visuals().weak_text_color()),
                    },
                };
                ui.add(egui::Label::new(RichText::new(text).color(color)).truncate());
            },
        );
    }

    fn form(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        ui.heading("New job");
        ui.add_space(6.0);
        egui::Grid::new("form")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                for (label, value) in [
                    ("Suite", &mut self.form.suite),
                    ("Owner", &mut self.form.owner),
                    ("Source", &mut self.form.source),
                    ("Revision", &mut self.form.revision),
                    ("Config", &mut self.form.configuration),
                ] {
                    ui.label(label);
                    ui.add(egui::TextEdit::singleline(value).desired_width(320.0));
                    ui.end_row();
                }
                ui.label("Cost");
                egui::ComboBox::from_id_salt("cost")
                    .width(320.0)
                    .selected_text(cost_label(&self.form.cost))
                    .show_ui(ui, |ui| {
                        for (cost, label) in COSTS {
                            ui.selectable_value(&mut self.form.cost, cost, label);
                        }
                    });
                ui.end_row();
            });
        ui.add_space(8.0);
        let f = &self.form;
        let complete = [&f.suite, &f.owner, &f.source, &f.revision, &f.configuration]
            .iter()
            .all(|v| !v.trim().is_empty());
        let submit = ui
            .add_enabled(
                complete && !self.submitting && self.demo.is_none(),
                egui::Button::new("Submit"),
            )
            .on_disabled_hover_text(if self.demo.is_some() {
                DEMO_HOVER
            } else {
                "Fill in every field"
            });
        if submit.clicked() {
            let suite = f.suite.trim();
            let mut configuration = f.configuration.trim().to_string();
            // phone/emulator are input aliases; invalid values are left for the daemon to refuse.
            if suite == tinline_task_manager::catalog::ANDROID_SUITE
                && let Ok(canonical) =
                    tinline_task_manager::catalog::android_configuration(&configuration)
            {
                configuration = canonical;
            }
            let spec = JobSpec {
                suite: suite.into(),
                owner: f.owner.trim().into(),
                source: f.source.trim().into(),
                revision: f.revision.trim().into(),
                configuration,
                cost: f.cost.clone(),
            };
            self.submitting = true;
            self.send(context, Kind::Submit, Action::Submit(spec));
        }
        ui.add_space(12.0);
        ui.label(
            RichText::new(
                "Executable catalog: task-manager.tests\nConfig: debug/offline; cost: quick\nSource: absolute root of a checkout containing tools/task-manager (legacy Tinline layout; the standalone repo is not one); revision: full HEAD\nSubmit, approve trusted source, then start worker.\nLive commands: tasks run in a terminal.\nNo arbitrary commands or device jobs.",
            )
            .weak()
            .small(),
        );
        ui.add_space(12.0);
        ui.separator();
        if ui
            .add_enabled(
                self.demo.is_none(),
                egui::Button::new("Start local worker (approved jobs only)"),
            )
            .on_disabled_hover_text(DEMO_HOVER)
            .clicked()
        {
            self.send(context, Kind::StartWorker, Action::StartWorker);
        }
        ui.label(RichText::new("Approval permits trusted local Rust code to execute with your account rights; this is not a sandbox.").weak().small());
    }

    /// Local proposals awaiting approval, from the daemon's list (approval needs
    /// the job spec, which snapshots do not carry). Live mode only.
    fn approvals(&self, ui: &mut egui::Ui, clicks: &mut Vec<Click>) {
        let Some(response) = &self.response else {
            return;
        };
        let proposals: Vec<_> = response
            .jobs
            .iter()
            .filter(|j| j.phase == Phase::Queued)
            .collect();
        let approved = response.jobs.iter().any(|j| j.phase == Phase::Approved);
        let worker_needed = approved && !response.execution_enabled;
        if proposals.is_empty() && !worker_needed {
            return;
        }
        section(ui, "NEEDS APPROVAL", proposals.len());
        for job in proposals {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Log").clicked() {
                        clicks.push(Click::Logs(job.id));
                    }
                    let requested = job.execution.as_ref().is_some_and(|e| e.cancel_requested);
                    let busy = requested || self.cancelling.contains(&job.id);
                    if ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(if busy { "Cancelling…" } else { "Cancel" }).small(),
                        )
                        .clicked()
                    {
                        clicks.push(Click::Cancel(job.id));
                    }
                    if tinline_task_manager::catalog::validate(&job.spec).is_ok() {
                        if ui
                            .small_button("Approve")
                            .on_hover_text("Approve trusted local source to execute with your account rights; not a sandbox.")
                            .clicked()
                        {
                            clicks.push(Click::Approve(job.id));
                        }
                    } else {
                        ui.label(RichText::new("approve in a terminal").weak().small())
                            .on_hover_text(format!(
                                "Live commands are approved with `tasks approve-command {}` (Android trials: approve-android-trial)",
                                job.id
                            ));
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.strong(format!("#{}", job.id));
                        ui.label(&job.spec.owner);
                        ui.add(
                            egui::Label::new(
                                RichText::new(crate::display::command_summary(job)).monospace(),
                            )
                            .truncate(),
                        );
                    });
                });
            });
        }
        if worker_needed {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Approved jobs wait for the local worker.").weak());
                if ui.small_button("Start local worker").clicked() {
                    clicks.push(Click::StartWorker);
                }
            });
        }
        ui.add_space(6.0);
    }

    fn cancel_busy(&self, id: i64) -> (bool, bool) {
        let requested = self
            .response
            .as_ref()
            .and_then(|r| r.jobs.iter().find(|j| j.id == id))
            .and_then(|j| j.execution.as_ref())
            .is_some_and(|e| e.cancel_requested);
        (requested || self.cancelling.contains(&id), requested)
    }

    /// Log and Cancel buttons for a target, right-to-left.
    fn job_buttons(
        &self,
        ui: &mut egui::Ui,
        cancel: &Target,
        logs: &Target,
        clicks: &mut Vec<Click>,
    ) {
        if let Some(id) = self.target_button(ui, "Log", logs, false) {
            clicks.push(Click::Logs(id));
        }
        let (busy, requested) = match cancel {
            Target::Local { id, .. } => self.cancel_busy(*id),
            Target::Disabled(_) => (false, false),
        };
        let label = if requested || busy {
            "Cancelling…"
        } else {
            "Cancel"
        };
        if let Some(id) = self.target_button(ui, label, cancel, busy) {
            clicks.push(Click::Cancel(id));
        }
    }

    fn target_button(
        &self,
        ui: &mut egui::Ui,
        label: &str,
        target: &Target,
        busy: bool,
    ) -> Option<i64> {
        match target {
            Target::Local { id, hover } => {
                let enabled = self.demo.is_none() && !busy;
                let response = ui
                    .add_enabled(enabled, egui::Button::new(label).small())
                    .on_hover_text(hover.as_str())
                    .on_disabled_hover_text(if self.demo.is_some() {
                        DEMO_HOVER
                    } else {
                        hover.as_str()
                    });
                response.clicked().then_some(*id)
            }
            Target::Disabled(reason) => {
                ui.add_enabled(false, egui::Button::new(label).small())
                    .on_disabled_hover_text(reason.as_str());
                None
            }
        }
    }

    fn dashboard(&mut self, ui: &mut egui::Ui, clicks: &mut Vec<Click>) {
        let view = std::mem::take(&mut self.view);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.label(RichText::new(&view.summary).strong());
                ui.add_space(4.0);
                alert_banner(ui, &view.alerts);
                machine_cards(ui, &view.machines);
                ui.add_space(8.0);
                if self.demo.is_none() {
                    self.approvals(ui, clicks);
                }
                section(ui, "RUNNING", view.running.len());
                if view.running.is_empty() {
                    ui.label(RichText::new("Nothing running.").weak());
                } else {
                    self.running_table(ui, &view.running, clicks);
                }
                ui.add_space(10.0);
                // Live mode lists local proposals under NEEDS APPROVAL instead.
                let queued: Vec<&QueuedRow> = view
                    .queued
                    .iter()
                    .filter(|q| self.demo.is_some() || !(q.proposal && q.local))
                    .collect();
                section(ui, "QUEUED", queued.len());
                if queued.is_empty() {
                    ui.label(RichText::new("Queue empty.").weak());
                } else {
                    self.queued_table(ui, &queued, clicks);
                }
                ui.add_space(10.0);
                self.recent_section(ui, &view.recent, clicks);
            });
        self.view = view;
    }

    /// Toggles a row's details open/closed.
    fn toggle(&mut self, key: &JobKey) {
        if !self.open_rows.remove(key) {
            self.open_rows.insert(key.clone());
        }
    }

    fn running_table(&mut self, ui: &mut egui::Ui, cards: &[RunningCard], clicks: &mut Vec<Click>) {
        let widths = layout_columns(&RUNNING_COLUMNS, ui.available_width());
        header_row(ui, &RUNNING_COLUMNS, &widths);
        let mut toggled = None;
        for (n, card) in cards.iter().enumerate() {
            let open = self.open_rows.contains(&card.key);
            let row =
                table_row(
                    ui,
                    &widths,
                    n,
                    open,
                    card.stale.is_some(),
                    |column, ui| match column {
                        0 => job_cell(ui, card.key.id, card.via),
                        1 => agent_label(ui, &card.agent, &card.owner),
                        2 => machine_badge(ui, &card.machine),
                        3 => command_cell(ui, &card.command),
                        4 => truncated(ui, RichText::new(&card.title)),
                        5 => warmth_badge(ui, card.warmth, &card.warmth_reason),
                        6 => match &card.progress {
                            Some((Some(f), text)) => {
                                ui.add(egui::ProgressBar::new(*f).text(text.as_str()));
                            }
                            Some((None, text)) => truncated(ui, RichText::new(text)),
                            None => truncated(ui, RichText::new("–").weak()),
                        },
                        7 => {
                            sparkline(ui, &card.cpu_history, CPU_LINE, 44.0)
                                .on_hover_text("CPU cores, recent history");
                            ui.label(short_number(&card.cpu_text));
                        }
                        8 => {
                            sparkline(ui, &card.rss_history, MEM_LINE, 44.0)
                                .on_hover_text("Resident memory (summed RSS), recent history");
                            ui.label(&card.rss_text);
                        }
                        9 => {
                            ui.label(card.elapsed_text.trim_end_matches(" elapsed"));
                        }
                        10 => {
                            ui.add(egui::Label::new(card.eta_text.as_str()).truncate())
                                .on_hover_text(&card.eta_basis);
                        }
                        _ => right_aligned(ui, |ui| {
                            self.job_buttons(ui, &card.cancel, &card.logs, clicks)
                        }),
                    },
                );
            if row.clicked() {
                toggled = Some(card.key.clone());
            }
            if open {
                detail_block(ui, &card.details, |ui| {
                    detail_line(ui, "elapsed", &card.elapsed_text);
                    detail_line(ui, "estimate", &card.eta_basis);
                    detail_line(ui, "cache", &card.warmth_reason);
                    detail_line(
                        ui,
                        "CPU / memory",
                        &format!("{} · {}", card.cpu_text, card.rss_text),
                    );
                    for note in [&card.offload_note, &card.stale].into_iter().flatten() {
                        detail_line(ui, "note", note);
                    }
                });
            }
        }
        if let Some(key) = toggled {
            self.toggle(&key);
        }
    }

    fn queued_table(&mut self, ui: &mut egui::Ui, rows: &[&QueuedRow], clicks: &mut Vec<Click>) {
        let widths = layout_columns(&QUEUED_COLUMNS, ui.available_width());
        header_row(ui, &QUEUED_COLUMNS, &widths);
        let mut toggled = None;
        for (n, row) in rows.iter().enumerate() {
            let open = self.open_rows.contains(&row.key);
            let response = table_row(ui, &widths, n, open, row.stale.is_some(), |column, ui| {
                match column {
                    0 => {
                        let position = if row.proposal {
                            "–".to_string()
                        } else {
                            row.position.to_string()
                        };
                        ui.label(RichText::new(position).weak());
                    }
                    1 => job_cell(ui, row.key.id, row.via),
                    2 => agent_label(ui, &row.agent, &row.owner),
                    3 => machine_badge(ui, &row.machine),
                    4 => command_cell(ui, &row.command),
                    5 => truncated(ui, RichText::new(&row.title)),
                    6 => warmth_badge(ui, row.warmth, &row.warmth_reason),
                    7 => {
                        let text = row
                            .waiting_text
                            .strip_prefix("waits for ")
                            .unwrap_or(&row.waiting_text);
                        ui.add(egui::Label::new(RichText::new(text).color(AMBER)).truncate())
                            .on_hover_text(&row.waiting_text);
                    }
                    8 => {
                        ui.add(egui::Label::new(row.start_text.as_str()).truncate()).on_hover_text(
                        "Sum of the remaining estimates ahead of this job in its slot queue; unknown when any of them has no history.",
                    );
                    }
                    _ => right_aligned(ui, |ui| {
                        self.job_buttons(ui, &row.cancel, &row.logs, clicks)
                    }),
                }
            });
            if response.clicked() {
                toggled = Some(row.key.clone());
            }
            if open {
                detail_block(ui, &row.details, |ui| {
                    detail_line(ui, "waiting", &row.waiting_text);
                    detail_line(ui, "start", &row.start_text);
                    detail_line(ui, "cache", &row.warmth_reason);
                    if let Some(stale) = &row.stale {
                        detail_line(ui, "note", stale);
                    }
                });
            }
        }
        if let Some(key) = toggled {
            self.toggle(&key);
        }
    }

    fn recent_section(&mut self, ui: &mut egui::Ui, rows: &[RecentRow], clicks: &mut Vec<Click>) {
        let mut machines: Vec<&str> = rows.iter().map(|r| r.machine.as_str()).collect();
        machines.sort();
        machines.dedup();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("RECENT").strong());
            ui.label(RichText::new(rows.len().to_string()).weak());
            ui.add_space(8.0);
            ui.label(RichText::new("owner").weak());
            ui.add(egui::TextEdit::singleline(&mut self.recent.owner).desired_width(110.0));
            egui::ComboBox::from_id_salt("recent-machine")
                .selected_text(self.recent.machine.as_deref().unwrap_or("all machines"))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.recent.machine, None, "all machines");
                    for m in &machines {
                        ui.selectable_value(&mut self.recent.machine, Some(m.to_string()), *m);
                    }
                });
            ui.checkbox(&mut self.recent.show_cancelled, "cancelled");
        });
        ui.separator();
        let filtered = ui_model::filter_recent(
            rows,
            &self.recent.owner,
            self.recent.machine.as_deref(),
            self.recent.show_cancelled,
        );
        if filtered.is_empty() {
            ui.label(RichText::new("No finished jobs match.").weak());
            return;
        }
        let shown = if self.recent.expanded {
            filtered.len()
        } else {
            filtered.len().min(RECENT_COMPACT)
        };
        let widths = layout_columns(&RECENT_COLUMNS, ui.available_width());
        header_row(ui, &RECENT_COLUMNS, &widths);
        let mut toggled = None;
        for (n, row) in filtered.iter().take(shown).enumerate() {
            let open = self.open_rows.contains(&row.key);
            let response = table_row(ui, &widths, n, open, false, |column, ui| match column {
                0 => job_cell(ui, row.key.id, row.via),
                1 => agent_label(ui, &row.agent, &row.owner),
                2 => machine_badge(ui, &row.machine),
                3 => {
                    let label = match row.outcome {
                        ui_model::Outcome::Passed => "passed",
                        ui_model::Outcome::Failed => "failed",
                        ui_model::Outcome::Cancelled => "cancelled",
                        ui_model::Outcome::Interrupted => "interrupted",
                    };
                    ui.label(
                        RichText::new(format!("{} {label}", row.outcome.symbol()))
                            .color(outcome_color(ui, row.outcome)),
                    );
                }
                4 => command_cell(ui, &row.command),
                5 => truncated(ui, RichText::new(&row.title)),
                6 => warmth_badge(ui, row.warmth, "cache state of this run"),
                7 => {
                    ui.label(&row.wall_text);
                }
                8 => truncated(ui, RichText::new(&row.finished_text).weak()),
                _ => right_aligned(ui, |ui| {
                    if let Some(id) = self.target_button(ui, "Log", &row.logs, false) {
                        clicks.push(Click::Logs(id));
                    }
                }),
            });
            if response.clicked() {
                toggled = Some(row.key.clone());
            }
            if open {
                detail_block(ui, &row.details, |ui| {
                    detail_line(ui, "result", &row.summary)
                });
            }
        }
        if let Some(key) = toggled {
            self.toggle(&key);
        }
        if filtered.len() > RECENT_COMPACT {
            let label = if self.recent.expanded {
                "show less".to_string()
            } else {
                format!("show all {}", filtered.len())
            };
            if ui.small_button(label).clicked() {
                self.recent.expanded = !self.recent.expanded;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tables: fixed columns, the command column takes what is left; when the window
// is narrow, the least important columns are dropped (their values stay in the
// row's details).
// ---------------------------------------------------------------------------

/// `width` 0 = the flexible column (at least `FLEX_MIN`). `drop`: 0 = never
/// dropped; higher numbers are dropped first when space runs out.
struct Column {
    title: &'static str,
    width: f32,
    drop: u8,
}

const FLEX_MIN: f32 = 160.0;
const CELL_GAP: f32 = 8.0;
const ROW_HEIGHT: f32 = 22.0;

const fn col(title: &'static str, width: f32, drop: u8) -> Column {
    Column { title, width, drop }
}

const RUNNING_COLUMNS: [Column; 12] = [
    col("Job", 72.0, 0),
    col("Agent", 120.0, 0),
    col("On", 58.0, 0),
    col("Command", 0.0, 0),
    col("Checkout", 110.0, 6),
    col("Cache", 48.0, 3),
    col("Progress", 140.0, 2),
    col("CPU", 84.0, 4),
    col("RSS", 100.0, 7),
    col("Elapsed", 52.0, 5),
    col("ETA", 170.0, 1),
    col("", 118.0, 0),
];

const QUEUED_COLUMNS: [Column; 10] = [
    col("#", 20.0, 0),
    col("Job", 72.0, 0),
    col("Agent", 120.0, 0),
    col("On", 58.0, 0),
    col("Command", 0.0, 0),
    col("Checkout", 110.0, 5),
    col("Cache", 48.0, 4),
    col("Waiting for", 210.0, 2),
    col("Starts", 130.0, 1),
    col("", 118.0, 0),
];

const RECENT_COLUMNS: [Column; 10] = [
    col("Job", 72.0, 0),
    col("Agent", 120.0, 0),
    col("On", 58.0, 0),
    col("Result", 92.0, 0),
    col("Command", 0.0, 0),
    col("Checkout", 110.0, 5),
    col("Cache", 48.0, 4),
    col("Wall", 52.0, 3),
    col("Finished", 90.0, 2),
    col("", 44.0, 0),
];

/// Column widths for the available width; None = column dropped.
fn layout_columns(columns: &[Column], available: f32) -> Vec<Option<f32>> {
    let mut shown: Vec<bool> = vec![true; columns.len()];
    loop {
        let visible = shown.iter().filter(|s| **s).count();
        let fixed: f32 = columns
            .iter()
            .zip(&shown)
            .filter(|(_, s)| **s)
            .map(|(c, _)| c.width)
            .sum();
        let gaps = CELL_GAP * visible.saturating_sub(1) as f32;
        let flex = available - fixed - gaps;
        if flex >= FLEX_MIN {
            return columns
                .iter()
                .zip(&shown)
                .map(|(c, s)| s.then_some(if c.width == 0.0 { flex } else { c.width }))
                .collect();
        }
        let victim = columns
            .iter()
            .enumerate()
            .filter(|(i, c)| shown[*i] && c.drop > 0)
            .max_by_key(|(i, c)| (c.drop, *i))
            .map(|(i, _)| i);
        match victim {
            Some(i) => shown[i] = false,
            // Nothing left to drop: keep the essentials, the flex column shrinks.
            None => {
                return columns
                    .iter()
                    .zip(&shown)
                    .map(|(c, s)| {
                        s.then_some(if c.width == 0.0 {
                            flex.max(40.0)
                        } else {
                            c.width
                        })
                    })
                    .collect();
            }
        }
    }
}

fn cells(ui: &egui::Ui, rect: egui::Rect, widths: &[Option<f32>]) -> Vec<(usize, egui::Rect)> {
    let _ = ui;
    let mut x = rect.left();
    let mut out = Vec::new();
    for (i, width) in widths.iter().enumerate() {
        if let Some(w) = width {
            out.push((
                i,
                egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(*w, rect.height())),
            ));
            x += w + CELL_GAP;
        }
    }
    out
}

fn header_row(ui: &mut egui::Ui, columns: &[Column], widths: &[Option<f32>]) {
    let total = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total, 18.0), Sense::hover());
    for (i, cell) in cells(ui, rect, widths) {
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(cell)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.shrink_clip_rect(cell);
                ui.label(RichText::new(columns[i].title).small().strong());
            },
        );
    }
    let y = rect.bottom();
    ui.painter().hline(
        rect.x_range(),
        y,
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
}

/// One clickable row (click toggles its details); `draw(column, ui)` fills each cell.
fn table_row(
    ui: &mut egui::Ui,
    widths: &[Option<f32>],
    index: usize,
    open: bool,
    stale: bool,
    mut draw: impl FnMut(usize, &mut egui::Ui),
) -> egui::Response {
    let total = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(total, ROW_HEIGHT), Sense::click());
    let fill = if open {
        Some(ui.visuals().selection.bg_fill.gamma_multiply(0.35))
    } else if response.hovered() {
        Some(ui.visuals().widgets.hovered.weak_bg_fill)
    } else if index.is_multiple_of(2) {
        Some(ui.visuals().faint_bg_color)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, 0.0, fill);
    }
    for (i, cell) in cells(ui, rect, widths) {
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(cell)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.shrink_clip_rect(cell);
                if stale {
                    ui.multiply_opacity(0.55);
                }
                draw(i, ui);
            },
        );
    }
    response.on_hover_text("Click for details")
}

fn right_aligned(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add);
}

fn truncated(ui: &mut egui::Ui, text: RichText) {
    ui.add(egui::Label::new(text).truncate());
}

fn job_cell(ui: &mut egui::Ui, id: i64, via: Option<i64>) {
    ui.label(RichText::new(format!("#{id}")).strong());
    if let Some(client) = via {
        ui.label(RichText::new(format!("⬅ #{client}")).weak().small())
            .on_hover_text(format!("offloaded from laptop job #{client}"));
    }
}

fn command_cell(ui: &mut egui::Ui, command: &str) {
    ui.add(egui::Label::new(RichText::new(command).monospace()).truncate())
        .on_hover_text(command);
}

/// "7.8 cores" -> "7.8" (the column says CPU cores).
fn short_number(text: &str) -> String {
    text.strip_suffix(" cores").unwrap_or(text).to_string()
}

fn agent_color(agent: &str) -> Color32 {
    let [r, g, b] = ui_model::agent_rgb(agent);
    Color32::from_rgb(r, g, b)
}

/// The agent's coloured dot and name; hover shows the full owner.
fn agent_label(ui: &mut egui::Ui, agent: &str, owner: &str) {
    let (rect, dot) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), 4.0, agent_color(agent));
    dot.on_hover_text(owner);
    ui.add(egui::Label::new(agent).truncate())
        .on_hover_text(owner);
}

fn detail_line(ui: &mut egui::Ui, name: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{name}:")).weak());
        ui.label(value);
    });
}

/// The expanded part of a row: full owner, argv, checkout, extras, raw JSON.
fn detail_block(ui: &mut egui::Ui, details: &ui_model::Details, extra: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 24,
            right: 6,
            top: 4,
            bottom: 6,
        })
        .fill(ui.visuals().extreme_bg_color)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("owner:").weak());
                let (agent, _) = ui_model::agent_of(&details.owner);
                let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 4.0, agent_color(&agent));
                ui.label(RichText::new(&details.owner).strong());
            });
            detail_line(ui, "checkout", &details.checkout);
            ui.label(RichText::new("argv:").weak());
            ui.add(egui::Label::new(RichText::new(&details.argv).monospace()).wrap());
            extra(ui);
            egui::CollapsingHeader::new(RichText::new("raw snapshot JSON").small())
                .id_salt(("raw", details.raw_json.len(), details.argv.as_str()))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            ui.add(egui::Label::new(
                                RichText::new(&details.raw_json).monospace().small(),
                            ));
                        });
                });
        });
}

fn section(ui: &mut egui::Ui, title: &str, count: usize) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).strong());
        ui.label(RichText::new(count.to_string()).weak());
    });
    ui.separator();
}

fn outcome_color(ui: &egui::Ui, outcome: ui_model::Outcome) -> Color32 {
    match outcome {
        ui_model::Outcome::Passed => GREEN,
        ui_model::Outcome::Failed => RED,
        ui_model::Outcome::Interrupted => AMBER,
        ui_model::Outcome::Cancelled => ui.visuals().weak_text_color(),
    }
}

fn level_color(ui: &egui::Ui, level: Level) -> Color32 {
    match level {
        Level::Ok => GREEN,
        Level::Warn => AMBER,
        Level::Bad => RED,
        Level::Unknown => ui.visuals().weak_text_color(),
    }
}

fn badge(ui: &mut egui::Ui, text: &str, fill: Color32) -> egui::Response {
    ui.label(
        RichText::new(format!(" {text} "))
            .small()
            .strong()
            .color(Color32::WHITE)
            .background_color(fill),
    )
}

fn machine_badge(ui: &mut egui::Ui, machine: &str) {
    let fill = if machine.starts_with("worker") {
        WORKER_BADGE
    } else {
        LAPTOP_BADGE
    };
    badge(ui, machine, fill);
}

fn warmth_badge(ui: &mut egui::Ui, warmth: Warmth, reason: &str) {
    let fill = match warmth {
        Warmth::Warm => WARM_BADGE,
        Warmth::Cold => COLD_BADGE,
        Warmth::Unknown => UNKNOWN_BADGE,
    };
    badge(ui, warmth.badge(), fill).on_hover_text(reason);
}

/// A horizontal usage bar; green, amber above 70 %, red above 90 %.
fn meter(ui: &mut egui::Ui, fraction: Option<f32>, width: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 12.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    if let Some(f) = fraction {
        let color = if f > 0.9 {
            RED
        } else if f > 0.7 {
            AMBER
        } else {
            GREEN
        };
        let mut fill = rect;
        fill.set_width(rect.width() * f.clamp(0.0, 1.0));
        painter.rect_filled(fill, 2.0, color);
    }
    response
}

/// A small line chart of 0..1 values (gaps where a sample is unknown).
fn sparkline(
    ui: &mut egui::Ui,
    values: &[Option<f32>],
    color: Color32,
    width: f32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 22.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let n = values.len();
    if n < 2 {
        return response;
    }
    let rect = rect.shrink(1.5);
    let point = |i: usize, v: f32| {
        egui::pos2(
            rect.left() + rect.width() * i as f32 / (n - 1) as f32,
            rect.bottom() - rect.height() * v.clamp(0.0, 1.0),
        )
    };
    let mut segment = Vec::new();
    for (i, value) in values.iter().enumerate() {
        match value {
            Some(v) => segment.push(point(i, *v)),
            None => {
                if segment.len() > 1 {
                    painter.line(std::mem::take(&mut segment), Stroke::new(1.5_f32, color));
                }
                segment.clear();
            }
        }
    }
    if segment.len() > 1 {
        painter.line(segment, Stroke::new(1.5_f32, color));
    }
    response
}

/// Active health alerts (`tasks alerts` has evidence and fixes); at most five lines.
fn alert_banner(ui: &mut egui::Ui, alerts: &[(String, String)]) {
    if alerts.is_empty() {
        return;
    }
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        for (severity, line) in alerts.iter().take(5) {
            let color = match severity.as_str() {
                "critical" => RED,
                "warn" => AMBER,
                _ => ui.visuals().weak_text_color(),
            };
            ui.label(RichText::new(line).color(color));
        }
        if alerts.len() > 5 {
            ui.label(RichText::new(format!("… {} more: tasks alerts", alerts.len() - 5)).weak());
        }
    });
    ui.add_space(4.0);
}

fn machine_cards(ui: &mut egui::Ui, cards: &[MachineCard]) {
    if cards.is_empty() {
        return;
    }
    if ui.available_width() >= 380.0 * cards.len() as f32 {
        ui.columns(cards.len(), |columns| {
            for (column, card) in columns.iter_mut().zip(cards) {
                machine_card(column, card);
            }
        });
    } else {
        for card in cards {
            machine_card(ui, card);
        }
    }
}

fn machine_card(ui: &mut egui::Ui, card: &MachineCard) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let offline = !matches!(card.status, Presence::Online);
        ui.horizontal(|ui| {
            ui.label(RichText::new(&card.title).strong().size(15.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (text, color, hover) = match &card.status {
                    Presence::Online => ("online".to_string(), GREEN, String::new()),
                    Presence::Connecting => (
                        "connecting…".into(),
                        ui.visuals().weak_text_color(),
                        "waiting for the first snapshot".into(),
                    ),
                    Presence::NotConfigured(why) => (
                        "not configured".into(),
                        ui.visuals().weak_text_color(),
                        why.clone(),
                    ),
                    Presence::Offline { last_seen, reason } => (
                        match last_seen {
                            Some(seen) => format!("offline · last seen {seen}"),
                            None => "offline".into(),
                        },
                        RED,
                        reason.clone(),
                    ),
                };
                let label = ui.label(RichText::new(text).color(color));
                if !hover.is_empty() {
                    label.on_hover_text(hover);
                }
                if let Some(stale) = &card.stale {
                    ui.label(RichText::new(stale).color(AMBER)).on_hover_text(
                        "no new snapshot for over 3 sample intervals; numbers may be out of date",
                    );
                }
            });
        });
        if offline {
            ui.multiply_opacity(0.5);
        }
        ui.horizontal(|ui| {
            meter(ui, card.cpu_fraction, 110.0);
            ui.label(&card.cpu_text);
            sparkline(ui, &card.cpu_history, CPU_LINE, 140.0)
                .on_hover_text(format!("CPU, {}", card.history_span));
        });
        ui.horizontal(|ui| {
            meter(ui, card.ram_fraction, 110.0);
            ui.label(&card.ram_text);
            sparkline(ui, &card.mem_history, MEM_LINE, 140.0)
                .on_hover_text(format!("Memory used, {}", card.history_span));
        });
        ui.horizontal(|ui| {
            ui.label(&card.swap_text);
            ui.label(RichText::new("pressure:").weak());
            ui.label(
                RichText::new(&card.pressure_text).color(level_color(ui, card.pressure_level)),
            )
            .on_hover_text(&card.pressure_detail);
        });
        ui.add(egui::Label::new(&card.slots_text).truncate())
            .on_hover_text(&card.slots_detail);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for chip in &card.chips {
                let text = RichText::new(&chip.text).monospace().size(11.0);
                let text = if chip.busy {
                    text.background_color(ui.visuals().selection.bg_fill)
                        .color(ui.visuals().selection.stroke.color)
                } else {
                    text.weak()
                };
                ui.label(text).on_hover_text(&chip.hover);
            }
        });
        ui.horizontal_wrapped(|ui| {
            if card.agents.is_empty() {
                ui.label(RichText::new("no agents active").weak().small());
            } else {
                ui.label(RichText::new("agents here:").weak().small());
                for agent in &card.agents {
                    agent_label(ui, agent, agent);
                }
            }
        });
        for warning in &card.warnings {
            ui.add(egui::Label::new(RichText::new(warning).weak().small()).truncate())
                .on_hover_text(warning);
        }
    });
}

impl Dashboard {
    /// One frame of the whole window (also driven headless by the tests).
    fn frame(&mut self, context: &egui::Context) {
        self.receive();
        if self.tab == Tab::Devices {
            self.scan_devices(context);
        }
        if self
            .last_poll
            .is_none_or(|at| at.elapsed() >= POLL_INTERVAL)
        {
            self.poll(context);
        }
        self.refresh_view();
        context.request_repaint_after(REPAINT_INTERVAL);
        egui::TopBottomPanel::top("header").show(context, |ui| {
            ui.add_space(6.0);
            self.header(ui);
            ui.add_space(2.0);
        });
        let mut clicks = Vec::new();
        egui::CentralPanel::default().show(context, |ui| match self.tab {
            Tab::Dashboard => self.dashboard(ui, &mut clicks),
            Tab::NewJob => {
                egui::ScrollArea::vertical().show(ui, |ui| self.form(ui, context));
            }
            Tab::Devices => {
                egui::ScrollArea::vertical().show(ui, |ui| self.devices(ui));
            }
        });
        for click in clicks {
            match click {
                Click::Approve(id) => self.send(context, Kind::Approve(id), Action::Approve(id)),
                Click::Logs(id) => self.send(context, Kind::Logs(id), Action::Logs(id)),
                Click::StartWorker => self.send(context, Kind::StartWorker, Action::StartWorker),
                Click::Cancel(id) => {
                    if self.demo.is_none() {
                        self.cancelling.push(id);
                    }
                    self.send(context, Kind::Cancel(id), Action::Cancel(id));
                }
            }
        }
        let mut refresh = None;
        if let Some(log) = &self.log {
            let mut open = true;
            egui::Window::new(format!("Job {} log tail", log.job_id))
                .open(&mut open)
                .default_size([800.0, 420.0])
                .show(context, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Refresh log").clicked() {
                            refresh = Some(log.job_id);
                        }
                        ui.checkbox(&mut self.raw_log, "Raw Cargo JSON");
                    });
                    if log.truncated {
                        ui.label("Showing last 32 KiB; retained log capped at 8 MiB.");
                    }
                    egui::ScrollArea::both().show(ui, |ui| {
                        if self.raw_log {
                            ui.monospace(&log.text);
                        } else {
                            ui.monospace(crate::log_view::readable_log(&log.text));
                        }
                    });
                });
            if !open {
                self.log = None;
            }
        }
        if let Some(id) = refresh {
            self.send(context, Kind::Logs(id), Action::Logs(id));
        }
    }
}

impl eframe::App for Dashboard {
    fn update(&mut self, context: &egui::Context, _: &mut eframe::Frame) {
        self.frame(context);
    }
}

fn action_message(kind: &Kind) -> String {
    match kind {
        Kind::Poll => String::new(),
        Kind::Submit => "Job submitted".into(),
        Kind::Cancel(id) => format!("Cancellation requested for job {id}"),
        Kind::Approve(id) => format!("Job {id} approved; start worker to execute"),
        Kind::StartWorker => "Local worker start requested".into(),
        Kind::Logs(id) => format!("Log tail loaded for job {id}"),
    }
}

fn cost_label(cost: &Cost) -> &'static str {
    COSTS
        .iter()
        .find(|(c, _)| c == cost)
        .map(|(_, l)| *l)
        .unwrap_or("?")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Default Source for the immutable self-suite form. The retained Tinline adapter
/// captures `SOURCE/tools/task-manager`, so a default exists only when this crate is
/// that embedded directory (legacy layout). In the standalone repository the crate
/// is the repository root and there is no valid default: the field stays empty
/// (Submit disabled) instead of pointing two levels above the checkout.
pub(crate) fn self_suite_source(manifest_dir: &Path) -> Option<PathBuf> {
    let manifest = std::fs::canonicalize(manifest_dir).ok()?;
    let root = manifest.parent()?.parent()?;
    let embedded = std::fs::canonicalize(root.join("tools/task-manager")).ok()?;
    (embedded == manifest).then(|| root.to_owned())
}

fn head_revision(source: &Path) -> Option<String> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// Whole-UI scale: `TASKS_UI_ZOOM` (0.5..=3), default 1.3 so text and charts
/// read comfortably on a laptop panel.
fn ui_zoom() -> f32 {
    std::env::var("TASKS_UI_ZOOM")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|z| z.is_finite())
        .map_or(1.3, |z| z.clamp(0.5, 3.0))
}

/// Opens the window. `fixtures`: replay snapshot files from that directory and
/// contact nothing (no daemon, adb or worker).
pub fn run(socket: PathBuf, fixtures: Option<PathBuf>) -> Result<(), String> {
    let fixture_sources = match &fixtures {
        Some(dir) => Some(FixtureSource::open_dir(dir)?),
        None => None,
    };
    let zoom = ui_zoom();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tinline task manager")
            .with_inner_size([1180.0 * zoom, 760.0 * zoom])
            .with_min_inner_size([760.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Tinline task manager",
        options,
        Box::new(move |creation| {
            // Ctrl +/- / Ctrl 0 still adjust this live (egui keyboard zoom).
            creation.egui_ctx.set_zoom_factor(zoom);
            let feeds: Vec<Feed> = match fixture_sources {
                Some(sources) => sources
                    .into_iter()
                    .map(|s| Feed::new(Box::new(s)))
                    .collect(),
                None => {
                    let repaint = |context: egui::Context| move || context.request_repaint();
                    let context = creation.egui_ctx.clone();
                    vec![
                        Feed::new(Box::new(LocalSource::start(
                            socket.clone(),
                            repaint(context.clone()),
                        ))),
                        Feed::new(Box::new(RemoteSource::start(
                            crate::ui_source::remote_command(),
                            repaint(context),
                        ))),
                    ]
                }
            };
            let demo = fixtures.map(|d| d.display().to_string());
            Ok(Box::new(Dashboard::new(socket, feeds, demo)))
        }),
    )
    .map_err(|e| e.to_string())
}
