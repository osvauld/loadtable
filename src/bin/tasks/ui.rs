#[cfg(test)]
#[path = "ui_tests.rs"]
mod tests;

use eframe::egui::{self, RichText};
use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tinline_task_manager::device::{self, Device};
use tinline_task_manager::{
    Action, Cost, Job, JobSpec, Phase, Response, RuntimeEstimate, request_with_reconnect,
};

const POLL_INTERVAL: Duration = Duration::from_millis(2500);
const STATUS_FADE: Duration = Duration::from_secs(4);
/// adb is queried off the UI thread; a slow or missing adb never blocks the window.
const DEVICE_INTERVAL: Duration = Duration::from_secs(10);
const COSTS: [(Cost, &str); 6] = [
    (Cost::Quick, "quick"),
    (Cost::CpuSimulation, "cpu simulation"),
    (Cost::RealTimeIntegration, "real-time integration"),
    (Cost::Stress, "stress"),
    (Cost::Measurement, "measurement"),
    (Cost::Device, "device"),
];

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
    /// (suite, configuration) -> per-stage history.
    estimates: HashMap<(String, String), Vec<RuntimeEstimate>>,
    /// (is_error, message, shown_at). Success messages fade back to the update time.
    status: Option<(bool, String, Instant)>,
    cancelling: Vec<i64>,
    submitting: bool,
    show_cancelled: bool,
    form: Form,
    log: Option<tinline_task_manager::execution::LogTail>,
    raw_log: bool,
    devices: Devices,
}

impl Dashboard {
    fn send(&mut self, context: &egui::Context, kind: Kind, action: Action) {
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
        if self.poll_in_flight {
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
                        self.estimates.clear();
                        for estimate in &response.estimates {
                            self.estimates
                                .entry((estimate.suite.clone(), estimate.configuration.clone()))
                                .or_default()
                                .push(estimate.clone());
                        }
                        self.response = Some(response);
                    }
                }
            }
        }
    }

    fn scan_devices(&mut self, context: &egui::Context) {
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
        ui.add_space(4.0);
        let mut test_on = None;
        match &self.devices.scan {
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
            ui.add(egui::TextEdit::singleline(&mut self.devices.source).desired_width(140.0));
        });
        if let Some(serial) = test_on {
            self.start_phone_test(&serial);
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Tinline task manager");
            ui.add_space(12.0);
            let execution = match &self.response {
                Some(r) if r.execution_enabled => {
                    "Local worker active — approved jobs only".to_string()
                }
                Some(_) => "Worker stopped — proposals never run automatically".to_string(),
                None => "Waiting for daemon".to_string(),
            };
            ui.label(RichText::new(execution).weak());
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
                    None => match self.last_update {
                        Some(at) => (
                            format!("Updated {} ago", ago(at.elapsed().as_secs())),
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
                    ui.add(egui::TextEdit::singleline(value).desired_width(170.0));
                    ui.end_row();
                }
                ui.label("Cost");
                egui::ComboBox::from_id_salt("cost")
                    .width(170.0)
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
        if ui
            .add_enabled(complete && !self.submitting, egui::Button::new("Submit"))
            .clicked()
        {
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
    }

    fn jobs(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        let jobs: Vec<Job> = self
            .response
            .as_ref()
            .map(|r| r.jobs.clone())
            .unwrap_or_default();
        let queued = jobs.iter().filter(|j| j.phase == Phase::Queued).count();
        ui.horizontal(|ui| {
            ui.strong(format!("{queued} queued"));
            ui.label(RichText::new(format!("· {} total", jobs.len())).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(&mut self.show_cancelled, "Show cancelled");
            });
        });
        ui.separator();
        let now = unix_now();
        if ui
            .button("Start local worker (approved jobs only)")
            .clicked()
        {
            self.send(context, Kind::StartWorker, Action::StartWorker);
        }
        ui.label(RichText::new("Approval permits trusted local Rust code to execute with your account rights; this is not a sandbox.").weak().small());
        // Keep essential status/actions in view at ordinary window widths.
        // Secondary metadata remains available on hover in compact mode.
        let compact = ui.available_width() < 1200.0;
        let mut cancel = None;
        let mut approve = None;
        let mut logs = None;
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("jobs")
                .striped(true)
                .spacing([16.0, 6.0])
                .show(ui, |ui| {
                    for heading in ["#", "Suite"] {
                        ui.strong(heading);
                    }
                    if !compact {
                        for heading in ["Owner", "Config", "Cost", "Revision", "Submitted"] {
                            ui.strong(heading);
                        }
                    }
                    for heading in ["Phase", "Elapsed", "Est. runtime", "Actions"] {
                        ui.strong(heading);
                    }
                    ui.end_row();
                    for job in &jobs {
                        let cancelled = job.phase == Phase::Cancelled;
                        if cancelled && !self.show_cancelled {
                            continue;
                        }
                        let text = |s: String| {
                            let t = RichText::new(s);
                            if cancelled { t.weak() } else { t }
                        };
                        let spec = &job.spec;
                        ui.label(text(job.id.to_string()));
                        ui.label(text(spec.suite.clone())).on_hover_text(format!(
                            "Owner: {}\nConfig: {}\nCost: {}\nSource: {}\nRevision: {}\nSubmitted: {} ago",
                            spec.owner, spec.configuration, cost_label(&spec.cost), spec.source,
                            spec.revision, ago(now.saturating_sub(job.submitted_at).max(0) as u64),
                        ));
                        if !compact {
                            ui.label(text(spec.owner.clone()));
                            ui.label(text(spec.configuration.clone()));
                            ui.label(text(cost_label(&spec.cost).into()));
                            ui.label(text(spec.revision.chars().take(10).collect()).monospace())
                                .on_hover_text(format!("{}\n{}", spec.source, spec.revision));
                            ui.label(text(format!(
                                "{} ago",
                                ago(now.saturating_sub(job.submitted_at).max(0) as u64)
                            )));
                        }
                        let detail = job
                            .execution
                            .as_ref()
                            .map(|e| {
                                let snapshot = e
                                    .snapshot
                                    .as_ref()
                                    .map(|s| s.digest.as_str())
                                    .unwrap_or("not captured");
                                let mut text = format!("{}\nSnapshot: {}", e.detail, snapshot);
                                for stage in &e.stages {
                                    text += &format!(
                                        "\n{}: wait {:.1}s, runtime {}s, exit {:?}, signal {:?}",
                                        stage.stage,
                                        stage.wait_seconds,
                                        stage
                                            .seconds
                                            .map(|s| format!("{s:.1}"))
                                            .unwrap_or_else(|| "unknown/interrupted".into()),
                                        stage.exit_code,
                                        stage.signal
                                    );
                                }
                                text
                            })
                            .unwrap_or_else(|| {
                                "Proposal: approval and worker start required".into()
                            });
                        let phase = if compact {
                            match job.phase {
                                Phase::Queued => "proposal",
                                Phase::WaitingBuild => "waiting: build",
                                Phase::WaitingRun => "waiting: run",
                                Phase::Interrupted => "interrupted",
                                _ => job.phase.label(),
                            }
                        } else {
                            job.phase.label()
                        };
                        ui.label(text(phase.into()))
                            .on_hover_text(format!("{}\n{detail}", job.phase.label()));
                        let elapsed = job.execution.as_ref().and_then(|e| e.started_at.map(|at|
                            duration((e.finished_at.unwrap_or(now) - at).max(0) as f64))).unwrap_or_else(|| "—".into());
                        ui.label(text(elapsed)).on_hover_text("Elapsed wall time includes preparation and resource waiting; not percentage progress.");
                        self.eta_cell(ui, spec, cancelled);
                        let layout = if compact {
                            egui::Layout::top_down(egui::Align::Min)
                        } else {
                            egui::Layout::left_to_right(egui::Align::Center)
                        };
                        ui.with_layout(layout, |ui| {
                            ui.set_min_width(90.0);
                            if job.phase == Phase::Queued
                                && tinline_task_manager::catalog::validate(spec).is_ok()
                                && ui.button("Approve").on_hover_text("Approve trusted local source to execute with your account rights; not a sandbox.").clicked()
                            {
                                approve = Some(job.id);
                            }
                            if !job.phase.terminal() {
                                let requested =
                                    job.execution.as_ref().is_some_and(|e| e.cancel_requested);
                                let busy = self.cancelling.contains(&job.id) || requested;
                                if ui
                                    .add_enabled(
                                        !busy,
                                        egui::Button::new(if requested {
                                            "Cancelling…"
                                        } else {
                                            "Cancel"
                                        })
                                        .small(),
                                    )
                                    .clicked()
                                {
                                    cancel = Some(job.id);
                                }
                            }
                            if ui.button("Logs").on_hover_text("Execution log tail; hover the phase for stage details.").clicked() {
                                logs = Some(job.id);
                            }
                        });
                        ui.end_row();
                    }
                });
            if jobs.is_empty() && self.response.is_some() {
                ui.add_space(12.0);
                ui.label(RichText::new("No jobs yet. Submit one from the left panel.").weak());
            }
        });
        if let Some(id) = approve {
            self.send(context, Kind::Approve(id), Action::Approve(id));
        }
        if let Some(id) = logs {
            self.send(context, Kind::Logs(id), Action::Logs(id));
        }
        if let Some(id) = cancel {
            self.cancelling.push(id);
            self.send(context, Kind::Cancel(id), Action::Cancel(id));
        }
    }

    fn eta_cell(&self, ui: &mut egui::Ui, spec: &JobSpec, cancelled: bool) {
        let key = (spec.suite.clone(), spec.configuration.clone());
        let stages = self.estimates.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        let worker_history = stages
            .iter()
            .any(|s| s.stage == "worker.build" || s.stage == "worker.run");
        let known: Vec<_> = stages
            .iter()
            .filter(|s| {
                s.seconds.is_some()
                    && if worker_history {
                        matches!(s.stage.as_str(), "worker.build" | "worker.run")
                    } else {
                        !s.stage.starts_with("worker.")
                    }
            })
            .collect();
        if known.is_empty() {
            ui.label(RichText::new("unknown").weak())
                .on_hover_text("No runtime history recorded.");
            return;
        }
        let total: f64 = known.iter().filter_map(|s| s.seconds).sum();
        // Least-sampled stage; per-stage counts are in the hover text.
        let runs = known.iter().map(|s| s.samples).min().unwrap_or(0);
        let range = known
            .iter()
            .map(|s| s.range)
            .collect::<Option<Vec<_>>>()
            .map(|r| {
                r.iter()
                    .fold((0.0, 0.0), |(a, b), (lo, hi)| (a + lo, b + hi))
            });
        let mut text = format!("~{}", duration(total));
        let detail = known
            .iter()
            .map(|s| {
                format!(
                    "{}: ~{} over {} run(s)",
                    s.stage,
                    duration(s.seconds.unwrap_or(0.0)),
                    s.samples
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let range_detail = range
            .map(|(lo, hi)| format!("\nEstimated spread: {}–{}", duration(lo), duration(hi)))
            .unwrap_or_default();
        text += &format!(" ({runs})");
        let detail = format!(
            "{} runtime history, recorded stages only; not a scheduled finish ETA.\n{runs} observation(s){range_detail}\n{detail}",
            if worker_history {
                "Worker-observed"
            } else {
                "Self-reported"
            }
        );
        let rich = RichText::new(text);
        ui.label(if cancelled { rich.weak() } else { rich })
            .on_hover_text(detail);
    }
}

impl eframe::App for Dashboard {
    fn update(&mut self, context: &egui::Context, _: &mut eframe::Frame) {
        self.receive();
        self.scan_devices(context);
        if self
            .last_poll
            .is_none_or(|at| at.elapsed() >= POLL_INTERVAL)
        {
            self.poll(context);
        }
        context.request_repaint_after(POLL_INTERVAL);
        egui::TopBottomPanel::top("header").show(context, |ui| {
            ui.add_space(6.0);
            self.header(ui);
            ui.add_space(2.0);
        });
        egui::SidePanel::left("form")
            .exact_width(250.0)
            .resizable(false)
            .show(context, |ui| {
                ui.add_space(8.0);
                self.form(ui, context);
                ui.separator();
                self.devices(ui);
            });
        egui::CentralPanel::default().show(context, |ui| self.jobs(ui, context));
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

fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

fn duration(seconds: f64) -> String {
    let s = seconds.round() as u64;
    match s {
        0..90 => format!("{s}s"),
        90..5400 => format!("{}m", (s + 30) / 60),
        _ => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
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

pub fn run(socket: PathBuf) -> eframe::Result {
    let default_source = self_suite_source(Path::new(env!("CARGO_MANIFEST_DIR")));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tinline task manager")
            .with_inner_size([1180.0, 640.0])
            .with_min_inner_size([760.0, 360.0]),
        ..Default::default()
    };
    let (tx, rx) = mpsc::channel();
    eframe::run_native(
        "Tinline task manager",
        options,
        Box::new(|_| {
            Ok(Box::new(Dashboard {
                socket,
                tx,
                rx,
                next_seq: 0,
                applied_seq: 0,
                poll_in_flight: false,
                last_poll: None,
                last_update: None,
                response: None,
                estimates: HashMap::new(),
                status: None,
                cancelling: Vec::new(),
                submitting: false,
                show_cancelled: false,
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
            }))
        }),
    )
}
