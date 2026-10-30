//! The egui window. Drawing only: every decision about text, order and availability comes
//! from the view model, and every daemon call runs on a feed thread (R-200).

use crate::actions::{self, Avail};
use crate::audit::{self, AuditData, AuditFilter};
use crate::columns::{self, MIN_WINDOW_WIDTH};
use crate::detail;
use crate::disk;
use crate::feed::{self, ActionOutcome, Dashboard, DashboardResult, Feed, Wake};
use crate::format;
use crate::link::DaemonLink;
use crate::logview::{LogSnapshot, SearchHits};
use crate::mailbox::Mailbox;
use crate::model::{self, JobRow, NoticeLevel, Online, Section, Tab, ViewModel};
use crate::paint::{self, PlanClick};
use crate::plans::{self, Entry, PlanRow};
use crate::resources;
use crate::stalls::{self, StallData};
use crate::stats;
use crate::theme::{self, LABEL, Palette, SECONDARY};
use eframe::egui::{
    self, Align, Color32, Key, Layout, Margin, Modifiers, RichText, Sense, Stroke, vec2,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tasks_core::JobId;
use tasks_core::metrics::JobMetricsView;
use tasks_daemon::api::GcView;
use tasks_daemon::api::HistoryView;

const REFRESH: Duration = Duration::from_secs(1);
/// Width of the side panel, and the window width from which it sits beside the list.
const DETAIL_W: f32 = 440.0;
/// Narrowest the side panel can be dragged to.
const DETAIL_MIN_W: f32 = 340.0;
/// Width the job list keeps however wide the side panel is dragged.
const LIST_MIN_W: f32 = 480.0;
const SIDE_BY_SIDE_FROM: f32 = 960.0;

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

/// The live metrics of the job the side panel shows.
struct MetricsState {
    job: i64,
    /// The newest good answer.
    view: Option<JobMetricsView>,
    /// Why the newest request failed.
    error: Option<String>,
    /// When the last request went out.
    asked_at: Option<Instant>,
    /// Whether the job still ran when it did.
    asked_live: bool,
    /// A request is out.
    inflight: bool,
}

impl MetricsState {
    fn new(job: i64) -> MetricsState {
        MetricsState {
            job,
            view: None,
            error: None,
            asked_at: None,
            asked_live: false,
            inflight: false,
        }
    }
}

/// An answer to a metrics request, with the job it was for.
type MetricsReply = (i64, Result<JobMetricsView, String>);

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
    scroll_to_selected: bool,
    raw_open: BTreeSet<i64>,
    /// Plans the user opened or closed, by plan id; the rest follow `open_by_default`.
    plan_open: BTreeMap<i64, bool>,
    /// Where each row was drawn last frame, by selection key (a job id or a plan key).
    row_rects: BTreeMap<i64, egui::Rect>,
    cancel: Option<CancelDialog>,
    confirm_drain: bool,
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
    disk_box: Arc<Mailbox<Result<GcView, String>>>,
    disk: Option<Result<GcView, String>>,
    disk_loading: bool,
    disk_asked: bool,
    /// Rows ticked for freeing, by the name the report gives them.
    disk_ticked: BTreeSet<String>,
    confirm_free: bool,
    metrics_box: Arc<Mailbox<MetricsReply>>,
    metrics: Option<MetricsState>,
    tz: jiff::tz::TimeZone,
    home: Option<PathBuf>,
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
            tab: Tab::Queue,
            selected: None,
            selected_row: None,
            scroll_to_selected: false,
            raw_open: BTreeSet::new(),
            plan_open: BTreeMap::new(),
            row_rects: BTreeMap::new(),
            cancel: None,
            confirm_drain: false,
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
            disk_box: Arc::new(Mailbox::new()),
            disk: None,
            disk_loading: false,
            disk_asked: false,
            disk_ticked: BTreeSet::new(),
            confirm_free: false,
            metrics_box: Arc::new(Mailbox::new()),
            metrics: None,
            tz: jiff::tz::TimeZone::system(),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }

    /// A window showing fixed data and calling nobody, for screenshots and tests.
    pub fn fixed(dash: &Dashboard, link: Arc<dyn DaemonLink>) -> App {
        let mut app = App::bare(link, "screenshot".into(), Arc::new(|| {}));
        app.vm = Some(model::build(dash, 0));
        app.last_ok = Some(Instant::now());
        // the fixtures live under /home/demo, and screenshots should not depend on the zone
        // of the machine that renders them
        app.home = Some(PathBuf::from("/home/demo"));
        app.tz = jiff::tz::TimeZone::UTC;
        app
    }

    /// Shows a tab (for screenshots).
    pub fn show_tab(&mut self, tab: Tab) {
        self.tab = tab;
    }

    /// Selects a job so the side panel shows it (for screenshots).
    pub fn select(&mut self, id: i64) {
        self.selected = Some(id);
        self.refresh_selected_row();
    }

    /// Selects a plan's parent row so the side panel shows the plan (for screenshots).
    pub fn select_plan(&mut self, plan_id: i64) {
        self.selected = Some(plans::plan_key(plan_id));
        self.refresh_selected_row();
    }

    /// Opens or closes a plan's steps.
    pub fn set_plan_open(&mut self, plan_id: i64, open: bool) {
        self.plan_open.insert(plan_id, open);
    }

    fn is_open(&self, plan: &PlanRow) -> bool {
        self.plan_open
            .get(&plan.plan_id)
            .copied()
            .unwrap_or(plan.open_by_default)
    }

    /// Where the row with this selection key was drawn in the last frame.
    pub fn row_rect(&self, key: i64) -> Option<egui::Rect> {
        self.row_rects.get(&key).copied()
    }

    /// Selects a step of a plan from the side panel, opening the plan so the row is on screen.
    fn select_step(&mut self, id: i64) {
        if let Some(pid) = self
            .vm
            .as_ref()
            .and_then(|vm| vm.plan_of_job(id))
            .map(|p| p.plan_id)
        {
            self.plan_open.insert(pid, true);
        }
        self.selected = Some(id);
        self.scroll_to_selected = true;
        self.refresh_selected_row();
    }

    /// The plan the selection stands for, when it is a plan's parent row.
    fn selected_plan(&self) -> Option<&PlanRow> {
        let pid = plans::plan_of_key(self.selected?)?;
        self.vm.as_ref()?.plan(pid)
    }

    /// Gives the Timings tab a history (for screenshots).
    pub fn set_history(&mut self, h: HistoryView) {
        self.history = Some(Ok(h));
        self.stats_asked = true;
    }

    /// Gives the Audit tab its rows (for screenshots).
    pub fn set_audit(&mut self, data: AuditData) {
        self.audit = Some(Ok(data));
        self.audit_asked = true;
    }

    /// Gives the Disk tab its report (for screenshots and tests).
    pub fn set_disk(&mut self, view: GcView) {
        disk::prune(&view, &mut self.disk_ticked);
        self.disk = Some(Ok(view));
        self.disk_asked = true;
    }

    /// Ticks a Disk row, as a click on it does.
    pub fn tick_disk(&mut self, checkout: &str) {
        self.disk_ticked.insert(checkout.to_string());
    }

    /// Opens the question before freeing the ticked rows, as the Free button does.
    pub fn ask_free(&mut self) {
        self.confirm_free = !self.disk_ticked.is_empty();
    }

    /// Gives the Stalls tab its rows (for screenshots).
    pub fn set_stalls(&mut self, data: StallData) {
        self.stalls = Some(Ok(data));
        self.stalls_asked = true;
    }

    /// Gives the side panel the live metrics of a job (for screenshots and tests).
    pub fn set_metrics(&mut self, id: i64, view: JobMetricsView) {
        let live = self
            .vm
            .as_ref()
            .and_then(|vm| vm.find(id))
            .is_some_and(|r| r.section == Section::Running);
        let mut st = MetricsState::new(id);
        st.view = Some(view);
        st.asked_at = Some(Instant::now());
        st.asked_live = live;
        self.metrics = Some(st);
    }

    /// Makes the side panel show that the metrics request for a job failed (for screenshots).
    pub fn set_metrics_error(&mut self, id: i64, error: &str) {
        let mut st = MetricsState::new(id);
        st.error = Some(error.to_string());
        st.asked_at = Some(Instant::now());
        self.metrics = Some(st);
    }

    /// Opens the cancel dialog for a job (for screenshots).
    pub fn ask_cancel(&mut self, id: i64) {
        self.open_cancel(id);
    }

    /// Opens the drain confirmation (for screenshots and tests).
    pub fn ask_drain(&mut self) {
        self.confirm_drain = true;
    }

    /// The selected job's id.
    pub fn selected_id(&self) -> Option<i64> {
        self.selected
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
        if plans::plan_of_key(id).is_some() {
            self.selected_row = None;
        } else if let Some(r) = vm.find(id) {
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
        if let Some((id, res)) = self.metrics_box.take()
            && let Some(m) = self.metrics.as_mut().filter(|m| m.job == id)
        {
            m.inflight = false;
            match res {
                Ok(v) => {
                    m.view = Some(v);
                    m.error = None;
                }
                Err(e) => m.error = Some(e),
            }
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
        if let Some(d) = self.disk_box.take() {
            if let Ok(v) = &d {
                disk::prune(v, &mut self.disk_ticked);
            }
            self.disk = Some(d);
            self.disk_loading = false;
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

    /// Asks for the shown job's metrics when it is time: every two seconds while it runs, and
    /// for a job that just ended once more. Nothing is asked for any other job (R-645).
    fn poll_metrics(&mut self, row: &JobRow) {
        if self.metrics.as_ref().is_none_or(|m| m.job != row.id) {
            self.metrics = Some(MetricsState::new(row.id));
        }
        let Some(m) = self.metrics.as_mut() else {
            return;
        };
        let wants = resources::wants(row);
        if m.inflight || !resources::due(wants, m.asked_at.map(|t| t.elapsed()), m.asked_live) {
            return;
        }
        m.inflight = true;
        m.asked_at = Some(Instant::now());
        m.asked_live = wants == resources::Wants::Live;
        let link = Arc::clone(&self.link);
        let id = JobId(row.id);
        let n = row.id;
        feed::spawn_once(
            Arc::clone(&self.metrics_box),
            Arc::clone(&self.wake),
            move || (n, feed::fetch_metrics(link.as_ref(), id)),
        );
    }

    /// How many metrics requests are out or answered for the shown job (for tests).
    pub fn metrics_requested(&self) -> bool {
        self.metrics.as_ref().is_some_and(|m| m.asked_at.is_some())
    }

    fn open_cancel(&mut self, id: i64) {
        let row = self.vm.as_ref().and_then(|vm| vm.find(id)).cloned();
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
        let p = Palette::current(ctx);
        self.keys(ctx);

        egui::TopBottomPanel::top("bar")
            .exact_height(40.0)
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .inner_margin(Margin::symmetric(14, 0)),
            )
            .show(ctx, |ui| self.top_bar(ui, p));
        self.notice_rows(ctx, p);
        egui::TopBottomPanel::bottom("foot")
            .exact_height(26.0)
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .inner_margin(Margin::symmetric(14, 0)),
            )
            .show(ctx, |ui| self.footer(ui, p));

        let listing = matches!(self.tab, Tab::Queue | Tab::History);
        if self.tab == Tab::Queue
            && let Some(vm) = &self.vm
        {
            egui::TopBottomPanel::top("machines")
                .frame(egui::Frame::new().fill(p.panel))
                .show_separator_line(false)
                .show(ctx, |ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                    paint::machine_strip(ui, p, &vm.machines);
                });
        }
        let wide = ctx.content_rect().width() >= SIDE_BY_SIDE_FROM;
        if listing && self.vm.is_some() {
            let frame = egui::Frame::new().fill(p.panel);
            if wide {
                // drag the edge to resize; the job list always keeps LIST_MIN_W
                let max_w = (ctx.content_rect().width() - LIST_MIN_W).max(DETAIL_MIN_W);
                egui::SidePanel::right("detail")
                    .default_width(DETAIL_W)
                    .width_range(DETAIL_MIN_W..=max_w)
                    .resizable(true)
                    .frame(frame)
                    .show(ctx, |ui| self.side_panel(ui, p));
            } else if self.selected_row.is_some() || self.selected_plan().is_some() {
                egui::TopBottomPanel::bottom("detail-below")
                    .resizable(true)
                    .default_height(330.0)
                    .min_height(140.0)
                    .frame(frame)
                    .show(ctx, |ui| self.side_panel(ui, p));
            }
        }
        let central = if listing {
            egui::Frame::new().fill(p.win)
        } else {
            egui::Frame::new()
                .fill(p.win)
                .inner_margin(Margin::symmetric(14, 12))
        };
        egui::CentralPanel::default()
            .frame(central)
            .show(ctx, |ui| match self.tab {
                Tab::Queue | Tab::History => self.job_list(ui, p),
                Tab::Timings => self.stats_tab(ui, p),
                Tab::Audit => self.audit_tab(ui, p),
                Tab::Stalls => self.stalls_tab(ui, p),
                Tab::Disk => self.disk_tab(ui, p),
            });
        self.cancel_dialog(ctx);
        self.drain_dialog(ctx);
        self.free_dialog(ctx);
        self.log_window(ctx);
    }

    /// Arrow keys move the selection, Enter opens the log, Escape lets go (R-587).
    fn keys(&mut self, ctx: &egui::Context) {
        if !matches!(self.tab, Tab::Queue | Tab::History)
            || ctx.wants_keyboard_input()
            || self.cancel.is_some()
            || self.confirm_drain
            || self.confirm_free
        {
            return;
        }
        let (down, up, enter, esc, right, left) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::ArrowRight),
                i.consume_key(Modifiers::NONE, Key::ArrowLeft),
            )
        });
        if down || up {
            if let Some(vm) = &self.vm {
                // only the rows on screen: the steps of a closed plan are skipped
                let ids = plans::visible_ids(vm, self.tab, &|p| self.is_open(p));
                self.selected = model::step_selection(&ids, self.selected, if up { -1 } else { 1 });
                self.scroll_to_selected = true;
            }
            self.refresh_selected_row();
        }
        if right && let Some(pid) = self.selected_plan().map(|p| p.plan_id) {
            self.plan_open.insert(pid, true);
        }
        if left {
            // closing a plan, or from one of its steps back up to it
            if let Some(pid) = self.selected_plan().map(|p| p.plan_id) {
                self.plan_open.insert(pid, false);
            } else if let Some(pid) = self
                .selected
                .and_then(|id| self.vm.as_ref()?.plan_of_job(id))
                .map(|p| p.plan_id)
            {
                self.selected = Some(plans::plan_key(pid));
                self.scroll_to_selected = true;
                self.refresh_selected_row();
            }
        }
        if esc {
            self.selected = None;
            self.selected_row = None;
        }
        if enter
            && let Some(row) = self.selected_row.clone()
            && row.actions.log.is_enabled()
        {
            self.open_log(&row);
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.label(
                RichText::new(NAME)
                    .font(theme::sans_semibold(theme::TITLE))
                    .color(p.ink),
            );
            ui.add_space(14.0);
            let open = self.vm.as_ref().map(ViewModel::open_jobs);
            for t in Tab::SHOWN {
                let count = if t == Tab::Queue { open } else { None };
                if tab_button(ui, p, t.label(), count, self.tab == t) {
                    self.tab = t;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.more_menu(ui, p);
                ui.add_space(6.0);
                let (color, text) = match (&self.error, self.last_ok, &self.vm) {
                    (Some(_), _, _) => (p.bad, "daemon unreachable".to_string()),
                    (None, Some(_), Some(vm)) => (p.ok, live_text(vm)),
                    _ => (p.faint, "connecting…".to_string()),
                };
                ui.label(
                    RichText::new(text)
                        .font(theme::sans(SECONDARY))
                        .color(p.dim),
                );
                let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, color);
            });
        });
    }

    /// The `⋯` menu: operator actions that are not navigation (R-589).
    fn more_menu(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 24.0), Sense::click());
        let fill = if resp.hovered() {
            p.raise
        } else {
            Color32::TRANSPARENT
        };
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(4), fill);
        for dx in [-5.0, 0.0, 5.0] {
            ui.painter()
                .circle_filled(rect.center() + vec2(dx, 0.0), 1.6, p.dim);
        }
        let resp = resp.on_hover_text("More");
        let draining = self.vm.as_ref().is_some_and(|vm| vm.draining);
        let mut resume = false;
        let mut ask_drain = false;
        egui::Popup::menu(&resp).show(|ui| {
            ui.set_min_width(200.0);
            if draining {
                if ui
                    .button("Resume queue")
                    .on_hover_text("Let queued jobs start again.")
                    .clicked()
                {
                    resume = true;
                    ui.close();
                }
            } else if ui
                .button("Drain queue…")
                .on_hover_text("Stop starting queued jobs. Running jobs finish.")
                .clicked()
            {
                ask_drain = true;
                ui.close();
            }
        });
        if resume {
            let who = self.who.clone();
            self.spawn_action(move |l| feed::set_drain(l, true, &who));
        }
        if ask_drain {
            self.confirm_drain = true;
        }
    }

    /// One full-width row per notice: bold title, plain sentence (R-582).
    fn notice_rows(&self, ctx: &egui::Context, p: &Palette) {
        let mut rows: Vec<(NoticeLevel, String, String)> = Vec::new();
        if let Some(e) = &self.error {
            let since = self.last_ok.map_or_else(
                || "never".to_string(),
                |t| format!("{} ago", format::duration(t.elapsed().as_secs_f64())),
            );
            rows.push((
                NoticeLevel::Critical,
                "Cannot reach the daemon".into(),
                format!("{e}. Showing data from {since}."),
            ));
        }
        if let Some(vm) = &self.vm {
            rows.extend(
                vm.notices
                    .iter()
                    .map(|n| (n.level, n.title.clone(), n.text.clone())),
            );
        }
        if rows.is_empty() {
            return;
        }
        egui::TopBottomPanel::top("notices")
            .frame(egui::Frame::new().fill(p.win))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (level, title, text) in rows {
                    let (tint, accent) = match level {
                        NoticeLevel::Info => (p.raise, p.dim),
                        NoticeLevel::Warn => (theme::mix(p.win, p.warn, 10), p.warn),
                        NoticeLevel::Critical => (theme::mix(p.win, p.bad, 10), p.bad),
                    };
                    egui::Frame::new()
                        .fill(tint)
                        .inner_margin(Margin::symmetric(14, 8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 10.0;
                                ui.label(
                                    RichText::new(title)
                                        .font(theme::sans_semibold(theme::BODY))
                                        .color(accent),
                                );
                                ui.label(
                                    RichText::new(text)
                                        .font(theme::sans(SECONDARY))
                                        .color(p.dim),
                                );
                            });
                        });
                    let (r, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                    ui.painter()
                        .hline(r.x_range(), r.min.y, Stroke::new(1.0_f32, p.line));
                }
            });
    }

    fn footer(&self, ui: &mut egui::Ui, p: &Palette) {
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            match &self.status_line {
                Some((ok, t)) => {
                    ui.label(
                        RichText::new(t)
                            .font(theme::sans(LABEL + 1.0))
                            .color(if *ok { p.ok } else { p.bad }),
                    );
                }
                None => {
                    let hint = if matches!(self.tab, Tab::Queue | Tab::History) {
                        "↑↓ select  ·  Enter log  ·  Esc deselect"
                    } else {
                        ""
                    };
                    ui.label(
                        RichText::new(hint)
                            .font(theme::sans(LABEL + 1.0))
                            .color(p.faint),
                    );
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let text = match self.last_ok {
                    Some(t) => format!("updated {}s ago", t.elapsed().as_secs()),
                    None => "no data yet".to_string(),
                };
                ui.label(
                    RichText::new(text)
                        .font(theme::sans(LABEL + 1.0))
                        .color(p.faint),
                );
                // this machine's relay and addresses; a missing transport is worth the warning colour
                if let Some(vm) = &self.vm
                    && let Some(line) = &vm.endpoint_line
                {
                    let colour = if line.ends_with("no peer transport") {
                        p.bad
                    } else {
                        p.faint
                    };
                    ui.add(
                        egui::Label::new(
                            RichText::new(line)
                                .font(theme::sans(LABEL + 1.0))
                                .color(colour),
                        )
                        .truncate(),
                    );
                }
            });
        });
    }

    /// The one list of jobs (Queue) or all finished jobs (History).
    fn job_list(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let Some(vm) = self.vm.clone() else {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(if self.error.is_some() {
                        "No data yet."
                    } else {
                        "Asking the daemon…"
                    })
                    .color(p.faint),
                );
            });
            return;
        };
        let groups: Vec<(Section, &str, Vec<Entry<'_>>, String)> = if self.tab == Tab::History {
            vec![(
                Section::Recent,
                "Finished",
                vm.entries(Tab::History, Section::Recent),
                "newest first".to_string(),
            )]
        } else {
            vec![
                (
                    Section::Running,
                    "Running",
                    vm.entries(Tab::Queue, Section::Running),
                    String::new(),
                ),
                (
                    Section::Queued,
                    "Waiting",
                    vm.entries(Tab::Queue, Section::Queued),
                    "in start order".to_string(),
                ),
                (
                    Section::Recent,
                    "Finished",
                    vm.entries(Tab::Queue, Section::Recent),
                    "last hour · the rest is in History".to_string(),
                ),
            ]
        };
        let mut click: Option<i64> = None;
        let mut toggle: Option<i64> = None;
        let mut rects: BTreeMap<i64, egui::Rect> = BTreeMap::new();
        let selected = self.selected;
        let scroll_to = std::mem::take(&mut self.scroll_to_selected);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                let cols =
                    columns::visible(ui.available_width().max(MIN_WINDOW_WIDTH) - 2.0 * paint::PAD);
                let mut any = false;
                for (_, title, entries, note) in &groups {
                    if entries.is_empty() {
                        continue;
                    }
                    any = true;
                    paint::group_header(ui, p, title, entries.len(), note);
                    for entry in entries {
                        match entry {
                            Entry::Job(row) => {
                                let sel = selected == Some(row.id);
                                let resp = ui
                                    .scope(|ui| {
                                        if row.stale {
                                            ui.set_opacity(0.55);
                                        }
                                        paint::job_row(ui, p, row, &cols, sel)
                                    })
                                    .inner;
                                rects.insert(row.id, resp.rect);
                                if sel && scroll_to {
                                    resp.scroll_to_me(None);
                                }
                                if resp.clicked() {
                                    click = Some(row.id);
                                }
                            }
                            Entry::Plan(plan) => {
                                let key = plans::plan_key(plan.plan_id);
                                let sel = selected == Some(key);
                                let open = self
                                    .plan_open
                                    .get(&plan.plan_id)
                                    .copied()
                                    .unwrap_or(plan.open_by_default);
                                let (resp, hit) = paint::plan_row(ui, p, plan, &cols, sel, open);
                                rects.insert(key, resp.rect);
                                if sel && scroll_to {
                                    resp.scroll_to_me(None);
                                }
                                match hit {
                                    PlanClick::Select => click = Some(key),
                                    PlanClick::Toggle => toggle = Some(plan.plan_id),
                                    PlanClick::None => {}
                                }
                                if !open {
                                    continue;
                                }
                                for step in &plan.steps {
                                    let sel = selected == Some(step.id);
                                    let resp = paint::step_row(ui, p, step, &cols, sel);
                                    rects.insert(step.id, resp.rect);
                                    if sel && scroll_to {
                                        resp.scroll_to_me(None);
                                    }
                                    if resp.clicked() {
                                        click = Some(step.id);
                                    }
                                }
                            }
                        }
                    }
                }
                if !any {
                    ui.add_space(24.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(if self.tab == Tab::History {
                                "No finished jobs yet."
                            } else {
                                "Nothing is running or waiting."
                            })
                            .font(theme::sans(theme::BODY))
                            .color(p.faint),
                        );
                    });
                }
                if vm.truncated {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("The job list was cut at the daemon's row limit.")
                            .font(theme::sans(SECONDARY))
                            .color(p.faint),
                    );
                }
            });
        self.row_rects = rects;
        if let Some(pid) = toggle {
            let now = self
                .vm
                .as_ref()
                .and_then(|vm| vm.plan(pid))
                .is_some_and(|p| self.is_open(p));
            self.plan_open.insert(pid, !now);
        }
        if let Some(id) = click {
            self.selected = Some(id);
            self.refresh_selected_row();
        }
    }

    /// One job in depth: who and how it is going, what it can do, when, and the facts.
    fn side_panel(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if let Some(plan) = self.selected_plan().cloned() {
            self.plan_panel(ui, p, &plan);
            return;
        }
        let Some(row) = self.selected_row.clone() else {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Select a job to see its phases, folder and actions.")
                        .font(theme::sans(SECONDARY))
                        .color(p.faint),
                );
            });
            return;
        };
        let now_ms = self.vm.as_ref().map_or(0, |vm| vm.now_ms);
        let listed = self.vm.as_ref().is_some_and(|vm| vm.find(row.id).is_some());
        let d = detail::detail(&row, now_ms, &self.tz, self.home.as_deref());
        self.poll_metrics(&row);
        let cores = self.vm.as_ref().and_then(|vm| {
            vm.machines
                .iter()
                .find(|m| m.name == row.machine)
                .and_then(|m| m.cpu_load.cores)
        });
        let panel = resources::panel(
            &row,
            self.metrics
                .as_ref()
                .filter(|m| m.job == row.id)
                .map_or_else(resources::Input::default, |m| resources::Input {
                    view: m.view.as_ref(),
                    error: m.error.as_deref(),
                    loading: m.inflight && m.view.is_none(),
                    cores,
                }),
        );
        let mut open_log = false;
        let mut open_cancel = false;
        let mut move_up = false;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
                let pad = Margin::symmetric(16, 14);
                // header
                egui::Frame::new().inner_margin(pad).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("#{}", row.id))
                                .font(theme::mono(theme::BODY - 0.5))
                                .color(p.faint),
                        );
                        agent_label(ui, p, &row);
                        // a long state (a refusal reason) is cut here; it is in full under Outcome
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&d.state)
                                        .font(theme::sans_medium(SECONDARY))
                                        .color(paint::tone_color(p, d.tone)),
                                )
                                .truncate(),
                            )
                            .on_hover_text(&d.state);
                        });
                    });
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(&row.command)
                                .font(theme::mono_medium(theme::BODY))
                                .color(p.ink),
                        )
                        .selectable(true)
                        .wrap(),
                    );
                    if let Some((pid, _)) = &row.job.plan {
                        let suite = row
                            .job
                            .suite
                            .as_ref()
                            .map_or(String::new(), |s| format!(" (suite {s})"));
                        ui.label(
                            RichText::new(format!("step of plan {pid}{suite}"))
                                .font(theme::sans(SECONDARY))
                                .color(p.dim),
                        );
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let log =
                            paint::button(ui, p, "Log", true, false, row.actions.log.is_enabled());
                        match &row.actions.log {
                            Avail::Disabled(r) => {
                                log.on_hover_text(r);
                            }
                            Avail::Enabled => {
                                if log
                                    .on_hover_text("Tail, follow and search the log.")
                                    .clicked()
                                {
                                    open_log = true;
                                }
                            }
                        }
                        let up_ok = row.actions.move_up.is_enabled() && listed;
                        let up = paint::button(ui, p, "Move up", false, false, up_ok);
                        match &row.actions.move_up {
                            Avail::Disabled(r) => {
                                up.on_hover_text(r);
                            }
                            Avail::Enabled => {
                                if up
                                    .on_hover_text("Ask for the next queue class up.")
                                    .clicked()
                                    && up_ok
                                {
                                    move_up = true;
                                }
                            }
                        }
                        let cancel_ok = row.actions.cancel.is_enabled() && listed;
                        let cancel = paint::button(ui, p, "Cancel…", false, true, cancel_ok);
                        match (&row.actions.cancel, listed) {
                            (Avail::Disabled(r), _) => {
                                cancel.on_hover_text(r);
                            }
                            (Avail::Enabled, false) => {
                                cancel.on_hover_text("This job is no longer in the list.");
                            }
                            (Avail::Enabled, true) => {
                                if cancel
                                    .on_hover_text("Ask for a reason, then cancel.")
                                    .clicked()
                                {
                                    open_cancel = true;
                                }
                            }
                        }
                    });
                });
                rule(ui, p);
                if let Some(strip) = &d.strip {
                    egui::Frame::new().inner_margin(pad).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        paint::phase_strip(ui, p, strip);
                    });
                    rule(ui, p);
                }
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(16, 12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::Grid::new("facts")
                            .num_columns(2)
                            .min_col_width(92.0)
                            .spacing(vec2(12.0, 6.0))
                            .show(ui, |ui| {
                                for (k, v) in &d.facts {
                                    ui.label(
                                        RichText::new(*k)
                                            .font(theme::sans(SECONDARY))
                                            .color(p.faint),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(v)
                                                .font(theme::sans(SECONDARY))
                                                .color(p.ink),
                                        )
                                        .wrap(),
                                    );
                                    ui.end_row();
                                }
                            });
                    });
                rule(ui, p);
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(16, 12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        paint::resources(ui, p, &panel);
                    });
                rule(ui, p);
                egui::Frame::new().inner_margin(pad).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let open = self.raw_open.contains(&row.id);
                    if paint::button(ui, p, "Raw JSON", false, false, true).clicked() {
                        if open {
                            self.raw_open.remove(&row.id);
                        } else {
                            self.raw_open.insert(row.id);
                        }
                    }
                    if open {
                        let text = serde_json::to_string_pretty(&row.job).unwrap_or_default();
                        ui.add(
                            egui::Label::new(
                                RichText::new(text)
                                    .font(theme::mono(LABEL + 0.5))
                                    .color(p.dim),
                            )
                            .selectable(true),
                        );
                    }
                });
            });
        if open_log {
            self.open_log(&row);
        }
        if open_cancel {
            self.open_cancel(row.id);
        }
        if move_up && let Some(to) = actions::class_above(row.job.priority) {
            let owner = row.job.owner.clone();
            self.spawn_action(move |l| feed::move_up(l, JobId(row.id), &owner, to));
        }
    }

    /// One plan: what it is, how far it got and its steps, the ones needing a look first.
    fn plan_panel(&mut self, ui: &mut egui::Ui, p: &Palette, plan: &PlanRow) {
        let now_ms = self.vm.as_ref().map_or(0, |vm| vm.now_ms);
        let lines = plan.step_lines(now_ms);
        let facts = plan.facts(now_ms);
        let mut pick: Option<i64> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(16, 14))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("plan {}", plan.plan_id))
                                    .font(theme::mono(theme::BODY - 0.5))
                                    .color(p.faint),
                            );
                            let (r, _) = ui.allocate_exact_size(vec2(3.0, 16.0), Sense::hover());
                            ui.painter().rect_filled(
                                r,
                                egui::CornerRadius::same(2),
                                p.agent(plan.hue),
                            );
                            ui.add_space(-2.0);
                            ui.label(
                                RichText::new(&plan.agent)
                                    .font(theme::sans_medium(theme::BODY))
                                    .color(p.ink),
                            )
                            .on_hover_text(&plan.owner);
                            if let Some(v) = plan.vendor {
                                ui.label(
                                    RichText::new(v)
                                        .font(theme::sans(LABEL + 0.5))
                                        .color(p.faint),
                                );
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let st = &plan.state;
                                let head = if st.head.is_empty() {
                                    format!("{} · {}", st.left, st.right)
                                } else {
                                    st.head.clone()
                                };
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(head)
                                            .font(theme::sans_medium(SECONDARY))
                                            .color(paint::tone_color(p, st.tone)),
                                    )
                                    .truncate(),
                                );
                            });
                        });
                        ui.add_space(4.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(&plan.title)
                                    .font(theme::sans_semibold(theme::BODY + 1.0))
                                    .color(p.ink),
                            )
                            .wrap(),
                        );
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(&plan.progress_text)
                                .font(theme::sans(SECONDARY))
                                .color(p.dim),
                        );
                        ui.add_space(6.0);
                        paint::plan_bar(ui, p, plan);
                    });
                rule(ui, p);
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(16, 12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::Grid::new("plan-facts")
                            .num_columns(2)
                            .min_col_width(92.0)
                            .spacing(vec2(12.0, 6.0))
                            .show(ui, |ui| {
                                for (k, v) in &facts {
                                    ui.label(
                                        RichText::new(*k)
                                            .font(theme::sans(SECONDARY))
                                            .color(p.faint),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(v)
                                                .font(theme::sans(SECONDARY))
                                                .color(p.ink),
                                        )
                                        .wrap(),
                                    );
                                    ui.end_row();
                                }
                            });
                    });
                rule(ui, p);
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(16, 12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new("Steps")
                                .font(theme::sans_semibold(theme::BODY - 0.5))
                                .color(p.ink),
                        );
                        ui.add_space(4.0);
                        for l in &lines {
                            if paint::step_line(ui, p, l).clicked() {
                                pick = Some(l.id);
                            }
                        }
                    });
            });
        if let Some(id) = pick {
            self.select_step(id);
        }
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

    fn drain_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_drain {
            return;
        }
        let mut decision: Option<bool> = None;
        egui::Window::new("Drain the queue?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("Queued jobs stop starting until someone resumes. Running jobs finish.");
                ui.horizontal(|ui| {
                    if ui.button("Drain").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Keep going").clicked() {
                        decision = Some(false);
                    }
                });
            });
        if let Some(go) = decision {
            self.confirm_drain = false;
            if go {
                let who = self.who.clone();
                self.spawn_action(move |l| feed::set_drain(l, false, &who));
            }
        }
    }

    fn log_window(&mut self, ctx: &egui::Context) {
        let Some(l) = &mut self.log else {
            return;
        };
        let mut open = true;
        let bad = Palette::current(ctx).bad;
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
                    ui.colored_label(bad, e);
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

    fn audit_tab(&mut self, ui: &mut egui::Ui, p: &Palette) {
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
                ui.colored_label(p.bad, e);
            }
            Some(Ok(d)) => {
                ui.weak(audit::summary(d));
                let rows: Vec<Vec<String>> =
                    d.rows.iter().map(|r| audit::cells(r).to_vec()).collect();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let cols: Vec<(&str, f32)> = audit::COLUMNS
                            .iter()
                            .copied()
                            .zip([150.0, 150.0, 90.0, 130.0, 60.0, 90.0, 0.0])
                            .collect();
                        paint::table(ui, p, egui::Id::new("audit"), &cols, &rows, &[0, 4], &[]);
                    });
            }
        }
    }

    fn stalls_tab(&mut self, ui: &mut egui::Ui, p: &Palette) {
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
                ui.colored_label(p.bad, e);
            }
            Some(Ok(d)) => {
                ui.weak(stalls::summary(d));
                let rows: Vec<Vec<String>> =
                    d.rows.iter().map(|r| stalls::cells(r).to_vec()).collect();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let cols: Vec<(&str, f32)> = stalls::COLUMNS
                            .iter()
                            .copied()
                            .zip([150.0, 80.0, 60.0, 250.0, 300.0, 0.0])
                            .collect();
                        paint::table(ui, p, egui::Id::new("stalls"), &cols, &rows, &[0], &[]);
                    });
            }
        }
    }

    /// Sends a `gc` request in the background; its report replaces the one shown.
    fn ask_disk(&mut self, req: tasks_daemon::api::Request) {
        self.disk_asked = true;
        self.disk_loading = true;
        let link = Arc::clone(&self.link);
        feed::spawn_once(
            Arc::clone(&self.disk_box),
            Arc::clone(&self.wake),
            move || disk::fetch(link.as_ref(), &req),
        );
    }

    /// The Disk tab (R-840): a card per machine, a row per checkout with a tick box, Free for
    /// the ticked rows, and the shared caches, which are only shown.
    fn disk_tab(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if !self.disk_asked {
            self.ask_disk(disk::report_request());
        }
        let view = match &self.disk {
            Some(Ok(v)) => Some(v.clone()),
            _ => None,
        };
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.disk_loading, egui::Button::new("Refresh"))
                .clicked()
            {
                self.disk_asked = false;
            }
            if let Some(v) = &view {
                let all = disk::reclaimable(v);
                if ui
                    .add_enabled(
                        !all.is_empty() && !self.disk_loading,
                        egui::Button::new("Select merged, gone and old"),
                    )
                    .clicked()
                {
                    self.disk_ticked = all;
                }
                if ui
                    .add_enabled(!self.disk_ticked.is_empty(), egui::Button::new("Clear"))
                    .clicked()
                {
                    self.disk_ticked.clear();
                }
                let label = disk::free_label(v, &self.disk_ticked);
                let can = !self.disk_ticked.is_empty() && !self.disk_loading;
                if paint::button(ui, p, &label, false, true, can).clicked() && can {
                    self.confirm_free = true;
                }
            }
            if self.disk_loading {
                ui.weak("measuring… (a few seconds)");
            }
        });
        ui.add_space(8.0);
        let v = match (&self.disk, view) {
            (None, _) => {
                ui.weak("Measuring build output on every machine…");
                return;
            }
            (Some(Err(e)), _) => {
                ui.colored_label(p.bad, e);
                return;
            }
            (_, Some(v)) => v,
            (Some(Ok(_)), None) => return,
        };
        paint::disk_cards(ui, p, &disk::cards(&v));
        ui.add_space(6.0);
        ui.weak(disk::summary(&v));
        for n in &v.notes {
            ui.colored_label(p.warn, n);
        }
        ui.add_space(4.0);
        // the table stretches its last column: an empty one, so the sizes keep their width
        let mut titles = disk::columns(&v);
        let right: Vec<usize> = (5..titles.len()).collect();
        let mut widths = vec![22.0, 64.0, 330.0, 200.0, 70.0];
        widths.extend(titles.iter().skip(5).map(|_| 80.0));
        titles.push(String::new());
        widths.push(0.0);
        let cols: Vec<(&str, f32)> = titles
            .iter()
            .map(String::as_str)
            .zip(widths.iter().copied())
            .collect();
        let now_ms = self.vm.as_ref().map_or(0, |vm| vm.now_ms);
        let rows: Vec<Vec<String>> = v
            .rows
            .iter()
            .map(|r| {
                let mut c = vec![String::new()];
                c.extend(disk::cells(&v, r, now_ms));
                c
            })
            .collect();
        let ticks: Vec<Option<bool>> = v
            .rows
            .iter()
            .map(|r| {
                (disk::selectable(r) && r.freed_bytes == 0)
                    .then(|| self.disk_ticked.contains(&r.checkout))
            })
            .collect();
        let tips: Vec<String> = v
            .rows
            .iter()
            .map(|r| match &r.kept {
                Some(k) => format!("Kept: {k}"),
                None => disk::state_help(r.state).to_string(),
            })
            .collect();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let clicked = paint::tick_table(
                    ui,
                    p,
                    egui::Id::new("disk"),
                    &cols,
                    &rows,
                    &[],
                    &right,
                    &ticks,
                    &tips,
                );
                for i in clicked {
                    if let Some(r) = v.rows.get(i)
                        && !self.disk_ticked.remove(&r.checkout)
                    {
                        self.disk_ticked.insert(r.checkout.clone());
                    }
                }
                let shared = disk::shared_lines(&v);
                if !shared.is_empty() {
                    ui.add_space(14.0);
                    paint::section_head(ui, p, "Shared by every checkout", "never freed here");
                    for line in shared {
                        ui.weak(line);
                    }
                }
            });
    }

    fn free_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_free {
            return;
        }
        let Some(Ok(v)) = &self.disk else {
            self.confirm_free = false;
            return;
        };
        let text = disk::confirm_text(v, &self.disk_ticked);
        let mut decision: Option<bool> = None;
        egui::Window::new("Free build output?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.label(text);
                ui.horizontal(|ui| {
                    if ui.button("Free").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Keep it").clicked() {
                        decision = Some(false);
                    }
                });
            });
        if let Some(go) = decision {
            self.confirm_free = false;
            if go && !self.disk_ticked.is_empty() {
                self.ask_disk(disk::free_request(&self.disk_ticked));
            }
        }
    }

    fn stats_tab(&mut self, ui: &mut egui::Ui, p: &Palette) {
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
                ui.colored_label(p.bad, e);
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
                        let secs = |v: Option<f64>| {
                            v.map_or_else(|| format::UNKNOWN.to_string(), format::duration)
                        };
                        for recipe in recipes {
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(recipe)
                                    .font(theme::sans_semibold(theme::BODY))
                                    .color(p.ink),
                            );
                            ui.add_space(4.0);
                            let table: Vec<Vec<String>> = rows
                                .iter()
                                .filter(|r| r.recipe == recipe)
                                .map(|r| {
                                    vec![
                                        r.machine.clone(),
                                        r.target.clone().unwrap_or_else(|| "(whole job)".into()),
                                        r.samples.to_string(),
                                        r.passed.to_string(),
                                        secs(r.median),
                                        secs(r.p90),
                                        secs(r.last),
                                        r.last_outcome
                                            .clone()
                                            .unwrap_or_else(|| format::UNKNOWN.to_string()),
                                        r.cold.to_string(),
                                    ]
                                })
                                .collect();
                            let cols = [
                                ("Machine", 120.0),
                                ("Target", 200.0),
                                ("Samples", 70.0),
                                ("Passed", 70.0),
                                ("Median", 80.0),
                                ("p90", 80.0),
                                ("Last", 80.0),
                                ("Last result", 110.0),
                                ("Cold", 60.0),
                            ];
                            paint::table(
                                ui,
                                p,
                                egui::Id::new(("timings", recipe)),
                                &cols,
                                &table,
                                &[],
                                &[2, 3, 4, 5, 6, 8],
                            );
                            ui.add_space(14.0);
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
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

/// `live · 2 peers`.
fn live_text(vm: &ViewModel) -> String {
    let up = vm
        .machines
        .iter()
        .filter(|m| !m.local && m.online == Online::Online)
        .count();
    match up {
        0 => "live · no peers".to_string(),
        1 => "live · 1 peer".to_string(),
        n => format!("live · {n} peers"),
    }
}

/// The agent's stripe, name and vendor on one line.
fn agent_label(ui: &mut egui::Ui, p: &Palette, row: &JobRow) {
    let (r, _) = ui.allocate_exact_size(vec2(3.0, 16.0), Sense::hover());
    ui.painter()
        .rect_filled(r, egui::CornerRadius::same(2), p.agent(row.hue));
    ui.add_space(-2.0);
    ui.label(
        RichText::new(&row.agent)
            .font(theme::sans_medium(theme::BODY))
            .color(p.ink),
    )
    .on_hover_text(&row.owner);
    if let Some(v) = row.vendor {
        ui.label(
            RichText::new(v)
                .font(theme::sans(LABEL + 0.5))
                .color(p.faint),
        );
    }
}

/// A 1px full-width line between side panel blocks.
fn rule(ui: &mut egui::Ui, p: &Palette) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter()
        .hline(r.x_range(), r.min.y, Stroke::new(1.0_f32, p.line));
}

/// A tab in the top bar; true when clicked.
fn tab_button(ui: &mut egui::Ui, p: &Palette, label: &str, count: Option<usize>, on: bool) -> bool {
    let font = if on {
        theme::sans_medium(theme::BODY)
    } else {
        theme::sans(theme::BODY)
    };
    let color = if on { p.ink } else { p.dim };
    let g = ui.painter().layout_no_wrap(label.to_string(), font, color);
    let c = count.map(|n| {
        ui.painter()
            .layout_no_wrap(n.to_string(), theme::sans(SECONDARY), p.faint)
    });
    let w = g.size().x + c.as_ref().map_or(0.0, |c| c.size().x + 5.0) + 20.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 26.0), Sense::click());
    if on {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(4), p.raise);
    } else if resp.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(4),
            theme::mix(p.panel, p.ink, 5),
        );
    }
    let y = rect.center().y;
    let mut x = rect.min.x + 10.0;
    let gw = g.size().x;
    ui.painter()
        .galley(egui::pos2(x, y - g.size().y / 2.0), g, Color32::PLACEHOLDER);
    x += gw + 5.0;
    if let Some(c) = c {
        ui.painter()
            .galley(egui::pos2(x, y - c.size().y / 2.0), c, Color32::PLACEHOLDER);
    }
    resp.clicked()
}

/// R-833: the name in the top bar and the window title.
pub const NAME: &str = "LoadTable";
/// R-833: the app id the desktop groups the window by (Wayland `app_id`, X11 `WM_CLASS`).
pub const APP_ID: &str = "loadtable";

/// Starts the window and blocks until it closes.
pub fn run(link: Arc<dyn DaemonLink>, who: String) -> Result<(), String> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(NAME)
            .with_app_id(APP_ID)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([MIN_WINDOW_WIDTH, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_ID,
        opts,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let wake: Wake = Arc::new(move || ctx.request_repaint());
            Ok(Box::new(App::new(link, who, wake)))
        }),
    )
    .map_err(|e| e.to_string())
}
