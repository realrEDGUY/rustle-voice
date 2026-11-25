//! Watched screen shares, from every connected server, as a grid of 1, 2×1, 2×2,
//! 3×2, 3×3, … tiles: inline in the main window's stage above the chat, or (after
//! "Pop out stream") in the Streams popout (a native OS window under eframe; an
//! embedded window in tests). Closing the popout brings the streams back to the
//! stage; watching continues. Once nothing is watched the model drops back to the
//! stage by itself (`AppModel::prune_ui`).

use egui::{Color32, CornerRadius, RichText, ViewportBuilder, ViewportClass, ViewportId};
use vc_proto::UserInfo;

use super::{Textures, icons, menus, screen, theme};
use crate::ServerKey;
use crate::app::{AppModel, UiCommand};
use crate::session::{ConnStatus, ScreenView};

pub const STREAMS_TITLE: &str = "Streams";
/// Height of a tile's control bar under its image.
const TILE_BAR: f32 = 38.0;

pub fn streams_viewport_id() -> ViewportId {
    ViewportId::from_hash_of("vc-streams")
}

/// `(cols, rows)` for `n` tiles: `cols = ceil(sqrt(n))`, `rows = ceil(n / cols)`.
pub fn grid_dims(n: usize) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    let mut cols = 1;
    while cols * cols < n {
        cols += 1;
    }
    (cols, n.div_ceil(cols))
}

/// Tile heading: `Screen: {nick}`, plus the server when several contribute.
pub fn tile_title(nick: &str, server: Option<&str>) -> String {
    match server {
        Some(s) => format!("Screen: {nick} ({s})"),
        None => format!("Screen: {nick}"),
    }
}

struct Tile<'a> {
    key: ServerKey,
    server: String,
    user: &'a UserInfo,
    view: Option<&'a ScreenView>,
    /// I muted this stream's sound.
    muted: bool,
    /// My sound volume for this stream (0.0-2.0, 1.0 = unchanged).
    stream_volume: f32,
}

/// Viewer-side toggle for one stream's sound (local only).
pub fn mute_stream_label(nick: &str, muted: bool) -> String {
    if muted {
        format!("Unmute {nick}'s stream")
    } else {
        format!("Mute {nick}'s stream")
    }
}

fn collect(model: &AppModel) -> Vec<Tile<'_>> {
    model
        .sessions
        .iter()
        .filter(|(_, s)| s.status == ConnStatus::Connected)
        .flat_map(|(key, s)| {
            let server = model
                .saved_server(*key)
                .map(|x| x.label.clone())
                .unwrap_or_else(|| s.server_name.clone());
            s.watched_here().into_iter().map(move |user| Tile {
                key: *key,
                server: server.clone(),
                user,
                view: s.screens.get(&user.id),
                muted: model.stream_is_muted(*key, user.id),
                stream_volume: model.settings.stream_volume(*key, &user.nick),
            })
        })
        .collect()
}

/// Is any stream watched (on any server)?
pub fn any_watched(model: &AppModel) -> bool {
    !collect(model).is_empty()
}

/// The inline stage above the chat: every watched stream, in a grid.
pub fn stage(ui: &mut egui::Ui, model: &AppModel, tex: &mut Textures, cmds: &mut Vec<UiCommand>) {
    let tiles = collect(model);
    if tiles.is_empty() {
        return;
    }
    let multi_server = tiles.iter().any(|t| t.key != tiles[0].key);
    let h = (ui.available_height() * 0.45).clamp(120.0, 380.0);
    egui::Panel::top("stage")
        .exact_size(h)
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(theme::STAGE)
                .inner_margin(egui::Margin::same(8)),
        )
        .show(ui, |ui| grid(ui, &tiles, multi_server, false, tex, cmds));
}

/// The Streams popout, while popped out.
pub fn draw(ctx: &egui::Context, model: &AppModel, tex: &mut Textures, cmds: &mut Vec<UiCommand>) {
    if !model.ui.streams_popped {
        return;
    }
    let tiles = collect(model);
    if tiles.is_empty() {
        return;
    }
    let multi_server = tiles.iter().any(|t| t.key != tiles[0].key);
    let builder = ViewportBuilder::default()
        .with_title(STREAMS_TITLE)
        .with_inner_size([960.0, 600.0])
        .with_resizable(true);
    ctx.show_viewport_immediate(streams_viewport_id(), builder, |ui, class| {
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            // Back to the main window's stage; watching continues.
            cmds.push(UiCommand::SetStreamsPopped(false));
        }
        match class {
            ViewportClass::Immediate => {
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(theme::STAGE).inner_margin(8))
                    .show(ui, |ui| {
                        grid(ui, &tiles, multi_server, true, tex, cmds);
                    });
            }
            _ => grid(ui, &tiles, multi_server, true, tex, cmds),
        }
    });
}

fn grid(
    ui: &mut egui::Ui,
    tiles: &[Tile<'_>],
    multi_server: bool,
    popped: bool,
    tex: &mut Textures,
    cmds: &mut Vec<UiCommand>,
) {
    let (cols, rows) = grid_dims(tiles.len());
    let gap = ui.spacing().item_spacing;
    let avail = ui.available_size();
    let cw = ((avail.x - gap.x * (cols as f32 - 1.0)) / cols as f32).max(120.0);
    let ch = ((avail.y - gap.y * (rows as f32 - 1.0)) / rows as f32).max(90.0);
    for row in tiles.chunks(cols) {
        ui.horizontal(|ui| {
            for t in row {
                ui.push_id((t.key, t.user.id.0), |ui| {
                    ui.allocate_ui(egui::vec2(cw, ch), |ui| {
                        ui.set_min_size(egui::vec2(cw, ch));
                        ui.vertical(|ui| tile(ui, t, multi_server, popped, cw, ch, tex, cmds));
                    });
                });
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn tile(
    ui: &mut egui::Ui,
    t: &Tile<'_>,
    multi_server: bool,
    popped: bool,
    cw: f32,
    ch: f32,
    tex: &mut Textures,
    cmds: &mut Vec<UiCommand>,
) {
    let nick = &t.user.nick;
    let img_h = (ch - TILE_BAR).max(1.0);
    let size_text = match t.view {
        Some(view) => {
            let tx = tex.screen(ui.ctx(), t.key, t.user.id, view);
            let [w, h] = view.image.size.map(|v| (v as f32).max(1.0));
            let scale = (cw / w).min(img_h / h);
            ui.vertical_centered(|ui| {
                ui.add(
                    egui::Image::new((tx.id(), egui::vec2(w * scale, h * scale)))
                        .corner_radius(CornerRadius::same(8)),
                );
            });
            format!("{}×{}", view.image.size[0], view.image.size[1])
        }
        None => {
            ui.allocate_ui(egui::vec2(cw, img_h), |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new(screen::WAITING).color(theme::MUTED));
                });
            });
            String::new()
        }
    };
    ui.horizontal(|ui| {
        theme::badge(ui, "LIVE", Color32::WHITE, theme::RED, Some("live"));
        ui.label(
            RichText::new(tile_title(nick, multi_server.then_some(t.server.as_str())))
                .strong()
                .color(theme::TEXT),
        );
        if !size_text.is_empty() {
            ui.label(RichText::new(&size_text).small().color(theme::DIM));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = screen::stop_watching_label(nick);
            let b = icons::icon_button(
                ui,
                icons::STOP,
                13.0,
                theme::TEXT,
                Some(theme::HOVER),
                &label,
                "Stop watching",
            );
            if b.clicked() {
                cmds.push(UiCommand::WatchScreen {
                    key: t.key,
                    user: t.user.id,
                    on: false,
                });
            }
            let (label, tip, to) = if popped {
                ("Back to main window", "Back to the main window", false)
            } else {
                ("Pop out stream", "Pop out to its own window", true)
            };
            let b = icons::icon_button(
                ui,
                icons::POP_OUT,
                13.0,
                theme::TEXT,
                Some(theme::HOVER),
                label,
                tip,
            );
            if b.clicked() {
                cmds.push(UiCommand::SetStreamsPopped(to));
            }
            let label = mute_stream_label(nick, t.muted);
            let glyph = if t.muted {
                icons::SPEAKER_OFF
            } else {
                icons::SPEAKER
            };
            let b = icons::icon_button(
                ui,
                glyph,
                13.0,
                if t.muted { theme::RED } else { theme::TEXT },
                Some(theme::HOVER),
                &label,
                &format!("{label} (right-click: volume)"),
            );
            if b.clicked() {
                cmds.push(UiCommand::MuteStream {
                    key: t.key,
                    user: t.user.id,
                    muted: !t.muted,
                });
            }
            // Right-click: this stream's sound volume (the bar keeps its layout).
            menus::attach(&b, false, |ui| {
                menus::stream_volume_slider(ui, t.key, nick, t.stream_volume, cmds);
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_dims_table() {
        let want = [
            (0, (0, 0)),
            (1, (1, 1)),
            (2, (2, 1)),
            (3, (2, 2)),
            (4, (2, 2)),
            (5, (3, 2)),
            (6, (3, 2)),
            (7, (3, 3)),
            (8, (3, 3)),
            (9, (3, 3)),
            (10, (4, 3)),
        ];
        for (n, d) in want {
            assert_eq!(grid_dims(n), d, "{n}");
        }
    }

    #[test]
    fn tile_title_adds_server_only_when_asked() {
        assert_eq!(tile_title("alice", None), "Screen: alice");
        assert_eq!(tile_title("alice", Some("Home")), "Screen: alice (Home)");
    }
}
