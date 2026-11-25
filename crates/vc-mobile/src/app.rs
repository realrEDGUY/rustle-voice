//! `MobileApp`: the desktop's headless `VoiceApp` (network, audio pipeline, model)
//! driven by the iOS app's main-thread timer instead of an egui frame.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use egui::ColorImage;
use vc_client::ServerKey;
use vc_client::app::{ChannelDialog, ChannelDialogMode, ServerDialog, UiCommand};
use vc_client::audio::{AudioBackend, ExternalAudio, ExternalBackend, FakeBackend, FakeMode};
use vc_client::gui::{AppOptions, PING_EVERY, VoiceApp};
use vc_client::media::UploadSource;
use vc_client::profile::Profile;
use vc_client::session::ConnStatus;
use vc_proto::{ChannelId, SessionId};

use crate::cmd::{self, MobileCmd};
use crate::view;

pub struct MobileApp {
    pub app: VoiceApp,
    audio: Option<ExternalAudio>,
    last_ping: Instant,
    last_snapshot: String,
    last_live: String,
}

impl MobileApp {
    /// `audio`: `"external"` (the host feeds audio through [`MobileApp::audio`])
    /// or `"fake-sine"` (a 440 Hz test tone, no host audio).
    pub fn new(profile_dir: &Path, audio: &str) -> Result<Self, String> {
        Self::with_options(profile_dir, audio, AppOptions::default())
    }

    pub fn with_options(profile_dir: &Path, audio: &str, opts: AppOptions) -> Result<Self, String> {
        let (backend, handle): (Box<dyn AudioBackend>, Option<ExternalAudio>) = match audio {
            "external" => {
                let b = ExternalBackend::new();
                let h = b.handle();
                (Box::new(b), Some(h))
            }
            "fake-sine" => (
                Box::new(FakeBackend::new(FakeMode::Sine(440.0)).realtime()),
                None,
            ),
            other => return Err(format!("unknown audio mode {other:?}")),
        };
        std::fs::create_dir_all(profile_dir)
            .map_err(|e| format!("cannot create {}: {e}", profile_dir.display()))?;
        let app = VoiceApp::with_options(
            None,
            backend,
            Profile::new(PathBuf::from(profile_dir)),
            vec![],
            opts,
        );
        Ok(Self {
            app,
            audio: handle,
            // Due at once: the first ping goes out as soon as a server connects.
            last_ping: Instant::now()
                .checked_sub(PING_EVERY)
                .unwrap_or_else(Instant::now),
            last_snapshot: String::new(),
            last_live: String::new(),
        })
    }

    /// The host audio handle (only in `"external"` mode).
    pub fn audio(&self) -> Option<ExternalAudio> {
        self.audio.clone()
    }

    /// One tick: drain the network, copy the meter, ping, refresh the pipeline.
    pub fn step(&mut self) {
        self.app.poll();
        {
            let p = self.app.pipeline.lock().unwrap_or_else(|e| e.into_inner());
            self.app.model.input_level_db = p.input_level_db();
            self.app.model.transmitting = p.is_transmitting();
        }
        let now = Instant::now();
        let mut cmds = Vec::new();
        let connected: Vec<ServerKey> = self
            .app
            .model
            .sessions
            .iter()
            .filter(|(_, s)| s.status == ConnStatus::Connected)
            .map(|(k, _)| *k)
            .collect();
        if !connected.is_empty() && now.saturating_duration_since(self.last_ping) >= PING_EVERY {
            self.last_ping = now;
            cmds.extend(connected.into_iter().map(UiCommand::Ping));
        }
        // Always dispatch, even with nothing to do, so the pipeline control block
        // follows the model (user gains, voice tuning of new servers).
        self.app.dispatch(cmds);
    }

    /// Push to talk held (`true`) or released. Applied to the pipeline at once.
    pub fn set_ptt(&mut self, down: bool) {
        self.app.model.ptt_down = down;
        *self.app.ctl.lock().unwrap_or_else(|e| e.into_inner()) = self.app.model.pipeline_ctl();
    }

    /// The banner shown when the host's audio engine failed (`None` once it runs).
    pub fn set_audio_warning(&mut self, msg: Option<&str>) {
        self.app.model.audio_warning = msg.map(str::to_string);
    }

    /// Run one JSON command. `Err` carries text for the UI.
    pub fn command(&mut self, json: &str) -> Result<(), String> {
        let c = cmd::parse(json)?;
        self.run(c)
    }

    /// Select `key`, then run `then` (commands that act on the selected server).
    fn on_server(&mut self, key: ServerKey, then: UiCommand) {
        self.app.dispatch(vec![UiCommand::Select(key), then]);
    }

    pub fn run(&mut self, c: MobileCmd) -> Result<(), String> {
        match c {
            MobileCmd::Select { key } => self.app.dispatch(vec![UiCommand::Select(key)]),
            MobileCmd::Connect { key } => self.app.dispatch(vec![UiCommand::Connect(key)]),
            MobileCmd::Disconnect { key } => self.app.dispatch(vec![UiCommand::Disconnect(key)]),
            MobileCmd::TalkHere { key } => self.app.dispatch(vec![UiCommand::TalkHere(key)]),
            MobileCmd::RemoveServer { key } => {
                self.app.dispatch(vec![UiCommand::RemoveServer(key)])
            }
            MobileCmd::SaveServer {
                editing,
                label,
                addr,
                nick,
                password,
                connect_now,
            } => {
                self.app.model.ui.server_dialog = Some(ServerDialog {
                    editing,
                    label,
                    addr,
                    nick,
                    password,
                    error: None,
                    connect_now,
                });
                self.app.dispatch(vec![UiCommand::SaveServerDialog]);
                if let Some(d) = self.app.model.ui.server_dialog.take() {
                    return Err(d.error.unwrap_or_else(|| "Not saved".into()));
                }
                // The desktop's edit dialog leaves the server disconnected; the
                // phone form's "Connect now" reconnects it with the new details.
                if let Some(key) = editing
                    && connect_now
                {
                    self.app.dispatch(vec![UiCommand::Connect(key)]);
                }
            }
            MobileCmd::Join { key, channel } => {
                self.on_server(key, UiCommand::Join(ChannelId(channel)))
            }
            MobileCmd::CreateChannel { key, name } => {
                self.channel_submit(key, ChannelDialogMode::New, name)
            }
            MobileCmd::RenameChannel { key, channel, name } => {
                self.channel_submit(key, ChannelDialogMode::Rename(ChannelId(channel)), name)
            }
            MobileCmd::DeleteChannel { key, channel } => {
                self.on_server(key, UiCommand::DeleteChannel(ChannelId(channel)))
            }
            MobileCmd::SendChat { key, text } => {
                self.app.dispatch(vec![UiCommand::Select(key)]);
                self.app.model.ui.chat_input = text;
                self.app.dispatch(vec![UiCommand::SendChat]);
                if !self.app.model.ui.chat_input.is_empty() {
                    self.app.model.ui.chat_input.clear();
                    return Err("Not sent".into());
                }
            }
            MobileCmd::ToggleMute => self.app.dispatch(vec![UiCommand::ToggleMute]),
            MobileCmd::ToggleDeafen => self.app.dispatch(vec![UiCommand::ToggleDeafen]),
            MobileCmd::SetInputMode { mode } => {
                self.app.dispatch(vec![UiCommand::SetInputMode(mode)])
            }
            MobileCmd::SetVadThreshold { db, persist } => {
                let mut s = self.app.model.settings.clone();
                s.vad_threshold_db = if db.is_finite() {
                    db.clamp(-60.0, 0.0)
                } else {
                    s.vad_threshold_db
                };
                self.app.dispatch(vec![UiCommand::UpdateSettings {
                    settings: Box::new(s),
                    persist,
                }]);
            }
            MobileCmd::SetMasterVolume { volume, persist } => {
                let mut s = self.app.model.settings.clone();
                s.master_volume = if volume.is_finite() {
                    volume.clamp(0.0, 2.0)
                } else {
                    s.master_volume
                };
                self.app.dispatch(vec![UiCommand::UpdateSettings {
                    settings: Box::new(s),
                    persist,
                }]);
            }
            MobileCmd::SetNickname { nick } => {
                self.app.model.ui.nick_edit = nick;
                self.app.dispatch(vec![UiCommand::CommitNickname]);
            }
            MobileCmd::SetUserVolume { key, nick, volume } => {
                let volume = if volume.is_finite() { volume } else { 1.0 };
                self.app
                    .dispatch(vec![UiCommand::SetUserVolume { key, nick, volume }])
            }
            MobileCmd::MuteUser { key, nick, muted } => self
                .app
                .dispatch(vec![UiCommand::MuteUser { key, nick, muted }]),
            MobileCmd::WatchScreen { key, user, on } => {
                self.app.dispatch(vec![UiCommand::WatchScreen {
                    key,
                    user: SessionId(user),
                    on,
                }])
            }
            MobileCmd::MuteStream { key, user, muted } => {
                self.app.dispatch(vec![UiCommand::MuteStream {
                    key,
                    user: SessionId(user),
                    muted,
                }])
            }
            MobileCmd::UploadFile { key, path } => self.on_server(
                key,
                UiCommand::UploadImage(UploadSource::File(PathBuf::from(path))),
            ),
            MobileCmd::TrustNewIdentity => self.app.dispatch(vec![UiCommand::TrustNewIdentity]),
            MobileCmd::CancelTofu => self.app.dispatch(vec![UiCommand::CancelTofu]),
            MobileCmd::DismissToast { text } => {
                if let Some(i) = self.app.model.ui.toasts.iter().position(|t| *t == text) {
                    self.app.dispatch(vec![UiCommand::DismissToast(i)]);
                }
            }
        }
        Ok(())
    }

    /// Create or rename a channel through the desktop's channel-edit flow.
    fn channel_submit(&mut self, key: ServerKey, mode: ChannelDialogMode, name: String) {
        self.app.dispatch(vec![UiCommand::Select(key)]);
        self.app.model.ui.channel_dialog = Some(ChannelDialog { key, mode, name });
        self.app
            .dispatch(vec![UiCommand::SubmitChannelDialog { key, mode }]);
        self.app.model.ui.channel_dialog = None;
    }

    /// The snapshot JSON, or `None` when it equals the last one returned.
    pub fn snapshot(&mut self) -> Option<String> {
        let json = serde_json::to_string(&view::snapshot(&self.app.model)).ok()?;
        if json == self.last_snapshot {
            return None;
        }
        self.last_snapshot = json.clone();
        Some(json)
    }

    /// The live JSON, or `None` when it equals the last one returned.
    pub fn live(&mut self) -> Option<String> {
        let json = serde_json::to_string(&view::live(&self.app.model, Instant::now())).ok()?;
        if json == self.last_live {
            return None;
        }
        self.last_live = json.clone();
        Some(json)
    }

    /// A decoded chat image, while it is kept.
    pub fn image(&self, key: ServerKey, id: u64) -> Option<Arc<ColorImage>> {
        self.app.model.session(key)?.images.get(&id).cloned()
    }

    /// The latest frame of a watched sharer, with its generation.
    pub fn screen(&self, key: ServerKey, user: u32) -> Option<(Arc<ColorImage>, u64)> {
        self.app
            .model
            .session(key)?
            .screens
            .get(&SessionId(user))
            .map(|v| (v.image.clone(), v.generation))
    }
}
