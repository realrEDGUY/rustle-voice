//! The centre column: the "not connected" card, or the channel header, the
//! stream stage, grouped chat messages and the composer. Also the image viewer
//! windows.

use std::sync::Arc;

use egui::{ColorImage, CornerRadius, Panel, RichText, Role, Sense};

use super::{Textures, icons, menus, name, streams, theme};
use crate::app::{AppModel, UiCommand};
use crate::session::{ChatLine, ConnStatus, format_ts_utc};

/// Largest thumbnail height and width in the chat.
const THUMB_MAX_H: f32 = 240.0;
const THUMB_MAX_W: f32 = 300.0;
/// Messages from the same nick this close together share one header.
const GROUP_GAP_MS: u64 = 5 * 60 * 1000;
pub const UPLOAD_HINT: &str = "Paste (Ctrl+V) or drop an image to send";
pub const DROP_HINT: &str = "Drop image to send";
pub const COMPOSER_HINT: &str =
    "Enter to send · paste or drop an image · right-click a message for options";

/// Stable id of the chat message field, so uploads can check what has focus.
pub fn input_id() -> egui::Id {
    egui::Id::unique("chat_input")
}

pub fn chat_line(l: &ChatLine) -> String {
    match l.image {
        Some(i) => format!(
            "[{}] {}: [image {}x{}]",
            format_ts_utc(l.ts_ms),
            l.from_nick,
            i.w,
            i.h
        ),
        None => format!("[{}] {}: {}", format_ts_utc(l.ts_ms), l.from_nick, l.text),
    }
}

/// Accessible name of a clickable chat thumbnail.
pub fn thumb_label(nick: &str) -> String {
    format!("View image from {nick}")
}

/// Does `l` start a new group after `prev` (other nick, or a long pause)?
pub fn starts_group(prev: Option<&ChatLine>, l: &ChatLine) -> bool {
    match prev {
        None => true,
        Some(p) => p.from_nick != l.from_nick || l.ts_ms.saturating_sub(p.ts_ms) > GROUP_GAP_MS,
    }
}

struct Row {
    line: ChatLine,
    head: bool,
    /// The decoded image on image lines, if still kept.
    image: Option<Arc<ColorImage>>,
}

pub fn draw(
    ui: &mut egui::Ui,
    model: &mut AppModel,
    tex: &mut Textures,
    cmds: &mut Vec<UiCommand>,
) {
    let Some(key) = model.selected else {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("No server selected. Add one on the left.").color(theme::MUTED));
        });
        return;
    };
    let status = model.status(key);
    if status != ConnStatus::Connected {
        offline(ui, model, key, &status, cmds);
        return;
    }
    let (title, in_voice, nicks, channel) = {
        let s = model.session(key).expect("selected has a session");
        let ch = s.my_channel();
        let title = match ch.and_then(|c| s.channels.get(&c)) {
            Some(c) => format!("Chat: {}", c.name),
            None => menus::server_title(model, key),
        };
        let users = ch.map(|c| s.users_in(c)).unwrap_or_default();
        let nicks: Vec<String> = users.iter().map(|u| u.nick.clone()).collect();
        let channel = ch.and_then(|c| s.channels.get(&c)).map(|c| c.name.clone());
        (title, users.len(), nicks, channel)
    };
    Panel::top("chat_header")
        .exact_size(56.0)
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(theme::BG)
                .inner_margin(egui::Margin::symmetric(16, 8))
                .stroke(egui::Stroke::new(1.0, theme::BORDER)),
        )
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.label(RichText::new(&title).heading().strong().color(theme::TEXT));
                if channel.is_some() {
                    ui.label(
                        RichText::new(format!("{in_voice} in voice"))
                            .small()
                            .color(theme::DIM),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for n in nicks.iter().take(5).rev() {
                        theme::avatar(ui, n, 24.0, false).on_hover_text(n);
                    }
                });
            });
        });
    if !model.ui.streams_popped && streams::any_watched(model) {
        streams::stage(ui, model, tex, cmds);
    }
    // The composer keeps the bottom; messages fill the space above it.
    let full = ui.available_rect_before_wrap();
    let split = egui::Id::unique("composer_split");
    let (top, bottom) = theme::split_bottom(ui, split, 0.0);
    {
        let mut b = theme::child(ui, bottom);
        composer(&mut b, model, channel.as_deref(), cmds);
        let h = b.min_rect().height();
        theme::store_bottom(ui, split, h);
    }
    let mut t = theme::child(ui, top);
    t.set_clip_rect(top.intersect(ui.clip_rect()));
    messages(&mut t, model, tex, channel.as_deref(), cmds);
    ui.advance_cursor_after_rect(full);
}

/// The selected server is not connected: say so and offer to connect.
fn offline(
    ui: &mut egui::Ui,
    model: &AppModel,
    key: crate::ServerKey,
    status: &ConnStatus,
    cmds: &mut Vec<UiCommand>,
) {
    let (label, addr) = model
        .saved_server(key)
        .map(|s| (s.label.clone(), s.addr.clone()))
        .unwrap_or_default();
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.3).max(20.0));
        icons::icon(ui, icons::POWER, 34.0, theme::DIM, None);
        ui.add_space(8.0);
        ui.label(
            RichText::new(format!("{label} is not connected"))
                .heading()
                .color(theme::TEXT),
        );
        let mut sub = addr;
        if matches!(
            status,
            ConnStatus::Failed(_) | ConnStatus::Reconnecting { .. }
        ) {
            sub = format!("{sub} · {}", status.label());
        }
        ui.label(RichText::new(sub).color(theme::MUTED));
        ui.add_space(10.0);
        let active = status.is_active();
        let text = if active { "Connecting…" } else { "Connect" };
        let b = ui.add_enabled(
            !active,
            egui::Button::new(RichText::new(text).color(theme::RAIL).strong())
                .fill(theme::ACCENT)
                .min_size(egui::vec2(140.0, 34.0)),
        );
        if name(b, Role::Button, format!("Connect {label}")).clicked() {
            cmds.push(UiCommand::Connect(key));
        }
    });
}

fn composer(
    ui: &mut egui::Ui,
    model: &mut AppModel,
    channel: Option<&str>,
    cmds: &mut Vec<UiCommand>,
) {
    let hovering = model.ui.files_hovering;
    egui::Frame::new()
        .fill(theme::BG)
        .inner_margin(egui::Margin {
            left: 16,
            right: 16,
            top: 6,
            bottom: 8,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::Frame::new()
                .fill(theme::CARD)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    let row = egui::Layout::left_to_right(egui::Align::Center);
                    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 32.0), row, |ui| {
                        ui.set_min_height(32.0);
                        icons::icon(ui, icons::IMAGE, 15.0, theme::MUTED, Some(UPLOAD_HINT));
                        let hint = match channel {
                            Some(c) => format!("Message {c}"),
                            None => "Join a channel to chat".to_string(),
                        };
                        // No visible label: the hint text says what the field is, and
                        // the accessible name "Message" is set below.
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut model.ui.chat_input)
                                .id(input_id())
                                .char_limit(vc_proto::MAX_CHAT)
                                .hint_text(hint)
                                .font(egui::TextStyle::Body)
                                .align(egui::Align2::LEFT_CENTER)
                                .margin(egui::Margin::symmetric(4, 0))
                                .frame(egui::Frame::NONE)
                                .desired_width((ui.available_width() - 40.0).max(40.0)),
                        );
                        let r = super::name(r, egui::accesskit::Role::TextInput, "Message");
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if enter {
                            cmds.push(UiCommand::SendChat);
                            r.request_focus();
                        }
                        let send = icons::icon_button(
                            ui,
                            icons::SEND,
                            14.0,
                            theme::ACCENT,
                            None,
                            "Send",
                            "Send (Enter)",
                        );
                        if send.clicked() {
                            cmds.push(UiCommand::SendChat);
                        }
                    });
                });
            let hint = if hovering { DROP_HINT } else { COMPOSER_HINT };
            ui.label(RichText::new(hint).small().color(theme::DIM));
        });
}

fn messages(
    ui: &mut egui::Ui,
    model: &AppModel,
    tex: &mut Textures,
    channel: Option<&str>,
    cmds: &mut Vec<UiCommand>,
) {
    let rows: Vec<Row> = {
        let images = model.selected_session().map(|s| &s.images);
        let view = model.chat_view();
        let mut prev: Option<&ChatLine> = None;
        let mut out = Vec::new();
        for l in view {
            out.push(Row {
                line: l.clone(),
                head: starts_group(prev, l),
                image: l
                    .image
                    .and_then(|i| images.and_then(|m| m.get(&i.id)).cloned()),
            });
            prev = Some(l);
        }
        out
    };
    let ctx = ui.ctx().clone();
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(16, 8))
        .show(ui, |ui| {
            if rows.is_empty() {
                let text = match channel {
                    None => "Join a channel on the left to talk and chat here.".to_string(),
                    Some(c) => format!("No messages in {c} yet."),
                };
                ui.add_space(20.0);
                ui.label(RichText::new(text).color(theme::MUTED));
                return;
            }
            egui::ScrollArea::vertical()
                .id_salt("chat_scroll")
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (i, r) in rows.iter().enumerate() {
                        ui.push_id(("msg", i), |ui| message(ui, model, &ctx, r, tex, cmds));
                    }
                });
        });
}

fn message(
    ui: &mut egui::Ui,
    model: &AppModel,
    ctx: &egui::Context,
    r: &Row,
    tex: &mut Textures,
    cmds: &mut Vec<UiCommand>,
) {
    let l = &r.line;
    if r.head {
        ui.add_space(8.0);
    }
    ui.horizontal_top(|ui| {
        if r.head {
            theme::avatar(ui, &l.from_nick, 38.0, false);
        } else {
            ui.add_space(38.0 + ui.spacing().item_spacing.x);
        }
        ui.vertical(|ui| {
            if r.head {
                ui.horizontal(|ui| {
                    // Painted: the line itself carries nick and time in its name.
                    let nick = egui::RichText::new(&l.from_nick)
                        .strong()
                        .color(theme::TEXT);
                    let time = egui::RichText::new(format_ts_utc(l.ts_ms))
                        .small()
                        .color(theme::DIM);
                    paint_text(ui, nick);
                    paint_text(ui, time);
                });
            }
            match l.image {
                None => {
                    let resp = ui.add(
                        egui::Label::new(RichText::new(&l.text).color(theme::TEXT))
                            .sense(Sense::click())
                            .wrap(),
                    );
                    let resp = name(resp, Role::Label, chat_line(l));
                    message_menu(&resp, model, l, cmds);
                }
                Some(info) => {
                    match &r.image {
                        Some(img) => {
                            let t = tex.image(ctx, info.id, img);
                            let [w, h] = img.size.map(|v| (v as f32).max(1.0));
                            let scale = (THUMB_MAX_H / h)
                                .min(THUMB_MAX_W / w)
                                .min(ui.available_width().max(1.0) / w)
                                .min(1.0);
                            let resp = ui.add(
                                egui::Image::new((t.id(), egui::vec2(w * scale, h * scale)))
                                    .corner_radius(CornerRadius::same(8))
                                    .sense(Sense::click()),
                            );
                            let resp = name(resp, Role::Image, thumb_label(&l.from_nick));
                            if resp.clicked() {
                                cmds.push(UiCommand::ViewImage(info.id));
                            }
                            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                            message_menu(&resp, model, l, cmds);
                        }
                        None => {
                            ui.label(RichText::new("[image expired]").color(theme::DIM));
                        }
                    }
                    let cap = ui.add(
                        egui::Label::new(
                            RichText::new(format!("{}×{}", info.w, info.h))
                                .small()
                                .color(theme::DIM),
                        )
                        .sense(Sense::click()),
                    );
                    let cap = name(cap, Role::Label, chat_line(l));
                    message_menu(&cap, model, l, cmds);
                }
            }
        });
    });
}

/// The right-click menu of one chat line.
fn message_menu(resp: &egui::Response, model: &AppModel, l: &ChatLine, cmds: &mut Vec<UiCommand>) {
    menus::attach(resp, false, |ui| {
        let m = menus::message_menu(l);
        menus::show(ui, model, &m, cmds);
    });
}

/// Text drawn without an accessibility node of its own.
fn paint_text(ui: &mut egui::Ui, text: RichText) {
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Body,
    );
    let (rect, _) = ui.allocate_exact_size(galley.size(), Sense::hover());
    ui.painter().galley(rect.min, galley, theme::TEXT);
}

/// Viewport id of the window for chat image `id`.
pub fn image_viewport_id(id: u64) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("vc-image", id))
}

/// Largest initial size of an image window's content.
const VIEWER_MAX: egui::Vec2 = egui::vec2(1200.0, 800.0);

/// One "Image from {nick}" window per image the user opened: a native OS window
/// under eframe (embedded in tests), resizable, image fitted and never upscaled.
pub fn viewer(
    ctx: &egui::Context,
    model: &AppModel,
    tex: &mut Textures,
    cmds: &mut Vec<UiCommand>,
) {
    for &id in &model.ui.viewing {
        let found = model.sessions.values().find_map(|s| {
            let img = s.images.get(&id)?;
            let nick = s
                .chat
                .iter()
                .find(|l| l.image.is_some_and(|i| i.id == id))
                .map(|l| l.from_nick.clone())
                .unwrap_or_default();
            Some((img.clone(), nick))
        });
        let Some((img, nick)) = found else {
            // Evicted or its server was removed.
            cmds.push(UiCommand::CloseImage(id));
            continue;
        };
        let t = tex.image(ctx, id, &img);
        let size = egui::vec2(img.size[0] as f32, img.size[1] as f32).max(egui::vec2(1.0, 1.0));
        let fit = (VIEWER_MAX.x / size.x).min(VIEWER_MAX.y / size.y).min(1.0);
        let inner = (size * fit + egui::vec2(24.0, 56.0)).max(egui::vec2(240.0, 160.0));
        let builder = egui::ViewportBuilder::default()
            .with_title(format!("Image from {nick}"))
            .with_inner_size(inner)
            .with_resizable(true);
        ctx.show_viewport_immediate(image_viewport_id(id), builder, |ui, class| {
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                cmds.push(UiCommand::CloseImage(id));
            }
            let mut body = |ui: &mut egui::Ui| {
                if ui.button("Close image").clicked() {
                    cmds.push(UiCommand::CloseImage(id));
                }
                let avail = ui.available_size().max(egui::vec2(1.0, 1.0));
                let scale = (avail.x / size.x).min(avail.y / size.y).min(1.0);
                ui.add(egui::Image::new((t.id(), size * scale)));
            };
            match class {
                egui::ViewportClass::Immediate => {
                    egui::CentralPanel::default().show(ui, body);
                }
                _ => body(ui),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vc_proto::ChannelId;

    fn line(nick: &str, ts_ms: u64) -> ChatLine {
        ChatLine {
            channel: ChannelId(1),
            from_nick: nick.into(),
            text: "x".into(),
            ts_ms,
            image: None,
        }
    }

    #[test]
    fn messages_group_by_nick_within_five_minutes() {
        let a = line("a", 0);
        assert!(starts_group(None, &a));
        assert!(!starts_group(Some(&a), &line("a", GROUP_GAP_MS)));
        assert!(starts_group(Some(&a), &line("a", GROUP_GAP_MS + 1)));
        assert!(starts_group(Some(&a), &line("b", 10)));
    }
}
