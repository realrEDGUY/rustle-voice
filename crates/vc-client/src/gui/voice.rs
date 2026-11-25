//! Bottom of the channel column: the voice card (where my voice goes, share and
//! leave) and the me bar (nick, mode, mute/deafen/settings).

use egui::{Color32, CornerRadius, RichText, Role, Sense, vec2};

use super::screen::{SHARING_SELF, watching_label};
use super::{icons, menus, name, theme};
use crate::app::{AppModel, UiCommand};
use crate::profile::InputMode;
use crate::session::ConnStatus;

pub fn draw(ui: &mut egui::Ui, model: &AppModel, cmds: &mut Vec<UiCommand>) {
    voice_card(ui, model, cmds);
    me_bar(ui, model, cmds);
}

/// "Voice connected" card for the talk-target server, while I am in a channel.
fn voice_card(ui: &mut egui::Ui, model: &AppModel, cmds: &mut Vec<UiCommand>) {
    let Some(k) = model.talk_target else {
        return;
    };
    let Some(s) = model.session(k) else {
        return;
    };
    if s.status != ConnStatus::Connected {
        return;
    }
    let Some(ch) = s.my_channel() else {
        return;
    };
    let sharing_here = model.screen_share == Some(k);
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(10))
        .outer_margin(egui::Margin::symmetric(0, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let title = if sharing_here {
                    "Live · voice connected"
                } else {
                    "Voice connected"
                };
                ui.label(RichText::new(title).strong().color(theme::ACCENT));
                if let Some(ms) = s.ping_ms {
                    ui.label(RichText::new(format!("{ms} ms")).small().color(theme::DIM));
                }
            });
            let ch_name = s
                .channels
                .get(&ch)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let mut sub = format!("{ch_name} / {}", menus::server_title(model, k));
            if sharing_here {
                ui.label(RichText::new(&sub).small().color(theme::MUTED));
                sub = watching_label(s.watchers);
            }
            ui.label(RichText::new(sub).small().color(theme::MUTED));
            ui.horizontal(|ui| {
                let (label, bg) = if model.screen_share.is_some() {
                    ("Stop sharing", Some(theme::ACCENT.linear_multiply(0.35)))
                } else {
                    ("Share screen", Some(theme::HOVER))
                };
                let b = icons::icon_button(
                    ui,
                    icons::SCREEN_SHARE,
                    15.0,
                    theme::TEXT,
                    bg,
                    label,
                    "Share screen (Ctrl+Shift+S)",
                );
                if b.clicked() {
                    cmds.push(UiCommand::ShareScreenOn(k));
                }
                let b = icons::icon_button(
                    ui,
                    icons::LEAVE,
                    15.0,
                    Color32::WHITE,
                    Some(theme::RED.linear_multiply(0.8)),
                    "Leave voice",
                    "Leave voice (disconnects from this server)",
                );
                if b.clicked() {
                    cmds.push(UiCommand::Disconnect(k));
                }
            });
            if let Some(e) = &model.stream_sound_error {
                ui.label(RichText::new(e).small().color(theme::AMBER));
            }
        });
}

fn me_bar(ui: &mut egui::Ui, model: &AppModel, cmds: &mut Vec<UiCommand>) {
    egui::Frame::new()
        .fill(theme::ME_BAR)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Width the right-hand cluster (LIVE + buttons) took last frame, so
            // the nick column leaves room for it instead of being overlapped.
            let right_id = ui.scope_id().with("me_bar_right_w");
            let right_w: f32 = ui.data(|d| d.get_temp(right_id)).unwrap_or(90.0);
            ui.horizontal(|ui| {
                let nick = menus::my_nick(model);
                let gap = ui.spacing().item_spacing.x;
                let text_w = (ui.available_width() - right_w - 30.0 - 3.0 * gap).max(30.0);
                // Avatar + nick + mode line: one clickable area for the me menu.
                let who = ui
                    .scope(|ui| {
                        ui.horizontal(|ui| {
                            theme::avatar(ui, &nick, 30.0, model.transmitting);
                            ui.vertical(|ui| {
                                ui.set_max_width(text_w);
                                // Painted, not a Label: the user row already carries
                                // the nick as an accessible name.
                                let (r, _) =
                                    ui.allocate_exact_size(vec2(text_w, 16.0), Sense::hover());
                                ui.painter().with_clip_rect(r).text(
                                    r.left_center(),
                                    egui::Align2::LEFT_CENTER,
                                    &nick,
                                    egui::FontId::proportional(14.0),
                                    theme::TEXT,
                                );
                                mode_line(ui, model);
                            });
                        });
                    })
                    .response;
                let who = ui.interact(who.rect, ui.scope_id().with("me_nick"), Sense::click());
                let who = name(who, Role::Button, format!("{nick} (you)"));
                menus::attach(&who, true, |ui| {
                    let m = menus::me_menu(model);
                    menus::show(ui, model, &m, cmds);
                });
                // Same height as the avatar block, so the buttons and LIVE sit
                // on its vertical centre.
                let right = egui::Layout::right_to_left(egui::Align::Center);
                let size = vec2(ui.available_width(), who.rect.height());
                let used = ui.allocate_ui_with_layout(size, right, |ui| {
                    let b = icons::icon_button(
                        ui,
                        icons::GEAR,
                        15.0,
                        theme::MUTED,
                        None,
                        "Settings",
                        "Settings",
                    );
                    if b.clicked() {
                        cmds.push(UiCommand::OpenSettings);
                    }
                    let deaf = model.voice.deafened;
                    let (glyph, label) = if deaf {
                        (icons::HEADPHONES_OFF, "Undeafen")
                    } else {
                        (icons::Glyph::plain(icons::HEADPHONES), "Deafen")
                    };
                    let b = icons::icon_button(
                        ui,
                        glyph,
                        15.0,
                        if deaf { theme::RED } else { theme::MUTED },
                        None,
                        label,
                        "Deafen (Ctrl+D)",
                    );
                    if b.clicked() {
                        cmds.push(UiCommand::ToggleDeafen);
                    }
                    let muted = model.voice.muted;
                    let (glyph, label) = if muted {
                        (icons::MIC_OFF, "Unmute")
                    } else {
                        (icons::Glyph::plain(icons::MIC), "Mute")
                    };
                    let b = icons::icon_button(
                        ui,
                        glyph,
                        15.0,
                        if muted { theme::RED } else { theme::MUTED },
                        None,
                        label,
                        "Mute (Ctrl+M). Right-click: input mode",
                    );
                    if b.clicked() {
                        cmds.push(UiCommand::ToggleMute);
                    }
                    // The mic button has its own click; the menu is on right-click only.
                    menus::attach(&b, false, |ui| {
                        let m = menus::mic_menu(model);
                        menus::show(ui, model, &m, cmds);
                    });
                    if model.screen_share.is_some() {
                        theme::badge(ui, "LIVE", Color32::WHITE, theme::RED, Some(SHARING_SELF));
                    }
                    ui.min_rect().width()
                });
                let w = used.inner.ceil();
                if (w - right_w).abs() > 0.5 {
                    ui.data_mut(|d| d.insert_temp(right_id, w));
                    ui.ctx().request_repaint();
                }
            });
        });
}

/// "Deafened" / "Muted" / the input mode; in PTT the label says held or released.
fn mode_line(ui: &mut egui::Ui, model: &AppModel) {
    let (text, accessible) = if model.voice.deafened {
        ("Deafened".to_string(), None)
    } else if model.voice.muted {
        ("Muted".to_string(), None)
    } else if model.settings.input_mode == InputMode::Ptt {
        let held = if model.ptt_down {
            "PTT: held"
        } else {
            "PTT: released"
        };
        (
            format!(
                "{} · hold {}",
                InputMode::Ptt.label(),
                model.settings.ptt_key
            ),
            Some(held),
        )
    } else {
        (model.settings.input_mode.label().to_string(), None)
    };
    let color = if model.voice.muted || model.voice.deafened {
        theme::RED
    } else {
        theme::DIM
    };
    let r = ui.add(egui::Label::new(RichText::new(&text).small().color(color)).truncate());
    if let Some(a) = accessible {
        name(r, Role::Label, a);
    }
}
