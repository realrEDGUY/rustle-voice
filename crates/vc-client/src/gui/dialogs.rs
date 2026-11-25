//! Add/Edit server and TOFU (new server identity) dialogs, as modals. Channel
//! names are edited inline in the channel column (`channels.rs`).

use egui::{CornerRadius, RichText, Role, Stroke};

use super::{icons, name, theme};
use crate::app::{AppModel, UiCommand};
use crate::tofu;

/// Shown under the address while it has no `:port`.
pub const PORT_HINT: &str = "Add a port after a colon, like :7878.";

/// A cheap, offline check that the address ends in `:port` (never resolves DNS:
/// it runs every frame).
pub fn has_port(addr: &str) -> bool {
    addr.trim()
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
}

fn field(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    password: bool,
    hint: &str,
    error: bool,
) -> egui::Response {
    let l = ui.label(RichText::new(label).small().color(theme::MUTED));
    let border = if error { theme::RED } else { theme::BORDER };
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(0x11, 0x13, 0x17))
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(value)
                    .password(password)
                    .hint_text(hint)
                    .frame(egui::Frame::NONE)
                    .desired_width(ui.available_width()),
            )
            .labelled_by(l.id)
        })
        .inner
}

fn modal_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(egui::Margin::same(22))
}

fn esc_pressed(ctx: &egui::Context, resp: &egui::ModalResponse<()>) -> bool {
    resp.is_top_modal
        && !resp.any_popup_open
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
}

pub fn draw(ctx: &egui::Context, model: &mut AppModel, cmds: &mut Vec<UiCommand>) {
    server_dialog(ctx, model, cmds);
    tofu_dialog(ctx, model, cmds);
}

fn server_dialog(ctx: &egui::Context, model: &mut AppModel, cmds: &mut Vec<UiCommand>) {
    let Some(d) = model.ui.server_dialog.as_mut() else {
        return;
    };
    let editing = d.editing.is_some();
    let resp = egui::Modal::new(egui::Id::unique("server_dialog"))
        .frame(modal_frame())
        .show(ctx, |ui| {
            ui.set_width(420.0);
            let (title, sub) = if editing {
                ("Edit server", "Changes reconnect this server.")
            } else {
                (
                    "Add a server",
                    "Only the address and a nickname are needed. Everything else can wait.",
                )
            };
            ui.label(RichText::new(title).heading().strong().color(theme::TEXT));
            ui.label(RichText::new(sub).color(theme::MUTED));
            ui.add_space(12.0);
            let bad_port = !d.addr.trim().is_empty() && !has_port(&d.addr);
            field(
                ui,
                "Address",
                &mut d.addr,
                false,
                "host:port, for example voice.example.net:7878",
                bad_port,
            );
            if bad_port {
                ui.label(RichText::new(PORT_HINT).small().color(theme::RED));
            }
            ui.add_space(8.0);
            field(ui, "Nickname", &mut d.nick, false, "", false);
            ui.add_space(8.0);
            let open = editing && (!d.label.is_empty() || !d.password.is_empty());
            egui::CollapsingHeader::new("More")
                .id_salt(("server_dialog_more", editing))
                .default_open(open)
                .show(ui, |ui| {
                    field(ui, "Label", &mut d.label, false, "Server name", false);
                    ui.add_space(6.0);
                    field(ui, "Password", &mut d.password, true, "", false);
                });
            if !editing {
                ui.add_space(6.0);
                ui.checkbox(&mut d.connect_now, "Connect now");
            }
            if let Some(e) = &d.error {
                ui.add_space(4.0);
                ui.label(RichText::new(e).color(theme::RED));
            }
            ui.add_space(14.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let primary = if editing {
                    "Save"
                } else if d.connect_now {
                    "Add and connect"
                } else {
                    "Add"
                };
                let b = ui.add(
                    egui::Button::new(RichText::new(primary).strong().color(theme::RAIL))
                        .fill(theme::ACCENT)
                        .min_size(egui::vec2(120.0, 32.0)),
                );
                if name(b, Role::Button, primary).clicked() {
                    cmds.push(UiCommand::SaveServerDialog);
                }
                if ui
                    .add(egui::Button::new("Cancel").min_size(egui::vec2(90.0, 32.0)))
                    .clicked()
                {
                    cmds.push(UiCommand::CancelServerDialog);
                }
            });
        });
    // Only Esc or Cancel close it: a stray click outside must not lose the input.
    if esc_pressed(ctx, &resp) {
        cmds.push(UiCommand::CancelServerDialog);
    }
}

fn tofu_dialog(ctx: &egui::Context, model: &AppModel, cmds: &mut Vec<UiCommand>) {
    let Some((key, fp)) = model.ui.tofu_prompt else {
        return;
    };
    let label = model
        .saved_server(key)
        .map(|s| s.label.clone())
        .unwrap_or_default();
    let hex = tofu::to_hex(&fp);
    let resp = egui::Modal::new(egui::Id::unique("tofu_dialog"))
        .frame(modal_frame())
        .show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(
                RichText::new(format!("{label} has a new identity"))
                    .heading()
                    .strong()
                    .color(theme::TEXT),
            );
            ui.label(RichText::new(format!(
                "The certificate of {label} is not the one seen before. Only trust it if the server owner regenerated it."
            )).color(theme::MUTED));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("NEW FINGERPRINT").small().strong().color(theme::DIM));
                let c = icons::icon_button(
                    ui,
                    icons::COPY,
                    12.0,
                    theme::MUTED,
                    None,
                    "Copy fingerprint",
                    "Copy fingerprint",
                );
                if c.clicked() {
                    ui.ctx().copy_text(hex.clone());
                }
            });
            ui.add(egui::Label::new(RichText::new(&hex).monospace().color(theme::TEXT)).wrap());
            ui.add_space(14.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("Trust new identity").color(theme::RED))
                            .min_size(egui::vec2(140.0, 32.0)),
                    )
                    .clicked()
                {
                    cmds.push(UiCommand::TrustNewIdentity);
                }
                let b = ui.add(
                    egui::Button::new(RichText::new("Stay safe, cancel").strong().color(theme::RAIL))
                        .fill(theme::ACCENT)
                        .min_size(egui::vec2(140.0, 32.0)),
                );
                if b.clicked() {
                    cmds.push(UiCommand::CancelTofu);
                }
            });
        });
    if esc_pressed(ctx, &resp) {
        cmds.push(UiCommand::CancelTofu);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_port_is_a_pure_string_check() {
        assert!(has_port("voice.example.net:7878"));
        assert!(has_port("[::1]:7878"));
        assert!(has_port(" 127.0.0.1:1 "));
        assert!(!has_port("voice.example.net"));
        assert!(!has_port("host:"));
        assert!(!has_port("host:99999"));
        assert!(!has_port(":7878"));
    }
}
