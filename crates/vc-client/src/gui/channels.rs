//! The 264 px channel column: server header (click for its menu), MOTD and
//! chips, voice channels with their users, inline channel create/rename, and the
//! voice card plus me bar at the bottom (`voice.rs`).

use std::time::Instant;

use egui::{Color32, CornerRadius, RichText, Role, Sense, Stroke, vec2};
use vc_proto::{ChannelId, ChannelInfo, UserInfo};

use super::{icons, menus, name, screen, streams, theme, voice};
use crate::ServerKey;
use crate::app::{AppModel, ChannelDialogMode, UiCommand};
use crate::session::{ClientSession, ConnStatus};

/// Display text for a user row.
pub fn user_line(nick: &str, muted: bool, deafened: bool, speaking: bool, sharing: bool) -> String {
    let mut s = nick.to_string();
    if muted {
        s.push_str(" [muted]");
    }
    if deafened {
        s.push_str(" [deaf]");
    }
    if speaking {
        s.push_str(" (speaking)");
    }
    if sharing {
        s.push_str(" [sharing]");
    }
    s
}

pub const FOOTER_HINT: &str =
    "Click a channel to join. Right-click servers, channels, people and messages for more.";

fn chip(ui: &mut egui::Ui, text: &str, fg: Color32) {
    egui::Frame::new()
        .fill(theme::HOVER)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().color(fg));
        });
}

pub fn draw(ui: &mut egui::Ui, model: &mut AppModel, now: Instant, cmds: &mut Vec<UiCommand>) {
    // The voice card and me bar keep the bottom of the column.
    let full = ui.available_rect_before_wrap();
    let split = egui::Id::unique("voice_split");
    let (top, bottom) = theme::split_bottom(ui, split, 8.0);
    {
        let mut b = theme::child(ui, bottom);
        voice::draw(&mut b, model, cmds);
        let h = b.min_rect().height();
        theme::store_bottom(ui, split, h);
    }
    let mut t = theme::child(ui, top);
    t.set_clip_rect(top.intersect(ui.clip_rect()));
    column_top(&mut t, model, now, cmds);
    ui.advance_cursor_after_rect(full);
}

fn column_top(ui: &mut egui::Ui, model: &mut AppModel, now: Instant, cmds: &mut Vec<UiCommand>) {
    if model.ui.channel_dialog.is_none() {
        ui.data_mut(|m| m.remove::<(ServerKey, u64)>(edit_focus_id()));
    }
    let model_ro: &AppModel = model;
    let Some(key) = model_ro.selected else {
        return; // the centre column says how to add one
    };
    let Some(s) = model_ro.session(key) else {
        return;
    };
    header(ui, model_ro, key, s, cmds);
    if s.status != ConnStatus::Connected {
        ui.add_space(12.0);
        ui.label(RichText::new("Connect to see channels.").color(theme::MUTED));
        return;
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("VOICE CHANNELS")
                .small()
                .strong()
                .color(theme::DIM),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let b = icons::icon_button(
                ui,
                icons::PLUS,
                13.0,
                theme::MUTED,
                None,
                "New channel",
                "New channel",
            );
            if b.clicked() {
                cmds.push(UiCommand::OpenNewChannel);
            }
        });
    });
    let current = s.my_channel();
    let channels: Vec<ChannelInfo> = s.channels.values().cloned().collect();
    egui::ScrollArea::vertical()
        .id_salt("channels_scroll")
        .auto_shrink([false, false])
        .max_height((ui.available_height() - 44.0).max(60.0))
        .show(ui, |ui| {
            for ch in &channels {
                let renaming =
                    model.ui.channel_dialog.as_ref().is_some_and(|d| {
                        d.key == key && d.mode == ChannelDialogMode::Rename(ch.id)
                    });
                if renaming {
                    inline_edit(ui, model, key, cmds);
                } else {
                    channel_row(ui, model, key, ch, current, cmds);
                }
                user_rows(ui, model, key, ch.id, now, cmds);
            }
            let creating = model
                .ui
                .channel_dialog
                .as_ref()
                .is_some_and(|d| d.key == key && d.mode == ChannelDialogMode::New);
            if creating {
                inline_edit(ui, model, key, cmds);
            }
        });
    ui.add_space(4.0);
    ui.label(RichText::new(FOOTER_HINT).small().color(theme::DIM));
}

fn header(
    ui: &mut egui::Ui,
    model: &AppModel,
    key: ServerKey,
    s: &ClientSession,
    cmds: &mut Vec<UiCommand>,
) {
    let title = menus::server_title(model, key);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(8), theme::HOVER);
        }
        ui.painter().text(
            rect.left_center() + vec2(8.0, 0.0),
            egui::Align2::LEFT_CENTER,
            &title,
            egui::FontId::proportional(16.0),
            theme::TEXT,
        );
        ui.painter().text(
            rect.right_center() - vec2(8.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            icons::CHEVRON,
            egui::FontId::proportional(12.0),
            theme::MUTED,
        );
    }
    let resp = name(resp, Role::Button, "Server options");
    menus::attach(&resp, true, |ui| {
        let m = menus::server_menu(model, key);
        menus::show(ui, model, &m, cmds);
    });
    ui.painter().hline(
        ui.max_rect().x_range(),
        rect.bottom() + 3.0,
        Stroke::new(1.0, theme::BORDER),
    );
    ui.add_space(6.0);
    if s.status == ConnStatus::Connected {
        if !s.motd.is_empty() {
            ui.label(RichText::new(&s.motd).color(theme::MUTED));
        }
        ui.horizontal_wrapped(|ui| {
            chip(
                ui,
                &format!("Voice: {}", s.voice_tuning.preset),
                theme::MUTED,
            );
            chip(
                ui,
                theme::status_short(&s.status),
                theme::status_color(&s.status),
            );
        });
    } else {
        ui.horizontal_wrapped(|ui| {
            chip(
                ui,
                theme::status_short(&s.status),
                theme::status_color(&s.status),
            );
        });
    }
}

fn channel_row(
    ui: &mut egui::Ui,
    model: &AppModel,
    key: ServerKey,
    ch: &ChannelInfo,
    current: Option<ChannelId>,
    cmds: &mut Vec<UiCommand>,
) {
    let here = current == Some(ch.id);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if here {
            Some(Color32::from_rgb(0x1F, 0x24, 0x2B))
        } else if resp.hovered() {
            Some(theme::HOVER)
        } else {
            None
        };
        if let Some(f) = fill {
            ui.painter().rect_filled(rect, CornerRadius::same(6), f);
        }
        ui.painter().text(
            rect.left_center() + vec2(8.0, 0.0),
            egui::Align2::LEFT_CENTER,
            icons::SPEAKER,
            egui::FontId::proportional(12.0),
            if here { theme::ACCENT } else { theme::DIM },
        );
        ui.painter().text(
            rect.left_center() + vec2(28.0, 0.0),
            egui::Align2::LEFT_CENTER,
            &ch.name,
            egui::FontId::proportional(14.0),
            if here { theme::TEXT } else { theme::MUTED },
        );
        let n = model.session(key).map_or(0, |s| s.users_in(ch.id).len());
        if n > 0 {
            ui.painter().text(
                rect.right_center() - vec2(8.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                n.to_string(),
                egui::FontId::proportional(11.5),
                theme::DIM,
            );
        }
    }
    let label = if here {
        ch.name.clone()
    } else {
        format!("Join {}", ch.name)
    };
    let resp = name(resp, Role::Button, label);
    if resp.clicked() && !here {
        cmds.push(UiCommand::Join(ch.id));
    }
    menus::attach(&resp, false, |ui| {
        let m = menus::channel_menu(model, key, ch.id);
        menus::show(ui, model, &m, cmds);
    });
    if !ch.permanent {
        // The "temp" badge sits over the row's right side, left of the count.
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(egui::Rect::from_min_max(
                    rect.right_top() - vec2(80.0, -7.0),
                    rect.right_bottom() - vec2(24.0, 0.0),
                ))
                .layout(egui::Layout::right_to_left(egui::Align::Min)),
        );
        theme::badge(&mut child, "temp", theme::AMBER, theme::HOVER, None)
            .on_hover_text("Temporary: removed when empty");
    }
}

/// The inline channel name field (rename in place, or a new row at the end).
fn inline_edit(ui: &mut egui::Ui, model: &mut AppModel, key: ServerKey, cmds: &mut Vec<UiCommand>) {
    let Some(d) = model.ui.channel_dialog.as_mut() else {
        return;
    };
    let mode = match d.mode {
        ChannelDialogMode::New => 0u64,
        ChannelDialogMode::Rename(c) => u64::from(c.0) + 1,
    };
    let flag = edit_focus_id();
    let resp = egui::Frame::new()
        .stroke(Stroke::new(1.5, theme::ACCENT))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(6, 3))
        .show(ui, |ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut d.name)
                    .hint_text("Channel name")
                    .desired_width(ui.available_width()),
            );
            name(r, Role::TextInput, "Channel name")
        })
        .inner;
    // Focus the field once when it appears, not every frame (a click elsewhere
    // must be able to blur it).
    let this = (key, mode);
    if ui.data(|m| m.get_temp::<(ServerKey, u64)>(flag)) != Some(this) {
        resp.request_focus();
        ui.data_mut(|m| m.insert_temp(flag, this));
    }
    // Targeted at this edit: if another one replaces it in the same frame (e.g.
    // "+ New channel" clicked while renaming), the blur below is ignored.
    let (key, mode) = (d.key, d.mode);
    let submit = UiCommand::SubmitChannelDialog { key, mode };
    let cancel = UiCommand::CancelChannelDialog { key, mode };
    // Order matters: Esc also blurs the field, so check it first.
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) && (resp.has_focus() || resp.lost_focus()) {
        cmds.push(cancel);
    } else if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        cmds.push(submit);
    } else if resp.lost_focus() {
        if d.name.trim().is_empty() {
            cmds.push(cancel);
        } else {
            cmds.push(submit);
        }
    }
}

/// Which inline edit (`(server, mode)`) already got its initial focus.
fn edit_focus_id() -> egui::Id {
    egui::Id::unique("chan_edit_focused")
}

/// A sharer's LIVE badge on their user row. Clicking it watches the stream, or
/// brings the Streams popout forward when the stream is already shown there. It
/// never stops watching. Mine, another channel's, and an already watched stream
/// on the main stage keep a plain badge.
fn live_badge(
    ui: &mut egui::Ui,
    model: &AppModel,
    s: &ClientSession,
    key: ServerKey,
    u: &UserInfo,
    me: bool,
    cmds: &mut Vec<UiCommand>,
) {
    let plain =
        |ui: &mut egui::Ui| theme::badge(ui, "LIVE", Color32::WHITE, theme::RED, Some("live"));
    let click_badge = |ui: &mut egui::Ui, label: &str| {
        theme::badge_with(
            ui,
            "LIVE",
            Color32::WHITE,
            theme::RED,
            label,
            Sense::click(),
        )
    };
    let focus_popout = |ui: &egui::Ui| {
        ui.ctx()
            .send_viewport_cmd_to(streams::streams_viewport_id(), egui::ViewportCommand::Focus);
    };
    let popped = model.ui.streams_popped;
    if me {
        plain(ui);
    } else if !s.can_watch(u) {
        plain(ui).on_hover_text("Join their channel to watch");
    } else if !s.watching.contains(&u.id) {
        let b = click_badge(ui, &screen::watch_stream_label(&u.nick)).on_hover_text("Watch stream");
        if b.clicked() {
            cmds.push(UiCommand::WatchScreen {
                key,
                user: u.id,
                on: true,
            });
            if popped {
                focus_popout(ui);
            }
        }
    } else if popped {
        let b = click_badge(ui, &screen::show_stream_label(&u.nick))
            .on_hover_text("Show the streams window");
        if b.clicked() {
            focus_popout(ui);
        }
    } else {
        plain(ui);
    }
}

fn user_rows(
    ui: &mut egui::Ui,
    model: &AppModel,
    key: ServerKey,
    ch: ChannelId,
    now: Instant,
    cmds: &mut Vec<UiCommand>,
) {
    let Some(s) = model.session(key) else {
        return;
    };
    for u in s.users_in(ch) {
        let me = Some(u.id) == s.my_id;
        let speaking = s.is_speaking(u.id, now);
        let text = user_line(&u.nick, u.muted, u.deafened, speaking, u.sharing);
        let row = ui
            .horizontal(|ui| {
                ui.add_space(20.0);
                theme::avatar(ui, &u.nick, 22.0, speaking);
                let shown = if me {
                    format!("{} (you)", u.nick)
                } else {
                    u.nick.clone()
                };
                let color = if speaking {
                    theme::TEXT
                } else if u.deafened {
                    theme::DIM
                } else {
                    theme::MUTED
                };
                let r = ui.add(
                    egui::Label::new(RichText::new(shown).color(color))
                        .sense(Sense::click())
                        .truncate(),
                );
                let r = name(r, Role::Label, text);
                if u.sharing {
                    live_badge(ui, model, s, key, u, me, cmds);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if u.deafened {
                        icons::icon(
                            ui,
                            icons::HEADPHONES_OFF,
                            11.0,
                            theme::RED,
                            Some("Deafened"),
                        );
                    }
                    if u.muted {
                        icons::icon(ui, icons::MIC_OFF, 11.0, theme::RED, Some("Muted"));
                    }
                    if !me && model.user_is_muted(key, &u.nick) {
                        icons::icon(
                            ui,
                            icons::SPEAKER_OFF,
                            11.0,
                            theme::AMBER,
                            Some("Muted for you"),
                        );
                    }
                });
                r
            })
            .inner;
        if me {
            menus::attach(&row, true, |ui| {
                let m = menus::me_menu(model);
                menus::show(ui, model, &m, cmds);
            });
        } else {
            let user = u.clone();
            menus::attach(&row, true, |ui| {
                let m = menus::user_menu(model, key, &user);
                menus::show(ui, model, &m, cmds);
            });
        }
    }
}
