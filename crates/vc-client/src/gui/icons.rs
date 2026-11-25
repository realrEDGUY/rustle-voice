//! Icons as font glyphs (egui's `monochrome_emoji_fonts`: Noto Emoji and
//! emoji-icon-font), plus `icon_button` that keeps an accessible name on them.
//! The "off" variants paint a strike-through over the base glyph.

use egui::{Color32, CornerRadius, FontId, Response, Role, Sense, Stroke, Ui, vec2};

use super::name;
use super::theme;

pub const MIC: &str = "🎤";
pub const HEADPHONES: &str = "🎧";
pub const GEAR: &str = "⚙";
pub const PLUS: &str = "+";
pub const CLOSE: &str = "✖";
pub const SCREEN_SHARE: &str = "🖥";
pub const LEAVE: &str = "📞";
pub const SPEAKER: &str = "🔊";
pub const SPEAKER_OFF: &str = "🔇";
pub const POP_OUT: &str = "⬈";
pub const SEND: &str = "➡";
pub const IMAGE: &str = "🖼";
pub const CHEVRON: &str = "⏷";
pub const POWER: &str = "🔌";
pub const COPY: &str = "🗐";
pub const STOP: &str = "⏹";

/// Every glyph above (the font coverage test walks this).
#[cfg(test)]
pub const ALL: [&str; 16] = [
    MIC,
    HEADPHONES,
    GEAR,
    PLUS,
    CLOSE,
    SCREEN_SHARE,
    LEAVE,
    SPEAKER,
    SPEAKER_OFF,
    POP_OUT,
    SEND,
    IMAGE,
    CHEVRON,
    POWER,
    COPY,
    STOP,
];

/// How a glyph is drawn: the base glyph, optionally struck through.
#[derive(Clone, Copy)]
pub struct Glyph {
    pub text: &'static str,
    pub struck: bool,
}

impl Glyph {
    pub const fn plain(text: &'static str) -> Self {
        Self {
            text,
            struck: false,
        }
    }

    pub const fn struck(text: &'static str) -> Self {
        Self { text, struck: true }
    }
}

pub const MIC_OFF: Glyph = Glyph::struck(MIC);
pub const HEADPHONES_OFF: Glyph = Glyph::struck(HEADPHONES);

impl From<&'static str> for Glyph {
    fn from(text: &'static str) -> Self {
        Self::plain(text)
    }
}

fn paint_glyph(ui: &Ui, rect: egui::Rect, glyph: Glyph, size: f32, color: Color32) {
    let p = ui.painter();
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph.text,
        FontId::proportional(size),
        color,
    );
    if glyph.struck {
        let r = size * 0.45;
        let c = rect.center();
        p.line_segment(
            [c + vec2(-r, -r), c + vec2(r, r)],
            Stroke::new(2.0, theme::RED),
        );
    }
}

/// A square icon button: hover background, tooltip, and `accessible_name` as
/// its name (tests and screen readers find it by that).
pub fn icon_button(
    ui: &mut Ui,
    glyph: impl Into<Glyph>,
    size: f32,
    fg: Color32,
    bg: Option<Color32>,
    accessible_name: &str,
    tooltip: &str,
) -> Response {
    icon_button_enabled(ui, true, glyph, size, fg, bg, accessible_name, tooltip)
}

#[allow(clippy::too_many_arguments)]
pub fn icon_button_enabled(
    ui: &mut Ui,
    enabled: bool,
    glyph: impl Into<Glyph>,
    size: f32,
    fg: Color32,
    bg: Option<Color32>,
    accessible_name: &str,
    tooltip: &str,
) -> Response {
    let glyph = glyph.into();
    let side = size + 14.0;
    // A disabled child ui reports the button as disabled and never clicked.
    let (rect, resp) = ui
        .scope(|ui| {
            if !enabled {
                ui.disable();
            }
            ui.allocate_exact_size(vec2(side, side), Sense::click())
        })
        .inner;
    if ui.is_rect_visible(rect) {
        let fill = if enabled && resp.hovered() {
            Some(bg.map_or(theme::HOVER, |b| b.gamma_multiply(1.2)))
        } else {
            bg
        };
        if let Some(f) = fill {
            ui.painter().rect_filled(rect, CornerRadius::same(8), f);
        }
        let fg = if enabled { fg } else { fg.gamma_multiply(0.4) };
        paint_glyph(ui, rect, glyph, size, fg);
    }
    let resp = if tooltip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tooltip)
    };
    let resp = if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    };
    name(resp, Role::Button, accessible_name)
}

/// A non-interactive icon; named (`Role::Image`) when it carries meaning, e.g.
/// "Muted" next to a user.
pub fn icon(
    ui: &mut Ui,
    glyph: impl Into<Glyph>,
    size: f32,
    color: Color32,
    label: Option<&str>,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size + 4.0, size + 4.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_glyph(ui, rect, glyph.into(), size, color);
    }
    match label {
        Some(l) => name(resp, Role::Image, l).on_hover_text(l),
        None => resp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_glyph_is_in_the_fonts() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        let font = FontId::proportional(14.0);
        let missing: Vec<String> = ALL
            .iter()
            .filter(|g| !ctx.fonts_mut(|f| f.has_glyphs(&font, g)))
            .map(|g| format!("{g:?} ({:X})", g.chars().next().unwrap() as u32))
            .collect();
        assert!(missing.is_empty(), "missing glyphs: {missing:?}");
    }
}
