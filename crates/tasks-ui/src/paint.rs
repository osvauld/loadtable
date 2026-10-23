//! Painting of the Queue tab's parts: the machine rows, the job list rows and the side
//! panel's strip. Drawing only: every text and every number comes from the view model.

use crate::columns::{self, Col, ColSpec, GAP};
use crate::detail::{SegKind, Segment, Strip};
use crate::model::{JobRow, MachineCard, Online, PhaseClass, Section, SlotLine, StateCell};
use crate::theme::{self, LABEL, Palette, SECONDARY};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Painter, Rect, Sense, Stroke, StrokeKind, Ui,
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

/// A thin horizontal gauge: `frac` of the track filled, with an optional limit mark.
fn gauge(
    painter: &Painter,
    rect: Rect,
    p: &Palette,
    frac: Option<f32>,
    fill: Color32,
    limit: Option<f32>,
) {
    let track = Rect::from_center_size(rect.center(), vec2(rect.width(), 6.0));
    painter.rect_filled(track, round(3.0), p.raise);
    if let Some(f) = frac {
        let w = (track.width() * f.clamp(0.0, 1.0)).max(if f > 0.0 { 3.0 } else { 0.0 });
        let fill_rect = Rect::from_min_size(track.min, vec2(w, track.height()));
        painter.rect_filled(fill_rect, round(3.0), fill);
    }
    if let Some(l) = limit {
        let x = track.min.x + track.width() * l.clamp(0.0, 1.0);
        painter.line_segment(
            [pos2(x, track.min.y - 3.0), pos2(x, track.max.y + 3.0)],
            Stroke::new(2.0_f32, p.ink.gamma_multiply(0.5)),
        );
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

/// The machine rows. Each machine is one row: name and role, memory-stall gauge with the
/// limit mark, CPU gauge, free RAM and the slot pips (R-582). An away machine gets one line
/// in place of the gauges.
pub fn machine_strip(ui: &mut Ui, p: &Palette, machines: &[MachineCard]) {
    let width = ui.available_width();
    let wide = width >= 900.0;
    let name_w = if wide { 170.0 } else { 140.0 };
    let ram_w = if wide { 110.0 } else { 0.0 };
    let flex = (width - 2.0 * PAD - name_w - ram_w).max(200.0);
    let (stall_w, cpu_w, slots_w) = if wide {
        (flex / 3.25, flex / 3.25, flex * 1.25 / 3.25)
    } else {
        (flex * 0.27, flex * 0.27, flex * 0.46)
    };
    let x_stall = PAD + name_w;
    let x_cpu = x_stall + stall_w;
    let x_ram = x_cpu + cpu_w;
    let x_slots = x_ram + ram_w;

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
            p.faint,
        );
    };
    head(PAD, "Machine");
    head(x_stall, "Memory stall");
    head(x_cpu, "CPU");
    if wide {
        head(x_ram, "RAM free");
    }
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
        text_at(
            &painter,
            nx + w + 6.0,
            cy,
            m.role,
            theme::sans(LABEL + 0.5),
            p.faint,
        );
        if let Some(note) = &m.away_note {
            text_at(
                &painter,
                rect.min.x + x_stall,
                cy,
                note,
                theme::sans(SECONDARY),
                p.dim.gamma_multiply(0.85),
            );
            resp.on_hover_text(&m.status_text);
            continue;
        }
        // memory stall: the scale ends at twice the limit, so the mark sits mid-gauge
        let over = m.stall.is_some_and(|s| s > m.stall_limit);
        let scale = m.stall_limit * 2.0;
        let gx = rect.min.x + x_stall;
        let val_w = 44.0;
        let g_rect = Rect::from_min_size(
            pos2(gx, cy - 5.0),
            vec2((stall_w - val_w - 16.0).max(30.0), 10.0),
        );
        gauge(
            &painter,
            g_rect,
            p,
            m.stall.map(|s| s / scale),
            if over { p.warn } else { p.dim },
            Some(0.5),
        );
        text_right(
            &painter,
            g_rect.max.x + 8.0 + val_w,
            cy,
            &crate::format::percent(m.stall),
            theme::sans(SECONDARY),
            if over { p.warn } else { p.dim },
        );
        let sx = Rect::from_min_max(pos2(gx, rect.min.y), pos2(rect.min.x + x_cpu, rect.max.y));
        // CPU
        let cx = rect.min.x + x_cpu;
        let c_rect = Rect::from_min_size(
            pos2(cx, cy - 5.0),
            vec2((cpu_w - val_w - 16.0).max(30.0), 10.0),
        );
        gauge(
            &painter,
            c_rect,
            p,
            m.cpu_busy.map(|c| c / 100.0),
            p.dim,
            None,
        );
        text_right(
            &painter,
            c_rect.max.x + 8.0 + val_w,
            cy,
            &crate::format::percent(m.cpu_busy),
            theme::sans(SECONDARY),
            p.dim,
        );
        let cr = Rect::from_min_max(pos2(cx, rect.min.y), pos2(rect.min.x + x_ram, rect.max.y));
        if wide {
            text_at(
                &painter,
                rect.min.x + x_ram,
                cy,
                &m.ram_free,
                theme::sans(SECONDARY),
                fade(p.ink),
            );
        }
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
        ui.interact(sx, resp.id.with("stall"), Sense::hover())
            .on_hover_text(format!("{}\n{}", m.pressure, m.swap));
        ui.interact(cr, resp.id.with("cpu"), Sense::hover())
            .on_hover_text(format!("{}\n{}", m.cpu, m.ram));
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
    // right part first: it keeps its room, the left part gets what is left
    let right_w = if s.right.is_empty() {
        0.0
    } else {
        text_right(
            painter,
            rect.max.x,
            line2,
            &s.right,
            theme::sans_medium(SECONDARY),
            p.ink,
        )
    };
    if !s.left.is_empty() {
        let room = (rect.width() - right_w - 10.0).max(20.0);
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

/// The labelled, empty region the live metrics will fill (graphs and the process table). It
/// shows no numbers: until the daemon reports them there is nothing honest to draw.
pub fn resources_placeholder(ui: &mut Ui, p: &Palette) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, 96.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.line_segment(
        [pos2(rect.min.x, rect.min.y), pos2(rect.max.x, rect.min.y)],
        Stroke::new(1.0_f32, p.line),
    );
    text_at(
        &painter,
        rect.min.x,
        rect.min.y + 16.0,
        "Resources",
        theme::sans_semibold(SECONDARY),
        p.ink,
    );
    painter.text(
        rect.center() + vec2(0.0, 10.0),
        Align2::CENTER_CENTER,
        "Live CPU, memory and disk for this job will show here.",
        theme::sans(LABEL + 0.5),
        p.faint,
    );
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
        let (rect, _) = ui.allocate_exact_size(vec2(width, 28.0), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.hline(rect.x_range(), rect.min.y, Stroke::new(1.0_f32, p.line));
        for (i, text) in row.iter().enumerate() {
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
}
