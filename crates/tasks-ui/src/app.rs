//! The egui window. Drawing only: every decision about text, order and availability comes
//! from the view model, and every daemon call runs on a feed thread (R-200).

use crate::actions::{self, Avail};
use crate::audit::{self, AuditData, AuditFilter};
use crate::columns::{self, ACTION_BAR_WIDTH, Col, ColSpec, GAP, MIN_WINDOW_WIDTH};
use crate::feed::{self, ActionOutcome, Dashboard, DashboardResult, Feed, Wake};
use crate::format;
use crate::link::DaemonLink;
use crate::logview::{LogSnapshot, SearchHits};
use crate::mailbox::Mailbox;
use crate::model::{self, JobRow, MachineCard, Online, PhaseClass, Section, Tab, ViewModel};
use crate::stalls::{self, StallData};
use crate::stats;
use eframe::egui::{self, Align, Color32, Layout, RichText, Sense, Stroke, vec2};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tasks_core::JobId;
use tasks_daemon::api::HistoryView;

const ROW_H: f32 = 20.0;
const CARD_W: f32 = 330.0;
const REFRESH: Duration = Duration::from_secs(1);

fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

fn phase_color(c: PhaseClass, dark: bool) -> Color32 {
    match (c, dark) {
        (PhaseClass::Running, true) => Color32::from_rgb(110, 190, 255),
        (PhaseClass::Running, false) => Color32::from_rgb(10, 90, 170),
        (PhaseClass::Passed, true) => Color32::from_rgb(120, 210, 130),
        (PhaseClass::Passed, false) => Color32::from_rgb(20, 120, 40),
        (PhaseClass::Failed, true) => Color32::from_rgb(255, 120, 110),
        (PhaseClass::Failed, false) => Color32::from_rgb(190, 30, 30),
        (PhaseClass::Waiting, true) => Color32::from_rgb(235, 200, 100),
        (PhaseClass::Waiting, false) => Color32::from_rgb(150, 100, 0),
        (PhaseClass::Neutral, true) => Color32::from_gray(170),
        (PhaseClass::Neutral, false) => Color32::from_gray(90),
    }
}

fn online_color(o: Online) -> Color32 {
    match o {
        Online::Online => Color32::from_rgb(70, 190, 90),
        Online::Stale => Color32::from_rgb(230, 170, 40),
        Online::Offline => Color32::from_rgb(210, 70, 60),
        Online::Unknown => Color32::from_gray(140),
    }
}

struct CancelDialog {
    id: i64,
    owner: String,
    prompt: String,
    reason: String,
}

struct LogWindow {
    job: i64,
    title: String,
    snap: LogSnapshot,
    updates: Arc<Mailbox<LogSnapshot>>,
    search: String,
    hits: Option<SearchHits>,
    hits_box: Arc<Mailbox<Result<SearchHits, String>>>,
    follow: bool,
    _feed: Feed,
}

/// The window's state.
pub struct App {
    link: Arc<dyn DaemonLink>,
    who: String,
    wake: Wake,
    dash_box: Arc<Mailbox<DashboardResult>>,
    _dash_feed: Option<Feed>,
    vm: Option<ViewModel>,
    error: Option<String>,
    last_ok: Option<Instant>,
    tab: Tab,
    selected: Option<i64>,
    selected_row: Option<JobRow>,
    raw_open: BTreeSet<i64>,
    cancel: Option<CancelDialog>,
    action_box: Arc<Mailbox<ActionOutcome>>,
    status_line: Option<(bool, String)>,
    log: Option<LogWindow>,
    history_box: Arc<Mailbox<Result<HistoryView, String>>>,
    history: Option<Result<HistoryView, String>>,
    history_loading: bool,
    stats_asked: bool,
    audit_box: Arc<Mailbox<Result<AuditData, String>>>,
    audit: Option<Result<AuditData, String>>,
    audit_filter: AuditFilter,
    audit_loading: bool,
    audit_asked: bool,
    stalls_box: Arc<Mailbox<Result<StallData, String>>>,
    stalls: Option<Result<StallData, String>>,
    stalls_loading: bool,
    stalls_asked: bool,
}

impl App {
    /// A window fed by the daemon behind `link`. `who` is the name drain/resume are logged
    /// under. `wake` is called from feed threads to ask for a redraw.
    pub fn new(link: Arc<dyn DaemonLink>, who: String, wake: Wake) -> App {
        let dash_box = Arc::new(Mailbox::new());
        let feed = feed::spawn_dashboard(
            Arc::clone(&link),
            Arc::clone(&dash_box),
            REFRESH,
            Arc::clone(&wake),
        );
        let mut app = App::bare(link, who, wake);
        app.dash_box = dash_box;
        app._dash_feed = Some(feed);
        app
    }

    fn bare(link: Arc<dyn DaemonLink>, who: String, wake: Wake) -> App {
        App {
            link,
            who,
            wake,
            dash_box: Arc::new(Mailbox::new()),
            _dash_feed: None,
            vm: None,
            error: None,
            last_ok: None,
            tab: Tab::Dashboard,
            selected: None,
            selected_row: None,
            raw_open: BTreeSet::new(),
            cancel: None,
            action_box: Arc::new(Mailbox::new()),
            status_line: None,
            log: None,
            history_box: Arc::new(Mailbox::new()),
            history: None,
            history_loading: false,
            stats_asked: false,
            audit_box: Arc::new(Mailbox::new()),
            audit: None,
            audit_filter: AuditFilter::default(),
            audit_loading: false,
            audit_asked: false,
            stalls_box: Arc::new(Mailbox::new()),
            stalls: None,
            stalls_loading: false,
            stalls_asked: false,
        }
    }

    /// A window showing fixed data and calling nobody, for screenshots and tests.
    pub fn fixed(dash: &Dashboard, link: Arc<dyn DaemonLink>) -> App {
        let mut app = App::bare(link, "screenshot".into(), Arc::new(|| {}));
        app.vm = Some(model::build(dash, 0));
        app.last_ok = Some(Instant::now());
        app
    }

    /// Shows a tab (for screenshots).
    pub fn show_tab(&mut self, tab: Tab) {
        self.tab = tab;
    }

    /// Selects a job so the details bar shows it (for screenshots).
    pub fn select(&mut self, id: i64) {
        self.selected = Some(id);
        self.refresh_selected_row();
    }

    /// Gives the Stats tab a history (for screenshots).
    pub fn set_history(&mut self, h: HistoryView) {
        self.history = Some(Ok(h));
        self.stats_asked = true;
    }

    /// Gives the Audit tab its rows (for screenshots).
    pub fn set_audit(&mut self, data: AuditData) {
        self.audit = Some(Ok(data));
        self.audit_asked = true;
    }

    /// Gives the Pressure events tab its rows (for screenshots).
    pub fn set_stalls(&mut self, data: StallData) {
        self.stalls = Some(Ok(data));
        self.stalls_asked = true;
    }

    /// Opens the cancel dialog for a job (for screenshots).
    pub fn ask_cancel(&mut self, id: i64) {
        self.open_cancel(id);
    }

    /// The view model on screen.
    pub fn view_model(&self) -> Option<&ViewModel> {
        self.vm.as_ref()
    }

    fn refresh_selected_row(&mut self) {
        let Some(id) = self.selected else {
            return;
        };
        let Some(vm) = &self.vm else {
            return;
        };
        if let Some(r) = vm
            .running
            .iter()
            .chain(&vm.queued)
            .chain(&vm.recent)
            .find(|r| r.id == id)
        {
            self.selected_row = Some(r.clone());
        }
    }

    /// Takes whatever the feeds delivered since the last frame.
    pub fn pump(&mut self) {
        if let Some(res) = self.dash_box.take() {
            match res {
                Ok(d) => {
                    let vm = model::build(&d, 0);
                    model::prune_open(&mut self.raw_open, &vm);
                    self.vm = Some(vm);
                    self.error = None;
                    self.last_ok = Some(Instant::now());
                    self.refresh_selected_row();
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(o) = self.action_box.take() {
            self.status_line = Some(match o.result {
                Ok(word) => (true, format!("{}: {word}", o.what)),
                Err(e) => (false, format!("{} refused: {e}", o.what)),
            });
        }
        if let Some(h) = self.history_box.take() {
            self.history = Some(h);
            self.history_loading = false;
        }
        if let Some(d) = self.audit_box.take() {
            self.audit = Some(d);
            self.audit_loading = false;
        }
        if let Some(d) = self.stalls_box.take() {
            self.stalls = Some(d);
            self.stalls_loading = false;
        }
        if let Some(l) = &mut self.log {
            if let Some(s) = l.updates.take() {
                l.snap = s;
            }
            if let Some(h) = l.hits_box.take() {
                l.hits = Some(match h {
                    Ok(lines) => lines,
                    Err(e) => vec![(false, format!("search failed: {e}"))],
                });
            }
        }
    }

    fn open_cancel(&mut self, id: i64) {
        let row = self
            .vm
            .as_ref()
            .and_then(|vm| {
                vm.running
                    .iter()
                    .chain(&vm.queued)
                    .chain(&vm.recent)
                    .find(|r| r.id == id)
            })
            .cloned();
        if let Some(r) = row {
            self.cancel = Some(CancelDialog {
                id,
                owner: r.job.owner.clone(),
                prompt: actions::cancel_prompt(&r.job),
                reason: String::new(),
            });
        }
    }

    fn open_log(&mut self, row: &JobRow) {
        let updates = Arc::new(Mailbox::new());
        let live = !row.job.phase.is_terminal();
        let feed = feed::spawn_log(
            Arc::clone(&self.link),
            row.job.id,
            live,
            REFRESH,
            Arc::clone(&updates),
            Arc::clone(&self.wake),
        );
        self.log = Some(LogWindow {
            job: row.id,
            title: format!("Log of job #{} ({})", row.id, row.agent),
            snap: LogSnapshot::default(),
            updates,
            search: String::new(),
            hits: None,
            hits_box: Arc::new(Mailbox::new()),
            follow: true,
            _feed: feed,
        });
    }

    fn spawn_action(&self, f: impl FnOnce(&dyn DaemonLink) -> ActionOutcome + Send + 'static) {
        let link = Arc::clone(&self.link);
        feed::spawn_once(
            Arc::clone(&self.action_box),
            Arc::clone(&self.wake),
            move || f(link.as_ref()),
        );
    }

    /// Draws one frame.
    pub fn draw(&mut self, ctx: &egui::Context) {
        self.pump();
        let dark = ctx.style().visuals.dark_mode;

        egui::TopBottomPanel::top("top").show(ctx, |ui| self.top_bar(ui));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| match &self.status_line {
                Some((true, t)) => {
                    ui.label(RichText::new(t).color(phase_color(PhaseClass::Passed, dark)));
                }
                Some((false, t)) => {
                    ui.label(RichText::new(t).color(phase_color(PhaseClass::Failed, dark)));
                }
                None => {
                    ui.weak("Click a row for details. Hover an agent for the full owner.");
                }
            });
        });
        if self.tab == Tab::Dashboard && self.selected_row.is_some() {
            egui::TopBottomPanel::bottom("details")
                .resizable(true)
                .default_height(260.0)
                .min_height(120.0)
                .show(ctx, |ui| self.details(ui));
        }
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Dashboard => self.dashboard(ui),
            Tab::Stats => self.stats_tab(ui),
            Tab::Audit => self.audit_tab(ui),
            Tab::PressureEvents => self.stalls_tab(ui),
        });
        self.cancel_dialog(ctx);
        self.log_window(ctx);
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let dark = ui.visuals().dark_mode;
        ui.horizontal_wrapped(|ui| {
            ui.heading("tasks");
            ui.separator();
            for t in Tab::SHOWN {
                if ui.selectable_label(self.tab == t, t.label()).clicked() {
                    self.tab = t;
                }
            }
            for t in Tab::PLANNED {
                ui.add_enabled(false, egui::Button::new(t.label()).frame(false))
                    .on_disabled_hover_text("Planned: see docs/DESIGN-peers-sync-audit.md");
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(vm) = &self.vm {
                    let (label, cancel) = if vm.draining {
                        ("Resume", true)
                    } else {
                        ("Drain", false)
                    };
                    let tip = if cancel {
                        "Let queued jobs start again."
                    } else {
                        "Stop starting queued jobs. Running jobs finish."
                    };
                    if ui.button(label).on_hover_text(tip).clicked() {
                        let who = self.who.clone();
                        self.spawn_action(move |l| feed::set_drain(l, cancel, &who));
                    }
                }
                match (&self.error, self.last_ok) {
                    (Some(_), _) => {
                        ui.label(
                            RichText::new("daemon unreachable")
                                .color(phase_color(PhaseClass::Failed, dark)),
                        );
                    }
                    (None, Some(_)) => {
                        ui.label(
                            RichText::new("live").color(phase_color(PhaseClass::Passed, dark)),
                        );
                    }
                    (None, None) => {
                        ui.weak("connecting...");
                    }
                }
            });
        });
        if let Some(e) = &self.error {
            let since = self.last_ok.map_or_else(
                || "never".to_string(),
                |t| format!("{} ago", format::duration(t.elapsed().as_secs_f64())),
            );
            ui.colored_label(
                phase_color(PhaseClass::Failed, dark),
                format!("Cannot reach the daemon: {e}. Showing data from {since}."),
            );
        }
        if let Some(vm) = &self.vm {
            if let Some(b) = &vm.banner {
                egui::Frame::new()
                    .fill(if dark {
                        Color32::from_rgb(80, 60, 10)
                    } else {
                        Color32::from_rgb(255, 238, 190)
                    })
                    .inner_margin(egui::Margin::same(6))
                    .corner_radius(egui::CornerRadius::same(4))
                    .show(ui, |ui| {
                        ui.label(RichText::new(b).strong());
                    });
            }
            for a in &vm.alerts {
                ui.colored_label(phase_color(PhaseClass::Waiting, dark), a);
            }
            for p in &vm.problems {
                ui.colored_label(
                    phase_color(PhaseClass::Failed, dark),
                    format!("policy: {p}"),
                );
            }
        }
    }

    fn dashboard(&mut self, ui: &mut egui::Ui) {
        let Some(vm) = self.vm.clone() else {
            ui.add_space(20.0);
            ui.weak(if self.error.is_some() {
                "No data yet."
            } else {
                "Asking the daemon..."
            });
            return;
        };
        let mut click: Option<i64> = None;
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let per_row = ((ui.available_width() / CARD_W) as usize).max(1);
                for chunk in vm.machines.chunks(per_row) {
                    ui.horizontal_top(|ui| {
                        for m in chunk {
                            ui.vertical(|ui| machine_card(ui, m));
                        }
                    });
                }
                ui.add_space(6.0);
                for (section, rows) in [
                    (Section::Running, &vm.running),
                    (Section::Queued, &vm.queued),
                    (Section::Recent, &vm.recent),
                ] {
                    ui.separator();
                    ui.label(
                        RichText::new(format!("{} ({})", section.title(), rows.len())).strong(),
                    );
                    if rows.is_empty() {
                        ui.weak("none");
                        continue;
                    }
                    if let Some(id) = job_table(ui, section, rows, self.selected) {
                        click = Some(id);
                    }
                }
                if vm.truncated {
                    ui.weak("The job list was cut at the daemon's row limit.");
                }
            });
        if let Some(id) = click {
            self.selected = Some(id);
            self.refresh_selected_row();
        }
    }

    fn details(&mut self, ui: &mut egui::Ui) {
        let Some(row) = self.selected_row.clone() else {
            return;
        };
        let listed = self.vm.as_ref().is_some_and(|vm| {
            vm.running
                .iter()
                .chain(&vm.queued)
                .chain(&vm.recent)
                .any(|r| r.id == row.id)
        });
        ui.horizontal_wrapped(|ui| {
            dot(ui, rgb(row.color));
            ui.label(RichText::new(format!("#{} {}", row.id, row.agent)).strong())
                .on_hover_text(&row.owner);
            ui.label(
                RichText::new(&row.phase).color(phase_color(row.class, ui.visuals().dark_mode)),
            );
            ui.set_min_width(ACTION_BAR_WIDTH.min(ui.available_width()));
            let cancel_ok = row.actions.cancel.is_enabled() && listed;
            let cancel = ui.add_enabled(cancel_ok, egui::Button::new("Cancel..."));
            let cancel = match (&row.actions.cancel, listed) {
                (Avail::Disabled(r), _) => cancel.on_disabled_hover_text(r),
                (Avail::Enabled, false) => {
                    cancel.on_disabled_hover_text("This job is no longer in the list.")
                }
                (Avail::Enabled, true) => cancel.on_hover_text("Ask for a reason, then cancel."),
            };
            if cancel.clicked() {
                self.open_cancel(row.id);
            }
            let log = ui.add_enabled(row.actions.log.is_enabled(), egui::Button::new("Log"));
            let log = match &row.actions.log {
                Avail::Disabled(r) => log.on_disabled_hover_text(r),
                Avail::Enabled => log.on_hover_text("Tail, follow and search the log."),
            };
            if log.clicked() {
                self.open_log(&row);
            }
            if ui.button("Close").clicked() {
                self.selected = None;
                self.selected_row = None;
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let j = &row.job;
            egui::Grid::new("details_grid").num_columns(2).show(ui, |ui| {
                let kv = |ui: &mut egui::Ui, k: &str, v: String| {
                    ui.label(RichText::new(k).weak());
                    ui.add(egui::Label::new(v).wrap());
                    ui.end_row();
                };
                kv(ui, "Owner", j.owner.clone());
                if let Some(f) = &j.figures {
                    kv(ui, "Cost", f.summary());
                }
                kv(ui, "Command", row.command.clone());
                kv(ui, "Directory", j.cwd.display().to_string());
                kv(ui, "Kind / placement", format!("{:?} / {:?}", j.kind, j.placement));
                kv(
                    ui,
                    "Machine",
                    match &j.forwarded_to {
                        Some((m, Some(id))) => format!("{} (forwarded to {m}, remote job #{id})", row.machine),
                        Some((m, None)) => format!("{} (forwarded to {m})", row.machine),
                        None => row.machine.clone(),
                    },
                );
                if let Some(p) = &row.remote_phase {
                    kv(ui, "Remote phase", p.clone());
                }
                if let Some((plan, step)) = &j.plan {
                    kv(ui, "Plan", format!("#{plan} step {step}"));
                }
                if !j.after.is_empty() {
                    let ids: Vec<_> = j.after.iter().map(|i| format!("#{}", i.0)).collect();
                    kv(ui, "After", ids.join(", "));
                }
                kv(
                    ui,
                    "Environment",
                    "Not sent by the daemon. Variables passed on the command line (NAME=value) are in the command above, long values cut.".into(),
                );
                if let Some(w) = &row.wait {
                    kv(ui, "Waiting for", w.clone());
                }
                if !row.queue_class.is_empty() {
                    let why = if row.queue_note.is_empty() {
                        row.queue_class.clone()
                    } else {
                        format!("{}: {}", row.queue_class, row.queue_note)
                    };
                    kv(ui, "Queue class", why);
                }
                if j.priority != tasks_core::Priority::Normal {
                    let mut p = format!("submitted {}", j.priority);
                    if let Some(r) = &j.priority_reason {
                        p.push_str(&format!(" (reason: {r})"));
                    }
                    kv(ui, "Priority", p);
                }
                if let Some(t) = &row.timeout {
                    kv(ui, "Wall-clock limit", t.clone());
                }
                if let Some(e) = j.exit {
                    kv(ui, "Exit", e.to_string());
                }
                if let Some(s) = &row.summary {
                    kv(ui, "Outcome", s.clone());
                }
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Timeline").strong());
            for (t, what) in timeline(j) {
                ui.label(format!("{}  {what}", format::utc_time(t)));
            }
            ui.add_space(4.0);
            let open = self.raw_open.contains(&row.id);
            if ui
                .button(if open { "Hide raw JSON" } else { "Show raw JSON" })
                .clicked()
            {
                if open {
                    self.raw_open.remove(&row.id);
                } else {
                    self.raw_open.insert(row.id);
                }
            }
            if open {
                let text = serde_json::to_string_pretty(j).unwrap_or_default();
                ui.add(egui::Label::new(RichText::new(text).monospace().small()).selectable(true));
            }
        });
    }

    fn cancel_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.cancel else {
            return;
        };
        let mut decision: Option<bool> = None;
        egui::Window::new("Cancel job")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(RichText::new(&d.prompt).strong());
                ui.label("The reason is kept with the job and shown in the tables.");
                ui.add(
                    egui::TextEdit::singleline(&mut d.reason)
                        .hint_text("reason (optional)")
                        .desired_width(340.0),
                );
                ui.horizontal(|ui| {
                    if ui.button(format!("Cancel job #{}", d.id)).clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Keep it running").clicked() {
                        decision = Some(false);
                    }
                });
            });
        match decision {
            Some(true) => {
                if let Some(d) = self.cancel.take() {
                    self.spawn_action(move |l| {
                        feed::cancel_job(l, JobId(d.id), &d.owner, &d.reason)
                    });
                }
            }
            Some(false) => self.cancel = None,
            None => {}
        }
    }

    fn log_window(&mut self, ctx: &egui::Context) {
        let Some(l) = &mut self.log else {
            return;
        };
        let mut open = true;
        let link = Arc::clone(&self.link);
        let wake = Arc::clone(&self.wake);
        egui::Window::new(l.title.clone())
            .id(egui::Id::new("log_window"))
            .open(&mut open)
            .default_size([760.0, 440.0])
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut l.follow, "Follow");
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut l.search)
                            .hint_text("search this log")
                            .desired_width(220.0),
                    );
                    let go = ui.button("Search").clicked()
                        || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if go && !l.search.is_empty() {
                        let (pat, job) = (l.search.clone(), JobId(l.job));
                        let link = Arc::clone(&link);
                        feed::spawn_once(Arc::clone(&l.hits_box), Arc::clone(&wake), move || {
                            feed::search_log(link.as_ref(), job, &pat)
                        });
                    }
                    if ui.button("Clear").clicked() {
                        l.search.clear();
                        l.hits = None;
                    }
                    ui.weak(format!(
                        "{} lines kept{}",
                        l.snap.lines.len(),
                        if l.snap.evicted > 0 {
                            format!(", {} older dropped", l.snap.evicted)
                        } else {
                            String::new()
                        }
                    ));
                    if l.snap.finished {
                        ui.weak("end of log");
                    }
                });
                if let Some(e) = &l.snap.error {
                    ui.colored_label(Color32::from_rgb(220, 90, 80), e);
                }
                ui.separator();
                let mono = egui::TextStyle::Monospace;
                let h = ui.text_style_height(&mono);
                let follow = l.follow && l.hits.is_none();
                match &l.hits {
                    Some(hits) => {
                        egui::ScrollArea::both()
                            .auto_shrink([false, false])
                            .show_rows(ui, h, hits.len(), |ui, range| {
                                for (m, line) in hits.iter().skip(range.start).take(range.len()) {
                                    let t = RichText::new(line).monospace();
                                    ui.label(if *m { t.strong() } else { t });
                                }
                            });
                    }
                    None => {
                        egui::ScrollArea::both()
                            .auto_shrink([false, false])
                            .stick_to_bottom(follow)
                            .show_rows(ui, h, l.snap.lines.len(), |ui, range| {
                                for line in l.snap.lines.iter().skip(range.start).take(range.len())
                                {
                                    ui.label(RichText::new(line).monospace());
                                }
                            });
                    }
                }
            });
        if !open {
            self.log = None;
        }
    }

    fn audit_tab(&mut self, ui: &mut egui::Ui) {
        if !self.audit_asked {
            self.audit_asked = true;
            self.audit_loading = true;
            let link = Arc::clone(&self.link);
            let filter = self.audit_filter.clone();
            feed::spawn_once(
                Arc::clone(&self.audit_box),
                Arc::clone(&self.wake),
                move || audit::fetch(link.as_ref(), &filter),
            );
        }
        let before = self.audit_filter.clone();
        ui.horizontal_wrapped(|ui| {
            ui.label("Action");
            egui::ComboBox::from_id_salt("audit-action")
                .selected_text(if self.audit_filter.action.is_empty() {
                    "all"
                } else {
                    self.audit_filter.action.as_str()
                })
                .show_ui(ui, |ui| {
                    for f in audit::FAMILIES {
                        ui.selectable_value(
                            &mut self.audit_filter.action,
                            f.to_string(),
                            if f.is_empty() { "all" } else { f },
                        );
                    }
                });
            ui.label("Peer");
            ui.add(
                egui::TextEdit::singleline(&mut self.audit_filter.peer)
                    .desired_width(110.0)
                    .hint_text("name or DID"),
            );
            ui.label("Job");
            ui.add(
                egui::TextEdit::singleline(&mut self.audit_filter.job)
                    .desired_width(60.0)
                    .hint_text("id"),
            );
            let apply = ui.button("Apply").clicked();
            if ui
                .add_enabled(!self.audit_loading, egui::Button::new("Refresh"))
                .clicked()
                || apply
                || (self.audit_filter.action != before.action)
            {
                self.audit_asked = false;
            }
            if self.audit_loading {
                ui.weak("loading...");
            }
        });
        match &self.audit {
            None => {
                ui.weak("No audit rows loaded yet.");
            }
            Some(Err(e)) => {
                ui.colored_label(Color32::from_rgb(220, 90, 80), e);
            }
            Some(Ok(d)) => {
                ui.weak(audit::summary(d));
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("audit-table")
                            .striped(true)
                            .num_columns(audit::COLUMNS.len())
                            .show(ui, |ui| {
                                for t in audit::COLUMNS {
                                    ui.label(RichText::new(t).strong());
                                }
                                ui.end_row();
                                for r in &d.rows {
                                    let c = audit::cells(r);
                                    for (i, text) in c.iter().enumerate() {
                                        let l = if i == 0 || i == 4 {
                                            RichText::new(text).monospace()
                                        } else {
                                            RichText::new(text)
                                        };
                                        ui.add(egui::Label::new(l).truncate()).on_hover_text(text);
                                    }
                                    ui.end_row();
                                }
                            });
                    });
            }
        }
    }

    fn stalls_tab(&mut self, ui: &mut egui::Ui) {
        if !self.stalls_asked {
            self.stalls_asked = true;
            self.stalls_loading = true;
            let link = Arc::clone(&self.link);
            feed::spawn_once(
                Arc::clone(&self.stalls_box),
                Arc::clone(&self.wake),
                move || stalls::fetch(link.as_ref()),
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.stalls_loading, egui::Button::new("Refresh"))
                .clicked()
            {
                self.stalls_asked = false;
            }
            if self.stalls_loading {
                ui.weak("loading...");
            }
        });
        match &self.stalls {
            None => {
                ui.weak("No pressure trips loaded yet.");
            }
            Some(Err(e)) => {
                ui.colored_label(Color32::from_rgb(220, 90, 80), e);
            }
            Some(Ok(d)) => {
                ui.weak(stalls::summary(d));
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("stalls-table")
                            .striped(true)
                            .num_columns(stalls::COLUMNS.len())
                            .show(ui, |ui| {
                                for t in stalls::COLUMNS {
                                    ui.label(RichText::new(t).strong());
                                }
                                ui.end_row();
                                for r in &d.rows {
                                    for (i, text) in stalls::cells(r).iter().enumerate() {
                                        let l = if i == 0 {
                                            RichText::new(text).monospace()
                                        } else {
                                            RichText::new(text)
                                        };
                                        ui.add(egui::Label::new(l).truncate()).on_hover_text(text);
                                    }
                                    ui.end_row();
                                }
                            });
                    });
            }
        }
    }

    fn stats_tab(&mut self, ui: &mut egui::Ui) {
        if !self.stats_asked {
            self.stats_asked = true;
            self.history_loading = true;
            let link = Arc::clone(&self.link);
            feed::spawn_once(
                Arc::clone(&self.history_box),
                Arc::clone(&self.wake),
                move || feed::fetch_history(link.as_ref()),
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.history_loading, egui::Button::new("Refresh"))
                .clicked()
            {
                self.stats_asked = false;
            }
            if self.history_loading {
                ui.weak("loading...");
            }
        });
        match &self.history {
            None => {
                ui.weak("No history loaded yet.");
            }
            Some(Err(e)) => {
                ui.colored_label(Color32::from_rgb(220, 90, 80), e);
            }
            Some(Ok(h)) => {
                let rows = stats::rows(h);
                if rows.is_empty() {
                    ui.weak("No timing samples yet.");
                }
                if h.truncated {
                    ui.weak("The daemon cut the history at its bounds.");
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut recipes: Vec<&str> =
                            rows.iter().map(|r| r.recipe.as_str()).collect();
                        recipes.dedup();
                        for recipe in recipes {
                            egui::CollapsingHeader::new(RichText::new(recipe).strong())
                                .id_salt(("recipe", recipe))
                                .default_open(true)
                                .show(ui, |ui| {
                                    egui::Grid::new(("stats", recipe))
                                        .striped(true)
                                        .num_columns(9)
                                        .show(ui, |ui| {
                                            for t in [
                                                "Machine",
                                                "Target",
                                                "Samples",
                                                "Passed",
                                                "Median",
                                                "p90",
                                                "Last",
                                                "Last result",
                                                "Cold",
                                            ] {
                                                ui.label(RichText::new(t).strong());
                                            }
                                            ui.end_row();
                                            for r in rows.iter().filter(|r| r.recipe == recipe) {
                                                let secs = |v: Option<f64>| {
                                                    v.map_or_else(
                                                        || format::UNKNOWN.to_string(),
                                                        format::duration,
                                                    )
                                                };
                                                ui.label(&r.machine);
                                                ui.label(
                                                    r.target.as_deref().unwrap_or("(whole job)"),
                                                );
                                                ui.label(r.samples.to_string());
                                                ui.label(r.passed.to_string());
                                                ui.label(secs(r.median));
                                                ui.label(secs(r.p90));
                                                ui.label(secs(r.last));
                                                ui.label(
                                                    r.last_outcome
                                                        .as_deref()
                                                        .unwrap_or(format::UNKNOWN),
                                                );
                                                ui.label(r.cold.to_string());
                                                ui.end_row();
                                            }
                                        });
                                });
                        }
                    });
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
        // ages tick between refreshes
        ctx.request_repaint_after(Duration::from_secs(2));
    }
}

fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

fn timeline(j: &tasks_daemon::api::JobView) -> Vec<(i64, String)> {
    let mut t = vec![(j.submitted_at_ms, "submitted".to_string())];
    if let Some(s) = j.started_at_ms {
        let on = match &j.forwarded_to {
            Some((m, _)) => format!("started (on {m})"),
            None => "started".to_string(),
        };
        t.push((s, on));
    }
    if let Some(f) = j.finished_at_ms {
        let mut w = format!("finished: {}", model::phase_text(&j.phase).0);
        if let Some(by) = &j.cancel_requested_by {
            w.push_str(&format!(", cancel asked by {by}"));
            if let Some(r) = &j.cancel_reason {
                w.push_str(&format!(" ({r})"));
            }
        }
        t.push((f, w));
    }
    t.sort_by_key(|(at, _)| *at);
    t
}

fn machine_card(ui: &mut egui::Ui, m: &MachineCard) {
    let dark = ui.visuals().dark_mode;
    let fill = if dark {
        Color32::from_gray(32)
    } else {
        Color32::from_gray(244)
    };
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(
            1.0,
            ui.visuals().widgets.noninteractive.bg_stroke.color,
        ))
        .inner_margin(egui::Margin::same(8))
        .corner_radius(egui::CornerRadius::same(6))
        .show(ui, |ui| {
            ui.set_width(300.0);
            if m.greyed {
                ui.set_opacity(0.6);
            }
            ui.horizontal(|ui| {
                dot(ui, online_color(m.online));
                ui.label(RichText::new(&m.name).heading());
                if m.local {
                    ui.weak("this machine");
                }
            });
            ui.label(&m.status_text);
            ui.label(&m.cpu);
            ui.label(&m.ram);
            ui.label(&m.swap);
            ui.label(format!("Pressure: {}", m.pressure));
            if let Some(b) = &m.blocker {
                ui.colored_label(
                    phase_color(PhaseClass::Waiting, dark),
                    format!("New compiles wait: {b}"),
                );
            }
            ui.label(format!("{} running, {} waiting", m.running, m.waiting));
            for s in &m.slots {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("{} {}/{} free", s.kind, s.free, s.total)).strong(),
                    );
                    for h in &s.holders {
                        if let Some(c) = h.color {
                            dot(ui, rgb(c));
                        }
                        ui.label(&h.label);
                    }
                });
            }
            if !m.agents.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.weak("agents here:");
                    for (a, c, n) in &m.agents {
                        dot(ui, rgb(*c));
                        ui.label(format!("{a} ({n})"));
                    }
                });
            }
            for n in &m.notes {
                ui.weak(n);
            }
        });
}

/// Draws one table; returns the job whose row was clicked.
fn job_table(
    ui: &mut egui::Ui,
    section: Section,
    rows: &[JobRow],
    selected: Option<i64>,
) -> Option<i64> {
    let cols = columns::visible(section, ui.available_width().max(MIN_WINDOW_WIDTH));
    let fixed: f32 = cols
        .iter()
        .filter(|c| c.col != Col::Command)
        .map(|c| c.width)
        .sum::<f32>()
        + GAP * cols.len().saturating_sub(1) as f32;
    let command_w = (ui.available_width() - fixed).max(200.0);
    let width_of = |c: &ColSpec| {
        if c.col == Col::Command {
            command_w
        } else {
            c.width
        }
    };
    let mut clicked = None;
    let dark = ui.visuals().dark_mode;

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = GAP;
        for c in &cols {
            ui.allocate_ui_with_layout(
                vec2(width_of(c), ROW_H),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_min_width(width_of(c));
                    ui.label(RichText::new(c.title).weak().small());
                },
            );
        }
    });
    for row in rows {
        let sel = selected == Some(row.id);
        let fill = if sel {
            ui.visuals().selection.bg_fill.gamma_multiply(0.5)
        } else {
            Color32::TRANSPARENT
        };
        egui::Frame::new().fill(fill).show(ui, |ui| {
            if row.stale {
                ui.set_opacity(0.55);
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                for c in &cols {
                    ui.allocate_ui_with_layout(
                        vec2(width_of(c), ROW_H),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            ui.set_min_width(width_of(c));
                            ui.set_max_width(width_of(c));
                            if let Some(r) = cell(ui, row, c.col, dark)
                                && r.clicked()
                            {
                                clicked = Some(row.id);
                            }
                        },
                    );
                }
            });
        });
    }
    clicked
}

/// Draws one cell. Returns the response of the part that selects the row.
fn cell(ui: &mut egui::Ui, row: &JobRow, col: Col, dark: bool) -> Option<egui::Response> {
    let text = columns::cell(row, col);
    match col {
        Col::Id => {
            Some(ui.add(egui::Label::new(RichText::new(text).monospace()).sense(Sense::click())))
        }
        Col::Agent => {
            dot(ui, rgb(row.color));
            ui.add(egui::Label::new(RichText::new(&row.agent).color(rgb(row.color))).truncate())
                .on_hover_text(&row.owner);
            None
        }
        Col::Command => {
            let r = ui
                .add(
                    egui::Label::new(RichText::new(&text).monospace())
                        .truncate()
                        .sense(Sense::click()),
                )
                .on_hover_text(&text);
            Some(r)
        }
        Col::Phase => {
            let r = ui.add(
                egui::Label::new(RichText::new(&text).color(phase_color(row.class, dark)))
                    .truncate(),
            );
            if let Some(s) = &row.summary {
                r.on_hover_text(s);
            }
            None
        }
        Col::Progress => {
            match row.progress {
                Some(f) => {
                    ui.add(
                        egui::ProgressBar::new(f)
                            .desired_width(70.0)
                            .desired_height(10.0),
                    );
                    ui.label(&text);
                }
                None => {
                    ui.weak(&text);
                }
            }
            None
        }
        Col::Machine => {
            let color = if row.remote {
                Color32::from_rgb(120, 160, 230)
            } else {
                ui.visuals().weak_text_color()
            };
            ui.label(RichText::new(&text).color(color).small());
            None
        }
        Col::Eta => {
            let r = ui.add(egui::Label::new(&text).truncate());
            if !row.eta_basis.is_empty() {
                r.on_hover_text(format!("ETA basis: {}", row.eta_basis));
            }
            None
        }
        Col::Wait => {
            let r = ui.add(
                egui::Label::new(
                    RichText::new(&text).color(phase_color(PhaseClass::Waiting, dark)),
                )
                .truncate(),
            );
            r.on_hover_text(&text);
            None
        }
        _ => {
            ui.add(egui::Label::new(&text).truncate())
                .on_hover_text(&text);
            None
        }
    }
}

/// Starts the window and blocks until it closes.
pub fn run(link: Arc<dyn DaemonLink>, who: String) -> Result<(), String> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("tasks")
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([MIN_WINDOW_WIDTH, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "tasks",
        opts,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let wake: Wake = Arc::new(move || ctx.request_repaint());
            Ok(Box::new(App::new(link, who, wake)))
        }),
    )
    .map_err(|e| e.to_string())
}
