//! Colours, fonts and sizes of the window (R-580). One palette for dark and one for light,
//! named after what they are for. Status colours (`ok`, `warn`, `bad`, and `accent` for
//! running) are used for status only; the four agent hues are muted and never read as one.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle,
    Theme, epaint::Shadow,
};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Every colour the window uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// The window behind everything.
    pub win: Color32,
    /// Bars and the side panel.
    pub panel: Color32,
    /// Buttons, gauge tracks, the selected tab.
    pub raise: Color32,
    /// Main text.
    pub ink: Color32,
    /// Secondary text.
    pub dim: Color32,
    /// Labels, ids, hints.
    pub faint: Color32,
    /// 1px separators.
    pub line: Color32,
    /// Selection and running.
    pub accent: Color32,
    /// Passed.
    pub ok: Color32,
    /// Waiting, pressure.
    pub warn: Color32,
    /// Failed.
    pub bad: Color32,
    /// The four agent hues.
    pub agents: [Color32; 4],
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// The dark palette.
pub const DARK: Palette = Palette {
    win: rgb(0x17, 0x19, 0x1b),
    panel: rgb(0x1e, 0x21, 0x23),
    raise: rgb(0x26, 0x2a, 0x2c),
    ink: rgb(0xe4, 0xe7, 0xe5),
    dim: rgb(0xa0, 0xa8, 0xa3),
    faint: rgb(0x6f, 0x77, 0x72),
    line: rgb(0x2f, 0x34, 0x37),
    accent: rgb(0x7f, 0x97, 0xff),
    ok: rgb(0x5c, 0xc9, 0x8a),
    warn: rgb(0xe5, 0xa4, 0x40),
    bad: rgb(0xf0, 0x70, 0x6a),
    // violet, cyan, orchid, slate: none of them red, green or amber
    agents: [
        rgb(0xa5, 0x97, 0xff),
        rgb(0x62, 0xc2, 0xd0),
        rgb(0xc5, 0x8a, 0xc0),
        rgb(0x9a, 0xa0, 0xaa),
    ],
};

/// The light palette.
pub const LIGHT: Palette = Palette {
    win: rgb(0xf4, 0xf5, 0xf3),
    panel: rgb(0xff, 0xff, 0xff),
    raise: rgb(0xec, 0xee, 0xea),
    ink: rgb(0x1b, 0x1f, 0x1d),
    dim: rgb(0x5f, 0x68, 0x63),
    faint: rgb(0x8b, 0x93, 0x8e),
    line: rgb(0xd9, 0xdd, 0xd8),
    accent: rgb(0x36, 0x57, 0xd6),
    ok: rgb(0x23, 0x82, 0x4f),
    warn: rgb(0xb4, 0x6d, 0x00),
    bad: rgb(0xc2, 0x36, 0x2f),
    agents: [
        rgb(0x6b, 0x5b, 0xd6),
        rgb(0x2a, 0x8a, 0x9a),
        rgb(0xa0, 0x50, 0x8f),
        rgb(0x7a, 0x7f, 0x8a),
    ],
};

impl Palette {
    /// The palette for a theme.
    pub fn of(dark: bool) -> &'static Palette {
        if dark { &DARK } else { &LIGHT }
    }

    /// The palette for the context's current theme.
    pub fn current(ctx: &egui::Context) -> &'static Palette {
        Palette::of(ctx.style().visuals.dark_mode)
    }

    /// The colour of an agent hue index (see `format::agent_hue`).
    pub fn agent(&self, hue: usize) -> Color32 {
        self.agents
            .get(hue % self.agents.len())
            .copied()
            .unwrap_or(self.agents[0])
    }
}

/// `over` laid on `base` at `pct` percent: for tints such as the banner and the selection.
pub fn mix(base: Color32, over: Color32, pct: u8) -> Color32 {
    let m = |b: u8, o: u8| {
        let p = u16::from(pct.min(100));
        ((u16::from(o) * p + u16::from(b) * (100 - p)) / 100) as u8
    };
    Color32::from_rgb(
        m(base.r(), over.r()),
        m(base.g(), over.g()),
        m(base.b(), over.b()),
    )
}

/// Type scale (points): the title, the body, secondary text, labels.
pub const TITLE: f32 = 15.0;
/// Body text.
pub const BODY: f32 = 13.5;
/// Secondary text: ids, commands, state lines.
pub const SECONDARY: f32 = 12.5;
/// Small labels and hints.
pub const LABEL: f32 = 11.0;

/// The medium weight of the UI face.
pub fn medium_family() -> FontFamily {
    FontFamily::Name("sans-medium".into())
}
/// The semibold weight of the UI face.
pub fn semibold_family() -> FontFamily {
    FontFamily::Name("sans-semibold".into())
}
/// The medium weight of the mono face.
pub fn mono_medium_family() -> FontFamily {
    FontFamily::Name("mono-medium".into())
}

/// UI text at `size`.
pub fn sans(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
/// Medium UI text.
pub fn sans_medium(size: f32) -> FontId {
    FontId::new(size, medium_family())
}
/// Semibold UI text.
pub fn sans_semibold(size: f32) -> FontId {
    FontId::new(size, semibold_family())
}
/// Monospace text.
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}
/// Medium monospace text.
pub fn mono_medium(size: f32) -> FontId {
    FontId::new(size, mono_medium_family())
}

/// The embedded fonts, installed ahead of egui's own so that its symbols stay as the fallback.
pub fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let faces: [(&str, &'static [u8]); 5] = [
        (
            "plex-sans",
            include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf"),
        ),
        (
            "plex-sans-medium",
            include_bytes!("../assets/fonts/IBMPlexSans-Medium.ttf"),
        ),
        (
            "plex-sans-semibold",
            include_bytes!("../assets/fonts/IBMPlexSans-SemiBold.ttf"),
        ),
        (
            "plex-mono",
            include_bytes!("../assets/fonts/plexmono_regular.ttf"),
        ),
        (
            "plex-mono-medium",
            include_bytes!("../assets/fonts/plexmono_medium.ttf"),
        ),
    ];
    for (name, bytes) in faces {
        fonts
            .font_data
            .insert(name.to_string(), Arc::new(FontData::from_static(bytes)));
    }
    let proportional = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let monospace = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    // each family: its own face first, then whatever egui already used for symbols
    let with = |first: &str, rest: &[String]| {
        let mut v = vec![first.to_string()];
        v.extend(rest.iter().cloned());
        v
    };
    fonts
        .families
        .insert(FontFamily::Proportional, with("plex-sans", &proportional));
    fonts
        .families
        .insert(FontFamily::Monospace, with("plex-mono", &monospace));
    fonts
        .families
        .insert(medium_family(), with("plex-sans-medium", &proportional));
    fonts
        .families
        .insert(semibold_family(), with("plex-sans-semibold", &proportional));
    fonts
        .families
        .insert(mono_medium_family(), with("plex-mono-medium", &monospace));
    fonts
}

fn visuals(p: &Palette, dark: bool) -> egui::Visuals {
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.panel_fill = p.win;
    v.window_fill = p.panel;
    v.extreme_bg_color = p.win;
    v.faint_bg_color = p.raise;
    v.code_bg_color = p.raise;
    v.weak_text_color = Some(p.dim);
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.bad;
    v.window_stroke = Stroke::new(1.0_f32, p.line);
    v.window_corner_radius = CornerRadius::same(6);
    v.menu_corner_radius = CornerRadius::same(4);
    v.window_shadow = Shadow::NONE;
    v.popup_shadow = Shadow::NONE;
    v.selection.bg_fill = mix(p.win, p.accent, 22);
    v.selection.stroke = Stroke::new(1.0_f32, p.accent);
    let r = CornerRadius::same(4);
    v.widgets.noninteractive.bg_fill = p.panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.line);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.ink);
    v.widgets.noninteractive.corner_radius = r;
    for (w, fill) in [
        (&mut v.widgets.inactive, p.raise),
        (&mut v.widgets.hovered, mix(p.raise, p.ink, 8)),
        (&mut v.widgets.active, mix(p.raise, p.ink, 14)),
        (&mut v.widgets.open, p.raise),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0_f32, p.line);
        w.fg_stroke = Stroke::new(1.0_f32, p.ink);
        w.corner_radius = r;
        w.expansion = 0.0;
    }
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.faint);
    v
}

/// Installs the fonts, both palettes' visuals and the type scale on `ctx`.
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());
    ctx.set_visuals_of(Theme::Dark, visuals(&DARK, true));
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT, false));
    ctx.all_styles_mut(|s| {
        let sizes: BTreeMap<TextStyle, FontId> = [
            (TextStyle::Heading, sans_semibold(TITLE)),
            (TextStyle::Body, sans(BODY)),
            (TextStyle::Button, sans(SECONDARY)),
            (TextStyle::Small, sans(LABEL)),
            (TextStyle::Monospace, mono(SECONDARY)),
        ]
        .into_iter()
        .collect();
        s.text_styles = sizes;
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 3.0);
        s.spacing.interact_size.y = 22.0;
        s.visuals.striped = false;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r580_agent_hues_are_never_a_status_colour() {
        // no hue sits near red, green or amber on the hue wheel
        for p in [&DARK, &LIGHT] {
            for c in p.agents {
                let h = hue_degrees(c);
                assert!(
                    !(0.0..25.0).contains(&h)
                        && !(345.0..=360.0).contains(&h)
                        && !(30.0..75.0).contains(&h)
                        && !(75.0..165.0).contains(&h),
                    "{c:?} hue {h}"
                );
            }
            for (i, a) in p.agents.iter().enumerate() {
                for b in p.agents.iter().skip(i + 1) {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn r580_the_palette_follows_the_theme_and_a_hue_index_wraps() {
        assert_eq!(Palette::of(true), &DARK);
        assert_eq!(Palette::of(false), &LIGHT);
        assert_eq!(DARK.agent(1), DARK.agents[1]);
        assert_eq!(DARK.agent(5), DARK.agents[1]);
        assert_ne!(DARK.win, LIGHT.win);
    }

    #[test]
    fn r580_mix_blends_by_percent() {
        let a = Color32::from_rgb(0, 0, 0);
        let b = Color32::from_rgb(200, 100, 50);
        assert_eq!(mix(a, b, 0), a);
        assert_eq!(mix(a, b, 100), b);
        assert_eq!(mix(a, b, 50), Color32::from_rgb(100, 50, 25));
        assert_eq!(mix(a, b, 250), b);
    }

    #[test]
    fn r580_the_embedded_faces_cover_every_family_the_window_uses() {
        let f = font_definitions();
        for fam in [
            FontFamily::Proportional,
            FontFamily::Monospace,
            medium_family(),
            semibold_family(),
            mono_medium_family(),
        ] {
            let list = f.families.get(&fam).expect("family");
            assert!(
                list.first().is_some_and(|n| n.starts_with("plex-")),
                "{fam:?}"
            );
            assert!(list.len() > 1, "egui's symbols stay as the fallback");
            for n in list {
                assert!(f.font_data.contains_key(n), "{n}");
            }
        }
    }

    fn hue_degrees(c: Color32) -> f32 {
        let (r, g, b) = (
            f32::from(c.r()) / 255.0,
            f32::from(c.g()) / 255.0,
            f32::from(c.b()) / 255.0,
        );
        let max = r.max(g).max(b);
        let d = max - r.min(g).min(b);
        if d == 0.0 {
            return 0.0;
        }
        let h = if max == r {
            ((g - b) / d).rem_euclid(6.0)
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        h * 60.0
    }
}
