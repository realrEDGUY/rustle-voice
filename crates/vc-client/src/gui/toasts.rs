use egui::Role;

use super::name;
use crate::app::{AppModel, UiCommand};

pub fn draw(ctx: &egui::Context, model: &AppModel, cmds: &mut Vec<UiCommand>) {
    if model.ui.toasts.is_empty() {
        return;
    }
    egui::Area::new(egui::Id::unique("toasts"))
        // Top right: at the bottom they covered the chat Send button.
        .anchor(egui::Align2::RIGHT_TOP, [-12.0, 40.0])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            for (i, t) in model.ui.toasts.iter().enumerate() {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(t);
                        let b = ui.button("Dismiss");
                        if name(b, Role::Button, format!("Dismiss {}", i + 1)).clicked() {
                            cmds.push(UiCommand::DismissToast(i));
                        }
                    });
                });
            }
        });
}
