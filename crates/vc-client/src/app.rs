//! Whole-app model: saved servers, one `ClientSession` per server, settings, UI state.
//! `apply_net` folds network events in; `handle` turns UI commands into `NetCmd`s.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use egui::ColorImage;
use vc_proto::{ChannelId, ClientMsg, STREAM_BIT, ServerMsg, SessionId};

use crate::ServerKey;
use crate::audio::DeviceInfo;
use crate::capture::CaptureCfg;
use crate::media::UploadSource;
use crate::net::{NetCmd, NetEvent};
use crate::pipeline::{PipelineCtl, StreamTuning};
use crate::probe::ProbeResult;
use crate::profile::{
    InputMode, Profile, SCREEN_FPS_MAX, SavedServer, ScreenEncoderPref, Settings, clamp_screen,
    screen_box, screen_height_label, screen_interval, validate_addr,
};
use crate::session::{ChatLine, ClientSession, ConnStatus, UiAction, error_text};
use crate::tofu::{self, Fingerprint};

/// Nick the Add dialog proposes while the Settings nickname is empty.
pub const DEFAULT_NICK: &str = "user";
/// Shown while "Test max" runs.
pub const PROBE_RUNNING: &str = "Testing\u{2026} (about 3 s)";
/// Prefix of the status shown when system-audio loopback cannot start.
pub const STREAM_SOUND_UNAVAILABLE: &str = "Stream sound unavailable";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerDialog {
    /// `None` = adding a new server.
    pub editing: Option<ServerKey>,
    pub label: String,
    pub addr: String,
    pub nick: String,
    pub password: String,
    pub error: Option<String>,
    /// Connect right after adding (Add only; Edit leaves it false).
    pub connect_now: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelDialogMode {
    New,
    Rename(ChannelId),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelDialog {
    pub key: ServerKey,
    pub mode: ChannelDialogMode,
    pub name: String,
}

/// Tabs of the Settings window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Voice,
    Share,
    Shortcuts,
    Profile,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 4] = [
        SettingsTab::Voice,
        SettingsTab::Share,
        SettingsTab::Shortcuts,
        SettingsTab::Profile,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::Voice => "Voice",
            SettingsTab::Share => "Screen share",
            SettingsTab::Shortcuts => "Shortcuts",
            SettingsTab::Profile => "Profile",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UiState {
    pub server_dialog: Option<ServerDialog>,
    pub channel_dialog: Option<ChannelDialog>,
    pub chat_input: String,
    pub toasts: Vec<String>,
    pub tofu_prompt: Option<(ServerKey, Fingerprint)>,
    /// The Settings window is open on this tab. It edits `AppModel::settings`
    /// directly (there is no draft): changes save as they are made.
    pub settings_tab: Option<SettingsTab>,
    /// The Profile tab's nickname field; committed when it loses focus or the
    /// window closes (`CommitNickname`).
    pub nick_edit: String,
    /// The push-to-talk key button waits for a key press.
    pub ptt_listening: bool,
    /// Watched streams are shown in their own window instead of the main stage.
    pub streams_popped: bool,
    /// Chat images open in their own viewer windows (`ChatImage::id`).
    pub viewing: BTreeSet<u64>,
    /// Files are being dragged over the window.
    pub files_hovering: bool,
}

/// Global mute/deafen (applies to every connected server).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VoiceFlags {
    pub muted: bool,
    pub deafened: bool,
    pub muted_before_deafen: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UiCommand {
    Select(ServerKey),
    Connect(ServerKey),
    Disconnect(ServerKey),
    TalkHere(ServerKey),
    OpenAddServer,
    OpenEditServer(ServerKey),
    SaveServerDialog,
    CancelServerDialog,
    RemoveServer(ServerKey),
    Join(ChannelId),
    OpenNewChannel,
    OpenRenameChannel(ChannelId),
    /// Submit the inline channel edit drawn for `(key, mode)`. Ignored when the
    /// open edit is another one (it was replaced in the same frame).
    SubmitChannelDialog {
        key: ServerKey,
        mode: ChannelDialogMode,
    },
    /// Cancel the inline channel edit drawn for `(key, mode)` (ignored likewise).
    CancelChannelDialog {
        key: ServerKey,
        mode: ChannelDialogMode,
    },
    DeleteChannel(ChannelId),
    SendChat,
    ToggleMute,
    ToggleDeafen,
    SetInputMode(InputMode),
    SetUserVolume {
        key: ServerKey,
        nick: String,
        volume: f32,
    },
    OpenSettings,
    /// Replace the settings with the Settings window's edited copy. Fields the
    /// window does not own (`user_volumes`, `stream_volumes`, `screen_audio`,
    /// `nickname`) are kept
    /// from the live settings. Written to disk only when `persist` (a slider being
    /// dragged sends `false` until the drag ends).
    UpdateSettings {
        settings: Box<Settings>,
        persist: bool,
    },
    SetSettingsTab(SettingsTab),
    /// Apply `UiState::nick_edit` as the default nickname.
    CommitNickname,
    SetPttListening(bool),
    CloseSettings,
    TrustNewIdentity,
    CancelTofu,
    DismissToast(usize),
    Ping(ServerKey),
    /// Send an image to the selected server's current channel.
    UploadImage(UploadSource),
    /// Start or stop sharing the screen on the selected server.
    ToggleScreenShare,
    /// Stop a running share, else start sharing on this server (the voice card).
    ShareScreenOn(ServerKey),
    /// Mute or unmute one user's voice, for me only (session-only).
    MuteUser {
        key: ServerKey,
        nick: String,
        muted: bool,
    },
    /// Show watched streams in their own window (`true`) or in the main stage.
    SetStreamsPopped(bool),
    /// Open a chat image in its own window.
    ViewImage(u64),
    /// Close one image window.
    CloseImage(u64),
    /// Start or stop watching a sharer on one server.
    WatchScreen {
        key: ServerKey,
        user: SessionId,
        on: bool,
    },
    /// Run "Test max" (local capture + encode limit) and apply its result.
    TestScreenMax,
    /// Share (or stop sharing) my system audio with my stream's watchers.
    SetStreamSound {
        on: bool,
    },
    /// Mute or unmute one sharer's stream sound, for me only.
    MuteStream {
        key: ServerKey,
        user: SessionId,
        muted: bool,
    },
    /// Set one sharer's stream sound volume (0.0-2.0), for me only; persisted
    /// per server and nick. Mute still wins over it.
    SetStreamVolume {
        key: ServerKey,
        nick: String,
        volume: f32,
    },
}

/// The session id that carries `user`'s stream sound (see `vc_proto::STREAM_BIT`).
pub fn stream_speaker(user: SessionId) -> SessionId {
    SessionId(user.0 | STREAM_BIT)
}

/// Milliseconds since the unix epoch (the ping nonce).
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct AppModel {
    pub profile: Profile,
    pub saved: Vec<SavedServer>,
    pub sessions: BTreeMap<ServerKey, ClientSession>,
    pub selected: Option<ServerKey>,
    pub talk_target: Option<ServerKey>,
    pub settings: Settings,
    pub voice: VoiceFlags,
    pub ui: UiState,
    pub input_level_db: f32,
    pub transmitting: bool,
    pub ptt_down: bool,
    pub devices_in: Vec<DeviceInfo>,
    pub devices_out: Vec<DeviceInfo>,
    pub audio_warning: Option<String>,
    /// Set when saved settings changed the devices; the app restarts audio streams.
    pub audio_restart_needed: bool,
    /// Id for the next received chat image (unique across servers).
    pub next_image_id: u64,
    /// The server my screen is shared on, if any.
    pub screen_share: Option<ServerKey>,
    /// "Test max" was asked for; `VoiceApp` starts the probe and clears this.
    pub probe_requested: bool,
    /// A "Test max" probe is running.
    pub probe_running: bool,
    /// Why the last "Test max" failed.
    pub probe_error: Option<String>,
    /// Streams whose sound I muted, by `(server, sharer)`. Local, not persisted.
    pub stream_muted: BTreeSet<(ServerKey, SessionId)>,
    /// Users whose voice I muted ("Mute for me"), by `(server, nick)`. Session-only
    /// on purpose, like `stream_muted`; their volume setting is left untouched.
    pub user_muted: BTreeSet<(ServerKey, String)>,
    /// Why stream sound could not start (shown until the user toggles it again).
    pub stream_sound_error: Option<String>,
    /// Hardware H.264 may be used for my share (off for fake-screen test apps, so
    /// the normal test suites never open a real encoder session).
    pub hw_encode_allowed: bool,
}

impl AppModel {
    /// Load the profile from disk (missing files give defaults).
    pub fn load(profile: Profile) -> Self {
        let mut toasts = Vec::new();
        let saved = profile.load_servers().unwrap_or_else(|e| {
            toasts.push(format!("Error: {e}"));
            vec![]
        });
        let settings = profile.load_settings().unwrap_or_else(|e| {
            toasts.push(format!("Error: {e}"));
            Settings::default()
        });
        let sessions = saved
            .iter()
            .map(|s| (s.key, ClientSession::new()))
            .collect();
        let selected = saved.first().map(|s| s.key);
        Self {
            profile,
            saved,
            sessions,
            selected,
            talk_target: None,
            settings,
            voice: VoiceFlags::default(),
            ui: UiState {
                toasts,
                ..UiState::default()
            },
            input_level_db: -100.0,
            transmitting: false,
            ptt_down: false,
            devices_in: vec![],
            devices_out: vec![],
            audio_warning: None,
            audio_restart_needed: false,
            next_image_id: 1,
            screen_share: None,
            probe_requested: false,
            probe_running: false,
            probe_error: None,
            stream_muted: BTreeSet::new(),
            user_muted: BTreeSet::new(),
            stream_sound_error: None,
            hw_encode_allowed: true,
        }
    }

    pub fn saved_server(&self, key: ServerKey) -> Option<&SavedServer> {
        self.saved.iter().find(|s| s.key == key)
    }

    pub fn session(&self, key: ServerKey) -> Option<&ClientSession> {
        self.sessions.get(&key)
    }

    pub fn selected_session(&self) -> Option<&ClientSession> {
        self.selected.and_then(|k| self.sessions.get(&k))
    }

    pub fn status(&self, key: ServerKey) -> ConnStatus {
        self.sessions
            .get(&key)
            .map(|s| s.status.clone())
            .unwrap_or_default()
    }

    /// Chat lines for the selected server's current channel.
    pub fn chat_view(&self) -> Vec<&ChatLine> {
        let Some(s) = self.selected_session() else {
            return vec![];
        };
        match s.my_channel() {
            Some(ch) => s.chat_in(ch),
            None => vec![],
        }
    }

    pub fn anything_busy(&self, now: Instant) -> bool {
        self.sessions.values().any(|s| {
            matches!(
                s.status,
                ConnStatus::Connecting | ConnStatus::Reconnecting { .. }
            ) || s.anyone_speaking(now)
        })
    }

    /// The server whose screen-share capture should be running, if any.
    pub fn wants_capture(&self) -> Option<ServerKey> {
        self.screen_share
            .filter(|k| self.status(*k) == ConnStatus::Connected)
    }

    /// Live capture settings: the Preferences clamped to the sharing server's caps,
    /// active only while my share has watchers, with that session's watcher epoch.
    /// Hardware H.264 is used when allowed, available, and every watcher decodes it.
    pub fn capture_cfg(&self) -> CaptureCfg {
        let mut c = self.settings.screen_capture_config();
        match self.wants_capture().and_then(|k| self.sessions.get(&k)) {
            Some(s) => {
                let (cap_fps, cap_h) = s.screen_caps();
                let (fps, h) = clamp_screen(
                    self.settings.screen_fps.clamp(1, SCREEN_FPS_MAX),
                    self.settings.screen_height,
                    cap_fps,
                    cap_h,
                );
                c.interval = screen_interval(fps);
                (c.max_w, c.max_h) = screen_box(h);
                c.active = s.watchers > 0;
                c.epoch = s.watch_epoch;
                c.video = self.hw_encode_allowed
                    && self.settings.screen_encoder == ScreenEncoderPref::Auto
                    && s.watchers_h264
                    && crate::video::detect().is_some();
            }
            None => c.active = false,
        }
        c
    }

    /// The server whose caps the Preferences show: the sharing one, else the selected.
    fn caps_session(&self) -> Option<&ClientSession> {
        self.wants_capture()
            .or(self.selected)
            .and_then(|k| self.sessions.get(&k))
            .filter(|s| s.status == ConnStatus::Connected)
    }

    /// The caps `(fps, height)` the Preferences compare choices with (0 = no cap).
    pub fn screen_caps(&self) -> (u32, u32) {
        self.caps_session().map_or((0, 0), |s| s.screen_caps())
    }

    /// "Server limit: 720p, 30 fps" when that server caps screen sharing.
    pub fn screen_limit_text(&self) -> Option<String> {
        let (fps, h) = self.screen_caps();
        let mut parts = Vec::new();
        if h > 0 {
            parts.push(screen_height_label(h));
        }
        if fps > 0 {
            parts.push(format!("{fps} fps"));
        }
        (!parts.is_empty()).then(|| format!("Server limit: {}", parts.join(", ")))
    }

    /// The line describing the last "Test max" (running, failed or its result).
    pub fn probe_text(&self) -> Option<String> {
        if self.probe_running {
            return Some(PROBE_RUNNING.to_string());
        }
        if let Some(e) = &self.probe_error {
            return Some(format!("Test max failed: {e}"));
        }
        self.settings.screen_tested.as_ref().map(|r| {
            format!(
                "Tested (local encode limit): {} @ {} fps",
                screen_height_label(r.height),
                r.fps
            )
        })
    }

    /// A "Test max" finished: apply and save its result (the user overrides it by
    /// picking other values in the Settings window).
    pub fn on_probe_result(&mut self, res: Result<ProbeResult, String>) {
        self.probe_running = false;
        match res {
            Ok(r) => {
                self.probe_error = None;
                self.settings.screen_fps = r.fps;
                self.settings.screen_height = r.height;
                self.settings.screen_tested = Some(r);
                self.persist_settings();
            }
            Err(e) => self.probe_error = Some(e),
        }
    }

    /// Where my stream sound goes: my sharing server, while stream sound is on and
    /// someone watches.
    pub fn stream_target(&self) -> Option<ServerKey> {
        if !self.settings.screen_audio {
            return None;
        }
        self.wants_capture()
            .filter(|k| self.sessions.get(k).is_some_and(|s| s.watchers > 0))
    }

    /// The system-audio loopback capture should be running.
    pub fn wants_loopback(&self) -> bool {
        self.stream_target().is_some()
    }

    /// Loopback capture could not start: turn stream sound off and say why. It is
    /// not retried until the user turns it on again.
    pub fn loopback_failed(&mut self, e: &str) {
        let text = format!("{STREAM_SOUND_UNAVAILABLE}: {e}");
        self.settings.screen_audio = false;
        self.persist_settings();
        self.toast(format!("Error: {text}"));
        self.stream_sound_error = Some(text);
    }

    /// Is `nick`'s voice muted for me on `key` ("Mute for me")?
    pub fn user_is_muted(&self, key: ServerKey, nick: &str) -> bool {
        self.user_muted.contains(&(key, nick.to_string()))
    }

    /// Is `user`'s stream sound muted for me on `key`?
    pub fn stream_is_muted(&self, key: ServerKey, user: SessionId) -> bool {
        self.stream_muted.contains(&(key, user))
    }

    /// A screen frame arrived from the network.
    pub fn on_screen_frame(&mut self, key: ServerKey, from: SessionId, image: Arc<ColorImage>) {
        if let Some(s) = self.sessions.get_mut(&key)
            && s.status == ConnStatus::Connected
        {
            s.on_screen_frame(from, image);
        }
    }

    /// A chat image arrived: add its line, with the decoded pixels if the network
    /// inbox still had them. Ignored once the connection is closed.
    #[allow(clippy::too_many_arguments)]
    pub fn on_image(
        &mut self,
        key: ServerKey,
        from: SessionId,
        channel: ChannelId,
        ts_ms: u64,
        w: u32,
        h: u32,
        image: Option<Arc<ColorImage>>,
    ) {
        let Some(s) = self.sessions.get_mut(&key) else {
            return;
        };
        if s.status == ConnStatus::Disconnected {
            return; // late image from a closed connection
        }
        let id = self.next_image_id;
        self.next_image_id += 1;
        s.on_image(id, from, channel, ts_ms, w, h, image);
    }

    /// Stop sharing wherever it is on (tell that server).
    pub fn stop_screen_share(&mut self) -> Vec<NetCmd> {
        match self.screen_share.take() {
            Some(k) => self.session_intent(k, UiAction::SetScreenShare(false)),
            None => vec![],
        }
    }

    /// Screen capture failed: toast and turn sharing off.
    pub fn capture_failed(&mut self, e: &str) -> Vec<NetCmd> {
        self.toast(format!("Error: Screen capture failed: {e}"));
        self.stop_screen_share()
    }

    fn toast(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self.ui.toasts.contains(&text) {
            return;
        }
        self.ui.toasts.push(text);
        if self.ui.toasts.len() > 5 {
            self.ui.toasts.remove(0);
        }
    }

    fn persist_servers(&mut self) {
        if let Err(e) = self.profile.save_servers(&self.saved) {
            self.toast(format!("Error: could not save servers: {e}"));
        }
    }

    fn persist_settings(&mut self) {
        if let Err(e) = self.profile.save_settings(&self.settings) {
            self.toast(format!("Error: could not save settings: {e}"));
        }
    }

    fn connect_cmd(&mut self, key: ServerKey) -> Vec<NetCmd> {
        let Some(s) = self.saved_server(key).cloned() else {
            return vec![];
        };
        let fingerprint = s.fingerprint.as_deref().and_then(tofu::from_hex);
        let sess = self.sessions.entry(key).or_default();
        sess.reset_connection();
        sess.status = ConnStatus::Connecting;
        vec![NetCmd::Connect {
            key,
            addr: s.addr,
            nick: s.nick,
            password: s.password,
            fingerprint,
        }]
    }

    fn disconnect_cmd(&mut self, key: ServerKey) -> Vec<NetCmd> {
        let active = self
            .sessions
            .get(&key)
            .is_some_and(|s| s.status.is_active());
        if let Some(s) = self.sessions.get_mut(&key) {
            s.status = ConnStatus::Disconnected;
            s.reset_connection();
        }
        if self.screen_share == Some(key) {
            self.screen_share = None;
        }
        self.fix_talk_target();
        if active {
            vec![NetCmd::Disconnect { key }]
        } else {
            vec![]
        }
    }

    fn fix_talk_target(&mut self) {
        let ok = self
            .talk_target
            .and_then(|k| self.sessions.get(&k))
            .is_some_and(|s| s.status == ConnStatus::Connected);
        if !ok {
            self.talk_target = self
                .sessions
                .iter()
                .find(|(_, s)| s.status == ConnStatus::Connected)
                .map(|(k, _)| *k);
        }
    }

    fn voice_state_msg(&self) -> ClientMsg {
        ClientMsg::SetVoiceState {
            muted: self.voice.muted,
            deafened: self.voice.deafened,
        }
    }

    fn sync_voice_flags(&mut self) -> Vec<NetCmd> {
        let msg = self.voice_state_msg();
        let mut out = Vec::new();
        for (k, s) in self.sessions.iter_mut() {
            s.muted = self.voice.muted;
            s.deafened = self.voice.deafened;
            s.muted_before_deafen = self.voice.muted_before_deafen;
            if s.status == ConnStatus::Connected {
                out.push(NetCmd::Send {
                    key: *k,
                    msg: msg.clone(),
                });
            }
        }
        out
    }

    fn session_intent(&mut self, key: ServerKey, a: UiAction) -> Vec<NetCmd> {
        let Some(s) = self.sessions.get_mut(&key) else {
            return vec![];
        };
        if s.status != ConnStatus::Connected {
            return vec![];
        }
        // Server errors already reached the toasts through `apply_net`; drop them first so
        // only the errors raised by this intent are toasted (a dismissed toast stays gone).
        s.errors.clear();
        let out: Vec<NetCmd> = s
            .intent(a)
            .into_iter()
            .map(|msg| NetCmd::Send { key, msg })
            .collect();
        let errors: Vec<String> = s.errors.drain(..).collect();
        for e in errors {
            self.toast(format!("Error: {e}"));
        }
        out
    }

    /// Fold one network event into the model. Returns follow-up commands.
    pub fn apply_net(&mut self, ev: NetEvent, now: Instant) -> Vec<NetCmd> {
        let out = self.apply_net_event(ev, now);
        self.prune_ui();
        out
    }

    fn apply_net_event(&mut self, ev: NetEvent, now: Instant) -> Vec<NetCmd> {
        let mut out = Vec::new();
        match ev {
            NetEvent::Status { key, status } => {
                let Some(s) = self.sessions.get_mut(&key) else {
                    return out;
                };
                if s.status == ConnStatus::Disconnected && status != ConnStatus::Disconnected {
                    // A late event from a connection the user already closed.
                    return out;
                }
                if status != ConnStatus::Connected {
                    s.reset_connection();
                    if self.screen_share == Some(key) {
                        self.screen_share = None;
                    }
                    self.stream_muted.retain(|(k, _)| *k != key);
                }
                let failed = match &status {
                    ConnStatus::Failed(t) => Some(format!("Error: {t}")),
                    _ => None,
                };
                s.status = status;
                self.fix_talk_target();
                if let Some(t) = failed {
                    // The server's Error message can be lost when it closes right after;
                    // the close code still yields the Failed status, so toast from it too.
                    self.toast(t);
                }
            }
            NetEvent::Msg { key, msg } => {
                let Some(s) = self.sessions.get_mut(&key) else {
                    return out;
                };
                if s.status == ConnStatus::Disconnected {
                    return out; // late message from a closed connection
                }
                let welcome = matches!(msg, ServerMsg::Welcome { .. });
                if let ServerMsg::Pong { nonce } = &msg {
                    // The nonce is the send time in ms (see `UiCommand::Ping`).
                    s.ping_ms = Some(unix_ms().saturating_sub(*nonce));
                }
                if let ServerMsg::ScreenShare { user, on: false } = &msg
                    && Some(*user) == s.my_id
                    && self.screen_share == Some(key)
                {
                    // The server ended my share (e.g. I moved channel).
                    self.screen_share = None;
                }
                if let ServerMsg::ScreenShare { user, on: false } = &msg {
                    self.stream_muted.remove(&(key, *user));
                }
                let toast = match &msg {
                    ServerMsg::Error { code, .. } => Some(format!("Error: {}", error_text(*code))),
                    _ => None,
                };
                s.apply(msg);
                if welcome {
                    s.muted = self.voice.muted;
                    s.deafened = self.voice.deafened;
                    if self.voice.muted || self.voice.deafened {
                        out.push(NetCmd::Send {
                            key,
                            msg: self.voice_state_msg(),
                        });
                    }
                    if self.talk_target.is_none() {
                        self.talk_target = Some(key);
                    }
                }
                if let Some(t) = toast {
                    self.toast(t);
                }
            }
            NetEvent::Voice { key, speaker, .. } => {
                // Stream sound is not speech: it never lights the speaking indicator.
                if speaker.0 & STREAM_BIT == 0
                    && let Some(s) = self.sessions.get_mut(&key)
                {
                    s.note_voice(speaker, now);
                }
            }
            NetEvent::Pinned { key, fp } => {
                if let Some(s) = self.saved.iter_mut().find(|s| s.key == key) {
                    s.fingerprint = Some(tofu::to_hex(&fp));
                    self.persist_servers();
                }
            }
            NetEvent::FingerprintChanged { key, seen } => {
                self.ui.tofu_prompt = Some((key, seen));
            }
            NetEvent::Closed { key, reason } => {
                tracing::info!("server {key} closed: {reason:?}");
                if self.screen_share == Some(key) {
                    self.screen_share = None;
                }
            }
            NetEvent::Image {
                key,
                from,
                channel,
                ts_ms,
                w,
                h,
                ..
            } => {
                // `VoiceApp` takes the pixels from the inbox and calls `on_image`
                // itself; here the line is added without them.
                self.on_image(key, from, channel, ts_ms, w, h, None);
            }
            NetEvent::Pong { key, rtt_ms } => {
                if let Some(s) = self.sessions.get_mut(&key)
                    && s.status != ConnStatus::Disconnected
                {
                    s.ping_ms = Some(rtt_ms);
                }
            }
            NetEvent::UploadFailed { key, error } => {
                tracing::info!("server {key}: upload failed: {error}");
                self.toast(format!("Error: {}", error.text()));
            }
        }
        out
    }

    fn next_key(&self) -> ServerKey {
        self.saved.iter().map(|s| s.key).max().unwrap_or(0) + 1
    }

    /// Drop UI state that no longer applies: an inline channel edit that is not
    /// shown any more, and the popped-out streams flag once nothing is watched.
    fn prune_ui(&mut self) {
        self.prune_channel_dialog();
        self.prune_streams_popped();
    }

    /// The inline channel edit is only drawn for the selected, connected server
    /// (and an existing channel when renaming). Otherwise it would stay open
    /// unseen and keep blocking pastes and shortcuts.
    fn prune_channel_dialog(&mut self) {
        let Some(d) = &self.ui.channel_dialog else {
            return;
        };
        let session = self
            .sessions
            .get(&d.key)
            .filter(|s| s.status == ConnStatus::Connected);
        let stale = Some(d.key) != self.selected
            || match (session, d.mode) {
                (None, _) => true,
                (Some(s), ChannelDialogMode::Rename(ch)) => !s.channels.contains_key(&ch),
                (Some(_), ChannelDialogMode::New) => false,
            };
        if stale {
            self.ui.channel_dialog = None;
        }
    }

    /// Streams go back to the main stage once nothing is watched, so the next
    /// watched stream never opens in a window by itself.
    fn prune_streams_popped(&mut self) {
        if self.sessions.values().all(|s| s.watching.is_empty()) {
            self.ui.streams_popped = false;
        }
    }

    /// Submit a channel edit (create or rename) to its server.
    /// An unchanged rename (or one of a channel that is gone) is a cancel.
    fn submit_channel_dialog(&mut self, d: ChannelDialog) -> Vec<NetCmd> {
        if let ChannelDialogMode::Rename(ch) = d.mode {
            let current = self
                .sessions
                .get(&d.key)
                .and_then(|s| s.channels.get(&ch))
                .map(|c| c.name.clone());
            match current {
                Some(c) if c.trim() != d.name.trim() => {}
                _ => return vec![],
            }
        }
        let a = match d.mode {
            ChannelDialogMode::New => UiAction::Create(d.name),
            ChannelDialogMode::Rename(ch) => UiAction::Rename(ch, d.name),
        };
        self.session_intent(d.key, a)
    }

    /// Before opening the edit `(key, mode)`: an open, different edit with a name
    /// typed is submitted (its blur arrives in the same frame, too late); a blank
    /// one is just replaced.
    fn flush_channel_dialog(&mut self, key: ServerKey, mode: ChannelDialogMode) -> Vec<NetCmd> {
        let other_with_text = self
            .ui
            .channel_dialog
            .as_ref()
            .is_some_and(|d| (d.key, d.mode) != (key, mode) && !d.name.trim().is_empty());
        match self.ui.channel_dialog.take() {
            Some(d) if other_with_text => self.submit_channel_dialog(d),
            _ => vec![],
        }
    }

    fn channel_dialog_is(&self, key: ServerKey, mode: ChannelDialogMode) -> bool {
        self.ui
            .channel_dialog
            .as_ref()
            .is_some_and(|d| d.key == key && d.mode == mode)
    }

    /// Execute one UI command. Returns the network commands to send.
    pub fn handle(&mut self, cmd: UiCommand) -> Vec<NetCmd> {
        let out = self.handle_cmd(cmd);
        self.prune_ui();
        out
    }

    fn handle_cmd(&mut self, cmd: UiCommand) -> Vec<NetCmd> {
        match cmd {
            UiCommand::Select(key) => {
                if self.saved_server(key).is_some() {
                    self.selected = Some(key);
                }
                vec![]
            }
            UiCommand::Connect(key) => self.connect_cmd(key),
            UiCommand::Disconnect(key) => self.disconnect_cmd(key),
            UiCommand::TalkHere(key) => {
                if self.status(key) == ConnStatus::Connected {
                    self.talk_target = Some(key);
                }
                vec![]
            }
            UiCommand::OpenAddServer => {
                let nick = if self.settings.nickname.trim().is_empty() {
                    DEFAULT_NICK.to_string()
                } else {
                    self.settings.nickname.clone()
                };
                self.ui.server_dialog = Some(ServerDialog {
                    editing: None,
                    label: String::new(),
                    addr: "127.0.0.1:7878".into(),
                    nick,
                    password: String::new(),
                    error: None,
                    connect_now: true,
                });
                vec![]
            }
            UiCommand::OpenEditServer(key) => {
                if let Some(s) = self.saved_server(key) {
                    self.ui.server_dialog = Some(ServerDialog {
                        editing: Some(key),
                        label: s.label.clone(),
                        addr: s.addr.clone(),
                        nick: s.nick.clone(),
                        password: s.password.clone().unwrap_or_default(),
                        error: None,
                        connect_now: false,
                    });
                }
                vec![]
            }
            UiCommand::CancelServerDialog => {
                self.ui.server_dialog = None;
                vec![]
            }
            UiCommand::SaveServerDialog => self.save_server_dialog(),
            UiCommand::RemoveServer(key) => {
                let out = self.disconnect_cmd(key);
                self.saved.retain(|s| s.key != key);
                self.sessions.remove(&key);
                if self.selected == Some(key) {
                    self.selected = self.saved.first().map(|s| s.key);
                }
                self.fix_talk_target();
                self.persist_servers();
                out
            }
            UiCommand::Join(ch) => match self.selected {
                Some(k) => self.session_intent(k, UiAction::Join(ch)),
                None => vec![],
            },
            UiCommand::OpenNewChannel => {
                let Some(k) = self.selected else {
                    return vec![];
                };
                let mode = ChannelDialogMode::New;
                if self.channel_dialog_is(k, mode) {
                    return vec![];
                }
                let out = self.flush_channel_dialog(k, mode);
                self.ui.channel_dialog = Some(ChannelDialog {
                    key: k,
                    mode,
                    name: String::new(),
                });
                out
            }
            UiCommand::OpenRenameChannel(ch) => {
                let Some(k) = self.selected else {
                    return vec![];
                };
                let mode = ChannelDialogMode::Rename(ch);
                if self.channel_dialog_is(k, mode) {
                    return vec![];
                }
                let out = self.flush_channel_dialog(k, mode);
                let name = self
                    .sessions
                    .get(&k)
                    .and_then(|s| s.channels.get(&ch))
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                self.ui.channel_dialog = Some(ChannelDialog { key: k, mode, name });
                out
            }
            UiCommand::CancelChannelDialog { key, mode } => {
                if self.channel_dialog_is(key, mode) {
                    self.ui.channel_dialog = None;
                }
                vec![]
            }
            UiCommand::SubmitChannelDialog { key, mode } => {
                if !self.channel_dialog_is(key, mode) {
                    return vec![];
                }
                let d = self.ui.channel_dialog.take().expect("checked");
                self.submit_channel_dialog(d)
            }
            UiCommand::DeleteChannel(ch) => match self.selected {
                Some(k) => self.session_intent(k, UiAction::Delete(ch)),
                None => vec![],
            },
            UiCommand::SendChat => {
                let Some(k) = self.selected else {
                    return vec![];
                };
                let text = std::mem::take(&mut self.ui.chat_input);
                let out = self.session_intent(k, UiAction::Chat(text.clone()));
                if out.is_empty() {
                    // Offline or rejected (e.g. too long): keep the draft.
                    self.ui.chat_input = text;
                }
                out
            }
            UiCommand::ToggleMute => {
                let mut tmp = ClientSession {
                    muted: self.voice.muted,
                    deafened: self.voice.deafened,
                    muted_before_deafen: self.voice.muted_before_deafen,
                    ..ClientSession::default()
                };
                tmp.intent(UiAction::ToggleMute);
                self.voice = VoiceFlags {
                    muted: tmp.muted,
                    deafened: tmp.deafened,
                    muted_before_deafen: tmp.muted_before_deafen,
                };
                self.sync_voice_flags()
            }
            UiCommand::ToggleDeafen => {
                let mut tmp = ClientSession {
                    muted: self.voice.muted,
                    deafened: self.voice.deafened,
                    muted_before_deafen: self.voice.muted_before_deafen,
                    ..ClientSession::default()
                };
                tmp.intent(UiAction::ToggleDeafen);
                self.voice = VoiceFlags {
                    muted: tmp.muted,
                    deafened: tmp.deafened,
                    muted_before_deafen: tmp.muted_before_deafen,
                };
                self.sync_voice_flags()
            }
            UiCommand::SetInputMode(m) => {
                if self.settings.input_mode != m {
                    self.settings.input_mode = m;
                    self.persist_settings();
                }
                vec![]
            }
            UiCommand::SetUserVolume { key, nick, volume } => {
                self.settings.set_user_volume(key, &nick, volume);
                self.persist_settings();
                vec![]
            }
            UiCommand::SetStreamVolume { key, nick, volume } => {
                self.settings.set_stream_volume(key, &nick, volume);
                self.persist_settings();
                vec![]
            }
            UiCommand::OpenSettings => {
                self.ui.settings_tab = Some(SettingsTab::Voice);
                self.ui.nick_edit = self.settings.nickname.clone();
                self.ui.ptt_listening = false;
                vec![]
            }
            UiCommand::SetSettingsTab(t) => {
                if self.ui.settings_tab.is_some() {
                    self.ui.settings_tab = Some(t);
                }
                // "Press a key…" belongs to the Voice tab: never keep listening unseen.
                self.ui.ptt_listening = false;
                vec![]
            }
            UiCommand::SetPttListening(on) => {
                self.ui.ptt_listening = on;
                vec![]
            }
            UiCommand::UpdateSettings { settings, persist } => {
                let mut d = *settings;
                if d.input_mode != self.settings.input_mode || d.input_mode != InputMode::Ptt {
                    self.ui.ptt_listening = false;
                }
                if d.input_device != self.settings.input_device
                    || d.output_device != self.settings.output_device
                {
                    self.audio_restart_needed = true;
                }
                // Not owned by the window: volumes change through the user menu / stream
                // tile, stream
                // sound through `SetStreamSound`, the nickname through `CommitNickname`,
                // the tested limit through "Test max".
                d.user_volumes = self.settings.user_volumes.clone();
                d.stream_volumes = self.settings.stream_volumes.clone();
                d.screen_audio = self.settings.screen_audio;
                d.nickname = self.settings.nickname.clone();
                d.screen_tested = self.settings.screen_tested.clone();
                self.settings = d;
                if persist {
                    self.persist_settings();
                }
                vec![]
            }
            UiCommand::CommitNickname => {
                self.commit_nickname();
                vec![]
            }
            UiCommand::CloseSettings => {
                self.commit_nickname();
                // Safety net: a drag that never reported its end is saved now.
                self.persist_settings();
                self.ui.settings_tab = None;
                self.ui.ptt_listening = false;
                vec![]
            }
            UiCommand::TrustNewIdentity => {
                let Some((key, fp)) = self.ui.tofu_prompt.take() else {
                    return vec![];
                };
                if let Some(s) = self.saved.iter_mut().find(|s| s.key == key) {
                    s.fingerprint = Some(tofu::to_hex(&fp));
                }
                self.persist_servers();
                self.connect_cmd(key)
            }
            UiCommand::CancelTofu => {
                self.ui.tofu_prompt = None;
                vec![]
            }
            UiCommand::DismissToast(i) => {
                if i < self.ui.toasts.len() {
                    self.ui.toasts.remove(i);
                }
                vec![]
            }
            UiCommand::Ping(key) => self.session_intent(key, UiAction::Ping(unix_ms())),
            UiCommand::UploadImage(source) => {
                let Some(s) = self
                    .selected
                    .and_then(|k| self.sessions.get(&k).map(|s| (k, s)))
                else {
                    return vec![];
                };
                let (key, s) = s;
                if s.status != ConnStatus::Connected {
                    self.toast("Error: Not connected");
                    return vec![];
                }
                vec![NetCmd::SendImage {
                    key,
                    source,
                    cap: s.image_cap(),
                }]
            }
            UiCommand::ToggleScreenShare => {
                // A running share is always stopped, whichever server is selected;
                // the toggle never moves a share to another server.
                if self.screen_share.is_some() {
                    return self.stop_screen_share();
                }
                match self.selected {
                    Some(key) => self.start_screen_share(key),
                    None => vec![],
                }
            }
            UiCommand::ShareScreenOn(key) => {
                if self.screen_share.is_some() {
                    return self.stop_screen_share();
                }
                self.start_screen_share(key)
            }
            UiCommand::MuteUser { key, nick, muted } => {
                if muted {
                    self.user_muted.insert((key, nick));
                } else {
                    self.user_muted.remove(&(key, nick));
                }
                vec![]
            }
            UiCommand::SetStreamsPopped(on) => {
                self.ui.streams_popped = on;
                vec![]
            }
            UiCommand::ViewImage(id) => {
                self.ui.viewing.insert(id);
                vec![]
            }
            UiCommand::CloseImage(id) => {
                self.ui.viewing.remove(&id);
                vec![]
            }
            UiCommand::WatchScreen { key, user, on } => {
                self.session_intent(key, UiAction::Watch(user, on))
            }
            UiCommand::TestScreenMax => {
                if !self.probe_running {
                    self.probe_running = true;
                    self.probe_requested = true;
                    self.probe_error = None;
                }
                vec![]
            }
            UiCommand::SetStreamSound { on } => {
                self.settings.screen_audio = on;
                self.stream_sound_error = None;
                self.persist_settings();
                vec![]
            }
            UiCommand::MuteStream { key, user, muted } => {
                if muted {
                    self.stream_muted.insert((key, user));
                } else {
                    self.stream_muted.remove(&(key, user));
                }
                vec![]
            }
        }
    }

    /// Start sharing on `key` if it is connected.
    fn start_screen_share(&mut self, key: ServerKey) -> Vec<NetCmd> {
        if self.status(key) != ConnStatus::Connected {
            return vec![];
        }
        self.screen_share = Some(key);
        self.session_intent(key, UiAction::SetScreenShare(true))
    }

    /// Apply the Settings window's nickname field as the default nickname. Servers
    /// still on the old default nick (or none) follow; a nick set in a server's own
    /// dialog is kept. No-op when empty or unchanged.
    fn commit_nickname(&mut self) {
        let nick = self.ui.nick_edit.trim().to_string();
        if nick.is_empty() || nick == self.settings.nickname {
            return;
        }
        // On a fresh profile the Add dialog's default nick is the old default.
        let old = if self.settings.nickname.trim().is_empty() {
            DEFAULT_NICK.to_string()
        } else {
            self.settings.nickname.clone()
        };
        let mut changed = false;
        for s in &mut self.saved {
            if s.nick.trim().is_empty() || s.nick == old {
                s.nick = nick.clone();
                changed = true;
            }
        }
        if changed {
            self.persist_servers();
        }
        self.settings.nickname = nick;
        self.persist_settings();
    }

    fn save_server_dialog(&mut self) -> Vec<NetCmd> {
        let Some(d) = self.ui.server_dialog.as_mut() else {
            return vec![];
        };
        let nick = d.nick.trim().to_string();
        if validate_addr(&d.addr).is_err() {
            d.error = Some("Invalid address".into());
            return vec![];
        }
        let label = match d.label.trim() {
            "" => derive_label(&d.addr, &self.saved, d.editing),
            l => l.to_string(),
        };
        let n = nick.chars().count();
        if n == 0 || n > vc_proto::MAX_NICK {
            d.error = Some("Nickname must be 1-32 characters".into());
            return vec![];
        }
        let d = self.ui.server_dialog.take().expect("checked");
        let password = if d.password.is_empty() {
            None
        } else {
            Some(d.password.clone())
        };
        let mut out = Vec::new();
        match d.editing {
            Some(key) => {
                out.extend(self.disconnect_cmd(key));
                if let Some(s) = self.saved.iter_mut().find(|s| s.key == key) {
                    if s.addr.trim() != d.addr.trim() {
                        s.fingerprint = None; // different server: pin afresh
                    }
                    s.label = label;
                    s.addr = d.addr.trim().to_string();
                    s.nick = nick;
                    s.password = password;
                }
            }
            None => {
                let key = self.next_key();
                self.saved.push(SavedServer {
                    key,
                    label,
                    addr: d.addr.trim().to_string(),
                    nick,
                    password,
                    fingerprint: None,
                });
                self.sessions.insert(key, ClientSession::new());
                self.selected = Some(key);
                self.persist_servers();
                if d.connect_now {
                    out.extend(self.connect_cmd(key));
                }
                return out;
            }
        }
        self.persist_servers();
        out
    }

    /// Control block for the audio pipeline.
    pub fn pipeline_ctl(&self) -> PipelineCtl {
        let mut gains = std::collections::HashMap::new();
        for (k, s) in &self.sessions {
            for u in s.users.values() {
                // Stream sound first: a voice-muted user's stream keeps its volume.
                let sg = self.settings.stream_volume(*k, &u.nick);
                if sg != 1.0 {
                    gains.insert((*k, stream_speaker(u.id)), sg);
                }
                if self.user_is_muted(*k, &u.nick) {
                    gains.insert((*k, u.id), 0.0);
                    continue;
                }
                let g = self.settings.user_volume(*k, &u.nick);
                if g != 1.0 {
                    gains.insert((*k, u.id), g);
                }
            }
        }
        for (k, user) in &self.stream_muted {
            gains.insert((*k, stream_speaker(*user)), 0.0);
        }
        let voice_tuning = self
            .sessions
            .iter()
            .filter(|(_, s)| s.status == ConnStatus::Connected)
            .map(|(k, s)| {
                let t = StreamTuning {
                    jitter_depth: s.voice_tuning.jitter_frames as usize,
                    playback_keep_frames: s.voice_tuning.playback_keep_frames as usize,
                };
                (*k, t)
            })
            .collect();
        PipelineCtl {
            talk_target: self.talk_target,
            stream_target: self.stream_target(),
            muted: self.voice.muted,
            deafened: self.voice.deafened,
            mode: self.settings.input_mode,
            ptt_down: self.ptt_down,
            vad_threshold_db: self.settings.vad_threshold_db,
            master: self.settings.master_volume,
            gains,
            voice_tuning,
        }
    }
}

/// A server label from its address when none was given: the host (brackets
/// trimmed), or the full address when another server already has that label.
fn derive_label(addr: &str, saved: &[SavedServer], editing: Option<ServerKey>) -> String {
    let addr = addr.trim();
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let taken = saved
        .iter()
        .any(|s| Some(s.key) != editing && s.label == host);
    if host.is_empty() || taken {
        addr.to_string()
    } else {
        host.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vc_proto::{ChannelInfo, SessionId, UserInfo};

    fn model() -> (AppModel, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let mut m = AppModel::load(Profile::new(d.path()));
        for (label, addr) in [("A", "127.0.0.1:1"), ("B", "127.0.0.1:2")] {
            m.handle(UiCommand::OpenAddServer);
            let dlg = m.ui.server_dialog.as_mut().unwrap();
            dlg.label = label.into();
            dlg.addr = addr.into();
            dlg.connect_now = false;
            assert!(m.handle(UiCommand::SaveServerDialog).is_empty());
        }
        (m, d)
    }

    fn welcome(me: u32, users: &[(u32, &str, u32)]) -> ServerMsg {
        welcome_caps(me, users, 0, 0)
    }

    fn welcome_caps(me: u32, users: &[(u32, &str, u32)], fps: u32, height: u32) -> ServerMsg {
        ServerMsg::Welcome {
            session_id: SessionId(me),
            server_name: "S".into(),
            motd: "m".into(),
            default_channel: ChannelId(1),
            channels: vec![
                ChannelInfo {
                    id: ChannelId(1),
                    name: "Lobby".into(),
                    permanent: true,
                },
                ChannelInfo {
                    id: ChannelId(2),
                    name: "AFK".into(),
                    permanent: true,
                },
            ],
            users: users
                .iter()
                .map(|(id, n, c)| UserInfo {
                    id: SessionId(*id),
                    nick: (*n).into(),
                    channel: ChannelId(*c),
                    muted: false,
                    deafened: false,
                    sharing: false,
                })
                .collect(),
            max_image_bytes: 1_000_000,
            max_screen_fps: fps,
            max_screen_height: height,
            voice_tuning: vc_proto::VoiceTuning::default(),
        }
    }

    fn connect(m: &mut AppModel, key: ServerKey, w: ServerMsg) {
        let now = Instant::now();
        m.handle(UiCommand::Connect(key));
        m.apply_net(NetEvent::Msg { key, msg: w }, now);
        m.apply_net(
            NetEvent::Status {
                key,
                status: ConnStatus::Connected,
            },
            now,
        );
    }

    #[test]
    fn pipeline_ctl_carries_voice_tuning_of_connected_servers() {
        let (mut m, _d) = model();
        // A third server that stays disconnected.
        m.handle(UiCommand::OpenAddServer);
        let dlg = m.ui.server_dialog.as_mut().unwrap();
        dlg.label = "C".into();
        dlg.addr = "127.0.0.1:3".into();
        dlg.connect_now = false;
        assert!(m.handle(UiCommand::SaveServerDialog).is_empty());
        assert_eq!(m.sessions.len(), 3);
        let mut w = welcome(1, &[(1, "me", 1)]);
        if let ServerMsg::Welcome { voice_tuning, .. } = &mut w {
            *voice_tuning = vc_proto::VoiceTuning {
                preset: "competitive".into(),
                jitter_frames: 2,
                playback_keep_frames: 2,
            };
        }
        connect(&mut m, 1, w);
        connect(&mut m, 2, welcome(1, &[(1, "me", 1)]));
        let ctl = m.pipeline_ctl();
        assert_eq!(ctl.voice_tuning.len(), 2, "{:?}", ctl.voice_tuning);
        assert_eq!(ctl.voice_tuning[&1].jitter_depth, 2);
        assert_eq!(ctl.voice_tuning[&1].playback_keep_frames, 2);
        assert_eq!(ctl.voice_tuning[&2].jitter_depth, 3);
        assert_eq!(ctl.voice_tuning[&2].playback_keep_frames, 3);
        // Disconnecting drops the server from the ctl.
        m.handle(UiCommand::Disconnect(1));
        assert!(!m.pipeline_ctl().voice_tuning.contains_key(&1));
    }

    #[test]
    fn app_multiserver_independent_state() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1), (2, "bob", 1)]));
        connect(&mut m, 2, welcome(1, &[(1, "me", 2)]));
        assert_eq!(m.sessions[&1].users.len(), 2);
        assert_eq!(m.sessions[&2].users.len(), 1);
        // A move on server 2 does not touch server 1 even with the same session id.
        m.apply_net(
            NetEvent::Msg {
                key: 2,
                msg: ServerMsg::UserMoved {
                    user: SessionId(1),
                    channel: ChannelId(1),
                },
            },
            Instant::now(),
        );
        assert_eq!(m.sessions[&2].my_channel(), Some(ChannelId(1)));
        assert_eq!(m.sessions[&1].my_channel(), Some(ChannelId(1)));
        m.apply_net(
            NetEvent::Msg {
                key: 1,
                msg: ServerMsg::UserLeft { user: SessionId(2) },
            },
            Instant::now(),
        );
        assert_eq!(m.sessions[&1].users.len(), 1);
        assert_eq!(m.sessions[&2].users.len(), 1);
        // Disconnecting one leaves the other connected.
        let cmds = m.handle(UiCommand::Disconnect(1));
        assert_eq!(cmds, vec![NetCmd::Disconnect { key: 1 }]);
        assert_eq!(m.status(1), ConnStatus::Disconnected);
        assert_eq!(m.status(2), ConnStatus::Connected);
        // Join goes to the selected server only.
        m.handle(UiCommand::Select(2));
        let cmds = m.handle(UiCommand::Join(ChannelId(2)));
        assert_eq!(
            cmds,
            vec![NetCmd::Send {
                key: 2,
                msg: ClientMsg::Join {
                    channel: ChannelId(2)
                }
            }]
        );
    }

    #[test]
    fn talk_target_defaults_to_first_connected() {
        let (mut m, _d) = model();
        assert_eq!(m.talk_target, None);
        connect(&mut m, 2, welcome(1, &[(1, "me", 1)]));
        assert_eq!(m.talk_target, Some(2));
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        assert_eq!(m.talk_target, Some(2), "first connected stays");
        m.handle(UiCommand::TalkHere(1));
        assert_eq!(m.talk_target, Some(1));
        assert_eq!(m.pipeline_ctl().talk_target, Some(1));
        // Target disconnects → falls back to the other connected server.
        m.handle(UiCommand::Disconnect(1));
        assert_eq!(m.talk_target, Some(2));
        // Talk here on a disconnected server is ignored.
        m.handle(UiCommand::TalkHere(1));
        assert_eq!(m.talk_target, Some(2));
        m.apply_net(
            NetEvent::Status {
                key: 2,
                status: ConnStatus::Reconnecting { attempt: 1 },
            },
            Instant::now(),
        );
        assert_eq!(m.talk_target, None);
    }

    #[test]
    fn edit_or_remove_connected_server_disconnects_first() {
        let (mut m, d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        m.handle(UiCommand::OpenEditServer(1));
        m.ui.server_dialog.as_mut().unwrap().label = "A2".into();
        let cmds = m.handle(UiCommand::SaveServerDialog);
        assert_eq!(cmds, vec![NetCmd::Disconnect { key: 1 }]);
        assert_eq!(m.saved_server(1).unwrap().label, "A2");
        assert_eq!(m.status(1), ConnStatus::Disconnected);
        connect(&mut m, 2, welcome(1, &[(1, "me", 1)]));
        let cmds = m.handle(UiCommand::RemoveServer(2));
        assert_eq!(cmds, vec![NetCmd::Disconnect { key: 2 }]);
        assert!(m.saved_server(2).is_none());
        assert!(!m.sessions.contains_key(&2));
        // Removing a disconnected server sends nothing.
        assert!(m.handle(UiCommand::RemoveServer(1)).is_empty());
        assert!(Profile::new(d.path()).load_servers().unwrap().is_empty());
    }

    #[test]
    fn intent_errors_become_toasts() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        // A server error already toasted and dismissed must not come back.
        m.apply_net(
            NetEvent::Msg {
                key: 1,
                msg: ServerMsg::Error {
                    code: vc_proto::ErrorCode::NotAllowed,
                    msg: String::new(),
                },
            },
            Instant::now(),
        );
        assert_eq!(m.ui.toasts, vec!["Error: Not allowed".to_string()]);
        m.handle(UiCommand::DismissToast(0));
        m.handle(UiCommand::Select(1));
        // Empty channel name: no command, a BadChannelName toast (round-2 Fix 3).
        m.handle(UiCommand::OpenNewChannel);
        assert!(
            m.handle(UiCommand::SubmitChannelDialog {
                key: 1,
                mode: ChannelDialogMode::New
            })
            .is_empty()
        );
        assert_eq!(m.ui.toasts, vec!["Error: Invalid channel name".to_string()]);
        m.handle(UiCommand::DismissToast(0));
        // Over-long chat: no command, a toast, and the draft is kept.
        m.ui.chat_input = "x".repeat(501);
        assert!(m.handle(UiCommand::SendChat).is_empty());
        assert_eq!(m.ui.toasts, vec!["Error: Message too long".to_string()]);
        assert_eq!(m.ui.chat_input.chars().count(), 501);
        assert!(m.sessions[&1].errors.is_empty(), "errors moved, not copied");
        // A valid chat clears the draft.
        m.ui.chat_input = "ok".into();
        assert_eq!(m.handle(UiCommand::SendChat).len(), 1);
        assert_eq!(m.ui.chat_input, "");
    }

    #[test]
    fn settings_nickname_updates_only_default_nicks() {
        // Case B: fresh install, empty Settings nickname, a server kept the dialog's DEFAULT_NICK.
        let d = tempfile::tempdir().unwrap();
        let mut m = AppModel::load(Profile::new(d.path()));
        assert_eq!(m.settings.nickname, "");
        m.handle(UiCommand::OpenAddServer);
        assert_eq!(m.ui.server_dialog.as_ref().unwrap().nick, DEFAULT_NICK);
        let dlg = m.ui.server_dialog.as_mut().unwrap();
        dlg.label = "A".into();
        dlg.connect_now = false;
        m.handle(UiCommand::SaveServerDialog);
        m.handle(UiCommand::OpenAddServer);
        let dlg = m.ui.server_dialog.as_mut().unwrap();
        dlg.label = "B".into();
        dlg.nick = "custom".into();
        dlg.connect_now = false;
        m.handle(UiCommand::SaveServerDialog);
        m.handle(UiCommand::OpenSettings);
        m.ui.nick_edit = "new".into();
        m.handle(UiCommand::CommitNickname);
        let nicks: Vec<_> = m.saved.iter().map(|s| s.nick.as_str()).collect();
        assert_eq!(nicks, ["new", "custom"]);
        let on_disk: Vec<_> = Profile::new(d.path())
            .load_servers()
            .unwrap()
            .into_iter()
            .map(|s| s.nick)
            .collect();
        assert_eq!(on_disk, ["new", "custom"], "servers.toml rewritten");

        // Case A: Settings nickname "old".
        let d = tempfile::tempdir().unwrap();
        let profile = Profile::new(d.path());
        profile
            .save_settings(&Settings {
                nickname: "old".into(),
                ..Settings::default()
            })
            .unwrap();
        let mut m = AppModel::load(profile);
        for (label, nick) in [("A", "old"), ("B", "custom"), ("C", "")] {
            m.saved.push(SavedServer {
                key: m.next_key(),
                label: label.into(),
                addr: "127.0.0.1:1".into(),
                nick: nick.into(),
                password: None,
                fingerprint: None,
            });
        }
        m.handle(UiCommand::OpenSettings);
        m.ui.nick_edit = " new ".into();
        // Closing the window commits the field (no separate Save).
        m.handle(UiCommand::CloseSettings);
        assert_eq!(m.ui.settings_tab, None);
        let nicks: Vec<_> = m.saved.iter().map(|s| s.nick.as_str()).collect();
        assert_eq!(nicks, ["new", "custom", "new"]);
        let on_disk = Profile::new(d.path()).load_servers().unwrap();
        let nicks: Vec<_> = on_disk.iter().map(|s| s.nick.clone()).collect();
        assert_eq!(nicks, ["new", "custom", "new"], "servers.toml rewritten");
        assert_eq!(
            Profile::new(d.path()).load_settings().unwrap().nickname,
            "new"
        );
    }

    #[test]
    fn chat_view_filters_by_server_and_channel() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        connect(&mut m, 2, welcome(1, &[(1, "me", 1)]));
        let line = |ch: u32, text: &str| ServerMsg::Chat {
            from: SessionId(1),
            channel: ChannelId(ch),
            text: text.into(),
            ts_ms: 0,
        };
        let now = Instant::now();
        m.apply_net(
            NetEvent::Msg {
                key: 1,
                msg: line(1, "s1-lobby"),
            },
            now,
        );
        m.apply_net(
            NetEvent::Msg {
                key: 1,
                msg: line(2, "s1-afk"),
            },
            now,
        );
        m.apply_net(
            NetEvent::Msg {
                key: 2,
                msg: line(1, "s2-lobby"),
            },
            now,
        );
        let texts = |m: &AppModel| {
            m.chat_view()
                .iter()
                .map(|l| l.text.clone())
                .collect::<Vec<_>>()
        };
        m.handle(UiCommand::Select(1));
        assert_eq!(texts(&m), ["s1-lobby"]);
        m.handle(UiCommand::Select(2));
        assert_eq!(texts(&m), ["s2-lobby"]);
        m.handle(UiCommand::Select(1));
        m.apply_net(
            NetEvent::Msg {
                key: 1,
                msg: ServerMsg::UserMoved {
                    user: SessionId(1),
                    channel: ChannelId(2),
                },
            },
            now,
        );
        assert_eq!(texts(&m), ["s1-afk"]);
    }

    #[test]
    fn upload_offline_toasts_and_sends_nothing() {
        let (mut m, _d) = model();
        m.handle(UiCommand::Select(1));
        let src = UploadSource::File("x.png".into());
        assert!(m.handle(UiCommand::UploadImage(src)).is_empty());
        assert_eq!(m.ui.toasts, vec!["Error: Not connected".to_string()]);
    }

    #[test]
    fn upload_cap_comes_from_welcome() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        m.handle(UiCommand::Select(1));
        let src = UploadSource::File("x.png".into());
        assert_eq!(
            m.handle(UiCommand::UploadImage(src.clone())),
            vec![NetCmd::SendImage {
                key: 1,
                source: src,
                cap: 1_000_000
            }]
        );
    }

    #[test]
    fn received_images_get_unique_ids_and_upload_errors_toast() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1), (2, "bob", 1)]));
        connect(&mut m, 2, welcome(1, &[(1, "me", 1)]));
        m.handle(UiCommand::Select(1));
        let image = Arc::new(ColorImage::filled([2, 2], egui::Color32::RED));
        for key in [1, 2, 1] {
            m.on_image(
                key,
                SessionId(2),
                ChannelId(1),
                0,
                2,
                2,
                Some(image.clone()),
            );
        }
        let ids: Vec<u64> = m.chat_view().iter().map(|l| l.image.unwrap().id).collect();
        assert_eq!(ids, [1, 3]);
        assert_eq!(m.chat_view()[0].from_nick, "bob");
        assert!(m.sessions[&1].images.contains_key(&1));
        // A pixel-free event (inbox already evicted) still adds the line, expired.
        m.apply_net(
            NetEvent::Image {
                key: 1,
                from: SessionId(2),
                channel: ChannelId(1),
                ts_ms: 0,
                seq: 99,
                w: 640,
                h: 480,
            },
            Instant::now(),
        );
        let last = m.chat_view().last().unwrap().image.unwrap();
        assert_eq!((last.id, last.w, last.h), (4, 640, 480));
        assert!(!m.sessions[&1].images.contains_key(&4), "expired");
        m.apply_net(
            NetEvent::UploadFailed {
                key: 1,
                error: crate::media::UploadError::NotImage,
            },
            Instant::now(),
        );
        assert_eq!(
            m.ui.toasts,
            vec!["Error: Not a PNG or JPEG image".to_string()]
        );
        // Images from a closed connection are ignored.
        m.handle(UiCommand::Disconnect(2));
        m.on_image(2, SessionId(2), ChannelId(1), 0, 2, 2, Some(image));
        assert_eq!(m.sessions[&2].chat.len(), 1, "late image dropped");
    }

    fn share_msg(key: ServerKey, user: u32, on: bool) -> NetEvent {
        NetEvent::Msg {
            key,
            msg: ServerMsg::ScreenShare {
                user: SessionId(user),
                on,
            },
        }
    }

    #[test]
    fn toggle_screen_share_on_and_off() {
        let (mut m, _d) = model();
        m.handle(UiCommand::Select(1));
        assert!(m.handle(UiCommand::ToggleScreenShare).is_empty(), "offline");
        assert_eq!(m.screen_share, None);
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        connect(&mut m, 2, welcome(1, &[(1, "me", 1)]));
        let on = |key| NetCmd::Send {
            key,
            msg: ClientMsg::SetScreenShare { on: true },
        };
        let off = |key| NetCmd::Send {
            key,
            msg: ClientMsg::SetScreenShare { on: false },
        };
        assert_eq!(m.handle(UiCommand::ToggleScreenShare), vec![on(1)]);
        assert_eq!(m.screen_share, Some(1));
        assert_eq!(m.wants_capture(), Some(1));
        assert_eq!(m.handle(UiCommand::ToggleScreenShare), vec![off(1)]);
        assert_eq!(m.screen_share, None);
        assert_eq!(m.wants_capture(), None);
        m.handle(UiCommand::Select(2));
        assert_eq!(m.handle(UiCommand::ToggleScreenShare), vec![on(2)]);
        assert_eq!(m.screen_share, Some(2));
    }

    #[test]
    fn toggle_stops_share_on_other_server() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        connect(&mut m, 2, welcome(1, &[(1, "me", 1)]));
        m.handle(UiCommand::Select(1));
        m.handle(UiCommand::ToggleScreenShare);
        assert_eq!(m.screen_share, Some(1));
        // With server 2 selected, the toggle stops the share on 1 and starts nothing.
        m.handle(UiCommand::Select(2));
        assert_eq!(
            m.handle(UiCommand::ToggleScreenShare),
            vec![NetCmd::Send {
                key: 1,
                msg: ClientMsg::SetScreenShare { on: false }
            }]
        );
        assert_eq!(m.screen_share, None);
        assert_eq!(m.wants_capture(), None);
        // Even with nothing selected, a running share can be stopped.
        m.handle(UiCommand::Select(1));
        m.handle(UiCommand::ToggleScreenShare);
        m.selected = None;
        assert_eq!(m.handle(UiCommand::ToggleScreenShare).len(), 1);
        assert_eq!(m.screen_share, None);
    }

    #[test]
    fn server_off_and_close_clear_screen_share() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1), (2, "bob", 1)]));
        m.handle(UiCommand::Select(1));
        m.handle(UiCommand::ToggleScreenShare);
        m.apply_net(share_msg(1, 1, true), Instant::now());
        assert!(m.sessions[&1].sharing);
        // Someone else stopping does not touch my share.
        m.apply_net(share_msg(1, 2, false), Instant::now());
        assert_eq!(m.screen_share, Some(1));
        m.apply_net(share_msg(1, 1, false), Instant::now());
        assert_eq!(m.screen_share, None);
        assert!(!m.sessions[&1].sharing);

        m.handle(UiCommand::ToggleScreenShare);
        assert_eq!(m.screen_share, Some(1));
        m.apply_net(
            NetEvent::Closed {
                key: 1,
                reason: crate::net::CloseReason::Timeout,
            },
            Instant::now(),
        );
        assert_eq!(m.screen_share, None);

        m.handle(UiCommand::ToggleScreenShare);
        m.apply_net(
            NetEvent::Status {
                key: 1,
                status: ConnStatus::Reconnecting { attempt: 1 },
            },
            Instant::now(),
        );
        assert_eq!(m.screen_share, None);
        assert_eq!(m.wants_capture(), None);
    }

    #[test]
    fn capture_failure_toasts_and_stops() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        m.handle(UiCommand::Select(1));
        m.handle(UiCommand::ToggleScreenShare);
        let cmds = m.capture_failed("boom");
        assert_eq!(
            cmds,
            vec![NetCmd::Send {
                key: 1,
                msg: ClientMsg::SetScreenShare { on: false }
            }]
        );
        assert_eq!(
            m.ui.toasts,
            vec!["Error: Screen capture failed: boom".to_string()]
        );
        assert_eq!(m.screen_share, None);
    }

    fn watch_cmd(key: ServerKey, user: u32, on: bool) -> NetCmd {
        NetCmd::Send {
            key,
            msg: ClientMsg::WatchScreen {
                user: SessionId(user),
                on,
            },
        }
    }

    #[test]
    fn watch_screen_sends_watch_msg() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1), (2, "bob", 1)]));
        m.apply_net(share_msg(1, 2, true), Instant::now());
        let cmd = UiCommand::WatchScreen {
            key: 1,
            user: SessionId(2),
            on: true,
        };
        assert_eq!(m.handle(cmd), vec![watch_cmd(1, 2, true)]);
        assert!(m.sessions[&1].watching.contains(&SessionId(2)));
        // Offline server: nothing.
        let cmd = UiCommand::WatchScreen {
            key: 9,
            user: SessionId(2),
            on: true,
        };
        assert!(m.handle(cmd).is_empty());
    }

    /// Connect 1 and 2 with sharers bob (on 1) and carol (on 2), and watch both.
    fn watching_two(m: &mut AppModel) {
        connect(m, 1, welcome(1, &[(1, "me", 1), (2, "bob", 1)]));
        connect(m, 2, welcome(1, &[(1, "me", 1), (3, "carol", 1)]));
        m.apply_net(share_msg(1, 2, true), Instant::now());
        m.apply_net(share_msg(2, 3, true), Instant::now());
        for (key, user) in [(1, 2), (2, 3)] {
            m.handle(UiCommand::WatchScreen {
                key,
                user: SessionId(user),
                on: true,
            });
        }
    }

    #[test]
    fn streams_popped_clears_when_the_last_stream_stops() {
        let (mut m, _d) = model();
        watching_two(&mut m);
        m.handle(UiCommand::SetStreamsPopped(true));
        // Two watched: stopping one keeps the popout.
        m.handle(UiCommand::WatchScreen {
            key: 1,
            user: SessionId(2),
            on: false,
        });
        assert!(m.ui.streams_popped);
        m.handle(UiCommand::WatchScreen {
            key: 2,
            user: SessionId(3),
            on: false,
        });
        assert!(!m.ui.streams_popped, "nothing watched: back to the stage");
    }

    #[test]
    fn streams_popped_clears_when_sharer_stops_or_server_drops() {
        let (mut m, _d) = model();
        watching_two(&mut m);
        m.handle(UiCommand::SetStreamsPopped(true));
        m.apply_net(share_msg(1, 2, false), Instant::now());
        assert!(m.ui.streams_popped, "carol is still watched");
        m.apply_net(
            NetEvent::Status {
                key: 2,
                status: ConnStatus::Reconnecting { attempt: 1 },
            },
            Instant::now(),
        );
        assert!(!m.ui.streams_popped);
        // Popping out with nothing watched does not stick either.
        m.handle(UiCommand::SetStreamsPopped(true));
        assert!(!m.ui.streams_popped);
    }

    fn rename_cmd(key: ServerKey, ch: u32, name: &str) -> NetCmd {
        NetCmd::Send {
            key,
            msg: ClientMsg::RenameChannel {
                channel: ChannelId(ch),
                name: name.into(),
            },
        }
    }

    #[test]
    fn new_channel_while_renaming_submits_the_rename_once() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        m.handle(UiCommand::Select(1));
        m.handle(UiCommand::OpenRenameChannel(ChannelId(2)));
        m.ui.channel_dialog.as_mut().unwrap().name = "Raid".into();
        // "+" and the field's blur land in the same frame, in this order.
        let out = m.handle(UiCommand::OpenNewChannel);
        assert_eq!(out, vec![rename_cmd(1, 2, "Raid")]);
        let stale = m.handle(UiCommand::SubmitChannelDialog {
            key: 1,
            mode: ChannelDialogMode::Rename(ChannelId(2)),
        });
        assert!(stale.is_empty(), "no Create(\"\") from the stale submit");
        let d = m.ui.channel_dialog.as_ref().unwrap();
        assert_eq!((d.key, d.mode), (1, ChannelDialogMode::New));
        assert!(m.ui.toasts.is_empty(), "{:?}", m.ui.toasts);
        // A stale cancel leaves the new edit open too.
        m.handle(UiCommand::CancelChannelDialog {
            key: 1,
            mode: ChannelDialogMode::Rename(ChannelId(2)),
        });
        assert!(m.ui.channel_dialog.is_some());
        // A blank edit is just replaced, nothing sent.
        assert!(
            m.handle(UiCommand::OpenRenameChannel(ChannelId(1)))
                .is_empty()
        );
    }

    #[test]
    fn unchanged_rename_sends_nothing() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        m.handle(UiCommand::Select(1));
        m.handle(UiCommand::OpenRenameChannel(ChannelId(2)));
        let out = m.handle(UiCommand::SubmitChannelDialog {
            key: 1,
            mode: ChannelDialogMode::Rename(ChannelId(2)),
        });
        assert!(out.is_empty(), "{out:?}");
        assert!(m.ui.channel_dialog.is_none());
        assert!(m.ui.toasts.is_empty(), "{:?}", m.ui.toasts);
    }

    #[test]
    fn unchanged_rename_flush_sends_nothing() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        m.handle(UiCommand::Select(1));
        m.handle(UiCommand::OpenRenameChannel(ChannelId(2)));
        let out = m.handle(UiCommand::OpenNewChannel);
        assert!(out.is_empty(), "{out:?}");
        let d = m.ui.channel_dialog.as_ref().unwrap();
        assert_eq!(d.mode, ChannelDialogMode::New);
    }

    #[test]
    fn hidden_channel_edits_are_pruned() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        connect(&mut m, 2, welcome(1, &[(1, "me", 1)]));
        // Another server selected.
        m.handle(UiCommand::Select(1));
        m.handle(UiCommand::OpenRenameChannel(ChannelId(2)));
        assert!(m.ui.channel_dialog.is_some());
        m.handle(UiCommand::Select(2));
        assert!(m.ui.channel_dialog.is_none());
        // Its server disconnects.
        m.handle(UiCommand::OpenNewChannel);
        assert!(m.ui.channel_dialog.is_some());
        m.handle(UiCommand::Disconnect(2));
        assert!(m.ui.channel_dialog.is_none());
        // The channel being renamed is deleted remotely.
        m.handle(UiCommand::Select(1));
        m.handle(UiCommand::OpenRenameChannel(ChannelId(2)));
        assert!(m.ui.channel_dialog.is_some());
        m.apply_net(
            NetEvent::Msg {
                key: 1,
                msg: ServerMsg::ChannelRemoved {
                    channel: ChannelId(2),
                },
            },
            Instant::now(),
        );
        assert!(m.ui.channel_dialog.is_none());
    }

    #[test]
    fn capture_cfg_follows_watchers_and_settings() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1), (2, "bob", 1)]));
        assert!(!m.capture_cfg().active, "no share");
        m.handle(UiCommand::Select(1));
        m.handle(UiCommand::ToggleScreenShare);
        m.apply_net(share_msg(1, 1, true), Instant::now());
        let c = m.capture_cfg();
        assert!(!c.active, "nobody watches");
        let count = |n| NetEvent::Msg {
            key: 1,
            msg: ServerMsg::ScreenWatchers {
                count: n,
                h264: false,
            },
        };
        m.apply_net(count(1), Instant::now());
        let c1 = m.capture_cfg();
        assert!(c1.active);
        assert!(c1.epoch > c.epoch);
        m.settings.screen_fps = 5;
        assert_eq!(
            m.capture_cfg().interval,
            std::time::Duration::from_millis(200)
        );
        m.apply_net(count(0), Instant::now());
        assert!(!m.capture_cfg().active);
    }

    fn sharing_with_watchers(m: &mut AppModel, w: ServerMsg) {
        connect(m, 1, w);
        m.handle(UiCommand::Select(1));
        m.handle(UiCommand::ToggleScreenShare);
        m.apply_net(share_msg(1, 1, true), Instant::now());
        m.apply_net(
            NetEvent::Msg {
                key: 1,
                msg: ServerMsg::ScreenWatchers {
                    count: 1,
                    h264: false,
                },
            },
            Instant::now(),
        );
    }

    #[test]
    fn capture_cfg_clamps_to_server_caps() {
        let (mut m, _d) = model();
        m.settings.screen_fps = 120;
        m.settings.screen_height = 0;
        sharing_with_watchers(
            &mut m,
            welcome_caps(1, &[(1, "me", 1), (2, "bob", 1)], 30, 720),
        );
        let c = m.capture_cfg();
        assert_eq!(c.interval, screen_interval(30));
        assert_eq!((c.max_w, c.max_h), (1280, 720));
        assert!(!c.video, "watchers without H.264 get JPEG");
        assert_eq!(
            m.screen_limit_text().as_deref(),
            Some("Server limit: 720p, 30 fps")
        );
        // Below the caps the Preferences apply as they are.
        m.settings.screen_fps = 10;
        m.settings.screen_height = 480;
        let c = m.capture_cfg();
        assert_eq!(c.interval, screen_interval(10));
        assert_eq!((c.max_w, c.max_h), (854, 480));
    }

    #[test]
    fn capture_cfg_uncapped_server_allows_120() {
        let (mut m, _d) = model();
        m.settings.screen_fps = 120;
        sharing_with_watchers(&mut m, welcome(1, &[(1, "me", 1), (2, "bob", 1)]));
        assert_eq!(m.capture_cfg().interval, screen_interval(120));
        assert_eq!(m.screen_limit_text(), None);
    }

    #[test]
    fn probe_result_applies_to_settings() {
        let (mut m, d) = model();
        m.handle(UiCommand::OpenSettings);
        m.handle(UiCommand::TestScreenMax);
        assert!(m.probe_requested && m.probe_running);
        assert_eq!(m.probe_text().as_deref(), Some(PROBE_RUNNING));
        let r = ProbeResult {
            height: 720,
            fps: 30,
            encoder: "CPU (JPEG)".into(),
        };
        m.on_probe_result(Ok(r.clone()));
        assert!(!m.probe_running);
        assert_eq!((m.settings.screen_fps, m.settings.screen_height), (30, 720));
        assert_eq!(
            m.probe_text().as_deref(),
            Some("Tested (local encode limit): 720p @ 30 fps")
        );
        // Saved to disk.
        let saved = Profile::new(d.path()).load_settings().unwrap();
        assert_eq!(saved.screen_tested, Some(r));
        assert_eq!((saved.screen_fps, saved.screen_height), (30, 720));
        // A failure is shown in the same line.
        m.handle(UiCommand::TestScreenMax);
        m.on_probe_result(Err("boom".into()));
        assert_eq!(m.probe_text().as_deref(), Some("Test max failed: boom"));
    }

    #[test]
    fn stream_target_needs_sound_on_and_watchers() {
        let (mut m, _d) = model();
        sharing_with_watchers(&mut m, welcome(1, &[(1, "me", 1), (2, "bob", 1)]));
        assert_eq!(m.pipeline_ctl().stream_target, None, "off by default");
        assert!(!m.wants_loopback());
        m.handle(UiCommand::SetStreamSound { on: true });
        assert_eq!(m.pipeline_ctl().stream_target, Some(1));
        assert!(m.wants_loopback());
        m.apply_net(
            NetEvent::Msg {
                key: 1,
                msg: ServerMsg::ScreenWatchers {
                    count: 0,
                    h264: false,
                },
            },
            Instant::now(),
        );
        assert_eq!(m.pipeline_ctl().stream_target, None, "nobody watches");
        // A loopback failure turns it off and explains.
        m.loopback_failed("too old");
        assert!(!m.settings.screen_audio);
        assert_eq!(
            m.stream_sound_error.as_deref(),
            Some("Stream sound unavailable: too old")
        );
        m.handle(UiCommand::SetStreamSound { on: true });
        assert_eq!(m.stream_sound_error, None);
    }

    #[test]
    fn muted_stream_gets_gain_zero_and_clears_when_share_stops() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1), (2, "bob", 1)]));
        m.apply_net(share_msg(1, 2, true), Instant::now());
        m.handle(UiCommand::MuteStream {
            key: 1,
            user: SessionId(2),
            muted: true,
        });
        assert!(m.stream_is_muted(1, SessionId(2)));
        let g = m.pipeline_ctl().gains;
        assert_eq!(g.get(&(1, stream_speaker(SessionId(2)))), Some(&0.0));
        assert_eq!(g.get(&(1, SessionId(2))), None, "bob's mic is untouched");
        m.apply_net(share_msg(1, 2, false), Instant::now());
        assert!(!m.stream_is_muted(1, SessionId(2)));
        assert!(m.pipeline_ctl().gains.is_empty());
    }

    #[test]
    fn stream_speaker_does_not_light_speaking() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1), (2, "bob", 1)]));
        let now = Instant::now();
        m.apply_net(
            NetEvent::Voice {
                key: 1,
                speaker: stream_speaker(SessionId(2)),
                seq: STREAM_BIT,
                payload: vec![],
            },
            now,
        );
        assert!(!m.sessions[&1].anyone_speaking(now));
        m.apply_net(
            NetEvent::Voice {
                key: 1,
                speaker: SessionId(2),
                seq: 0,
                payload: vec![],
            },
            now,
        );
        assert!(m.sessions[&1].anyone_speaking(now));
    }

    #[test]
    fn update_settings_persists_only_when_asked_and_keeps_unowned_fields() {
        let (mut m, d) = model();
        m.settings.set_user_volume(1, "bob", 1.5);
        m.settings.screen_audio = true;
        m.settings.nickname = "nick".into();
        m.handle(UiCommand::OpenSettings);
        // A stale snapshot of the window: other volumes, sound off, no nick.
        let mut s = Settings {
            vad_threshold_db: -20.0,
            ..Settings::default()
        };
        s.screen_audio = false;
        m.handle(UiCommand::UpdateSettings {
            settings: Box::new(s.clone()),
            persist: false,
        });
        assert_eq!(m.settings.vad_threshold_db, -20.0);
        assert!((m.settings.user_volume(1, "bob") - 1.5).abs() < 1e-6);
        assert!(m.settings.screen_audio);
        assert_eq!(m.settings.nickname, "nick");
        assert!(!m.audio_restart_needed);
        let on_disk = Profile::new(d.path()).load_settings().unwrap();
        assert_ne!(
            on_disk.vad_threshold_db, -20.0,
            "not written while dragging"
        );
        m.handle(UiCommand::UpdateSettings {
            settings: Box::new(s),
            persist: true,
        });
        let on_disk = Profile::new(d.path()).load_settings().unwrap();
        assert_eq!(
            on_disk.vad_threshold_db, -20.0,
            "written when the drag ends"
        );
    }

    #[test]
    fn update_settings_device_change_restarts_audio() {
        let (mut m, _d) = model();
        let s = Settings {
            output_device: Some("x".into()),
            ..m.settings.clone()
        };
        m.handle(UiCommand::UpdateSettings {
            settings: Box::new(s),
            persist: true,
        });
        assert!(m.audio_restart_needed);
    }

    #[test]
    fn commit_nickname_ignores_blank_and_unchanged() {
        let (mut m, d) = model();
        m.handle(UiCommand::OpenSettings);
        m.ui.nick_edit = "   ".into();
        m.handle(UiCommand::CommitNickname);
        assert_eq!(m.settings.nickname, "");
        m.ui.nick_edit = "neo".into();
        m.handle(UiCommand::CommitNickname);
        assert_eq!(m.settings.nickname, "neo");
        assert_eq!(
            Profile::new(d.path()).load_settings().unwrap().nickname,
            "neo"
        );
        // Both servers were on the old default nick and follow.
        assert!(m.saved.iter().all(|s| s.nick == "neo"));
    }

    #[test]
    fn save_server_derives_label_and_connects_now() {
        let (mut m, _d) = model();
        for (addr, want) in [
            ("localhost:7878", "localhost"),
            ("[::1]:7878", "::1"),
            // A label already in use falls back to the full address.
            ("localhost:9000", "localhost:9000"),
        ] {
            m.handle(UiCommand::OpenAddServer);
            let dlg = m.ui.server_dialog.as_mut().unwrap();
            dlg.addr = addr.into();
            dlg.connect_now = false;
            assert!(m.handle(UiCommand::SaveServerDialog).is_empty());
            let s = m.saved.last().unwrap();
            assert_eq!(s.label, want);
            assert_eq!(m.selected, Some(s.key), "a new server is selected");
        }
        m.handle(UiCommand::OpenAddServer);
        let dlg = m.ui.server_dialog.as_mut().unwrap();
        assert!(dlg.connect_now, "on by default");
        dlg.addr = "127.0.0.1:4".into();
        let cmds = m.handle(UiCommand::SaveServerDialog);
        let key = m.saved.last().unwrap().key;
        assert!(
            matches!(cmds.as_slice(), [NetCmd::Connect { key: k, .. }] if *k == key),
            "{cmds:?}"
        );
        assert_eq!(m.status(key), ConnStatus::Connecting);
        assert_eq!(m.selected, Some(key));
    }

    #[test]
    fn stream_volume_sets_stream_gain_and_mute_wins() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1), (2, "bob", 1)]));
        m.apply_net(share_msg(1, 2, true), Instant::now());
        m.handle(UiCommand::SetStreamVolume {
            key: 1,
            nick: "bob".into(),
            volume: 1.5,
        });
        let g = m.pipeline_ctl().gains;
        assert_eq!(g.get(&(1, stream_speaker(SessionId(2)))), Some(&1.5));
        assert_eq!(g.get(&(1, SessionId(2))), None, "bob's voice is untouched");
        m.handle(UiCommand::MuteStream {
            key: 1,
            user: SessionId(2),
            muted: true,
        });
        let g = m.pipeline_ctl().gains;
        assert_eq!(g.get(&(1, stream_speaker(SessionId(2)))), Some(&0.0));
        m.handle(UiCommand::MuteStream {
            key: 1,
            user: SessionId(2),
            muted: false,
        });
        let g = m.pipeline_ctl().gains;
        assert_eq!(g.get(&(1, stream_speaker(SessionId(2)))), Some(&1.5));
        // Muting bob's voice leaves the stream volume alone.
        m.handle(UiCommand::MuteUser {
            key: 1,
            nick: "bob".into(),
            muted: true,
        });
        let g = m.pipeline_ctl().gains;
        assert_eq!(g.get(&(1, SessionId(2))), Some(&0.0));
        assert_eq!(g.get(&(1, stream_speaker(SessionId(2)))), Some(&1.5));
    }

    #[test]
    fn update_settings_keeps_stream_volumes() {
        let (mut m, _d) = model();
        m.handle(UiCommand::SetStreamVolume {
            key: 1,
            nick: "bob".into(),
            volume: 0.4,
        });
        m.handle(UiCommand::UpdateSettings {
            settings: Box::new(Settings::default()),
            persist: false,
        });
        assert!((m.settings.stream_volume(1, "bob") - 0.4).abs() < 1e-6);
    }

    #[test]
    fn mute_user_gain_zero_keeps_volume() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1), (2, "bob", 1)]));
        m.handle(UiCommand::SetUserVolume {
            key: 1,
            nick: "bob".into(),
            volume: 1.5,
        });
        m.handle(UiCommand::MuteUser {
            key: 1,
            nick: "bob".into(),
            muted: true,
        });
        assert!(m.user_is_muted(1, "bob"));
        assert_eq!(m.pipeline_ctl().gains.get(&(1, SessionId(2))), Some(&0.0));
        assert!((m.settings.user_volume(1, "bob") - 1.5).abs() < 1e-6);
        m.handle(UiCommand::MuteUser {
            key: 1,
            nick: "bob".into(),
            muted: false,
        });
        assert_eq!(m.pipeline_ctl().gains.get(&(1, SessionId(2))), Some(&1.5));
    }

    #[test]
    fn ptt_listening_stops_on_tab_or_mode_change() {
        let (mut m, _d) = model();
        m.settings.input_mode = InputMode::Ptt;
        m.handle(UiCommand::OpenSettings);
        m.handle(UiCommand::SetPttListening(true));
        m.handle(UiCommand::SetSettingsTab(SettingsTab::Profile));
        assert!(!m.ui.ptt_listening);
        m.handle(UiCommand::SetSettingsTab(SettingsTab::Voice));
        m.handle(UiCommand::SetPttListening(true));
        let s = Settings {
            input_mode: InputMode::Vad,
            ..m.settings.clone()
        };
        m.handle(UiCommand::UpdateSettings {
            settings: Box::new(s),
            persist: true,
        });
        assert!(!m.ui.ptt_listening);
    }

    #[test]
    fn share_screen_on_targets_its_server() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        connect(&mut m, 2, welcome(1, &[(1, "me", 1)]));
        m.handle(UiCommand::Select(1));
        assert_eq!(
            m.handle(UiCommand::ShareScreenOn(2)),
            vec![NetCmd::Send {
                key: 2,
                msg: ClientMsg::SetScreenShare { on: true }
            }]
        );
        assert_eq!(m.screen_share, Some(2));
        // Pressed again (on any server) it stops the running share.
        assert_eq!(m.handle(UiCommand::ShareScreenOn(1)).len(), 1);
        assert_eq!(m.screen_share, None);
        m.handle(UiCommand::Disconnect(2));
        assert!(m.handle(UiCommand::ShareScreenOn(2)).is_empty(), "offline");
    }

    #[test]
    fn pong_sets_ping_ms() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        let nonce = unix_ms() - 40;
        m.apply_net(
            NetEvent::Msg {
                key: 1,
                msg: ServerMsg::Pong { nonce },
            },
            Instant::now(),
        );
        let ms = m.sessions[&1].ping_ms.unwrap();
        assert!((40..10_000).contains(&ms), "{ms}");
        m.handle(UiCommand::Disconnect(1));
        assert_eq!(m.sessions[&1].ping_ms, None);
    }

    #[test]
    fn net_pong_sets_ping_ms() {
        let (mut m, _d) = model();
        connect(&mut m, 1, welcome(1, &[(1, "me", 1)]));
        m.apply_net(NetEvent::Pong { key: 1, rtt_ms: 7 }, Instant::now());
        assert_eq!(m.sessions[&1].ping_ms, Some(7));
        m.handle(UiCommand::Disconnect(1));
        assert_eq!(m.sessions[&1].ping_ms, None);
        // A late answer from the closed connection is ignored.
        m.apply_net(NetEvent::Pong { key: 1, rtt_ms: 9 }, Instant::now());
        assert_eq!(m.sessions[&1].ping_ms, None);
    }

    #[test]
    fn image_windows_open_and_close_independently() {
        let (mut m, _d) = model();
        m.handle(UiCommand::ViewImage(3));
        m.handle(UiCommand::ViewImage(5));
        m.handle(UiCommand::ViewImage(3));
        assert_eq!(m.ui.viewing.iter().copied().collect::<Vec<_>>(), [3, 5]);
        m.handle(UiCommand::CloseImage(3));
        assert_eq!(m.ui.viewing.iter().copied().collect::<Vec<_>>(), [5]);
    }
}
