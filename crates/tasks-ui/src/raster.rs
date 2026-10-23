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
            for tri in mesh.indices.chunks_exact(3) {
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
    ctx.set_visuals(if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
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
    let bg = if dark {
        egui::Visuals::dark().panel_fill
    } else {
        egui::Visuals::light().panel_fill
    };
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
        let bg = rgb
            .chunks_exact(3)
            .next()
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        rgb.chunks_exact(3).filter(|p| *p != bg.as_slice()).count()
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

    #[test]
    fn r205_symbols_used_by_the_window_exist_in_the_default_fonts() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let mut texts = String::new();
        for d in [fixtures::busy(), fixtures::offline(), fixtures::idle()] {
            let vm = model::build(&d, 0);
            for m in &vm.machines {
                texts.push_str(&format!(
                    "{} {} {} {} {} {} {:?}",
                    m.name, m.status_text, m.cpu, m.ram, m.swap, m.pressure, m.notes
                ));
                for s in &m.slots {
                    for h in &s.holders {
                        texts.push_str(&h.label);
                    }
                }
            }
            for r in vm.running.iter().chain(&vm.queued).chain(&vm.recent) {
                for section in [
                    model::Section::Running,
                    model::Section::Queued,
                    model::Section::Recent,
                ] {
                    for c in crate::columns::specs(section) {
                        texts.push_str(c.title);
                        texts.push_str(&crate::columns::cell(r, c.col));
                    }
                }
            }
            texts.push_str(&vm.banner.unwrap_or_default());
            texts.push_str(&vm.alerts.join(" "));
        }
        for t in Tab::SHOWN.iter().chain(&Tab::PLANNED) {
            texts.push_str(t.label());
        }
        texts
            .push_str("Cancel... Log Close Drain Resume Follow Search Clear Refresh Show raw JSON");
        // the Audit tab: filters, column titles and the rows' own symbols
        texts.push_str("Action Peer Job Apply all name or DID id #101 did:key:z6Mk…EGta");
        for t in crate::audit::COLUMNS.iter().chain(&crate::audit::FAMILIES) {
            texts.push_str(t);
        }
        let font = egui::FontId::proportional(14.0);
        let mono = egui::FontId::monospace(14.0);
        let missing: Vec<char> = ctx.fonts_mut(|f| {
            texts
                .chars()
                .filter(|c| !c.is_control())
                .filter(|c| !f.has_glyph(&font, *c) || !f.has_glyph(&mono, *c))
                .collect()
        });
        assert!(missing.is_empty(), "no glyph for {missing:?}");
    }

    /// Writes screenshots when `TASKS_UI_SCREENSHOT_DIR` is set; otherwise only checks that
    /// every view renders.
    #[test]
    fn screenshots() {
        let dir = std::env::var_os("TASKS_UI_SCREENSHOT_DIR").map(std::path::PathBuf::from);
        let history = history();
        let shots: Vec<(&str, App, [usize; 2], bool)> = vec![
            (
                "busy-dark",
                {
                    let mut a = App::fixed(&fixtures::busy(), dead_link());
                    a.select(101);
                    a
                },
                [1400, 960],
                true,
            ),
            (
                "offline-light",
                App::fixed(&fixtures::offline(), dead_link()),
                [1400, 820],
                false,
            ),
            (
                "narrow-dark",
                App::fixed(&fixtures::busy(), dead_link()),
                [700, 900],
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
                [1100, 760],
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
            (
                "stats-dark",
                {
                    let mut a = App::fixed(&fixtures::idle(), dead_link());
                    a.show_tab(Tab::Stats);
                    a.set_history(history.clone());
                    a
                },
                [1100, 600],
                true,
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
