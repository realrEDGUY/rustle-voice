//! The redesign's palette, sizes and small drawing helpers (avatars, badges).
//! Fonts stay egui's defaults; `install` only sets dark visuals and text sizes.

use egui::{
    Color32, CornerRadius, CursorIcon, FontId, Response, Role, Sense, Stroke, TextStyle, Ui, vec2,
};

use crate::session::ConnStatus;

pub const RAIL: Color32 = Color32::from_rgb(0x0B, 0x0D, 0x10);
pub const CHANNELS: Color32 = Color32::from_rgb(0x12, 0x14, 0x18);
pub const BG: Color32 = Color32::from_rgb(0x16, 0x19, 0x1E);
pub const ME_BAR: Color32 = Color32::from_rgb(0x0F, 0x11, 0x14);
pub const CARD: Color32 = Color32::from_rgb(0x18, 0x1B, 0x20);
pub const STAGE: Color32 = Color32::from_rgb(0x0B, 0x0D, 0x10);
pub const BORDER: Color32 = Color32::from_rgb(0x2A, 0x30, 0x38);
pub const HOVER: Color32 = Color32::from_rgb(0x1C, 0x20, 0x26);
pub const ACCENT: Color32 = Color32::from_rgb(0x4A, 0xDE, 0x9A);
pub const RED: Color32 = Color32::from_rgb(0xF2, 0x6B, 0x6B);
pub const AMBER: Color32 = Color32::from_rgb(0xE5, 0xB9, 0x4E);
pub const OFF: Color32 = Color32::from_rgb(0x5A, 0x62, 0x6D);
pub const TEXT: Color32 = Color32::from_rgb(0xE8, 0xEA, 0xED);
pub const MUTED: Color32 = Color32::from_rgb(0x9B, 0xA4, 0xAF);
pub const DIM: Color32 = Color32::from_rgb(0x7A, 0x83, 0x8E);

/// Width of the server rail.
pub const RAIL_W: f32 = 72.0;
/// Width of the channel column.
pub const COL_W: f32 = 264.0;

/// Avatar colours (the design's user palette).
const AVATARS: [Color32; 9] = [
    Color32::from_rgb(0x5B, 0x8D, 0xEF),
    Color32::from_rgb(0xE0, 0x80, 0x5A),
    Color32::from_rgb(0x9B, 0x7B, 0xE8),
    Color32::from_rgb(0x3F, 0xB8, 0xAF),
    Color32::from_rgb(0xD9, 0xA2, 0x3B),
    Color32::from_rgb(0xE4, 0x6F, 0x9A),
    Color32::from_rgb(0x7D, 0x8A, 0x99),
    Color32::from_rgb(0x6F, 0xB3, 0x5B),
    Color32::from_rgb(0xC7, 0x6B, 0xD1),
];

/// Dark visuals in the design's colours, and its text sizes.
pub fn install(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = CARD;
        v.extreme_bg_color = Color32::from_rgb(0x11, 0x13, 0x17);
        v.faint_bg_color = CHANNELS;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_corner_radius = CornerRadius::same(12);
        v.menu_corner_radius = CornerRadius::same(8);
        v.selection.bg_fill = ACCENT.linear_multiply(0.35);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.hyperlink_color = ACCENT;
        v.override_text_color = None;
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(8);
        }
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x20, 0x24, 0x2B);
        v.widgets.inactive.bg_fill = Color32::from_rgb(0x20, 0x24, 0x2B);
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x25, 0x2A, 0x32);
        v.widgets.hovered.bg_fill = Color32::from_rgb(0x25, 0x2A, 0x32);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.active.weak_bg_fill = Color32::from_rgb(0x2A, 0x30, 0x39);
        v.widgets.active.bg_fill = Color32::from_rgb(0x2A, 0x30, 0x39);
        v.widgets.open.weak_bg_fill = Color32::from_rgb(0x2A, 0x30, 0x39);
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(17.0));
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(14.0));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(14.0));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(11.5));
    });
}

/// 32-bit FNV-1a.
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// A stable avatar colour for a nickname.
pub fn avatar_color(nick: &str) -> Color32 {
    AVATARS[fnv1a(nick) as usize % AVATARS.len()]
}

/// Up to two initials: the first letters of the first two words, else the first
/// two characters, uppercased.
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let s: String = if words.len() >= 2 {
        words
            .iter()
            .take(2)
            .filter_map(|w| w.chars().next())
            .collect()
    } else {
        name.trim().chars().take(2).collect()
    };
    s.to_uppercase()
}

/// A round avatar with initials, with an accent ring while speaking. Decorative:
/// it carries no accessible name of its own.
pub fn avatar(ui: &mut Ui, nick: &str, size: f32, speaking: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let c = rect.center();
        let r = size / 2.0;
        p.circle_filled(c, r, avatar_color(nick));
        if speaking {
            p.circle_stroke(c, r + 2.0, Stroke::new(2.0, ACCENT));
        }
        p.text(
            c,
            egui::Align2::CENTER_CENTER,
            initials(nick),
            FontId::proportional((size * 0.38).max(8.0)),
            Color32::WHITE,
        );
    }
    resp
}

/// A small rounded label, e.g. "LIVE" or "temp", named `name` (default: its text).
pub fn badge(ui: &mut Ui, text: &str, fg: Color32, bg: Color32, name: Option<&str>) -> Response {
    badge_with(ui, text, fg, bg, name.unwrap_or(text), Sense::hover())
}

/// `badge` with its own `sense`. A clickable badge is a button (pointer cursor,
/// lighter on hover, `Role::Button`); otherwise a label.
pub fn badge_with(
    ui: &mut Ui,
    text: &str,
    fg: Color32,
    bg: Color32,
    name: &str,
    sense: Sense,
) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), FontId::proportional(10.5), fg);
    let size = galley.size() + vec2(10.0, 4.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, sense);
    let clickable = sense.senses_click();
    if clickable {
        resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    }
    if ui.is_rect_visible(rect) {
        let fill = if clickable && resp.hovered() {
            bg.gamma_multiply(1.25)
        } else {
            bg
        };
        ui.painter().rect_filled(rect, CornerRadius::same(4), fill);
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, fg);
    }
    let role = if clickable { Role::Button } else { Role::Label };
    super::name(resp, role, name)
}

/// A one-word status for chips (the full text is on the status dot and card).
pub fn status_short(status: &ConnStatus) -> &'static str {
    match status {
        ConnStatus::Connected => "Connected",
        ConnStatus::Connecting => "Connecting…",
        ConnStatus::Reconnecting { .. } => "Reconnecting",
        ConnStatus::Failed(_) => "Failed",
        ConnStatus::Disconnected => "Offline",
    }
}

/// The status dot colour of a server.
pub fn status_color(status: &ConnStatus) -> Color32 {
    match status {
        ConnStatus::Connected => ACCENT,
        ConnStatus::Connecting | ConnStatus::Reconnecting { .. } => AMBER,
        ConnStatus::Failed(_) => RED,
        ConnStatus::Disconnected => OFF,
    }
}

/// Split `ui`'s free space into `(top, bottom)`: `bottom` is pinned to the
/// bottom edge, as tall as its content measured last frame (`store_bottom`).
/// Used instead of nested bottom panels, whose content can outgrow their clip.
pub fn split_bottom(ui: &Ui, id: egui::Id, gap: f32) -> (egui::Rect, egui::Rect) {
    let full = ui.available_rect_before_wrap();
    let h = ui
        .data(|d| d.get_temp::<f32>(id))
        .unwrap_or(0.0)
        .clamp(0.0, full.height());
    let bottom = egui::Rect::from_min_max(egui::pos2(full.left(), full.bottom() - h), full.max);
    let top = egui::Rect::from_min_max(
        full.min,
        egui::pos2(full.right(), (bottom.top() - gap).max(full.top())),
    );
    (top, bottom)
}

/// Remember the bottom part's content height for the next frame's split.
pub fn store_bottom(ui: &Ui, id: egui::Id, h: f32) {
    let old = ui.data(|d| d.get_temp::<f32>(id));
    if old.is_none_or(|o| (o - h).abs() > 0.5) {
        ui.data_mut(|d| d.insert_temp(id, h));
        // Lay out again in this same frame, so nothing is drawn (or clicked) at
        // last frame's position.
        ui.ctx().request_discard("bottom part changed height");
    }
}

/// A child ui filling `rect`, top to bottom.
pub fn child(ui: &mut Ui, rect: egui::Rect) -> Ui {
    ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    )
}

/// A frame for one of the main columns.
pub fn column(fill: Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(10, 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_from_words_or_letters() {
        assert_eq!(initials("Home Base"), "HB");
        assert_eq!(initials("night shift crew"), "NS");
        assert_eq!(initials("nova"), "NO");
        assert_eq!(initials("x"), "X");
        assert_eq!(initials(""), "");
    }

    #[test]
    fn avatar_color_is_stable() {
        assert_eq!(avatar_color("alice"), avatar_color("alice"));
        assert!(AVATARS.contains(&avatar_color("bob")));
        // Different nicks spread over the palette.
        let n: std::collections::BTreeSet<_> = ["a", "b", "c", "d", "e", "f", "g", "h"]
            .iter()
            .map(|s| format!("{:?}", avatar_color(s)))
            .collect();
        assert!(n.len() > 2);
    }
}
