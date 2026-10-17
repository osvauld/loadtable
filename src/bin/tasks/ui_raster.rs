//! Test-only software rasteriser: draws egui's tessellated meshes into an RGB
//! image, so layout screenshots can be taken headless (no window, no GPU, no
//! display server, no new dependency). Blending is done in gamma space, like
//! egui's own glow painter; antialiasing comes from egui's feathered meshes.
use eframe::egui::{
    self, Color32, TextureId,
    epaint::{ClippedPrimitive, ImageData, Primitive},
};
use std::collections::HashMap;

#[derive(Default)]
pub struct Raster {
    textures: HashMap<TextureId, ([usize; 2], Vec<Color32>)>,
}

impl Raster {
    /// Applies one frame's texture updates (the font atlas and its patches).
    pub fn apply(&mut self, delta: &egui::TexturesDelta) {
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

    /// RGB bytes of the primitives drawn over `background`.
    pub fn draw(
        &self,
        primitives: &[ClippedPrimitive],
        pixels_per_point: f32,
        [width, height]: [usize; 2],
        background: Color32,
    ) -> Vec<u8> {
        let bg = [
            background.r() as f32 / 255.0,
            background.g() as f32 / 255.0,
            background.b() as f32 / 255.0,
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
            let (cx0, cy0) = (
                (clip.min.x * pixels_per_point).max(0.0),
                (clip.min.y * pixels_per_point).max(0.0),
            );
            let (cx1, cy1) = (
                (clip.max.x * pixels_per_point).min(width as f32),
                (clip.max.y * pixels_per_point).min(height as f32),
            );
            for tri in mesh.indices.as_chunks::<3>().0 {
                let v = [
                    &mesh.vertices[tri[0] as usize],
                    &mesh.vertices[tri[1] as usize],
                    &mesh.vertices[tri[2] as usize],
                ];
                let p = v.map(|v| (v.pos.x * pixels_per_point, v.pos.y * pixels_per_point));
                let area =
                    (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
                if area.abs() < 1e-6 {
                    continue;
                }
                let x0 = p
                    .iter()
                    .map(|q| q.0)
                    .fold(f32::MAX, f32::min)
                    .max(cx0)
                    .floor() as i64;
                let x1 = p
                    .iter()
                    .map(|q| q.0)
                    .fold(f32::MIN, f32::max)
                    .min(cx1)
                    .ceil() as i64;
                let y0 = p
                    .iter()
                    .map(|q| q.1)
                    .fold(f32::MAX, f32::min)
                    .max(cy0)
                    .floor() as i64;
                let y1 = p
                    .iter()
                    .map(|q| q.1)
                    .fold(f32::MIN, f32::max)
                    .min(cy1)
                    .ceil() as i64;
                for y in y0.max(0)..y1.min(height as i64) {
                    for x in x0.max(0)..x1.min(width as i64) {
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
                            let vertex = w0 * v[0].color.to_array()[c] as f32
                                + w1 * v[1].color.to_array()[c] as f32
                                + w2 * v[2].color.to_array()[c] as f32;
                            *out = vertex / 255.0 * texel[c] as f32 / 255.0;
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

/// Binary PPM (P6): trivially written, converted to PNG outside the test.
pub fn ppm(rgb: &[u8], [width, height]: [usize; 2]) -> Vec<u8> {
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    out.extend_from_slice(rgb);
    out
}
