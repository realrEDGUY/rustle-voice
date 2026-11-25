//! egui GUI. `draw` is a function of the model that returns UI commands; its only
//! side effect is uploading textures for chat images and screen frames into the
//! `Textures` cache. `VoiceApp` wires it to the network, the audio pipeline,
//! screen capture and eframe.

mod channels;
mod chat;
mod dialogs;
mod icons;
mod menus;
mod screen;
mod settings;
mod sidebar;
mod streams;
mod theme;
mod toasts;
mod voice;

pub use chat::image_viewport_id;
pub use settings::{OVER_LIMIT, PTT_KEY_BUTTON, SHARE_SOUND, TEST_MAX, USE_HW_ENCODING};
pub use streams::{grid_dims, mute_stream_label, streams_viewport_id};

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{
    CentralPanel, ColorImage, Key, KeyboardShortcut, Modifiers, Panel, Response, Role,
    TextureHandle, WidgetInfo,
};
use vc_proto::{ChannelId, SessionId};

use crate::ServerKey;
use crate::app::{AppModel, UiCommand};
use crate::audio::AudioBackend;
use crate::capture::{
    CaptureCfg, CaptureHandle, CaptureThread, FakeScreen, GdiScreen, ScreenSource,
};
use crate::media::UploadSource;
use crate::net::{NetCmd, NetEvent, NetHandle, NetOptions};
use crate::pipeline::{AudioPipeline, PipelineCtl, PipelineThread};
use crate::probe::ProbeHandle;
use crate::profile::{InputMode, Profile};
use crate::session::{ConnStatus, ScreenView};

pub const NO_AUDIO_BANNER: &str = "No audio device: running without audio";

/// Give a widget an explicit accessible name (visible text unchanged).
pub(crate) fn name(resp: Response, role: Role, label: impl Into<String>) -> Response {
    let label = label.into();
    let enabled = resp.enabled();
    resp.widget_info(|| WidgetInfo::labeled(role, enabled, &label));
    resp
}

/// GPU textures for chat images and screen frames, created lazily from the
/// model's decoded images and dropped once the model no longer has them.
#[derive(Default)]
pub struct Textures {
    images: HashMap<u64, TextureHandle>,
    screens: HashMap<(ServerKey, SessionId), (u64, TextureHandle)>,
}

impl Textures {
    pub fn image(&mut self, ctx: &egui::Context, id: u64, img: &Arc<ColorImage>) -> TextureHandle {
        self.images
            .entry(id)
            .or_insert_with(|| {
                ctx.load_texture(
                    format!("chat-image-{id}"),
                    img.clone(),
                    egui::TextureOptions::LINEAR,
                )
            })
            .clone()
    }

    pub fn screen(
        &mut self,
        ctx: &egui::Context,
        key: ServerKey,
        sid: SessionId,
        view: &ScreenView,
    ) -> TextureHandle {
        match self.screens.get_mut(&(key, sid)) {
            Some((generation, tex)) => {
                if *generation != view.generation {
                    tex.set(view.image.clone(), egui::TextureOptions::LINEAR);
                    *generation = view.generation;
                }
                tex.clone()
            }
            None => {
                let tex = ctx.load_texture(
                    format!("screen-{key}-{}", sid.0),
                    view.image.clone(),
                    egui::TextureOptions::LINEAR,
                );
                self.screens
                    .insert((key, sid), (view.generation, tex.clone()));
                tex
            }
        }
    }

    /// Drop textures whose image or screen is gone from the model.
    pub fn prune(&mut self, model: &AppModel) {
        self.images
            .retain(|id, _| model.sessions.values().any(|s| s.images.contains_key(id)));
        self.screens.retain(|(k, sid), _| {
            model
                .sessions
                .get(k)
                .is_some_and(|s| s.screens.contains_key(sid))
        });
    }

    pub fn len(&self) -> usize {
        self.images.len() + self.screens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Every popout viewport open this frame (Streams window, image windows), so
/// input such as push to talk can be read from any of them.
pub fn popout_ids(model: &AppModel) -> Vec<egui::ViewportId> {
    let mut ids = Vec::new();
    if model.ui.streams_popped && model.sessions.values().any(|s| !s.watching.is_empty()) {
        ids.push(streams_viewport_id());
    }
    ids.extend(model.ui.viewing.iter().map(|id| image_viewport_id(*id)));
    ids
}

/// Draw the whole UI for one frame.
pub fn draw(
    ui: &mut egui::Ui,
    model: &mut AppModel,
    now: Instant,
    tex: &mut Textures,
) -> Vec<UiCommand> {
    let mut cmds = Vec::new();
    if let Some(w) = model.audio_warning.clone() {
        Panel::top("audio_banner")
            .frame(egui::Frame::new().fill(theme::RAIL).inner_margin(6))
            .show(ui, |ui| {
                ui.colored_label(theme::AMBER, w);
            });
    }
    let ctx = ui.ctx().clone();
    shortcuts(&ctx, model, &mut cmds);
    Panel::left("rail")
        .exact_size(theme::RAIL_W)
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(theme::RAIL)
                .inner_margin(egui::Margin::symmetric(0, 8)),
        )
        .show(ui, |ui| sidebar::draw(ui, model, &mut cmds));
    Panel::left("channels")
        .exact_size(theme::COL_W)
        .resizable(false)
        .frame(theme::column(theme::CHANNELS))
        .show(ui, |ui| channels::draw(ui, model, now, &mut cmds));
    CentralPanel::default()
        .frame(egui::Frame::new().fill(theme::BG))
        .show(ui, |ui| chat::draw(ui, model, tex, &mut cmds));
    chat::viewer(&ctx, model, tex, &mut cmds);
    streams::draw(&ctx, model, tex, &mut cmds);
    dialogs::draw(&ctx, model, &mut cmds);
    settings::draw(&ctx, model, &mut cmds);
    toasts::draw(&ctx, model, &mut cmds);
    cmds
}

/// The command for a shortcut, or `None`. `typing`: a text field has focus (only
/// F2 is held back then; the Ctrl combos work while typing). A shortcut on the
/// push-to-talk key is skipped, so holding it to talk never triggers one.
/// `rename`: my channel on the selected server (F2 renames it).
pub fn shortcut_cmd(
    key: Key,
    mods: Modifiers,
    typing: bool,
    ptt_key: &str,
    rename: Option<ChannelId>,
) -> Option<UiCommand> {
    if key.name() == ptt_key {
        return None;
    }
    let ctrl = mods.command && !mods.alt;
    match key {
        // No capture backend (Linux): the hotkey does nothing.
        Key::S if ctrl && mods.shift && crate::capture::SUPPORTED => {
            Some(UiCommand::ToggleScreenShare)
        }
        Key::M if ctrl && !mods.shift => Some(UiCommand::ToggleMute),
        Key::D if ctrl && !mods.shift => Some(UiCommand::ToggleDeafen),
        Key::F2 if !mods.any() && !typing => rename.map(UiCommand::OpenRenameChannel),
        _ => None,
    }
}

/// The channel F2 renames: mine, on a connected server. (Whether the server
/// allows it is its call; a refusal shows as a toast.)
pub fn f2_target(s: &crate::session::ClientSession) -> Option<ChannelId> {
    if s.status != ConnStatus::Connected {
        return None;
    }
    s.my_channel()
}

/// Global shortcuts: Ctrl+Shift+S, Ctrl+M, Ctrl+D and F2. None while a dialog,
/// the Settings window or an inline channel edit is open. Runs before the panels
/// so the keys are consumed before a text field sees them.
fn shortcuts(ctx: &egui::Context, model: &AppModel, cmds: &mut Vec<UiCommand>) {
    let ui = &model.ui;
    if ui.server_dialog.is_some()
        || ui.settings_tab.is_some()
        || ui.tofu_prompt.is_some()
        || ui.channel_dialog.is_some()
    {
        return;
    }
    let typing = ctx.egui_wants_keyboard_input();
    let rename = model.selected_session().and_then(f2_target);
    let combos = [
        (Modifiers::COMMAND | Modifiers::SHIFT, Key::S),
        (Modifiers::COMMAND, Key::M),
        (Modifiers::COMMAND, Key::D),
        (Modifiers::NONE, Key::F2),
    ];
    for (mods, key) in combos {
        let Some(c) = shortcut_cmd(key, mods, typing, &model.settings.ptt_key, rename) else {
            continue;
        };
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(mods, key))) {
            cmds.push(c);
        }
    }
}

/// How often connected servers are pinged (for the menu's and voice card's ms).
pub const PING_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub struct AppOptions {
    pub net: NetOptions,
    /// Run the 20 ms audio clock thread. Tests turn it off and call `tick_audio`.
    pub audio_thread: bool,
    /// Share a synthetic gradient instead of the real screen (tests).
    pub fake_screen: bool,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self {
            net: NetOptions::default(),
            audio_thread: true,
            fake_screen: false,
        }
    }
}

pub struct VoiceApp {
    // Field order is drop order: stop the audio clock before the network.
    _thread: Option<PipelineThread>,
    pub model: AppModel,
    pub net: NetHandle,
    pub pipeline: Arc<Mutex<AudioPipeline>>,
    pub ctl: Arc<Mutex<PipelineCtl>>,
    backend: Box<dyn AudioBackend>,
    streams: Vec<Box<dyn Any>>,
    pub textures: Textures,
    capture: Option<(ServerKey, CaptureThread)>,
    /// Live settings the capture thread reads; mirrored from the model.
    capture_cfg: CaptureHandle,
    fake_screen: bool,
    /// A running "Test max" probe.
    probe: Option<ProbeHandle>,
    /// The system-audio loopback capture (stream sound) while it runs.
    loopback: Option<Box<dyn Any>>,
    ctx: Option<egui::Context>,
    /// When connected servers were last pinged (no Drop; order does not matter).
    last_ping: Instant,
}

impl VoiceApp {
    /// Production constructor (used by `main` and the eframe test).
    pub fn new(
        ctx: egui::Context,
        backend: Box<dyn AudioBackend>,
        profile: Profile,
        autoconnect: Vec<String>,
    ) -> Self {
        Self::with_options(
            Some(ctx),
            backend,
            profile,
            autoconnect,
            AppOptions::default(),
        )
    }

    pub fn with_options(
        ctx: Option<egui::Context>,
        backend: Box<dyn AudioBackend>,
        profile: Profile,
        autoconnect: Vec<String>,
        opts: AppOptions,
    ) -> Self {
        if let Some(c) = &ctx {
            theme::install(c);
        }
        let mut model = AppModel::load(profile);
        model.hw_encode_allowed = !opts.fake_screen;
        let net = NetHandle::with_options(opts.net);
        if let Some(c) = &ctx {
            net.set_ctx(c.clone());
        }
        let pipeline = Arc::new(Mutex::new(AudioPipeline::new()));
        {
            let p = pipeline.clone();
            net.set_voice_sink(Arc::new(move |key, speaker, seq, payload| {
                p.lock()
                    .expect("poisoned")
                    .on_voice(key, speaker, seq, payload);
            }));
        }
        let ctl = Arc::new(Mutex::new(model.pipeline_ctl()));
        let capture_cfg: CaptureHandle = Arc::new(Mutex::new(model.capture_cfg()));
        let thread = if opts.audio_thread {
            let tx = net.sender();
            Some(AudioPipeline::spawn_thread(
                pipeline.clone(),
                ctl.clone(),
                move |frames| {
                    for (key, seq, payload) in frames {
                        let _ = tx.send(NetCmd::SendVoice { key, seq, payload });
                    }
                },
            ))
        } else {
            None
        };
        let mut app = Self {
            probe: None,
            loopback: None,
            ctx: ctx.clone(),
            model,
            net,
            pipeline,
            ctl,
            backend,
            streams: Vec::new(),
            textures: Textures::default(),
            capture: None,
            capture_cfg,
            fake_screen: opts.fake_screen,
            _thread: thread,
            // Due at once: the first ping goes out as soon as a server connects.
            last_ping: Instant::now()
                .checked_sub(PING_EVERY)
                .unwrap_or_else(Instant::now),
        };
        app.refresh_devices();
        app.start_audio();
        for label in autoconnect {
            match app
                .model
                .saved
                .iter()
                .find(|s| s.label == label)
                .map(|s| s.key)
            {
                Some(k) => app.dispatch(vec![UiCommand::Connect(k)]),
                None => app
                    .model
                    .ui
                    .toasts
                    .push(format!("Error: no saved server named {label:?}")),
            }
        }
        app
    }

    pub fn refresh_devices(&mut self) {
        self.model.devices_in = self.backend.inputs();
        self.model.devices_out = self.backend.outputs();
    }

    /// (Re)start capture and playback with the configured devices.
    pub fn start_audio(&mut self) {
        self.streams.clear();
        let mut failed = Vec::new();
        let (cap_cb, play_cb) = {
            let p = self.pipeline.lock().expect("poisoned");
            (p.capture_cb(), p.playback_cb())
        };
        let input = self.model.settings.input_device.clone();
        let output = self.model.settings.output_device.clone();
        match self.backend.start_capture(input.as_deref(), cap_cb) {
            Ok(s) => self.streams.push(s),
            Err(e) => failed.push(format!("input: {e}")),
        }
        match self.backend.start_playback(output.as_deref(), play_cb) {
            Ok(s) => self.streams.push(s),
            Err(e) => failed.push(format!("output: {e}")),
        }
        if failed.is_empty() {
            self.model.audio_warning = None;
        } else {
            tracing::warn!("audio unavailable ({})", failed.join("; "));
            self.model.audio_warning = Some(NO_AUDIO_BANNER.to_string());
        }
    }

    /// Drain network events into the model.
    pub fn poll(&mut self) {
        let now = Instant::now();
        let mut follow = Vec::new();
        while let Some(ev) = self.net.try_recv() {
            if let NetEvent::Status { key, status } = &ev
                && !status.is_active()
            {
                self.pipeline.lock().expect("poisoned").forget_server(*key);
            }
            if let NetEvent::Image {
                key,
                from,
                channel,
                ts_ms,
                seq,
                w,
                h,
            } = ev
            {
                let image = self.net.take_image(seq);
                self.model.on_image(key, from, channel, ts_ms, w, h, image);
                continue;
            }
            follow.extend(self.model.apply_net(ev, now));
        }
        for c in follow {
            self.net.send(c);
        }
        for (key, from, image) in self.net.take_screen_frames() {
            self.model.on_screen_frame(key, from, image);
        }
        // Runs from `logic` too, so a minimised sharer still reacts to new watchers.
        self.mirror_capture_cfg();
        self.poll_probe();
        self.sync_loopback();
    }

    /// Start a requested "Test max", and fold a finished one into the model.
    fn poll_probe(&mut self) {
        if self.model.probe_requested {
            self.model.probe_requested = false;
            let fake = self.fake_screen;
            let make: Box<dyn Fn() -> Box<dyn ScreenSource> + Send> = Box::new(move || {
                if fake {
                    Box::new(FakeScreen::default())
                } else {
                    Box::new(GdiScreen)
                }
            });
            let s = &self.model.settings;
            let video = !fake
                && s.screen_encoder == crate::profile::ScreenEncoderPref::Auto
                && crate::video::detect().is_some();
            self.probe = Some(ProbeHandle::spawn(
                make,
                s.screen_quality,
                video,
                self.ctx.clone(),
            ));
        }
        let done = self.probe.as_ref().and_then(|p| p.take_result());
        if let Some(res) = done {
            self.probe = None;
            self.model.on_probe_result(res);
        }
    }

    /// Is a "Test max" probe running (tests)?
    pub fn probing(&self) -> bool {
        self.probe.is_some()
    }

    /// Start or stop the stream-sound loopback capture to match the model.
    fn sync_loopback(&mut self) {
        let want = self.model.wants_loopback();
        if want == self.loopback.is_some() {
            return;
        }
        if !want {
            self.loopback = None;
            return;
        }
        let cb = self.pipeline.lock().expect("poisoned").stream_cb();
        match self.backend.start_loopback(cb) {
            Ok(b) => self.loopback = Some(b),
            Err(e) => {
                tracing::warn!("stream sound unavailable: {e}");
                self.model.loopback_failed(&e.to_string());
                *self.ctl.lock().expect("poisoned") = self.model.pipeline_ctl();
            }
        }
    }

    /// Is the stream-sound loopback capture running (tests)?
    pub fn loopback_running(&self) -> bool {
        self.loopback.is_some()
    }

    fn mirror_capture_cfg(&self) {
        *self.capture_cfg.lock().unwrap_or_else(|e| e.into_inner()) = self.model.capture_cfg();
    }

    /// The live capture settings the capture thread reads (tests).
    pub fn capture_cfg(&self) -> CaptureCfg {
        *self.capture_cfg.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Is the screen-capture thread running (and for which server)?
    pub fn capturing(&self) -> Option<ServerKey> {
        self.capture.as_ref().map(|(k, _)| *k)
    }

    /// Start or stop the capture thread to match the model, and surface its errors.
    fn sync_capture(&mut self, ctx: &egui::Context) {
        let err = self.capture.as_ref().and_then(|(_, t)| t.take_error());
        if let Some(e) = err {
            self.capture = None;
            for n in self.model.capture_failed(&e) {
                self.net.send(n);
            }
        }
        let want = self.model.wants_capture();
        if self.capturing() == want {
            return;
        }
        self.capture = None; // joins the old thread
        if let Some(key) = want {
            let source: Box<dyn ScreenSource> = if self.fake_screen {
                Box::new(FakeScreen::default())
            } else {
                Box::new(GdiScreen)
            };
            let t = CaptureThread::start(
                source,
                key,
                self.net.sender(),
                Some(ctx.clone()),
                self.capture_cfg.clone(),
            );
            self.capture = Some((key, t));
        }
    }

    /// Clipboard images and dropped files become upload commands. Runs before
    /// `draw`, so pasting works while the chat field has focus (TextEdit ignores
    /// image pastes). Dropped files are only passed on by path: reading and decoding
    /// happen on the network runtime, never on the UI thread.
    ///
    /// Uploads are only accepted from the main chat context: no dialog, settings or
    /// TOFU prompt open, and focus on the chat field or nowhere. Image windows are
    /// separate viewports, so they do not block uploads.
    /// Otherwise a Ctrl+V meant for another field would post the clipboard image.
    fn read_uploads(&mut self, ui: &egui::Ui) -> Vec<UiCommand> {
        let ui_state = &self.model.ui;
        let modal = ui_state.server_dialog.is_some()
            || ui_state.channel_dialog.is_some()
            || ui_state.settings_tab.is_some()
            || ui_state.tofu_prompt.is_some()
            || egui::Popup::is_any_open(ui.ctx());
        let focus_ok = ui
            .ctx()
            .memory(|m| m.focused())
            .is_none_or(|id| id == chat::input_id());
        let accept = !modal && focus_ok;
        let (pasted, dropped, hovering) = ui.input(|i| {
            let pasted: Vec<Arc<ColorImage>> = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::PasteImage(img) => Some(img.clone()),
                    _ => None,
                })
                .collect();
            let dropped: Vec<std::path::PathBuf> = i
                .raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect();
            (pasted, dropped, !i.raw.hovered_files.is_empty())
        });
        self.model.ui.files_hovering = hovering && accept;
        if !accept {
            return Vec::new();
        }
        let mut cmds: Vec<UiCommand> = pasted
            .into_iter()
            .map(|img| UiCommand::UploadImage(UploadSource::Pasted(img)))
            .collect();
        cmds.extend(
            dropped
                .into_iter()
                .map(|p| UiCommand::UploadImage(UploadSource::File(p))),
        );
        cmds
    }

    /// Execute UI commands (model update + network commands).
    pub fn dispatch(&mut self, cmds: Vec<UiCommand>) {
        for c in cmds {
            if c == UiCommand::OpenSettings {
                self.refresh_devices();
            }
            for n in self.model.handle(c) {
                self.net.send(n);
            }
        }
        if self.model.audio_restart_needed {
            self.model.audio_restart_needed = false;
            self.start_audio();
        }
        *self.ctl.lock().expect("poisoned") = self.model.pipeline_ctl();
        self.mirror_capture_cfg();
        self.poll_probe();
        self.sync_loopback();
    }

    /// Manually run one audio tick (tests without the audio thread).
    pub fn tick_audio(&mut self) {
        *self.ctl.lock().expect("poisoned") = self.model.pipeline_ctl();
        let c = self.ctl.lock().expect("poisoned").clone();
        let frames = self.pipeline.lock().expect("poisoned").tick(&c);
        for (key, seq, payload) in frames {
            self.net.send(NetCmd::SendVoice { key, seq, payload });
        }
    }

    /// One GUI frame: poll, read PTT, draw, dispatch, schedule repaint.
    pub fn frame(&mut self, ui: &mut egui::Ui) {
        self.poll();
        let now = Instant::now();
        let ctx = ui.ctx().clone();
        self.model.ptt_down = if self.model.settings.input_mode == InputMode::Ptt
            && !ctx.egui_wants_keyboard_input()
            && !self.model.ui.ptt_listening
        {
            match egui::Key::from_name(&self.model.settings.ptt_key) {
                // Held in any of our windows: the main one or a popout.
                Some(k) => std::iter::once(egui::ViewportId::ROOT)
                    .chain(popout_ids(&self.model))
                    .any(|id| ctx.input_for(id, |i| i.key_down(k))),
                None => false,
            }
        } else {
            false
        };
        {
            let p = self.pipeline.lock().expect("poisoned");
            self.model.input_level_db = p.input_level_db();
            self.model.transmitting = p.is_transmitting();
        }
        let mut cmds = self.read_uploads(ui);
        let connected: Vec<ServerKey> = self
            .model
            .sessions
            .iter()
            .filter(|(_, s)| s.status == ConnStatus::Connected)
            .map(|(k, _)| *k)
            .collect();
        if !connected.is_empty() {
            if now.saturating_duration_since(self.last_ping) >= PING_EVERY {
                self.last_ping = now;
                cmds.extend(connected.iter().map(|k| UiCommand::Ping(*k)));
            }
            // Keep the ping (and its ms) fresh while the app is idle.
            ctx.request_repaint_after(PING_EVERY);
        }
        cmds.extend(draw(ui, &mut self.model, now, &mut self.textures));
        self.dispatch(cmds);
        self.sync_capture(&ctx);
        self.textures.prune(&self.model);
        if self.model.anything_busy(now) || self.model.transmitting {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if !self.streams.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

#[cfg(feature = "desktop")]
impl eframe::App for VoiceApp {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_cmd_mapping() {
        let ch = Some(ChannelId(3));
        let ctrl = Modifiers::COMMAND;
        assert_eq!(
            shortcut_cmd(Key::M, ctrl, true, "Space", ch),
            Some(UiCommand::ToggleMute),
            "Ctrl combos work while typing"
        );
        assert_eq!(
            shortcut_cmd(Key::D, ctrl, false, "Space", ch),
            Some(UiCommand::ToggleDeafen)
        );
        assert_eq!(
            shortcut_cmd(Key::S, ctrl | Modifiers::SHIFT, false, "Space", ch),
            crate::capture::SUPPORTED.then_some(UiCommand::ToggleScreenShare)
        );
        assert_eq!(shortcut_cmd(Key::S, ctrl, false, "Space", ch), None);
        assert_eq!(
            shortcut_cmd(Key::M, Modifiers::NONE, false, "Space", ch),
            None
        );
        assert_eq!(
            shortcut_cmd(Key::F2, Modifiers::NONE, false, "Space", ch),
            Some(UiCommand::OpenRenameChannel(ChannelId(3)))
        );
        assert_eq!(
            shortcut_cmd(Key::F2, Modifiers::NONE, true, "Space", ch),
            None,
            "F2 is held back while typing"
        );
        assert_eq!(
            shortcut_cmd(Key::F2, Modifiers::NONE, false, "Space", None),
            None
        );
    }

    #[test]
    fn f2_targets_my_channel_including_default() {
        use vc_proto::UserInfo;
        let mut s = crate::session::ClientSession::new();
        s.status = ConnStatus::Connected;
        s.my_id = Some(SessionId(1));
        s.default_channel = Some(ChannelId(1));
        let me = |ch| UserInfo {
            id: SessionId(1),
            nick: "me".into(),
            channel: ChannelId(ch),
            muted: false,
            deafened: false,
            sharing: false,
        };
        s.users.insert(SessionId(1), me(1));
        assert_eq!(f2_target(&s), Some(ChannelId(1)), "default channel too");
        s.users.insert(SessionId(1), me(2));
        assert_eq!(f2_target(&s), Some(ChannelId(2)));
        s.status = ConnStatus::Disconnected;
        assert_eq!(f2_target(&s), None);
    }

    #[test]
    fn shortcut_on_ptt_key_is_skipped() {
        let ch = Some(ChannelId(3));
        assert_eq!(
            shortcut_cmd(Key::F2, Modifiers::NONE, false, "F2", ch),
            None
        );
        assert_eq!(
            shortcut_cmd(Key::M, Modifiers::COMMAND, false, "M", ch),
            None
        );
        assert_eq!(
            shortcut_cmd(Key::D, Modifiers::COMMAND, false, "M", ch),
            Some(UiCommand::ToggleDeafen)
        );
    }
}
