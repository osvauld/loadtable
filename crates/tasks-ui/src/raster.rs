//! A software rasteriser for headless screenshots: draws egui's tessellated meshes into an
//! RGB image, so layouts can be reviewed without a window, a GPU or a display server.
//! Blending is done in gamma space like egui's own glow painter; antialiasing comes from
//! egui's feathered meshes. Test and tooling code: it indexes freely.
#![allow(clippy::indexing_slicing)]

use crate::app::App;
use eframe::egui::{
    self, Color32, TextureId,
    epaint::{ClippedPrimitive, ImageData, Primitive},
};
use std::collections::HashMap;
use std::path::Path;

#[derive(Default)]
struct Raster {
    textures: HashMap<TextureId, ([usize; 2], Vec<Color32>)>,
}

impl Raster {
    fn apply(&mut self, delta: &egui::TexturesDelta) {
        for (id, update) in &delta.set {
            let ImageData::Color(image) = &update.image;
            match update.pos {
                None => {
                    self.textures
                        .insert(*id, (image.size, image.pixels.clone()));
                }
                Some([x0, y0]) => {
                    let Some((size, pixels)) = self.textures.get_mut(id) else {
                        continue;
                    };
                    for y in 0..image.size[1] {
                        for x in 0..image.size[0] {
                            let (tx, ty) = (x0 + x, y0 + y);
                            if tx < size[0] && ty < size[1] {
                                pixels[ty * size[0] + tx] = image.pixels[y * image.size[0] + x];
                            }
                        }
                    }
                }
            }
        }
        for id in &delta.free {
            self.textures.remove(id);
        }
    }

    fn draw(
        &self,
        primitives: &[ClippedPrimitive],
        ppp: f32,
        [width, height]: [usize; 2],
        background: Color32,
    ) -> Vec<u8> {
        let bg = [
            f32::from(background.r()) / 255.0,
            f32::from(background.g()) / 255.0,
            f32::from(background.b()) / 255.0,
        ];
        let mut image = vec![bg; width * height];
        for primitive in primitives {
            let Primitive::Mesh(mesh) = &primitive.primitive else {
                continue;
            };
            let Some((tsize, texels)) = self.textures.get(&mesh.texture_id) else {
                continue;
            };
            let clip = primitive.clip_rect;
            let (cx0, cy0) = ((clip.min.x * ppp).max(0.0), (clip.min.y * ppp).max(0.0));
            let (cx1, cy1) = (
                (clip.max.x * ppp).min(width as f32),
                (clip.max.y * ppp).min(height as f32),
            );
            for tri in mesh.indices.as_chunks::<3>().0 {
                let v = [
                    &mesh.vertices[tri[0] as usize],
                    &mesh.vertices[tri[1] as usize],
                    &mesh.vertices[tri[2] as usize],
                ];
                let p = v.map(|v| (v.pos.x * ppp, v.pos.y * ppp));
                let area =
                    (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
                if area.abs() < 1e-6 {
                    continue;
                }
                let min_x = p
                    .iter()
                    .map(|q| q.0)
                    .fold(f32::MAX, f32::min)
                    .max(cx0)
                    .floor() as i64;
                let max_x = p
                    .iter()
                    .map(|q| q.0)
                    .fold(f32::MIN, f32::max)
                    .min(cx1)
                    .ceil() as i64;
                let min_y = p
                    .iter()
                    .map(|q| q.1)
                    .fold(f32::MAX, f32::min)
                    .max(cy0)
                    .floor() as i64;
                let max_y = p
                    .iter()
                    .map(|q| q.1)
                    .fold(f32::MIN, f32::max)
                    .min(cy1)
                    .ceil() as i64;
                for y in min_y.max(0)..max_y.min(height as i64) {
                    for x in min_x.max(0)..max_x.min(width as i64) {
                        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                        if px < cx0 || px >= cx1 || py < cy0 || py >= cy1 {
                            continue;
                        }
                        let edge = |a: (f32, f32), b: (f32, f32)| {
                            (b.0 - a.0) * (py - a.1) - (px - a.0) * (b.1 - a.1)
                        };
                        let w0 = edge(p[1], p[2]) / area;
                        let w1 = edge(p[2], p[0]) / area;
                        let w2 = edge(p[0], p[1]) / area;
                        if w0 < -1e-4 || w1 < -1e-4 || w2 < -1e-4 {
                            continue;
                        }
                        let u = w0 * v[0].uv.x + w1 * v[1].uv.x + w2 * v[2].uv.x;
                        let t = w0 * v[0].uv.y + w1 * v[1].uv.y + w2 * v[2].uv.y;
                        let tx = ((u * tsize[0] as f32) as usize).min(tsize[0] - 1);
                        let ty = ((t * tsize[1] as f32) as usize).min(tsize[1] - 1);
                        let texel = texels[ty * tsize[0] + tx].to_array();
                        let mut src = [0.0f32; 4];
                        for (c, out) in src.iter_mut().enumerate() {
                            let vertex = w0 * f32::from(v[0].color.to_array()[c])
                                + w1 * f32::from(v[1].color.to_array()[c])
                                + w2 * f32::from(v[2].color.to_array()[c]);
                            *out = vertex / 255.0 * f32::from(texel[c]) / 255.0;
                        }
                        let dst = &mut image[y as usize * width + x as usize];
                        for c in 0..3 {
                            dst[c] = (src[c] + dst[c] * (1.0 - src[3])).clamp(0.0, 1.0);
                        }
                    }
                }
            }
        }
        image
            .iter()
            .flat_map(|px| px.map(|c| (c * 255.0).round() as u8))
            .collect()
    }
}

/// Draws `app` for a window of `size` physical pixels and returns RGB bytes.
pub fn render(app: &mut App, size: [usize; 2], ppp: f32, dark: bool) -> Vec<u8> {
    let ctx = egui::Context::default();
    crate::theme::install(&ctx);
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });
    ctx.set_pixels_per_point(ppp);
    let screen = egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(size[0] as f32 / ppp, size[1] as f32 / ppp),
    );
    let mut raster = Raster::default();
    let mut last = None;
    // egui settles its layout over a few passes
    for i in 0..3 {
        let input = egui::RawInput {
            screen_rect: Some(screen),
            time: Some(f64::from(i) * 0.1),
            ..Default::default()
        };
        let out = ctx.run(input, |ctx| app.draw(ctx));
        raster.apply(&out.textures_delta);
        last = Some(out);
    }
    let Some(out) = last else {
        return Vec::new();
    };
    let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
    let bg = crate::theme::Palette::of(dark).win;
    raster.draw(&prims, out.pixels_per_point, size, bg)
}

/// Writes RGB bytes as a PNG.
pub fn write_png(path: &Path, rgb: &[u8], [w, h]: [usize; 2]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgb).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::link::{FakeLink, LinkError};
    use crate::model::{self, Tab};
    use std::sync::Arc;

    fn dead_link() -> Arc<FakeLink> {
        Arc::new(FakeLink::from_fn(|_| Err(LinkError("fixture".into()))))
    }

    fn lit_pixels(rgb: &[u8]) -> usize {
        let px = rgb.as_chunks::<3>().0;
        let bg = px.first().map(|p| p.to_vec()).unwrap_or_default();
        px.iter().filter(|p| p.as_slice() != bg.as_slice()).count()
    }

    #[test]
    fn the_fixture_dashboards_render_something_in_both_themes() {
        for dark in [true, false] {
            let mut app = App::fixed(&fixtures::busy(), dead_link());
            let size = [1180, 780];
            let rgb = render(&mut app, size, 1.0, dark);
            assert_eq!(rgb.len(), size[0] * size[1] * 3);
            assert!(lit_pixels(&rgb) > 20_000, "dark={dark}");
        }
    }

    /// Job 91 of the busy fixture, refused before it started with a long reason.
    fn long_refusal() -> App {
        let mut d = fixtures::busy();
        for j in &mut d.jobs {
            if j.id.0 == 91 {
                j.phase = tasks_core::Phase::NotStarted {
                    reason: "refused (asked with --peer mac): mac has not answered since this \
                             daemon started, so there is nothing to wait for; kurukkan excluded: \
                             not requested, mac excluded: unreachable, never seen"
                        .into(),
                };
            }
        }
        let mut a = App::fixed(&d, dead_link());
        a.show_tab(Tab::History);
        a.select(91);
        a
    }

    #[test]
    fn r651_a_long_state_is_cut_inside_the_side_panel_not_pushed_over_its_edge() {
        let size = [1280, 700];
        let rgb = render(&mut long_refusal(), size, 1.0, true);
        let px = |x: usize, y: usize| {
            let i = (y * size[0] + x) * 3;
            rgb[i..i + 3].to_vec()
        };
        // the panel's 16 px left margin holds no text: each pixel row of it is one colour
        // (background, or a section rule that crosses the whole panel)
        let edge = size[0] - 440;
        for y in 90..size[1] - 40 {
            let first = px(edge + 4, y);
            for x in edge + 5..edge + 12 {
                assert_eq!(px(x, y), first, "ink at ({x}, {y}) in the panel's margin");
            }
        }
    }

    #[test]
    fn r651_dragging_the_side_panels_edge_widens_it() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut app = long_refusal();
        let id = egui::Id::new("detail");
        let width = |ctx: &egui::Context| {
            egui::containers::panel::PanelState::load(ctx, id).map(|s| s.rect.width())
        };
        frame(&mut app, &ctx, vec![]);
        let before = width(&ctx).expect("the side panel was laid out");
        let edge = 1400.0 - before;
        let at = |x: f32| egui::pos2(x, 400.0);
        let button = |x: f32, pressed: bool| egui::Event::PointerButton {
            pos: at(x),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(&mut app, &ctx, vec![egui::Event::PointerMoved(at(edge))]);
        frame(&mut app, &ctx, vec![button(edge, true)]);
        for step in 1..=10 {
            let x = edge - 20.0 * step as f32;
            frame(&mut app, &ctx, vec![egui::Event::PointerMoved(at(x))]);
        }
        frame(&mut app, &ctx, vec![button(edge - 200.0, false)]);
        frame(&mut app, &ctx, vec![]);
        let after = width(&ctx).expect("the side panel was laid out");
        assert!(
            after > before + 150.0,
            "the panel went from {before} to {after} px"
        );
    }

    #[test]
    fn r205_symbols_used_by_the_window_exist_in_the_embedded_fonts() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let mut texts = String::new();
        for d in [
            fixtures::busy(),
            fixtures::offline(),
            fixtures::idle(),
            fixtures::three_machines(),
            fixtures::mac(),
        ] {
            let vm = model::build(&d, 0);
            for m in &vm.machines {
                texts.push_str(&format!(
                    "{} {} {} {} {} {} {} {} {:?} {:?}",
                    m.name,
                    m.role,
                    m.status_text,
                    m.cpu,
                    m.ram,
                    m.swap,
                    m.pressure,
                    m.ram_used,
                    m.away_note,
                    m.notes
                ));
                for s in &m.slots {
                    texts.push_str(&s.kind);
                    for h in &s.holders {
                        texts.push_str(&h.label);
                    }
                }
            }
            for r in vm.running.iter().chain(&vm.queued).chain(&vm.recent) {
                for c in crate::columns::specs() {
                    texts.push_str(&crate::columns::cell(r, c.col));
                }
                let det = crate::detail::detail(r, vm.now_ms, &jiff::tz::TimeZone::UTC, None);
                texts.push_str(&det.state);
                for (k, v) in &det.facts {
                    texts.push_str(k);
                    texts.push_str(v);
                }
                for v in [fixtures::metrics_running(), fixtures::metrics_forwarded()] {
                    let panel = crate::resources::panel(
                        r,
                        crate::resources::Input {
                            view: Some(&v),
                            error: Some("x"),
                            cores: Some(8.0),
                            ..Default::default()
                        },
                    );
                    texts.push_str(&format!("{panel:?}"));
                }
                if let Some(s) = &det.strip {
                    texts.push_str(&s.times.join(" "));
                    for g in &s.segments {
                        texts.push_str(&g.caption);
                    }
                }
            }
            for n in &vm.notices {
                texts.push_str(&n.title);
                texts.push_str(&n.text);
            }
        }
        for t in Tab::SHOWN.iter().chain(&Tab::PLANNED) {
            texts.push_str(t.label());
        }
        for s in [
            model::Section::Running,
            model::Section::Queued,
            model::Section::Recent,
        ] {
            texts.push_str(s.title());
        }
        texts.push_str(
            "Log Move up Cancel… Cancel job Keep it running Drain queue… Resume queue Drain \
             Keep going Follow Search Clear Refresh Raw JSON live · connecting… daemon unreachable \
             Resources Processes Machine Memory CPU, by job Slots ↑↓ select · Enter log · Esc deselect \
             updated 3s ago Select a job to see its phases, folder and actions. \
             PROGRAM CPU IN CORES MEMORY usual peak last hour · the rest is in History",
        );
        // the Disk tab: its buttons, the report's texts and the question before freeing
        {
            let v = fixtures::disk();
            let all = crate::disk::reclaimable(&v);
            texts.push_str("Select merged, gone and old Clear Free Keep it measuring… (a few seconds) \
                 Measuring build output on every machine… Shared by every checkout never freed here \
                 Free build output? Kept: ");
            texts.push_str(&crate::disk::summary(&v));
            texts.push_str(&crate::disk::confirm_text(&v, &all));
            for c in crate::disk::cards(&v) {
                texts.push_str(&c.line);
            }
            for t in crate::disk::columns(&v) {
                texts.push_str(&t);
            }
            for r in &v.rows {
                texts.push_str(&crate::disk::cells(&v, r, fixtures::NOW_MS).join(" "));
                texts.push_str(crate::disk::state_help(r.state));
            }
            texts.push_str(&crate::disk::shared_lines(&v).join(" "));
        }
        // the Audit tab: filters, column titles and the rows' own symbols
        texts.push_str("Action Peer Job Apply all name or DID id #101 did:key:z6Mk…EGta");
        for t in crate::audit::COLUMNS.iter().chain(&crate::audit::FAMILIES) {
            texts.push_str(t);
        }
        let families = [
            egui::FontFamily::Proportional,
            egui::FontFamily::Monospace,
            crate::theme::medium_family(),
            crate::theme::semibold_family(),
            crate::theme::mono_medium_family(),
        ];
        let missing: Vec<(egui::FontFamily, char)> = ctx.fonts_mut(|f| {
            families
                .iter()
                .flat_map(|fam| {
                    let font = egui::FontId::new(14.0, fam.clone());
                    texts
                        .chars()
                        .filter(|c| !c.is_control())
                        .filter(|c| !f.has_glyph(&font, *c))
                        .map(|c| (fam.clone(), c))
                        .collect::<Vec<_>>()
                })
                .collect()
        });
        assert!(missing.is_empty(), "no glyph for {missing:?}");
        // the faces are Plex's own: the first font of the family has the Latin letters
        assert!(!texts.is_empty());
    }

    enum Pick {
        Job(i64),
        Plan,
    }

    /// The fixture of plan 2 mid-run, open or closed, with one thing selected.
    fn plan_app(open: bool, pick: Pick) -> App {
        let mut a = App::fixed(&fixtures::with_plan(), dead_link());
        a.set_plan_open(2, open);
        match pick {
            Pick::Job(id) => {
                a.select(id);
                if id == 101 {
                    a.set_metrics(101, fixtures::metrics_running());
                }
            }
            Pick::Plan => a.select_plan(2),
        }
        a
    }

    /// Leftmost x in `[0, to)` whose pixel in row `y` differs from the row's own background.
    fn first_ink(rgb: &[u8], width: usize, y: usize, from: usize, to: usize) -> Option<usize> {
        let px = |x: usize| rgb[(y * width + x) * 3..(y * width + x) * 3 + 3].to_vec();
        let bg = px(from);
        (from..to).find(|x| px(*x) != bg)
    }

    // R-749
    #[test]
    fn r749_steps_sit_indented_under_their_plan_row() {
        let size = [1400, 1100];
        let mut app = plan_app(true, Pick::Job(101));
        let rgb = render(&mut app, size, 1.0, true);
        let parent = app
            .row_rect(crate::plans::plan_key(2))
            .expect("plan row drawn");
        let child = app.row_rect(336).expect("step row drawn");
        assert!(child.min.y >= parent.max.y, "the step is under the plan");
        let cy = |r: egui::Rect| (r.center().y) as usize;
        // the plan's marker starts at the list's padding; the step's id is pushed in by INDENT
        let pad = crate::paint::PAD as usize;
        let p_ink = first_ink(&rgb, size[0], cy(parent), 2, 80).expect("marker ink");
        assert!(p_ink <= pad + 2, "marker at {p_ink}");
        // skip the guide line at PAD + 5; the id text starts after it
        let guide = pad + 5;
        let c_ink = first_ink(&rgb, size[0], cy(child), guide + 2, 120).expect("id ink");
        assert!(
            c_ink >= pad + crate::paint::INDENT as usize - 1,
            "the step's id starts at {c_ink}"
        );
        // and the plan's own text is not that far in
        let p_text = first_ink(&rgb, size[0], cy(parent), pad + 14, 120).expect("plan text");
        assert!(c_ink + 2 >= p_text, "id {c_ink} vs plan label {p_text}");
        // every step of the open plan has a row
        for id in 330..=369 {
            assert!(app.row_rect(id).is_some(), "step {id}");
        }
    }

    // R-749
    #[test]
    fn r749_no_row_draws_past_the_right_edge_even_when_narrow() {
        for width in [700usize, 1100] {
            let size = [width, 2400];
            let mut app = App::fixed(&fixtures::with_plan(), dead_link());
            app.set_plan_open(2, true);
            let rgb = render(&mut app, size, 1.0, true);
            let key_plan = crate::plans::plan_key(2);
            let mut keys = vec![101, key_plan];
            keys.extend(330..=369);
            for key in keys {
                let r = app.row_rect(key).expect("row drawn");
                let right = r.max.x as usize;
                for y in (r.min.y as usize + 2)..(r.max.y as usize - 1) {
                    let px =
                        |x: usize| rgb[(y * size[0] + x) * 3..(y * size[0] + x) * 3 + 3].to_vec();
                    let edge = px(right - 1);
                    for x in right.saturating_sub(9)..right {
                        assert_eq!(
                            px(x),
                            edge,
                            "ink at ({x}, {y}) in the margin of row {key} at width {width}"
                        );
                    }
                }
            }
        }
    }

    // R-744
    #[test]
    fn r744_right_opens_and_left_closes_a_plan_and_left_on_a_step_goes_up() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut app = plan_app(false, Pick::Plan);
        frame(&mut app, &ctx, vec![]);
        assert!(
            app.row_rect(330).is_none(),
            "plan 2 starts with its steps hidden"
        );
        frame(&mut app, &ctx, vec![key(egui::Key::ArrowRight)]);
        frame(&mut app, &ctx, vec![]);
        assert!(app.row_rect(330).is_some(), "Right opened it");
        frame(&mut app, &ctx, vec![key(egui::Key::ArrowDown)]);
        assert_eq!(
            app.selected_id(),
            Some(336),
            "Down went to the first step shown (the failed one)"
        );
        frame(&mut app, &ctx, vec![key(egui::Key::ArrowLeft)]);
        assert_eq!(
            app.selected_id(),
            Some(crate::plans::plan_key(2)),
            "Left climbed to the plan"
        );
        frame(&mut app, &ctx, vec![key(egui::Key::ArrowLeft)]);
        frame(&mut app, &ctx, vec![]);
        assert!(app.row_rect(330).is_none(), "Left closed it");
        // Down from a closed plan skips its steps
        frame(&mut app, &ctx, vec![key(egui::Key::ArrowDown)]);
        assert_ne!(app.selected_id(), Some(336));
    }

    /// Writes screenshots when `TASKS_UI_SCREENSHOT_DIR` is set; otherwise only checks that
    /// every view renders.
    #[test]
    fn screenshots() {
        let dir = std::env::var_os("TASKS_UI_SCREENSHOT_DIR").map(std::path::PathBuf::from);
        let history = history();
        let main = |dark: bool| {
            let mut a = App::fixed(&fixtures::mac(), dead_link());
            a.select(101);
            a.set_metrics(101, fixtures::metrics_running());
            (a, [1400, 1240], dark)
        };
        let shots: Vec<(&str, App, [usize; 2], bool)> = vec![
            ("main-dark", main(true).0, main(true).1, true),
            ("main-light", main(false).0, main(false).1, false),
            (
                "offline-light",
                {
                    let mut a = App::fixed(&fixtures::offline(), dead_link());
                    a.select(103);
                    a
                },
                [1400, 860],
                false,
            ),
            (
                "waiting-dark",
                {
                    let mut a = App::fixed(&fixtures::three_machines(), dead_link());
                    a.select(111);
                    a
                },
                [1400, 900],
                true,
            ),
            (
                "narrow-dark",
                {
                    let mut a = App::fixed(&fixtures::mac(), dead_link());
                    a.select(101);
                    a.set_metrics(101, fixtures::metrics_running());
                    a
                },
                [700, 1400],
                true,
            ),
            (
                "forwarded-dark",
                {
                    let mut a = App::fixed(&fixtures::mac(), dead_link());
                    a.select(104);
                    a.set_metrics(104, fixtures::metrics_forwarded());
                    a
                },
                [1400, 1240],
                true,
            ),
            (
                "fetching-dark",
                {
                    let mut a = App::fixed(&fixtures::fetching(), dead_link());
                    a.select(104);
                    a
                },
                [1400, 1240],
                true,
            ),
            (
                "finished-forward-light",
                {
                    let mut a = App::fixed(&fixtures::mac(), dead_link());
                    a.select(94);
                    a.set_metrics(94, fixtures::metrics_forwarded());
                    a
                },
                [1400, 1240],
                false,
            ),
            (
                "plan-collapsed-dark",
                plan_app(false, Pick::Job(101)),
                [1400, 900],
                true,
            ),
            (
                "plan-expanded-dark",
                plan_app(true, Pick::Job(336)),
                [1400, 1100],
                true,
            ),
            (
                "plan-detail-dark",
                plan_app(true, Pick::Plan),
                [1400, 900],
                true,
            ),
            (
                "plan-expanded-light",
                plan_app(true, Pick::Job(336)),
                [1400, 1100],
                false,
            ),
            (
                "plan-narrow-dark",
                plan_app(true, Pick::Plan),
                [700, 1400],
                true,
            ),
            ("refused-long-dark", long_refusal(), [1280, 700], true),
            (
                "battery-low-dark",
                App::fixed(&fixtures::mac_low_battery(), dead_link()),
                [1400, 600],
                true,
            ),
            (
                "metrics-unavailable-dark",
                {
                    let mut a = App::fixed(&fixtures::mac(), dead_link());
                    a.select(102);
                    a.set_metrics_error(102, "the daemon did not answer");
                    a
                },
                [1400, 900],
                true,
            ),
            (
                "cancel-dark",
                {
                    let mut a = App::fixed(&fixtures::busy(), dead_link());
                    a.select(110);
                    a.ask_cancel(110);
                    a
                },
                [1280, 800],
                true,
            ),
            (
                "drain-dark",
                {
                    let mut a = App::fixed(&fixtures::three_machines(), dead_link());
                    a.ask_drain();
                    a
                },
                [1100, 620],
                true,
            ),
            (
                "history-dark",
                {
                    let mut a = App::fixed(&fixtures::busy(), dead_link());
                    a.show_tab(Tab::History);
                    a.select(91);
                    a
                },
                [1280, 600],
                true,
            ),
            (
                "timings-dark",
                {
                    let mut a = App::fixed(&fixtures::idle(), dead_link());
                    a.show_tab(Tab::Timings);
                    a.set_history(history.clone());
                    a
                },
                [1100, 600],
                true,
            ),
            (
                "disk-dark",
                {
                    let mut a = App::fixed(&fixtures::idle(), dead_link());
                    a.show_tab(Tab::Disk);
                    a.set_disk(fixtures::disk());
                    a.tick_disk("~/app-worktrees/v2");
                    a.tick_disk("(unknown checkout 4ca7e0dc2b4d)");
                    a
                },
                [1280, 560],
                true,
            ),
            (
                "disk-light",
                {
                    let mut a = App::fixed(&fixtures::idle(), dead_link());
                    a.show_tab(Tab::Disk);
                    a.set_disk(fixtures::disk());
                    a
                },
                [1280, 560],
                false,
            ),
            (
                "stalls-dark",
                {
                    let mut a = App::fixed(&fixtures::idle(), dead_link());
                    a.show_tab(Tab::Stalls);
                    a.set_stalls(stall_rows());
                    a
                },
                [1280, 500],
                true,
            ),
            (
                "audit-dark",
                {
                    let mut a = App::fixed(&fixtures::idle(), dead_link());
                    a.show_tab(Tab::Audit);
                    a.set_audit(audit_rows());
                    a
                },
                [1280, 600],
                true,
            ),
            (
                "audit-light-empty",
                {
                    let mut a = App::fixed(&fixtures::idle(), dead_link());
                    a.show_tab(Tab::Audit);
                    a.set_audit(crate::audit::AuditData {
                        rows: vec![],
                        truncated: false,
                    });
                    a
                },
                [900, 400],
                false,
            ),
        ];
        for (name, mut app, size, dark) in shots {
            let rgb = render(&mut app, size, 1.0, dark);
            assert!(lit_pixels(&rgb) > 5_000, "{name}");
            if let Some(dir) = &dir {
                let _ = std::fs::create_dir_all(dir);
                assert_eq!(
                    write_png(&dir.join(format!("{name}.png")), &rgb, size),
                    Ok(())
                );
            }
        }
    }

    /// The shots on loadtable.osvauld.com, at twice the scale for sharp screens: the queue as
    /// the front page's preview, and the whole window with a job open. Written with the others
    /// when `TASKS_UI_SCREENSHOT_DIR` is set.
    #[test]
    fn site_screenshots() {
        let dir = std::env::var_os("TASKS_UI_SCREENSHOT_DIR").map(std::path::PathBuf::from);
        for dark in [true, false] {
            let theme = if dark { "dark" } else { "light" };
            let hero = App::fixed(&fixtures::mac(), dead_link());
            let mut window = App::fixed(&fixtures::mac(), dead_link());
            window.select(101);
            window.set_metrics(101, fixtures::metrics_running());
            for (name, mut app, size) in [
                (format!("site-hero-{theme}"), hero, [1520, 1400]),
                (format!("site-window-{theme}"), window, [2360, 1520]),
            ] {
                let rgb = render(&mut app, size, 2.0, dark);
                assert!(lit_pixels(&rgb) > 20_000, "{name}");
                if let Some(dir) = &dir {
                    let _ = std::fs::create_dir_all(dir);
                    assert_eq!(
                        write_png(&dir.join(format!("{name}.png")), &rgb, size),
                        Ok(())
                    );
                }
            }
        }
    }

    /// Every text one frame of `app` draws.
    fn drawn_text(app: &mut App) -> Vec<String> {
        drawn(app).into_iter().map(|(t, _)| t).collect()
    }

    /// Each text a frame of `app` draws, with where it lands.
    fn drawn(app: &mut App) -> Vec<(String, egui::Rect)> {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        drawn_in(app, &ctx)
    }

    /// [`drawn`] in a context earlier frames ran in, so a window opened then is laid out.
    fn drawn_in(app: &mut App, ctx: &egui::Context) -> Vec<(String, egui::Rect)> {
        fn walk(shape: &egui::Shape, into: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(t) => into.push((
                    t.galley.text().to_string(),
                    t.galley.rect.translate(t.pos.to_vec2()),
                )),
                egui::Shape::Vec(v) => {
                    for s in v {
                        walk(s, into);
                    }
                }
                _ => {}
            }
        }
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        let out = ctx.run(input, |ctx| app.draw(ctx));
        let mut texts = Vec::new();
        for c in &out.shapes {
            walk(&c.shape, &mut texts);
        }
        texts
    }

    // R-835
    #[test]
    fn r835_a_worker_on_low_battery_says_so_in_place_of_its_role() {
        let texts = drawn_text(&mut App::fixed(&fixtures::mac_low_battery(), dead_link()));
        assert!(texts.iter().any(|t| t == "battery 22%"), "{texts:?}");
        // the other machines keep their role
        let texts = drawn_text(&mut App::fixed(&fixtures::mac(), dead_link()));
        assert!(!texts.iter().any(|t| t.starts_with("battery")), "{texts:?}");
    }

    // R-838
    #[test]
    fn r838_a_long_time_left_is_cut_to_its_cell_and_nothing_is_drawn_under_it() {
        let mut d = fixtures::busy();
        for r in d.status.running.iter_mut().filter(|r| r.id.0 == 102) {
            r.eta_secs = Some(658.0);
            r.eta_low_secs = None;
            r.eta_rough_from = Some("nixos".into());
        }
        let texts = drawn(&mut App::fixed(&d, dead_link()));
        let (_, at) = texts
            .iter()
            .find(|(t, _)| t.starts_with("~10m58s left"))
            .unwrap_or_else(|| panic!("the time left is drawn: {texts:?}"));
        let under: Vec<_> = texts
            .iter()
            .filter(|(t, r)| !t.starts_with("~10m58s left") && r.shrink(0.5).intersects(*at))
            .collect();
        assert!(under.is_empty(), "drawn over the time left: {under:?}");
    }

    // R-833
    #[test]
    fn r833_the_window_says_loadtable_not_its_old_name() {
        assert_eq!(crate::app::NAME, "LoadTable");
        assert_eq!(crate::app::APP_ID, "loadtable");
        let texts = drawn_text(&mut App::fixed(&fixtures::busy(), dead_link()));
        assert!(texts.iter().any(|t| t == "LoadTable"), "{texts:?}");
        assert!(!texts.iter().any(|t| t == "tasks"), "{texts:?}");
    }

    /// Runs one frame of `app` with `events` as input.
    fn frame(app: &mut App, ctx: &egui::Context, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            events,
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| app.draw(ctx));
    }

    fn key(k: egui::Key) -> egui::Event {
        egui::Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn r645_the_window_asks_for_metrics_only_of_a_running_job_and_not_twice_in_a_row() {
        let link = Arc::new(FakeLink::from_fn(|req| match req {
            tasks_daemon::api::Request::JobMetrics { .. } => Ok(
                tasks_daemon::api::Response::JobMetrics(fixtures::metrics_running()),
            ),
            _ => Err(LinkError("unexpected".into())),
        }));
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut app = App::fixed(&fixtures::mac(), link.clone());
        // a waiting job: nothing is asked
        app.select(110);
        for _ in 0..3 {
            frame(&mut app, &ctx, vec![]);
        }
        assert!(link.requests().is_empty());
        // a running one: one request, however many frames pass before the answer
        app.select(101);
        for _ in 0..5 {
            frame(&mut app, &ctx, vec![]);
        }
        let asked = |l: &FakeLink| {
            l.requests()
                .iter()
                .filter(|r| matches!(r, tasks_daemon::api::Request::JobMetrics { .. }))
                .count()
        };
        assert_eq!(asked(&link), 1);
        assert!(app.metrics_requested());
    }

    #[test]
    fn r587_arrow_keys_move_the_selection_and_escape_lets_go() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut app = App::fixed(&fixtures::busy(), dead_link());
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.selected_id(), None);
        frame(&mut app, &ctx, vec![key(egui::Key::ArrowDown)]);
        assert_eq!(app.selected_id(), Some(101), "the first row");
        frame(&mut app, &ctx, vec![key(egui::Key::ArrowDown)]);
        assert_eq!(app.selected_id(), Some(102));
        frame(&mut app, &ctx, vec![key(egui::Key::ArrowUp)]);
        assert_eq!(app.selected_id(), Some(101));
        frame(&mut app, &ctx, vec![key(egui::Key::Escape)]);
        assert_eq!(app.selected_id(), None);
        // Enter on a job with a log opens the log window: its feed asks the daemon
        let link = dead_link();
        let mut app = App::fixed(&fixtures::busy(), link.clone());
        app.select(101);
        frame(&mut app, &ctx, vec![key(egui::Key::Enter)]);
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !link.requests().is_empty(),
            "the log feed asked the daemon for job 101"
        );
    }

    #[test]
    fn r589_drain_asks_first_and_sends_nothing_until_confirmed() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let link = dead_link();
        let mut app = App::fixed(&fixtures::busy(), link.clone());
        app.ask_drain();
        frame(&mut app, &ctx, vec![]);
        frame(&mut app, &ctx, vec![]);
        assert!(
            link.requests()
                .iter()
                .all(|r| !matches!(r, tasks_daemon::api::Request::Drain { .. })),
            "nothing was drained by opening the question"
        );
        // while the question is open the arrow keys do not move the list behind it
        frame(&mut app, &ctx, vec![key(egui::Key::ArrowDown)]);
        assert_eq!(app.selected_id(), None);
    }

    // R-840
    #[test]
    fn r840_the_disk_tab_frees_only_after_asking_and_only_the_ticked_rows() {
        use tasks_daemon::api::Request;
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let link = dead_link();
        let mut app = App::fixed(&fixtures::idle(), link.clone());
        app.show_tab(Tab::Disk);
        app.set_disk(fixtures::disk());
        let text = drawn_text(&mut app).join("\n");
        for want in [
            "build cache 134 GiB · 98 GiB can be freed",
            "could not report: mac runs an older loadtabled",
            "~/app-worktrees/v2",
            "v2 (in dev)",
            "in use",
            "Shared by every checkout",
        ] {
            assert!(text.contains(want), "{want:?} not drawn in:\n{text}");
        }
        // R-844: with nothing ticked the button names no size
        assert!(text.lines().any(|l| l == "Free"), "a bare Free in:\n{text}");
        assert!(!text.contains("Free 0 B"));
        app.tick_disk("~/app-worktrees/v2");
        app.tick_disk("~/task-manager");
        app.set_disk(fixtures::disk());
        app.ask_free();
        frame(&mut app, &ctx, vec![]);
        frame(&mut app, &ctx, vec![]);
        assert!(
            link.requests().iter().all(|r| !matches!(
                r,
                Request::Gc { checkouts, .. } if !checkouts.is_empty()
            )),
            "nothing was freed by opening the question"
        );
        let text = drawn_in(&mut app, &ctx)
            .into_iter()
            .map(|(t, _)| t)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Free 46 GiB from 1 checkout?"),
            "the in-use row is never ticked: {text}"
        );
    }

    fn stall_rows() -> crate::stalls::StallData {
        use tasks_daemon::api::{TripDetails, TripProc};
        let row = |seq: i64, resource: &str, reason: &str| crate::stalls::StallRow {
            seq,
            at_ms: fixtures::NOW_MS - seq * 600_000,
            trip: TripDetails {
                resource: resource.into(),
                source: "psi".into(),
                reason: reason.into(),
                top_io: vec![TripProc {
                    pid: 99,
                    name: "firefox".into(),
                    io_bytes: 25 << 30,
                    rss_bytes: 1 << 30,
                }],
                ..TripDetails::default()
            },
        };
        crate::stalls::StallData {
            rows: vec![
                row(1, "memory", "memory stalls 12% over 10%"),
                row(2, "io", "I/O pressure 22% >= 10%"),
                row(3, "cpu", "CPU pressure 64% >= 40%"),
            ],
            truncated: false,
            unreadable: 0,
        }
    }

    fn audit_rows() -> crate::audit::AuditData {
        use tasks_daemon::api::AuditRowView;
        let row =
            |seq: i64, action: &str, job: Option<i64>, details: serde_json::Value| AuditRowView {
                seq,
                at_ms: fixtures::NOW_MS - (20 - seq) * 60_000,
                actor: "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK".into(),
                actor_name: Some("laptop".into()),
                owner: "claude@laptop".into(),
                action: action.into(),
                job,
                peer: Some("did:key:z6MksPeerPeerPeerPeerPeerPeerPeerPeerPeerPeerPeer".into()),
                peer_name: Some("nixos".into()),
                target: None,
                details,
                prev_hash: "0".into(),
                hash: "1".into(),
            };
        crate::audit::AuditData {
            rows: vec![
                row(
                    5,
                    "job.cancelled",
                    Some(101),
                    serde_json::json!({"by": "claude@laptop", "reason": "wrong branch", "result": "requested"}),
                ),
                row(
                    4,
                    "job.started",
                    Some(101),
                    serde_json::json!({"machine": "nixos"}),
                ),
                row(3, "conn.up", None, serde_json::json!({"path": "direct"})),
                row(
                    2,
                    "grant.issued",
                    None,
                    serde_json::json!({"role": "submit", "direction": "issued"}),
                ),
                row(
                    1,
                    "pair.ticket_minted",
                    None,
                    serde_json::json!({"roles": ["submit"]}),
                ),
            ],
            truncated: false,
        }
    }

    fn history() -> tasks_daemon::api::HistoryView {
        use tasks_daemon::api::{
            HistoryView, MachineHistory, RecipeHistory, SampleView, SeriesView,
        };
        let s = |secs: f64, outcome: &str, cache: &str, at: u64| SampleView {
            secs,
            cache: cache.into(),
            outcome: outcome.into(),
            at_ms: at,
        };
        let series = |t: Option<&str>, v: &[f64]| SeriesView {
            target: t.map(str::to_string),
            samples: v
                .iter()
                .enumerate()
                .map(|(i, x)| s(*x, "passed", if i == 0 { "cold" } else { "warm" }, i as u64))
                .collect(),
        };
        HistoryView {
            recipes: vec![
                RecipeHistory {
                    recipe: "cargo test".into(),
                    machines: vec![
                        MachineHistory {
                            machine: "laptop".into(),
                            targets: vec!["test:lifecycle".into()],
                            series: vec![
                                series(None, &[212.0, 118.0, 121.0, 117.0, 130.0]),
                                series(Some("test:lifecycle"), &[41.0, 22.0, 23.0, 21.0]),
                            ],
                        },
                        MachineHistory {
                            machine: "nixos".into(),
                            targets: vec![],
                            series: vec![series(None, &[64.0, 51.0, 52.0])],
                        },
                    ],
                },
                RecipeHistory {
                    recipe: "cargo clippy".into(),
                    machines: vec![MachineHistory {
                        machine: "laptop".into(),
                        targets: vec![],
                        series: vec![series(None, &[48.0, 31.0, 30.0])],
                    }],
                },
            ],
            truncated: false,
        }
    }
}
