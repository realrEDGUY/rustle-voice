//! The 72 px server rail: one rounded initials button per saved server (status
//! dot, talk marker), then "+" to add one. Right-click a server for its menu.

use egui::{Color32, CornerRadius, FontId, Role, Sense, Stroke, vec2};

use super::{icons, menus, name, theme};
use crate::app::{AppModel, UiCommand};

const BTN: f32 = 48.0;

pub fn draw(ui: &mut egui::Ui, model: &AppModel, cmds: &mut Vec<UiCommand>) {
    egui::ScrollArea::vertical()
        .id_salt("servers_scroll")
        .auto_shrink([false, true])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(4.0);
                for s in &model.saved {
                    server_button(ui, model, s.key, &s.label, cmds);
                    ui.add_space(6.0);
                }
                ui.add_space(2.0);
                let w = ui.available_width().min(40.0);
                let (r, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
                ui.painter()
                    .hline(r.x_range(), r.center().y, Stroke::new(1.0, theme::BORDER));
                ui.add_space(6.0);
                let add = icons::icon_button(
                    ui,
                    icons::PLUS,
                    22.0,
                    theme::ACCENT,
                    Some(theme::CHANNELS),
                    "Add server",
                    "Add a server",
                );
                if ui.is_rect_visible(add.rect) {
                    ui.painter().rect_stroke(
                        add.rect.shrink(1.0),
                        CornerRadius::same(12),
                        Stroke::new(1.0, theme::BORDER),
                        egui::StrokeKind::Inside,
                    );
                }
                if add.clicked() {
                    cmds.push(UiCommand::OpenAddServer);
                }
            });
        });
}

fn server_button(
    ui: &mut egui::Ui,
    model: &AppModel,
    key: crate::ServerKey,
    label: &str,
    cmds: &mut Vec<UiCommand>,
) {
    let status = model.status(key);
    let selected = model.selected == Some(key);
    let (rect, resp) = ui.allocate_exact_size(vec2(BTN, BTN), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let fill = if selected {
            theme::ACCENT
        } else if resp.hovered() {
            theme::HOVER
        } else {
            Color32::from_rgb(0x1A, 0x1E, 0x24)
        };
        let radius = if selected || resp.hovered() { 14 } else { 24 };
        p.rect_filled(rect, CornerRadius::same(radius), fill);
        let fg = if selected { theme::RAIL } else { theme::TEXT };
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            theme::initials(label),
            FontId::proportional(16.0),
            fg,
        );
        if selected {
            // The white pill at the rail's left edge.
            let x = ui.max_rect().left() + 1.0;
            let pill = egui::Rect::from_min_max(
                egui::pos2(x, rect.center().y - 14.0),
                egui::pos2(x + 4.0, rect.center().y + 14.0),
            );
            p.rect_filled(pill, CornerRadius::same(2), Color32::WHITE);
        }
    }
    let tip = format!("{label} · {}", status.label());
    let resp = name(resp.on_hover_text(tip), Role::Button, label);
    if resp.clicked() {
        cmds.push(UiCommand::Select(key));
    }
    menus::attach(&resp, false, |ui| {
        let m = menus::server_menu(model, key);
        menus::show(ui, model, &m, cmds);
    });
    // Status dot at the bottom right, named "{label}: {status}".
    let dot = egui::Rect::from_center_size(rect.right_bottom() - vec2(5.0, 5.0), vec2(12.0, 12.0));
    let dot_resp = ui.interact(dot, ui.scope_id().with(("status_dot", key)), Sense::hover());
    if ui.is_rect_visible(dot) {
        ui.painter().circle_filled(dot.center(), 6.0, theme::RAIL);
        ui.painter()
            .circle_filled(dot.center(), 4.0, theme::status_color(&status));
    }
    name(
        dot_resp,
        Role::Label,
        format!("{label}: {}", status.label()),
    );
    if model.talk_target == Some(key) {
        // Mic badge at the top right: this is where my voice goes.
        let b = egui::Rect::from_center_size(rect.right_top() + vec2(-4.0, 5.0), vec2(16.0, 16.0));
        let b_resp = ui.interact(b, ui.scope_id().with(("talk_badge", key)), Sense::hover());
        if ui.is_rect_visible(b) {
            ui.painter().circle_filled(b.center(), 8.0, theme::RAIL);
            ui.painter().text(
                b.center(),
                egui::Align2::CENTER_CENTER,
                icons::MIC,
                FontId::proportional(10.0),
                theme::ACCENT,
            );
        }
        name(
            b_resp.on_hover_text("Talking here"),
            Role::Label,
            format!("{label} [talking]"),
        );
    }
}
