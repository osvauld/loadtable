//! Painting of the Queue tab's parts: the machine rows, the job list rows and the side
//! panel's strip. Drawing only: every text and every number comes from the view model.

use crate::columns::{self, Col, ColSpec, GAP};
use crate::detail::{SegKind, Segment, Strip};
use crate::machine::MemLevel;
use crate::model::{JobRow, MachineCard, Online, PhaseClass, Section, SlotLine, StateCell};
use crate::plans::{self, PlanRow};
use crate::resources::{self, Graph, Procs};
use crate::theme::{self, LABEL, Palette, SECONDARY};
use eframe::egui::{
    self, Color32, CornerRadius, FontId, Painter, Rect, RichText, Sense, Stroke, StrokeKind, Ui,
    epaint::text::LayoutJob, pos2, vec2,
};
use std::sync::Arc;

/// Height of one job row.
pub const ROW_H: f32 = 46.0;
/// Height of one machine row.
pub const MACHINE_H: f32 = 34.0;
/// Side padding of the strip and the list.
pub const PAD: f32 = 14.0;

/// The colour of a status class.
pub fn tone_color(p: &Palette, c: PhaseClass) -> Color32 {
    match c {
        PhaseClass::Running => p.accent,
        PhaseClass::Passed => p.ok,
        PhaseClass::Failed => p.bad,
        PhaseClass::Waiting => p.warn,
        PhaseClass::Neutral => p.dim,
    }
}

/// `text` laid out on one line, cut with an ellipsis at `width`.
pub fn fit(
    painter: &Painter,
    text: &str,
    font: FontId,
    color: Color32,
    width: f32,
) -> Arc<egui::Galley> {
    let mut job = LayoutJob::simple(text.to_string(), font, color, width.max(1.0));
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    painter.layout_job(job)
}

/// Draws `galley` with its left edge at `x`, centred on `cy`.
fn put(painter: &Painter, x: f32, cy: f32, galley: Arc<egui::Galley>) {
    let pos = pos2(x, cy - galley.size().y / 2.0);
    painter.galley(pos, galley, Color32::PLACEHOLDER);
}

/// Draws `text` at `x`, vertically centred on `cy`; returns its width.
fn text_at(painter: &Painter, x: f32, cy: f32, text: &str, font: FontId, color: Color32) -> f32 {
    let g = painter.layout_no_wrap(text.to_string(), font, color);
    let w = g.size().x;
    put(painter, x, cy, g);
    w
}

/// Draws `text` ending at `right`; returns its width.
fn text_right(
    painter: &Painter,
    right: f32,
    cy: f32,
    text: &str,
    font: FontId,
    color: Color32,
) -> f32 {
    let g = painter.layout_no_wrap(text.to_string(), font, color);
    let w = g.size().x;
    put(painter, right - w, cy, g);
    w
}

fn round(r: f32) -> CornerRadius {
    CornerRadius::same(r as u8)
}

/// The dot of a machine's reachability.
fn online_color(p: &Palette, o: Online) -> Color32 {
    match o {
        Online::Online => p.ok,
        Online::Stale => p.warn,
        Online::Offline | Online::Unknown => p.faint,
    }
}

fn pips(painter: &Painter, p: &Palette, x: f32, cy: f32, s: &SlotLine, max_x: f32) -> f32 {
    let label = text_at(painter, x, cy, &s.kind, theme::sans(SECONDARY - 0.5), p.dim);
    let mut cx = x + label + 6.0;
    let used = s.total.saturating_sub(s.free);
    // a very large pool reads better as a figure than as a row of squares
    if s.total > 12 {
        let w = text_at(
            painter,
            cx,
            cy,
            &format!("{used}/{}", s.total),
            theme::sans(SECONDARY),
            p.ink,
        );
        return cx + w - x;
    }
    for i in 0..s.total {
        if cx + 9.0 > max_x {
            break;
        }
        let r = Rect::from_min_size(pos2(cx, cy - 4.5), vec2(9.0, 9.0));
        if i < used {
            painter.rect_filled(r, round(2.0), p.accent);
        } else {
            painter.rect_stroke(
                r.shrink(0.75),
                round(2.0),
                Stroke::new(1.5_f32, p.faint),
                StrokeKind::Middle,
            );
        }
        cx += 12.0;
    }
    cx - x
}

/// The colour of a memory level's word.
fn level_color(p: &Palette, l: MemLevel) -> Color32 {
    match l {
        MemLevel::Ok => p.ok,
        MemLevel::Tight => p.warn,
        MemLevel::Critical => p.bad,
        MemLevel::Unknown => p.dim,
    }
}

/// The CPU cell: the bar split by job, then `47% busy`.
fn cpu_cell(painter: &Painter, p: &Palette, m: &MachineCard, x: f32, cy: f32, w: f32) {
    let val_w = 70.0;
    let bar = Rect::from_min_size(pos2(x, cy - 4.0), vec2((w - val_w - 18.0).max(30.0), 8.0));
    painter.rect_filled(bar, round(2.0), p.raise);
    let mut left = bar.min.x;
    for piece in m.cpu_load.pieces() {
        let pw = (bar.width() * piece.frac).max(2.0).min(bar.max.x - left);
        if pw <= 0.0 {
            break;
        }
        let color = piece.hue.map_or(p.faint, |h| p.agent(h));
        painter.rect_filled(
            Rect::from_min_size(pos2(left, bar.min.y), vec2(pw, bar.height())),
            0.0,
            color,
        );
        left += pw;
    }
    // with no per-job split the bar still shows how busy the machine is
    if m.cpu_load.pieces().is_empty()
        && let Some(b) = m.cpu_busy
    {
        painter.rect_filled(
            Rect::from_min_size(
                bar.min,
                vec2(bar.width() * (b / 100.0).clamp(0.0, 1.0), bar.height()),
            ),
            0.0,
            p.faint,
        );
    }
    let (text, color) = match m.cpu_busy {
        Some(b) => (format!("{b:.0}% busy"), p.ink),
        None => ("no reading".to_string(), p.dim),
    };
    text_at(
        painter,
        bar.max.x + 10.0,
        cy,
        &text,
        theme::sans(SECONDARY),
        color,
    );
}

/// The machine rows. Each machine is one row: name and role, memory in words with RAM used,
/// CPU busy with the bar split by job, and the slot pips (R-582, R-644, R-646). An away
/// machine gets one line in place of the figures.
pub fn machine_strip(ui: &mut Ui, p: &Palette, machines: &[MachineCard]) {
    let width = ui.available_width();
    let wide = width >= 900.0;
    let name_w = if wide { 170.0 } else { 140.0 };
    let flex = (width - 2.0 * PAD - name_w).max(300.0);
    let (mem_w, cpu_w) = if wide {
        (flex * 0.27, flex * 0.33)
    } else {
        (flex * 0.34, flex * 0.30)
    };
    let x_mem = PAD + name_w;
    let x_cpu = x_mem + mem_w;
    let x_slots = x_cpu + cpu_w;
    let slots_w = flex - mem_w - cpu_w;

    // header
    let (hr, _) = ui.allocate_exact_size(vec2(width, 24.0), Sense::hover());
    let hp = ui.painter_at(hr);
    hp.rect_filled(hr, 0.0, p.panel);
    let head = |x: f32, t: &str| {
        text_at(
            &hp,
            hr.min.x + x,
            hr.center().y + 1.0,
            &t.to_uppercase(),
            theme::sans(LABEL),
            p.dim,
        );
    };
    head(PAD, "Machine");
    head(x_mem, "Memory");
    head(x_cpu, "CPU, by job");
    head(x_slots, "Slots");

    for m in machines {
        let (rect, resp) = ui.allocate_exact_size(vec2(width, MACHINE_H), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, p.panel);
        painter.hline(rect.x_range(), rect.min.y, Stroke::new(1.0_f32, p.line));
        let cy = rect.center().y;
        let fade = |c: Color32| if m.greyed { c.gamma_multiply(0.6) } else { c };
        painter.circle_filled(
            pos2(rect.min.x + PAD + 4.0, cy),
            4.0,
            online_color(p, m.online),
        );
        let nx = rect.min.x + PAD + 16.0;
        let w = text_at(
            &painter,
            nx,
            cy,
            &m.name,
            theme::sans_semibold(theme::BODY),
            fade(p.ink),
        );
        // R-835: a low battery takes the role's place, in the warning colour
        let (role, role_color) = match &m.battery_low {
            Some(b) => (b.as_str(), p.warn),
            None => (m.role, p.dim),
        };
        text_at(
            &painter,
            nx + w + 6.0,
            cy,
            role,
            theme::sans(LABEL + 0.5),
            role_color,
        );
        if let Some(note) = &m.away_note {
            text_at(
                &painter,
                rect.min.x + x_mem,
                cy,
                note,
                theme::sans(SECONDARY),
                p.dim.gamma_multiply(0.85),
            );
            resp.on_hover_text(&m.status_text);
            continue;
        }
        // memory: the level in a word, RAM used beside it; the stall figures are the tooltip
        let mx = rect.min.x + x_mem;
        let word = text_at(
            &painter,
            mx,
            cy,
            m.mem_level.word(),
            theme::sans_semibold(SECONDARY),
            fade(level_color(p, m.mem_level)),
        );
        let ram = fit(
            &painter,
            &m.ram_used,
            theme::sans(SECONDARY),
            fade(p.ink),
            (mem_w - word - 22.0).max(30.0),
        );
        put(&painter, mx + word + 10.0, cy, ram);
        let mem_box =
            Rect::from_min_max(pos2(mx, rect.min.y), pos2(rect.min.x + x_cpu, rect.max.y));
        ui.interact(mem_box, resp.id.with("mem"), Sense::hover())
            .on_hover_text(format!("{}\n{}\n{}", m.pressure, m.ram, m.swap));
        // CPU
        let cx = rect.min.x + x_cpu;
        cpu_cell(&painter, p, m, cx, cy, cpu_w - 10.0);
        let cpu_box =
            Rect::from_min_max(pos2(cx, rect.min.y), pos2(rect.min.x + x_slots, rect.max.y));
        ui.interact(cpu_box, resp.id.with("cpu"), Sense::hover())
            .on_hover_text(format!("{}\n\n{}", m.cpu_load.tooltip(), m.cpu));
        // slots
        let mut px = rect.min.x + x_slots;
        let right_edge = (px + slots_w).min(rect.max.x - PAD);
        for s in &m.slots {
            if px >= right_edge {
                break;
            }
            let used = pips(&painter, p, px, cy, s, right_edge);
            if !s.holders.is_empty() {
                let tip = Rect::from_min_size(pos2(px, rect.min.y), vec2(used, MACHINE_H));
                ui.interact(tip, resp.id.with(&s.kind), Sense::hover())
                    .on_hover_text(
                        s.holders
                            .iter()
                            .map(|h| h.label.as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
            }
            px += used + 18.0;
        }
        if let Some(b) = &m.blocker {
            resp.on_hover_text(format!("A new compile here waits: {b}"));
        }
    }
    let (end, _) = ui.allocate_exact_size(vec2(width, 1.0), Sense::hover());
    ui.painter()
        .hline(end.x_range(), end.min.y, Stroke::new(1.0_f32, p.line));
}

/// A group header of the job list: `Running  3`.
pub fn group_header(ui: &mut Ui, p: &Palette, title: &str, count: usize, note: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let cy = rect.center().y + 4.0;
    let w = text_at(
        &painter,
        rect.min.x + PAD,
        cy,
        title,
        theme::sans_semibold(theme::BODY - 0.5),
        p.ink,
    );
    let w2 = text_at(
        &painter,
        rect.min.x + PAD + w + 8.0,
        cy,
        &count.to_string(),
        theme::sans(SECONDARY),
        p.faint,
    );
    if !note.is_empty() {
        text_at(
            &painter,
            rect.min.x + PAD + w + w2 + 16.0,
            cy,
            note,
            theme::sans(SECONDARY),
            p.faint,
        );
    }
}

/// Narrowest room the left part of a state cell is drawn in; with less it is left out.
const MIN_LEFT: f32 = 40.0;

/// The state cell: a bar or a head line, then the two-part line under it.
fn state_cell(painter: &Painter, p: &Palette, rect: Rect, s: &StateCell, section: Section) {
    let top = rect.min.y + 9.0;
    // a cell with only one line is centred, not hung from the top
    let single = s.bar.is_none() && s.left.is_empty() && s.right.is_empty();
    let line1 = if single {
        rect.center().y
    } else {
        rect.min.y + 13.0
    };
    let line2 = rect.min.y + 31.0;
    match s.bar {
        Some(f) => {
            let track = Rect::from_min_size(pos2(rect.min.x, top), vec2(rect.width(), 5.0));
            painter.rect_filled(track, round(3.0), p.raise);
            if f > 0.0 {
                let w = (track.width() * f).max(4.0);
                painter.rect_filled(
                    Rect::from_min_size(track.min, vec2(w, 5.0)),
                    round(3.0),
                    p.accent,
                );
            }
        }
        None => {
            let color = tone_color(p, s.tone);
            let font = if section == Section::Recent {
                theme::sans_medium(SECONDARY)
            } else {
                theme::sans(SECONDARY)
            };
            let g = fit(painter, &s.head, font, color, rect.width());
            put(painter, rect.min.x, line1, g);
        }
    }
    // right part first (the time left): it is cut to the cell, and the left part gets what is
    // left, or is not drawn when too little is, never drawn under it (R-838)
    let right_w = if s.right.is_empty() {
        0.0
    } else {
        let g = fit(
            painter,
            &s.right,
            theme::sans_medium(SECONDARY),
            p.ink,
            rect.width(),
        );
        let w = g.size().x;
        put(painter, rect.max.x - w, line2, g);
        w
    };
    let room = rect.width() - right_w - 10.0;
    if !s.left.is_empty() && room >= MIN_LEFT {
        put(
            painter,
            rect.min.x,
            line2,
            fit(painter, &s.left, theme::sans(SECONDARY), p.dim, room),
        );
    }
}

/// One job row; returns its response.
pub fn job_row(
    ui: &mut Ui,
    p: &Palette,
    row: &JobRow,
    cols: &[ColSpec],
    selected: bool,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
    let painter = ui.painter_at(rect);
    if selected {
        painter.rect_filled(rect, 0.0, theme::mix(p.win, p.accent, 14));
        painter.rect_filled(
            Rect::from_min_size(rect.min, vec2(2.0, rect.height())),
            0.0,
            p.accent,
        );
    } else if resp.hovered() {
        painter.rect_filled(rect, 0.0, theme::mix(p.win, p.ink, 4));
    }
    painter.hline(rect.x_range(), rect.min.y, Stroke::new(1.0_f32, p.line));

    let fixed: f32 = cols
        .iter()
        .filter(|s| s.col != Col::Command)
        .map(|s| s.width)
        .sum::<f32>()
        + GAP * cols.len().saturating_sub(1) as f32
        + 2.0 * PAD;
    let command_w = (width - fixed).max(
        columns::specs()
            .iter()
            .find(|s| s.col == Col::Command)
            .map_or(120.0, |s| s.width),
    );
    let cy = rect.center().y;
    let mut x = rect.min.x + PAD;
    for s in cols {
        let w = if s.col == Col::Command {
            command_w
        } else {
            s.width
        };
        let cell = Rect::from_min_size(pos2(x, rect.min.y), vec2(w, ROW_H));
        match s.col {
            Col::Id => {
                text_at(
                    &painter,
                    x,
                    cy,
                    &format!("#{}", row.id),
                    theme::mono(SECONDARY),
                    p.faint,
                );
            }
            Col::Agent => {
                let hue = p.agent(row.hue);
                painter.rect_filled(
                    Rect::from_center_size(pos2(x + 1.5, cy), vec2(3.0, 18.0)),
                    round(2.0),
                    hue,
                );
                let name = fit(
                    &painter,
                    &row.agent,
                    theme::sans_medium(theme::BODY),
                    p.ink,
                    w - 10.0,
                );
                let name_w = name.size().x;
                put(&painter, x + 10.0, cy, name);
                if let Some(v) = row.vendor {
                    let room = w - 10.0 - name_w - 6.0;
                    if room > 24.0 {
                        put(
                            &painter,
                            x + 10.0 + name_w + 6.0,
                            cy + 1.0,
                            fit(&painter, v, theme::sans(LABEL + 0.5), p.faint, room),
                        );
                    }
                }
                ui.interact(cell, resp.id.with("agent"), Sense::hover())
                    .on_hover_text(&row.owner);
            }
            Col::Command => {
                put(
                    &painter,
                    x,
                    cy,
                    fit(&painter, &row.command, theme::mono(SECONDARY), p.ink, w),
                );
            }
            Col::Machine => {
                let g = painter.layout_no_wrap(
                    row.machine.clone(),
                    theme::sans(SECONDARY - 0.5),
                    p.dim,
                );
                let chip =
                    Rect::from_min_size(pos2(x, cy - 9.5), vec2((g.size().x + 12.0).min(w), 19.0));
                painter.rect_stroke(
                    chip,
                    round(3.0),
                    Stroke::new(1.0_f32, p.line),
                    StrokeKind::Inside,
                );
                painter.with_clip_rect(chip).galley(
                    pos2(chip.min.x + 6.0, cy - g.size().y / 2.0),
                    g,
                    Color32::PLACEHOLDER,
                );
            }
            Col::State => {
                state_cell(&painter, p, cell, &row.state, row.section);
            }
        }
        x += w + GAP;
    }
    resp
}

/// Height of a plan's step row: smaller than a job row, a step has one line to say.
pub const STEP_H: f32 = 34.0;
/// How far a step is pushed in from its plan's marker.
pub const INDENT: f32 = 16.0;

/// What a click on a plan row meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanClick {
    /// Nothing.
    None,
    /// The row: select the plan.
    Select,
    /// The disclosure marker: open or close it.
    Toggle,
}

fn row_back(painter: &Painter, rect: Rect, p: &Palette, selected: bool, hovered: bool, tint: bool) {
    if selected {
        painter.rect_filled(rect, 0.0, theme::mix(p.win, p.accent, 14));
        painter.rect_filled(
            Rect::from_min_size(rect.min, vec2(2.0, rect.height())),
            0.0,
            p.accent,
        );
    } else if hovered {
        painter.rect_filled(rect, 0.0, theme::mix(p.win, p.ink, 4));
    } else if tint {
        painter.rect_filled(rect, 0.0, theme::mix(p.win, p.ink, 2));
    }
    painter.hline(rect.x_range(), rect.min.y, Stroke::new(1.0_f32, p.line));
}

fn command_width(width: f32, cols: &[ColSpec]) -> f32 {
    let fixed: f32 = cols
        .iter()
        .filter(|s| s.col != Col::Command)
        .map(|s| s.width)
        .sum::<f32>()
        + GAP * cols.len().saturating_sub(1) as f32
        + 2.0 * PAD;
    (width - fixed).max(
        columns::specs()
            .iter()
            .find(|s| s.col == Col::Command)
            .map_or(120.0, |s| s.width),
    )
}

fn chip(painter: &Painter, p: &Palette, x: f32, cy: f32, max_w: f32, text: &str, font: FontId) {
    let g = painter.layout_no_wrap(text.to_string(), font, p.dim);
    let chip = Rect::from_min_size(
        pos2(x, cy - 9.5),
        vec2((g.size().x + 12.0).min(max_w), 19.0),
    );
    painter.rect_stroke(
        chip,
        round(3.0),
        Stroke::new(1.0_f32, p.line),
        StrokeKind::Inside,
    );
    painter.with_clip_rect(chip).galley(
        pos2(chip.min.x + 6.0, cy - g.size().y / 2.0),
        g,
        Color32::PLACEHOLDER,
    );
}

/// A plan's parent row: marker, agent, title over the counts, machine and the state cell.
pub fn plan_row(
    ui: &mut Ui,
    p: &Palette,
    plan: &PlanRow,
    cols: &[ColSpec],
    selected: bool,
    open: bool,
) -> (egui::Response, PlanClick) {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
    let painter = ui.painter_at(rect);
    row_back(&painter, rect, p, selected, resp.hovered(), false);
    let command_w = command_width(width, cols);
    let cy = rect.center().y;
    let mut x = rect.min.x + PAD;
    let mut toggled = false;
    for s in cols {
        let w = if s.col == Col::Command {
            command_w
        } else {
            s.width
        };
        let cell = Rect::from_min_size(pos2(x, rect.min.y), vec2(w, ROW_H));
        match s.col {
            Col::Id => {
                let mark = Rect::from_min_size(pos2(x - 4.0, rect.min.y), vec2(w + 4.0, ROW_H));
                let hit = ui.interact(mark, resp.id.with("marker"), Sense::click());
                toggled = hit.clicked();
                let c = if hit.hovered() { p.ink } else { p.dim };
                let (mx, my) = (x + 5.0, cy);
                let pts = if open {
                    vec![
                        pos2(mx - 4.5, my - 2.5),
                        pos2(mx + 4.5, my - 2.5),
                        pos2(mx, my + 3.5),
                    ]
                } else {
                    vec![
                        pos2(mx - 2.5, my - 4.5),
                        pos2(mx - 2.5, my + 4.5),
                        pos2(mx + 3.5, my),
                    ]
                };
                painter.add(egui::Shape::convex_polygon(pts, c, Stroke::NONE));
                text_at(
                    &painter,
                    x + 16.0,
                    cy,
                    "plan",
                    theme::mono(SECONDARY - 1.0),
                    p.faint,
                );
                hit.on_hover_text(if open {
                    "Hide the steps"
                } else {
                    "Show the steps"
                });
            }
            Col::Agent => {
                painter.rect_filled(
                    Rect::from_center_size(pos2(x + 1.5, cy), vec2(3.0, 18.0)),
                    round(2.0),
                    p.agent(plan.hue),
                );
                let name = fit(
                    &painter,
                    &plan.agent,
                    theme::sans_medium(theme::BODY),
                    p.ink,
                    w - 10.0,
                );
                let name_w = name.size().x;
                put(&painter, x + 10.0, cy, name);
                if let Some(v) = plan.vendor {
                    let room = w - 10.0 - name_w - 6.0;
                    if room > 24.0 {
                        put(
                            &painter,
                            x + 10.0 + name_w + 6.0,
                            cy + 1.0,
                            fit(&painter, v, theme::sans(LABEL + 0.5), p.faint, room),
                        );
                    }
                }
                ui.interact(cell, resp.id.with("agent"), Sense::hover())
                    .on_hover_text(&plan.owner);
            }
            Col::Command => {
                put(
                    &painter,
                    x,
                    rect.min.y + 15.0,
                    fit(
                        &painter,
                        &plan.title,
                        theme::sans_semibold(theme::BODY),
                        p.ink,
                        w,
                    ),
                );
                put(
                    &painter,
                    x,
                    rect.min.y + 33.0,
                    fit(
                        &painter,
                        &plan.progress_text,
                        theme::sans(SECONDARY),
                        p.dim,
                        w,
                    ),
                );
            }
            Col::Machine => {
                if !plan.machine.is_empty() {
                    chip(
                        &painter,
                        p,
                        x,
                        cy,
                        w,
                        &plan.machine,
                        theme::sans(SECONDARY - 0.5),
                    );
                }
            }
            Col::State => state_cell(&painter, p, cell, &plan.state, plan.section),
        }
        x += w + GAP;
    }
    let click = if !resp.clicked() && !toggled {
        PlanClick::None
    } else if toggled {
        PlanClick::Toggle
    } else {
        PlanClick::Select
    };
    (resp, click)
}

/// One step of a plan, pushed in under its parent: id, step name and flags, machine, state.
pub fn step_row(
    ui: &mut Ui,
    p: &Palette,
    row: &JobRow,
    cols: &[ColSpec],
    selected: bool,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, STEP_H), Sense::click());
    let painter = ui.painter_at(rect);
    row_back(&painter, rect, p, selected, resp.hovered(), true);
    // the line that ties the steps to their parent
    let guide = rect.min.x + PAD + 5.0;
    painter.vline(guide, rect.y_range(), Stroke::new(1.0_f32, p.line));
    let command_w = command_width(width, cols);
    let cy = rect.center().y;
    let mut x = rect.min.x + PAD;
    // the name starts under the agent column and may run on into the command column
    let name_room = cols
        .iter()
        .filter(|s| matches!(s.col, Col::Agent | Col::Command))
        .map(|s| {
            if s.col == Col::Command {
                command_w
            } else {
                s.width
            }
        })
        .sum::<f32>()
        + GAP;
    let mut agent_x = x;
    for s in cols {
        let w = if s.col == Col::Command {
            command_w
        } else {
            s.width
        };
        let cell = Rect::from_min_size(pos2(x, rect.min.y), vec2(w, STEP_H));
        match s.col {
            Col::Id => {
                text_at(
                    &painter,
                    x + INDENT,
                    cy,
                    &format!("#{}", row.id),
                    theme::mono(SECONDARY - 0.5),
                    p.faint,
                );
            }
            Col::Agent => agent_x = x,
            Col::Command => {
                let label = row.step.as_deref().unwrap_or(&row.command);
                // tags come first so a long name is cut, never the flag
                let tags_w: f32 = row.tags.len() as f32 * 56.0;
                let g = fit(
                    &painter,
                    label,
                    theme::mono(SECONDARY),
                    p.ink,
                    (name_room - 10.0 - tags_w).max(60.0),
                );
                let name_w = g.size().x;
                put(&painter, agent_x + 10.0, cy, g);
                let mut tx = agent_x + 10.0 + name_w + 8.0;
                for t in &row.tags {
                    let g = painter.layout_no_wrap(t.clone(), theme::sans(LABEL), p.dim);
                    let tag =
                        Rect::from_min_size(pos2(tx, cy - 8.0), vec2(g.size().x + 10.0, 16.0));
                    painter.rect_filled(tag, round(3.0), theme::mix(p.win, p.ink, 8));
                    painter.galley(
                        pos2(tag.min.x + 5.0, cy - g.size().y / 2.0),
                        g,
                        Color32::PLACEHOLDER,
                    );
                    tx += tag.width() + 5.0;
                }
            }
            Col::Machine => {
                chip(
                    &painter,
                    p,
                    x,
                    cy,
                    w,
                    &row.machine,
                    theme::sans(SECONDARY - 0.5),
                );
            }
            Col::State => {
                let (head, tone, right) = plans::child_state(row);
                let right_w = text_right(
                    &painter,
                    cell.max.x,
                    cy,
                    &right,
                    theme::sans_medium(SECONDARY),
                    p.ink,
                );
                put(
                    &painter,
                    cell.min.x,
                    cy,
                    fit(
                        &painter,
                        &head,
                        theme::sans(SECONDARY),
                        tone_color(p, tone),
                        (w - right_w - 10.0).max(20.0),
                    ),
                );
            }
        }
        x += w + GAP;
    }
    resp
}

/// The plan's progress as one bar: passed, failed and running parts of the whole, with the
/// counts under it.
pub fn plan_bar(ui: &mut Ui, p: &Palette, plan: &PlanRow) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, round(4.0), p.raise);
    let total = plan.summary.total.max(1) as f32;
    let mut x = rect.min.x;
    for (n, color) in [
        (plan.summary.passed, p.ok),
        (plan.summary.failed, p.bad),
        (plan.summary.cancelled, p.dim),
        (plan.summary.running, p.accent),
    ] {
        if n == 0 {
            continue;
        }
        let w = (rect.width() * n as f32 / total).max(3.0);
        let seg = Rect::from_min_max(
            pos2(x, rect.min.y),
            pos2((x + w).min(rect.max.x), rect.max.y),
        );
        painter.rect_filled(seg, round(2.0), color);
        x += w + 1.0;
    }
}

/// One line of a plan's step list in the side panel: name, state, time. Clickable.
pub fn step_line(ui: &mut Ui, p: &Palette, l: &plans::StepLine) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
    let painter = ui.painter_at(rect);
    if resp.hovered() {
        painter.rect_filled(rect, round(3.0), theme::mix(p.panel, p.ink, 6));
    }
    let cy = rect.center().y;
    text_right(
        &painter,
        rect.max.x - 6.0,
        cy,
        &l.time,
        theme::sans(SECONDARY),
        p.dim,
    );
    // fixed columns so the states line up whatever the times say
    let state_w = 64.0;
    let sx = rect.max.x - 6.0 - 56.0 - 10.0 - state_w;
    put(
        &painter,
        sx,
        cy,
        fit(
            &painter,
            &l.state,
            theme::sans_medium(SECONDARY),
            tone_color(p, l.tone),
            state_w,
        ),
    );
    put(
        &painter,
        rect.min.x + 6.0,
        cy,
        fit(
            &painter,
            &l.name,
            theme::mono(SECONDARY),
            p.ink,
            (sx - rect.min.x - 18.0).max(20.0),
        ),
    );
    resp
}

/// The phase strip: proportional blocks, hatched for what is still to come, with the clock
/// times under them.
pub fn phase_strip(ui: &mut Ui, p: &Palette, strip: &Strip) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, 52.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let gap = 2.0;
    let usable = width - gap * strip.segments.len().saturating_sub(1) as f32;
    let mut x = rect.min.x;
    let mut cap_end = f32::MIN;
    for Segment {
        kind,
        share,
        caption,
    } in &strip.segments
    {
        let w = (usable * share).max(2.0);
        let bar = Rect::from_min_size(pos2(x, rect.min.y + 2.0), vec2(w, 8.0));
        match kind {
            SegKind::Queued => {
                painter.rect_filled(bar, round(2.0), p.faint);
            }
            SegKind::Sync => {
                painter.rect_filled(bar, round(2.0), p.agent(1));
            }
            SegKind::Running => {
                painter.rect_filled(bar, round(2.0), p.accent);
            }
            SegKind::Remaining => {
                painter.rect_stroke(
                    bar,
                    round(2.0),
                    Stroke::new(1.0_f32, p.line),
                    StrokeKind::Inside,
                );
                let mut hx = bar.min.x + 3.0;
                while hx + 4.0 < bar.max.x - 2.0 {
                    painter.line_segment(
                        [pos2(hx, bar.center().y), pos2(hx + 4.0, bar.center().y)],
                        Stroke::new(1.5_f32, p.faint),
                    );
                    hx += 7.0;
                }
            }
        }
        // a caption starts under its block, pushed right past the one before it and left
        // enough to stay inside the strip; one that cannot fit is left out
        let g = painter.layout_no_wrap(caption.clone(), theme::sans(LABEL + 0.5), p.dim);
        let at = x.max(cap_end + 8.0).min(rect.max.x - g.size().x);
        if at >= cap_end + 8.0 || cap_end == f32::MIN {
            painter.galley(pos2(at, rect.min.y + 14.0), g.clone(), Color32::PLACEHOLDER);
            cap_end = at + g.size().x;
        }
        x += w + gap;
    }
    let y = rect.min.y + 36.0;
    let n = strip.times.len();
    for (i, t) in strip.times.iter().enumerate() {
        let font = theme::sans(LABEL);
        if i == 0 {
            text_at(&painter, rect.min.x, y + 6.0, t, font, p.faint);
        } else if i + 1 == n {
            text_right(&painter, rect.max.x, y + 6.0, t, font, p.faint);
        } else {
            let g = painter.layout_no_wrap(t.clone(), font, p.faint);
            let cx = rect.min.x + width * (i as f32 / (n - 1).max(1) as f32);
            put(&painter, cx - g.size().x / 2.0, y + 6.0, g);
        }
    }
}

/// A rounded button in the palette: `primary` fills it with the accent, `danger` writes the
/// label in the failure colour. Returns the response; disabled ones take a tooltip.
pub fn button(
    ui: &mut Ui,
    p: &Palette,
    label: &str,
    primary: bool,
    danger: bool,
    enabled: bool,
) -> egui::Response {
    let font = theme::sans_medium(SECONDARY);
    let color = if primary {
        p.panel
    } else if danger {
        p.bad
    } else {
        p.ink
    };
    let g = ui.painter().layout_no_wrap(label.to_string(), font, color);
    let size = vec2(g.size().x + 20.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(
        size,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let painter = ui.painter();
    let (fill, stroke) = if primary {
        (p.accent, p.accent)
    } else {
        (p.raise, p.line)
    };
    let fill = if resp.hovered() && enabled {
        theme::mix(fill, p.ink, 10)
    } else {
        fill
    };
    let fade = if enabled { 1.0 } else { 0.4 };
    painter.rect_filled(rect, round(4.0), fill.gamma_multiply(fade));
    painter.rect_stroke(
        rect,
        round(4.0),
        Stroke::new(1.0_f32, stroke.gamma_multiply(fade)),
        StrokeKind::Inside,
    );
    painter.galley(rect.center() - g.size() / 2.0, g, Color32::PLACEHOLDER);
    if !enabled {
        painter.rect_filled(rect, round(4.0), Color32::TRANSPARENT);
    }
    resp
}

/// A section heading of the side panel: the name on the left, a faint note on the right.
pub fn section_head(ui: &mut Ui, p: &Palette, title: &str, note: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let cy = rect.center().y;
    let w = text_at(
        &painter,
        rect.min.x,
        cy,
        title,
        theme::sans_semibold(SECONDARY),
        p.ink,
    );
    if !note.is_empty() {
        let room = (rect.width() - w - 16.0).max(40.0);
        let g = fit(&painter, note, theme::sans(LABEL + 0.5), p.dim, room);
        let x = rect.max.x - g.size().x;
        put(&painter, x, cy, g);
    }
}

/// A dashed horizontal line from `x0` to `x1` at `y`.
fn dashed(painter: &Painter, x0: f32, x1: f32, y: f32, stroke: Stroke) {
    let mut x = x0;
    while x < x1 {
        painter.line_segment([pos2(x, y), pos2((x + 4.0).min(x1), y)], stroke);
        x += 7.0;
    }
}

/// One live graph: its label and value on top, the line under them with the scale at its top
/// left and, when there is one, the dashed marker named at its right.
fn graph(ui: &mut Ui, p: &Palette, g: &Graph, span: &str, color: Color32) {
    let width = ui.available_width();
    let (top, resp) = ui.allocate_exact_size(vec2(width, 20.0), Sense::hover());
    let tp = ui.painter_at(top);
    let cy = top.center().y;
    let lw = text_at(
        &tp,
        top.min.x,
        cy,
        g.label,
        theme::sans_medium(SECONDARY),
        p.ink,
    );
    text_at(
        &tp,
        top.min.x + lw + 8.0,
        cy,
        span,
        theme::sans(LABEL + 0.5),
        p.dim,
    );
    let vw = (width - lw - 16.0).max(40.0);
    let val = fit(&tp, &g.text, theme::sans_medium(SECONDARY), p.ink, vw);
    let vx = top.max.x - val.size().x;
    put(&tp, vx, cy, val);
    resp.on_hover_text(&g.tip);

    let (rect, _) = ui.allocate_exact_size(vec2(width, 44.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, round(3.0), p.win);
    painter.rect_stroke(
        rect,
        round(3.0),
        Stroke::new(1.0_f32, p.line),
        StrokeKind::Inside,
    );
    let inner = rect.shrink2(vec2(0.0, 3.0));
    let n = g.points.len();
    let xy = |i: usize, v: f32| {
        let x = if n <= 1 {
            inner.max.x
        } else {
            inner.min.x + inner.width() * i as f32 / (n - 1) as f32
        };
        pos2(x, inner.max.y - inner.height() * v.clamp(0.0, 1.0))
    };
    let pts: Vec<_> = g
        .points
        .iter()
        .enumerate()
        .map(|(i, v)| xy(i, *v))
        .collect();
    // the area under the line, a trapezoid per step so each piece is convex
    for w in pts.windows(2) {
        if let [a, b] = w {
            painter.add(egui::Shape::convex_polygon(
                vec![*a, *b, pos2(b.x, inner.max.y), pos2(a.x, inner.max.y)],
                color.gamma_multiply(0.18),
                Stroke::NONE,
            ));
        }
    }
    if pts.len() >= 2 {
        painter.add(egui::Shape::line(pts, Stroke::new(1.5_f32, color)));
    } else if let Some(pt) = pts.first() {
        painter.circle_filled(*pt, 2.5, color);
    }
    if let Some(m) = g.marker {
        let y = inner.max.y - inner.height() * m.clamp(0.0, 1.0);
        dashed(
            &painter,
            rect.min.x,
            rect.max.x,
            y,
            Stroke::new(1.0_f32, p.dim),
        );
        let t = painter.layout_no_wrap("usual peak".to_string(), theme::sans(LABEL), p.dim);
        let ty = (y - t.size().y - 1.0).max(rect.min.y + 1.0);
        painter.galley(
            pos2(rect.max.x - t.size().x - 4.0, ty),
            t,
            Color32::PLACEHOLDER,
        );
    }
    text_at(
        &painter,
        rect.min.x + 5.0,
        rect.max.y - 8.0,
        &g.scale,
        theme::sans(LABEL),
        p.dim,
    );
}

/// A label and its value, one line of the Resources block.
fn fact_line(ui: &mut Ui, p: &Palette, label: &str, value: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let cy = rect.center().y;
    text_at(
        &painter,
        rect.min.x,
        cy,
        label,
        theme::sans(SECONDARY),
        p.dim,
    );
    let g = fit(
        &painter,
        value,
        theme::sans(SECONDARY),
        p.ink,
        (rect.width() - 104.0).max(40.0),
    );
    put(&painter, rect.min.x + 104.0, cy, g);
}

/// The processes grouped by program: program and count, CPU in cores with a bar, memory.
fn proc_table(ui: &mut Ui, p: &Palette, procs: &Procs) {
    let width = ui.available_width();
    let cpu_x = (width * 0.42).max(120.0);
    let (hr, _) = ui.allocate_exact_size(vec2(width, 20.0), Sense::hover());
    let hp = ui.painter_at(hr);
    let hy = hr.center().y + 2.0;
    let head = |x: f32, t: &str| {
        text_at(
            &hp,
            hr.min.x + x,
            hy,
            &t.to_uppercase(),
            theme::sans(LABEL),
            p.dim,
        );
    };
    head(0.0, "Program");
    head(cpu_x, "CPU in cores");
    let mem_head = hp.layout_no_wrap("MEMORY".to_string(), theme::sans(LABEL), p.dim);
    put(&hp, hr.max.x - mem_head.size().x, hy, mem_head);
    for r in &procs.rows {
        let (rect, _) = ui.allocate_exact_size(vec2(width, 22.0), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.hline(rect.x_range(), rect.min.y, Stroke::new(1.0_f32, p.line));
        let cy = rect.center().y;
        let name_color = if r.others { p.dim } else { p.ink };
        let font = if r.others {
            theme::sans(SECONDARY)
        } else {
            theme::mono(SECONDARY)
        };
        let nw = text_at(&painter, rect.min.x, cy, &r.name, font, name_color);
        if !r.count.is_empty() {
            text_at(
                &painter,
                rect.min.x + nw + 6.0,
                cy,
                &r.count,
                theme::sans(LABEL + 0.5),
                p.dim,
            );
        }
        let bar_w = 44.0;
        let track = Rect::from_min_size(pos2(rect.min.x + cpu_x, cy - 2.5), vec2(bar_w, 5.0));
        painter.rect_filled(track, round(2.0), p.raise);
        if r.cpu_frac > 0.0 {
            painter.rect_filled(
                Rect::from_min_size(track.min, vec2((bar_w * r.cpu_frac).max(2.0), 5.0)),
                round(2.0),
                p.accent,
            );
        }
        text_at(
            &painter,
            track.max.x + 8.0,
            cy,
            &r.cpu,
            theme::sans(SECONDARY),
            p.ink,
        );
        text_right(
            &painter,
            rect.max.x,
            cy,
            &r.mem,
            theme::sans(SECONDARY),
            p.ink,
        );
    }
}

/// The Resources block: heading, CPU and memory graphs, disk lines, process table. Every
/// word and number comes from [`resources::Panel`].
pub fn resources(ui: &mut Ui, p: &Palette, panel: &resources::Panel) {
    section_head(ui, p, "Resources", &panel.note);
    ui.add_space(4.0);
    match &panel.body {
        resources::Body::Message(m) => {
            ui.add(
                egui::Label::new(RichText::new(m).font(theme::sans(SECONDARY)).color(p.dim)).wrap(),
            );
        }
        resources::Body::Live(l) => {
            graph(ui, p, &l.cpu, &l.span, p.accent);
            ui.add_space(6.0);
            graph(ui, p, &l.mem, &l.span, p.agent(0));
            ui.add_space(6.0);
            for (k, v) in &l.disk {
                fact_line(ui, p, k, v);
            }
            if let Some(s) = &l.stale {
                ui.add(
                    egui::Label::new(
                        RichText::new(s)
                            .font(theme::sans(LABEL + 0.5))
                            .color(p.warn),
                    )
                    .wrap(),
                );
            }
            if let Some(procs) = &l.procs {
                ui.add_space(10.0);
                section_head(
                    ui,
                    p,
                    "Processes",
                    &format!("grouped by program · {}", procs.note),
                );
                proc_table(ui, p, procs);
            }
        }
    }
}

/// A full-width table for the tabs that list rows: fixed column widths, the last column takes
/// the rest. A cell that does not fit is cut with an ellipsis and shows its text on hover.
/// `mono` columns are set in the mono face, `right` columns are right-aligned (numbers).
pub fn table(
    ui: &mut Ui,
    p: &Palette,
    id: egui::Id,
    cols: &[(&str, f32)],
    rows: &[Vec<String>],
    mono: &[usize],
    right: &[usize],
) {
    tick_table(ui, p, id, cols, rows, mono, right, &[], &[]);
}

/// [`table`] with a tick box in the first column of each row whose `ticks` entry is `Some`
/// (R-840). A click anywhere on such a row toggles it; the rows clicked are returned. A row
/// with a `tips` text shows it on hover.
#[allow(clippy::too_many_arguments)]
pub fn tick_table(
    ui: &mut Ui,
    p: &Palette,
    id: egui::Id,
    cols: &[(&str, f32)],
    rows: &[Vec<String>],
    mono: &[usize],
    right: &[usize],
    ticks: &[Option<bool>],
    tips: &[String],
) -> Vec<usize> {
    let mut clicked = Vec::new();
    let ticking = !ticks.is_empty();
    let width = ui.available_width();
    let gap = 14.0;
    let fixed: f32 = cols.iter().rev().skip(1).map(|c| c.1 + gap).sum();
    let last_w = (width - fixed - PAD).max(120.0);
    let col_x = |i: usize| -> (f32, f32) {
        let x: f32 = cols.iter().take(i).map(|c| c.1 + gap).sum();
        let w = if i + 1 == cols.len() {
            last_w
        } else {
            cols.get(i).map_or(60.0, |c| c.1)
        };
        (x, w)
    };
    let (hr, _) = ui.allocate_exact_size(vec2(width, 26.0), Sense::hover());
    let hp = ui.painter_at(hr);
    for (i, (title, _)) in cols.iter().enumerate() {
        let (x, w) = col_x(i);
        let g = fit(&hp, title, theme::sans_medium(SECONDARY), p.dim, w);
        let gx = if right.contains(&i) {
            hr.min.x + x + w - g.size().x
        } else {
            hr.min.x + x
        };
        put(&hp, gx, hr.center().y, g);
    }
    for (r, row) in rows.iter().enumerate() {
        let tick = ticks.get(r).copied().flatten();
        let sense = if tick.is_some() {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, resp) = ui.allocate_exact_size(vec2(width, 28.0), sense);
        let painter = ui.painter_at(rect);
        if tick == Some(true) {
            painter.rect_filled(rect, 0.0, p.accent.gamma_multiply(0.12));
        } else if tick.is_some() && resp.hovered() {
            painter.rect_filled(rect, 0.0, p.raise);
        }
        painter.hline(rect.x_range(), rect.min.y, Stroke::new(1.0_f32, p.line));
        if let Some(on) = tick {
            let b =
                Rect::from_center_size(pos2(rect.min.x + 8.0, rect.center().y), vec2(13.0, 13.0));
            if on {
                painter.rect_filled(b, round(3.0), p.accent);
                painter.line_segment(
                    [
                        pos2(b.min.x + 3.0, b.center().y),
                        pos2(b.center().x - 1.0, b.max.y - 3.5),
                    ],
                    Stroke::new(1.6_f32, p.panel),
                );
                painter.line_segment(
                    [
                        pos2(b.center().x - 1.0, b.max.y - 3.5),
                        pos2(b.max.x - 3.0, b.min.y + 3.5),
                    ],
                    Stroke::new(1.6_f32, p.panel),
                );
            } else {
                painter.rect_stroke(
                    b,
                    round(3.0),
                    Stroke::new(1.0_f32, p.dim),
                    StrokeKind::Inside,
                );
            }
        }
        if resp.clicked() {
            clicked.push(r);
        }
        if let Some(t) = tips.get(r).filter(|t| !t.is_empty()) {
            resp.on_hover_text(t);
        }
        for (i, text) in row.iter().enumerate() {
            // the first column holds the tick box, drawn above
            if ticking && i == 0 {
                continue;
            }
            let (x, w) = col_x(i);
            let font = if mono.contains(&i) {
                theme::mono(SECONDARY)
            } else {
                theme::sans(SECONDARY)
            };
            let g = fit(&painter, text, font, p.ink, w);
            let elided = g.elided;
            let gx = if right.contains(&i) {
                rect.min.x + x + w - g.size().x
            } else {
                rect.min.x + x
            };
            put(&painter, gx, rect.center().y, g);
            if elided {
                let cell =
                    Rect::from_min_size(pos2(rect.min.x + x, rect.min.y), vec2(w, rect.height()));
                ui.interact(cell, id.with((r, i)), Sense::hover())
                    .on_hover_text(text);
            }
        }
    }
    clicked
}

/// The Disk tab's machine cards: the name over what its build cache keeps (R-840).
pub fn disk_cards(ui: &mut Ui, p: &Palette, cards: &[crate::disk::DiskCard]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
        for c in cards {
            egui::Frame::new()
                .fill(p.panel)
                .stroke(Stroke::new(1.0_f32, p.line))
                .corner_radius(round(6.0))
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(&c.name)
                                .font(theme::sans_semibold(theme::BODY))
                                .color(p.ink),
                        );
                        ui.label(
                            RichText::new(&c.line)
                                .font(theme::sans(SECONDARY))
                                .color(if c.failed { p.warn } else { p.dim }),
                        );
                    });
                });
        }
    });
}
