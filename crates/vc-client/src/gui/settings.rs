//! The Settings window: a modal with Voice, Screen share, Shortcuts and Profile
//! tabs. There is no draft and no Save button: each frame the window edits a copy
//! of the live settings and sends it back with `UpdateSettings` when it changed
//! (written to disk at once, or when a slider drag ends).

use egui::{Color32, CornerRadius, RichText, Role, Sense, Stroke, vec2};

use super::{icons, name, theme};
use crate::app::{AppModel, SettingsTab, UiCommand};
use crate::audio::DeviceInfo;
use crate::profile::{
    InputMode, SCREEN_FPS_CHOICES, SCREEN_HEIGHT_CHOICES, SCREEN_QUALITY_MAX, SCREEN_QUALITY_MIN,
    ScreenEncoderPref, Settings, screen_height_label,
};

/// Button that measures the local capture + encode limit and applies it.
pub const TEST_MAX: &str = "Test max";
/// Switch for hardware H.264 (off = always CPU JPEG).
pub const USE_HW_ENCODING: &str = "GPU encoding (H.264)";
/// Switch for sharing my system audio with my stream.
pub const SHARE_SOUND: &str = "Share computer sound";
/// Suffix on a choice above the current server's cap.
pub const OVER_LIMIT: &str = " (over server limit)";
/// The push-to-talk key button's accessible name.
pub const PTT_KEY_BUTTON: &str = "Push-to-talk key";
pub const LISTENING: &str = "Press a key…";

/// A frame-rate choice's label, marked when over the server's cap (0 = none).
pub fn fps_choice_label(fps: u32, cap: u32) -> String {
    let over = cap > 0 && fps > cap;
    format!("{fps} fps{}", if over { OVER_LIMIT } else { "" })
}

/// A resolution choice's label, marked when over the server's cap (0 = none).
/// Native counts as over any height cap.
pub fn height_choice_label(h: u32, cap: u32) -> String {
    let over = cap > 0 && (h == 0 || h > cap);
    format!(
        "{}{}",
        screen_height_label(h),
        if over { OVER_LIMIT } else { "" }
    )
}

/// The Preferences encoder line.
pub fn encoder_text(hw: Option<&crate::video::HwEncoder>, failed: bool) -> String {
    match hw {
        Some(e) if failed => format!("Encoder: {} (hardware failed; using CPU JPEG)", e.name),
        Some(e) => format!("Encoder: {} (hardware)", e.name),
        None => "Encoder: CPU (JPEG)".into(),
    }
}

/// Keys allowed for push to talk, stored as `egui::Key::name()`.
pub fn ptt_keys() -> Vec<&'static str> {
    let mut v = vec!["Space"];
    v.extend([
        "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
    ]);
    v.extend([
        "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R",
        "S", "T", "U", "V", "W", "X", "Y", "Z",
    ]);
    v
}

/// The shortcut list shown on the Shortcuts tab: `(action, keys)`.
pub fn shortcut_rows(ptt_key: &str) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        ("Push-to-talk", ptt_key.to_string()),
        ("Mute microphone", "Ctrl+M".into()),
        ("Deafen", "Ctrl+D".into()),
    ];
    if crate::capture::SUPPORTED {
        rows.push(("Share screen", "Ctrl+Shift+S".into()));
    }
    rows.push(("Rename channel", "F2".into()));
    rows
}

fn device_combo(
    ui: &mut egui::Ui,
    label: &str,
    devices: &[DeviceInfo],
    value: &mut Option<String>,
) {
    let current = match value {
        None => "System default".to_string(),
        Some(id) => devices
            .iter()
            .find(|d| &d.id == id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| format!("Unavailable ({id})")),
    };
    ui.label(RichText::new(label).small().color(theme::MUTED));
    egui::ComboBox::from_id_salt(label)
        .selected_text(current)
        .width(ui.available_width().min(420.0))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "System default");
            for d in devices {
                ui.selectable_value(value, Some(d.id.clone()), &d.name);
            }
        })
        .response
        .widget_info(|| egui::WidgetInfo::labeled(Role::ComboBox, true, label));
}

/// One segment of a segmented control; `dim` marks an over-limit choice.
fn segment(ui: &mut egui::Ui, on: bool, label: &str, dim: bool) -> bool {
    let mut text = RichText::new(label);
    if dim {
        text = text.strikethrough().color(theme::OFF);
    } else if on {
        text = text.color(theme::TEXT);
    } else {
        text = text.color(theme::MUTED);
    }
    let r = ui.add(egui::Button::selectable(on, text));
    let r = if dim {
        r.on_hover_text("Over server limit")
    } else {
        r
    };
    r.widget_info(|| egui::WidgetInfo::selected(Role::RadioButton, true, on, label));
    r.clicked()
}

/// A labelled on/off switch row with a sub-line.
fn switch(ui: &mut egui::Ui, on: &mut bool, label: &str, sub: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - 60.0).max(80.0));
            ui.label(RichText::new(label).color(theme::TEXT));
            ui.label(RichText::new(sub).small().color(theme::DIM));
        });
        let (rect, resp) = ui.allocate_exact_size(vec2(44.0, 24.0), Sense::click());
        if resp.clicked() {
            *on = !*on;
            changed = true;
        }
        if ui.is_rect_visible(rect) {
            let track = if *on {
                theme::ACCENT
            } else {
                Color32::from_rgb(0x3A, 0x41, 0x4B)
            };
            ui.painter()
                .rect_filled(rect, CornerRadius::same(12), track);
            let x = if *on {
                rect.right() - 12.0
            } else {
                rect.left() + 12.0
            };
            ui.painter()
                .circle_filled(egui::pos2(x, rect.center().y), 9.0, Color32::WHITE);
        }
        let v = *on;
        resp.widget_info(|| egui::WidgetInfo::selected(Role::CheckBox, true, v, label));
    });
    changed
}

fn section(ui: &mut egui::Ui, text: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(text).small().strong().color(theme::DIM));
}

/// What the window needs from the model besides the settings themselves.
struct View {
    devices_in: Vec<DeviceInfo>,
    devices_out: Vec<DeviceInfo>,
    cap_fps: u32,
    cap_h: u32,
    limit_text: Option<String>,
    probe_text: Option<String>,
    probe_running: bool,
    level_db: f32,
    listening: bool,
    screen_audio: bool,
    sound_error: Option<String>,
}

pub fn draw(ctx: &egui::Context, model: &mut AppModel, cmds: &mut Vec<UiCommand>) {
    let Some(tab) = model.ui.settings_tab else {
        return;
    };
    let (cap_fps, cap_h) = model.screen_caps();
    let v = View {
        devices_in: model.devices_in.clone(),
        devices_out: model.devices_out.clone(),
        cap_fps,
        cap_h,
        limit_text: model.screen_limit_text(),
        probe_text: model.probe_text(),
        probe_running: model.probe_running,
        level_db: model.input_level_db,
        listening: model.ui.ptt_listening,
        screen_audio: model.settings.screen_audio,
        sound_error: model.stream_sound_error.clone(),
    };
    let mut s = model.settings.clone();
    // "Press a key…": the first allowed key becomes the PTT key, and is consumed
    // so it does nothing else this frame.
    // Listening only makes sense where its button is: the Voice tab in PTT mode.
    let capture = tab == SettingsTab::Voice && s.input_mode == InputMode::Ptt;
    if v.listening && !capture {
        cmds.push(UiCommand::SetPttListening(false));
    }
    if v.listening && capture {
        let allowed = ptt_keys();
        let got = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Key {
                    key, pressed: true, ..
                } if allowed.contains(&key.name()) => Some(*key),
                _ => None,
            })
        });
        if let Some(k) = got {
            ctx.input_mut(|i| {
                i.events
                    .retain(|e| !matches!(e, egui::Event::Key { key, .. } if *key == k))
            });
            s.ptt_key = k.name().to_string();
            cmds.push(UiCommand::SetPttListening(false));
        }
    }
    let mut dragging = false;
    let mut drag_ended = false;
    let screen = ctx.content_rect().size();
    let size = vec2(760.0, 540.0)
        .min(screen - vec2(32.0, 32.0))
        .max(vec2(360.0, 260.0));
    let nick_edit = &mut model.ui.nick_edit;
    let resp = egui::Modal::new(egui::Id::unique("settings_window"))
        .frame(
            egui::Frame::new()
                .fill(theme::CARD)
                .stroke(Stroke::new(1.0, theme::BORDER))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(egui::Margin::same(0)),
        )
        .show(ctx, |ui| {
            ui.set_min_size(size);
            ui.set_max_size(size);
            ui.horizontal_top(|ui| {
                nav(ui, tab, size.y, cmds);
                ui.vertical(|ui| {
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(tab.label())
                                .heading()
                                .strong()
                                .color(theme::TEXT),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(12.0);
                            let x = icons::icon_button(
                                ui,
                                icons::CLOSE,
                                13.0,
                                theme::MUTED,
                                None,
                                "Close settings",
                                "Close (Esc)",
                            );
                            if x.clicked() {
                                cmds.push(UiCommand::CloseSettings);
                            }
                        });
                    });
                    egui::ScrollArea::vertical()
                        .id_salt("settings_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width() - 16.0);
                            ui.spacing_mut().slider_width = 300.0;
                            match tab {
                                SettingsTab::Voice => {
                                    voice_tab(ui, &mut s, &v, &mut dragging, &mut drag_ended, cmds)
                                }
                                SettingsTab::Share => {
                                    share_tab(ui, &mut s, &v, &mut dragging, &mut drag_ended, cmds)
                                }
                                SettingsTab::Shortcuts => shortcuts_tab(ui, &s),
                                SettingsTab::Profile => profile_tab(ui, nick_edit, cmds),
                            }
                        });
                });
            });
        });
    // Esc closes (or only stops "Press a key…"), unless a combo popup took it.
    if resp.is_top_modal
        && !resp.any_popup_open
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        if v.listening {
            cmds.push(UiCommand::SetPttListening(false));
        } else {
            cmds.push(UiCommand::CloseSettings);
        }
    }
    if s != model.settings || drag_ended {
        cmds.push(UiCommand::UpdateSettings {
            settings: Box::new(s),
            persist: drag_ended || !dragging,
        });
    }
}

fn nav(ui: &mut egui::Ui, tab: SettingsTab, h: f32, cmds: &mut Vec<UiCommand>) {
    egui::Frame::new()
        .fill(theme::CHANNELS)
        .inner_margin(egui::Margin::same(14))
        .corner_radius(CornerRadius {
            nw: 14,
            sw: 14,
            ne: 0,
            se: 0,
        })
        .show(ui, |ui| {
            // The frame sits in a horizontal row; its own content stacks top-down.
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                ui.set_width(170.0);
                ui.set_height(h - 28.0);
                ui.label(
                    RichText::new("Settings")
                        .heading()
                        .strong()
                        .color(theme::TEXT),
                );
                ui.add_space(10.0);
                for t in SettingsTab::ALL {
                    let on = t == tab;
                    let text =
                        RichText::new(t.label()).color(if on { theme::TEXT } else { theme::MUTED });
                    let r = ui.add_sized(
                        [ui.available_width(), 30.0],
                        egui::Button::selectable(on, text),
                    );
                    r.widget_info(|| egui::WidgetInfo::selected(Role::Tab, true, on, t.label()));
                    if r.clicked() && !on {
                        cmds.push(UiCommand::SetSettingsTab(t));
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new("Changes save as you make them.")
                                .small()
                                .color(theme::DIM),
                        )
                        .wrap(),
                    );
                });
            });
        });
}

fn voice_tab(
    ui: &mut egui::Ui,
    s: &mut Settings,
    v: &View,
    dragging: &mut bool,
    drag_ended: &mut bool,
    cmds: &mut Vec<UiCommand>,
) {
    device_combo(ui, "Microphone", &v.devices_in, &mut s.input_device);
    ui.add_space(6.0);
    device_combo(ui, "Speakers", &v.devices_out, &mut s.output_device);
    section(ui, "INPUT MODE");
    ui.horizontal(|ui| {
        for m in InputMode::ALL {
            if segment(ui, s.input_mode == m, m.label(), false) {
                s.input_mode = m;
            }
        }
    });
    match s.input_mode {
        InputMode::Vad => {
            section(ui, "SENSITIVITY");
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::Slider::new(&mut s.vad_threshold_db, -60.0..=0.0)
                        .clamping(egui::SliderClamping::Never)
                        .show_value(false),
                );
                let r = name(r, Role::Slider, "Sensitivity");
                *dragging |= r.dragged();
                *drag_ended |= r.drag_stopped();
                ui.label(
                    RichText::new(format!("Opens at {:.0} dB", s.vad_threshold_db))
                        .color(theme::MUTED),
                );
            });
            level_bar(ui, v.level_db, s.vad_threshold_db);
            ui.label(
                RichText::new("Speak normally. The bar turns green when your voice opens the mic.")
                    .small()
                    .color(theme::DIM),
            );
        }
        InputMode::Ptt => {
            section(ui, "PUSH-TO-TALK KEY");
            let text = if v.listening {
                LISTENING.to_string()
            } else {
                s.ptt_key.clone()
            };
            let stroke = if v.listening {
                theme::ACCENT
            } else {
                Color32::from_rgb(0x3A, 0x41, 0x4B)
            };
            let r = ui.add(
                egui::Button::new(RichText::new(&text).strong())
                    .stroke(Stroke::new(1.5, stroke))
                    .min_size(vec2(140.0, 34.0)),
            );
            let r = name(r, Role::Button, PTT_KEY_BUTTON);
            if r.clicked() && !v.listening {
                // Drop focus, or Space would press this button again.
                r.surrender_focus();
                cmds.push(UiCommand::SetPttListening(true));
            }
        }
        InputMode::Open => {}
    }
    section(ui, "OUTPUT");
    let mut pct = (s.master_volume * 100.0).round();
    let r = ui.add(
        egui::Slider::new(&mut pct, 0.0..=200.0)
            .suffix(" %")
            .text("Output volume"),
    );
    let r = name(r, Role::Slider, "Output volume");
    *dragging |= r.dragged();
    *drag_ended |= r.drag_stopped();
    if r.changed() {
        s.master_volume = pct / 100.0;
    }
}

/// Live input level with the threshold tick; green once it opens the mic.
fn level_bar(ui: &mut egui::Ui, level_db: f32, thr: f32) {
    let pct = |db: f32| ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width().min(420.0), 8.0), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(4), theme::HOVER);
    let mut fill = rect;
    fill.set_width(rect.width() * pct(level_db));
    let color = if level_db >= thr {
        theme::ACCENT
    } else {
        theme::OFF
    };
    p.rect_filled(fill, CornerRadius::same(4), color);
    let x = rect.left() + rect.width() * pct(thr);
    p.vline(x, rect.y_range().expand(3.0), Stroke::new(2.0, theme::TEXT));
}

fn share_tab(
    ui: &mut egui::Ui,
    s: &mut Settings,
    v: &View,
    dragging: &mut bool,
    drag_ended: &mut bool,
    cmds: &mut Vec<UiCommand>,
) {
    if let Some(t) = &v.limit_text {
        ui.label(RichText::new(t).color(theme::AMBER));
    }
    section(ui, "RESOLUTION");
    ui.horizontal_wrapped(|ui| {
        for h in SCREEN_HEIGHT_CHOICES {
            let over = v.cap_h > 0 && (h == 0 || h > v.cap_h);
            if segment(
                ui,
                s.screen_height == h,
                &height_choice_label(h, v.cap_h),
                over,
            ) {
                s.screen_height = h;
            }
        }
    });
    section(ui, "FRAME RATE");
    ui.horizontal_wrapped(|ui| {
        for f in SCREEN_FPS_CHOICES {
            let over = v.cap_fps > 0 && f > v.cap_fps;
            if segment(ui, s.screen_fps == f, &fps_choice_label(f, v.cap_fps), over) {
                s.screen_fps = f;
            }
        }
    });
    ui.add_space(10.0);
    let hw = crate::video::detect();
    let mut use_hw = s.screen_encoder == ScreenEncoderPref::Auto;
    let sub = encoder_text(hw.as_ref(), crate::video::hw_failed());
    if switch(ui, &mut use_hw, USE_HW_ENCODING, &sub) {
        s.screen_encoder = if use_hw {
            ScreenEncoderPref::Auto
        } else {
            ScreenEncoderPref::Jpeg
        };
    }
    ui.add_space(6.0);
    let mut sound = v.screen_audio;
    if switch(
        ui,
        &mut sound,
        SHARE_SOUND,
        "Watchers hear what your PC plays.",
    ) {
        // Not through the settings copy: stream sound has its own command.
        cmds.push(UiCommand::SetStreamSound { on: sound });
    }
    if let Some(e) = &v.sound_error {
        ui.label(RichText::new(e).small().color(theme::AMBER));
    }
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        let label = if v.probe_running {
            "Testing…"
        } else {
            TEST_MAX
        };
        let r = ui.add_enabled(!v.probe_running, egui::Button::new(label));
        if name(r, Role::Button, TEST_MAX).clicked() {
            cmds.push(UiCommand::TestScreenMax);
        }
        ui.label(
            RichText::new(
                "Finds the highest setting this PC can capture and encode, then picks it.",
            )
            .small()
            .color(theme::DIM),
        );
    });
    if let Some(t) = &v.probe_text {
        ui.label(RichText::new(t).color(theme::MUTED));
    }
    section(ui, "JPEG QUALITY");
    let r = ui.add(
        egui::Slider::new(
            &mut s.screen_quality,
            SCREEN_QUALITY_MIN..=SCREEN_QUALITY_MAX,
        )
        .text("JPEG quality"),
    );
    let r = name(r, Role::Slider, "JPEG quality");
    *dragging |= r.dragged();
    *drag_ended |= r.drag_stopped();
}

fn shortcuts_tab(ui: &mut egui::Ui, s: &Settings) {
    egui::Grid::new("shortcuts_grid")
        .num_columns(2)
        .spacing([40.0, 10.0])
        .show(ui, |ui| {
            for (what, keys) in shortcut_rows(&s.ptt_key) {
                ui.label(RichText::new(what).color(theme::TEXT));
                egui::Frame::new()
                    .fill(theme::HOVER)
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(egui::Margin::symmetric(8, 3))
                    .show(ui, |ui| {
                        ui.label(RichText::new(keys).monospace().color(theme::MUTED));
                    });
                ui.end_row();
            }
        });
}

fn profile_tab(ui: &mut egui::Ui, nick_edit: &mut String, cmds: &mut Vec<UiCommand>) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        theme::avatar(ui, nick_edit.as_str(), 56.0, false);
        ui.vertical(|ui| {
            let l = ui.label(
                RichText::new("Default nickname")
                    .small()
                    .color(theme::MUTED),
            );
            let r = ui
                .add(
                    egui::TextEdit::singleline(nick_edit)
                        .char_limit(vc_proto::MAX_NICK)
                        .desired_width(260.0),
                )
                .labelled_by(l.id);
            if r.lost_focus() {
                cmds.push(UiCommand::CommitNickname);
            }
        });
    });
    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "Each server can still use its own nickname. Set it in that server's right-click menu, Edit server.",
        )
        .small()
        .color(theme::DIM),
    );
    ui.label(
        RichText::new("applies on next Connect")
            .small()
            .color(theme::DIM),
    );
    section(ui, "ABOUT");
    ui.label(
        RichText::new(crate::update::status_text())
            .small()
            .color(theme::MUTED),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_labels_mark_over_cap() {
        assert_eq!(fps_choice_label(60, 0), "60 fps");
        assert_eq!(fps_choice_label(60, 30), "60 fps (over server limit)");
        assert_eq!(fps_choice_label(30, 30), "30 fps");
        assert_eq!(height_choice_label(1080, 720), "1080p (over server limit)");
        assert_eq!(height_choice_label(720, 720), "720p");
        assert_eq!(
            height_choice_label(0, 720),
            "Native (max 2560) (over server limit)"
        );
        assert_eq!(height_choice_label(0, 0), "Native (max 2560)");
        assert_eq!(encoder_text(None, false), "Encoder: CPU (JPEG)");
        let hw = crate::video::HwEncoder {
            name: "NVIDIA H.264 Encoder MFT".into(),
        };
        assert_eq!(
            encoder_text(Some(&hw), false),
            "Encoder: NVIDIA H.264 Encoder MFT (hardware)"
        );
    }

    #[test]
    fn ptt_keys_are_valid_egui_keys() {
        let keys = ptt_keys();
        assert_eq!(keys.len(), 1 + 12 + 26);
        for k in keys {
            let key = egui::Key::from_name(k).unwrap_or_else(|| panic!("{k}"));
            assert_eq!(key.name(), k);
        }
    }

    #[test]
    fn shortcut_rows_show_ptt_key() {
        let rows = shortcut_rows("V");
        assert_eq!(rows[0], ("Push-to-talk", "V".to_string()));
        assert_eq!(
            rows.iter().any(|(_, k)| k == "Ctrl+Shift+S"),
            crate::capture::SUPPORTED
        );
    }
}
