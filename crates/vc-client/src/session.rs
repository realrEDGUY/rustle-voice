//! Sans-IO per-server client state: `apply(ServerMsg)` and `intent(UiAction)`.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::ColorImage;

use vc_proto::{
    ChannelId, ChannelInfo, ClientMsg, ErrorCode, MAX_CHAT, ServerMsg, SessionId, UserInfo,
    VoiceTuning,
};

pub const SPEAKING_WINDOW: Duration = Duration::from_millis(300);
pub const MAX_ERRORS: usize = 20;
/// Decoded chat images kept per server; older ones show as "[image expired]".
pub const MAX_IMAGES: usize = 24;
/// Image cap assumed until the server's Welcome says otherwise.
pub const DEFAULT_IMAGE_CAP: u32 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Reconnecting {
        attempt: u32,
    },
    Failed(String),
}

impl ConnStatus {
    /// The text shown in the sidebar status label.
    pub fn label(&self) -> String {
        match self {
            ConnStatus::Disconnected => "Disconnected".into(),
            ConnStatus::Connecting => "Connecting…".into(),
            ConnStatus::Connected => "Connected".into(),
            ConnStatus::Reconnecting { attempt } => format!("Reconnecting ({attempt})"),
            ConnStatus::Failed(e) => format!("Error: {e}"),
        }
    }

    /// Is a connection wanted (connected, connecting, or retrying)?
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            ConnStatus::Connected | ConnStatus::Connecting | ConnStatus::Reconnecting { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatLine {
    pub channel: ChannelId,
    pub from_nick: String,
    /// Empty on image lines.
    pub text: String,
    pub ts_ms: u64,
    pub image: Option<ChatImage>,
}

/// An image chat line: `id` keys `ClientSession::images` (and the GUI texture cache);
/// `w`×`h` are the dimensions as sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatImage {
    pub id: u64,
    pub w: u32,
    pub h: u32,
}

/// Latest frame of one sharer; `generation` changes with every new frame.
#[derive(Debug, Clone)]
pub struct ScreenView {
    pub image: Arc<ColorImage>,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    Join(ChannelId),
    Create(String),
    Rename(ChannelId, String),
    Delete(ChannelId),
    Chat(String),
    SetScreenShare(bool),
    /// Start (`true`) or stop watching a sharer in my channel.
    Watch(SessionId, bool),
    ToggleMute,
    ToggleDeafen,
    Ping(u64),
}

#[derive(Debug, Clone, Default)]
pub struct ClientSession {
    pub status: ConnStatus,
    pub my_id: Option<SessionId>,
    pub server_name: String,
    pub motd: String,
    pub default_channel: Option<ChannelId>,
    pub channels: BTreeMap<ChannelId, ChannelInfo>,
    pub users: BTreeMap<SessionId, UserInfo>,
    pub chat: Vec<ChatLine>,
    pub muted: bool,
    pub deafened: bool,
    pub muted_before_deafen: bool,
    pub last_voice: HashMap<SessionId, Instant>,
    pub errors: VecDeque<String>,
    /// Round-trip time of the last answered ping, in ms (set by `AppModel`, which
    /// reads the clock; `apply` stays pure).
    pub ping_ms: Option<u64>,
    /// Decoded chat images by `ChatImage::id`, at most `MAX_IMAGES`.
    pub images: BTreeMap<u64, Arc<ColorImage>>,
    /// Latest frame per sharer in my channel.
    pub screens: BTreeMap<SessionId, ScreenView>,
    /// From Welcome; 0 until then (see `image_cap`).
    pub max_image_bytes: u32,
    /// Server screen-share caps from Welcome (0 = no cap).
    pub max_screen_fps: u32,
    pub max_screen_height: u32,
    /// From Welcome, clamped; balanced until then.
    pub voice_tuning: VoiceTuning,
    /// The server says I am sharing my screen.
    pub sharing: bool,
    /// Sharers I opted in to watch (opt-in is per share).
    pub watching: BTreeSet<SessionId>,
    /// How many users watch my share, as last told by the server.
    pub watchers: u32,
    /// Bumped whenever my watcher count rises, so capture sends a fresh frame.
    pub watch_epoch: u64,
    /// Every watcher of my share decodes H.264 (and there is at least one).
    pub watchers_h264: bool,
}

/// Human text for each error code.
pub fn error_text(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::BadVersion => "Protocol version mismatch",
        ErrorCode::BadPassword => "Wrong server password",
        ErrorCode::NickTaken => "Nickname already in use",
        ErrorCode::BadNick => "Invalid nickname",
        ErrorCode::ServerFull => "Server is full",
        ErrorCode::NotHandshaken => "Not logged in yet",
        ErrorCode::NoSuchChannel => "No such channel",
        ErrorCode::ChannelNameTaken => "Channel name already in use",
        ErrorCode::BadChannelName => "Invalid channel name",
        ErrorCode::NotAllowed => "Not allowed",
        ErrorCode::ChatTooLong => "Message too long",
        ErrorCode::SlowConsumer => "Disconnected: too slow",
        ErrorCode::HelloTimeout => "Login timed out",
        ErrorCode::ImageTooLarge => "Image too large",
    }
}

/// `HH:MM:SS UTC` for a unix-ms timestamp.
pub fn format_ts_utc(ts_ms: u64) -> String {
    let secs = ts_ms / 1000;
    let day = secs % 86_400;
    format!(
        "{:02}:{:02}:{:02} UTC",
        day / 3600,
        (day % 3600) / 60,
        day % 60
    )
}

impl ClientSession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn my_channel(&self) -> Option<ChannelId> {
        self.my_id
            .and_then(|id| self.users.get(&id))
            .map(|u| u.channel)
    }

    /// Largest image body to send: the server's limit, or 4 MiB before Welcome.
    pub fn image_cap(&self) -> u32 {
        if self.max_image_bytes == 0 {
            DEFAULT_IMAGE_CAP
        } else {
            self.max_image_bytes
        }
    }

    /// The server's screen-share caps `(fps, height)`; 0 = no cap.
    pub fn screen_caps(&self) -> (u32, u32) {
        (self.max_screen_fps, self.max_screen_height)
    }

    fn nick_or_id(&self, id: SessionId) -> String {
        self.nick_of(id)
            .map(str::to_string)
            .unwrap_or_else(|| format!("user {}", id.0))
    }

    /// Add an image line of `w`×`h` (the size as sent) and keep the decoded image,
    /// evicting the oldest past the cap. `None` (pixels already dropped by the
    /// network inbox) still adds the line, which then reads "[image expired]".
    #[allow(clippy::too_many_arguments)]
    pub fn on_image(
        &mut self,
        id: u64,
        from: SessionId,
        channel: ChannelId,
        ts_ms: u64,
        w: u32,
        h: u32,
        image: Option<Arc<ColorImage>>,
    ) {
        let from_nick = self.nick_or_id(from);
        self.chat.push(ChatLine {
            channel,
            from_nick,
            text: String::new(),
            ts_ms,
            image: Some(ChatImage { id, w, h }),
        });
        let Some(image) = image else {
            return;
        };
        self.images.insert(id, image);
        while self.images.len() > MAX_IMAGES {
            self.images.pop_first();
        }
    }

    /// Keep a screen frame if it comes from someone else sharing in my channel
    /// whom I watch.
    pub fn on_screen_frame(&mut self, from: SessionId, image: Arc<ColorImage>) {
        if Some(from) == self.my_id || !self.watching.contains(&from) {
            return;
        }
        let Some(mine) = self.my_channel() else {
            return;
        };
        let ok = self
            .users
            .get(&from)
            .is_some_and(|u| u.sharing && u.channel == mine);
        if !ok {
            return;
        }
        let generation = self.screens.get(&from).map_or(1, |v| v.generation + 1);
        self.screens.insert(from, ScreenView { image, generation });
    }

    /// Sharers in my channel other than me, in id order.
    pub fn sharers_here(&self) -> Vec<&UserInfo> {
        self.users.values().filter(|u| self.can_watch(u)).collect()
    }

    /// `user` shares in my channel and is not me: the one gate for watching (menu, LIVE badge, tiles).
    pub fn can_watch(&self, user: &UserInfo) -> bool {
        user.sharing && Some(user.id) != self.my_id && self.my_channel() == Some(user.channel)
    }

    /// Sharers in my channel I watch, in id order.
    pub fn watched_here(&self) -> Vec<&UserInfo> {
        self.sharers_here()
            .into_iter()
            .filter(|u| self.watching.contains(&u.id))
            .collect()
    }

    /// Stop watching `user` locally (no wire message).
    fn forget_watch(&mut self, user: SessionId) {
        self.watching.remove(&user);
        self.screens.remove(&user);
    }

    pub fn nick_of(&self, id: SessionId) -> Option<&str> {
        self.users.get(&id).map(|u| u.nick.as_str())
    }

    pub fn users_in(&self, ch: ChannelId) -> Vec<&UserInfo> {
        self.users.values().filter(|u| u.channel == ch).collect()
    }

    /// Chat lines of one channel.
    pub fn chat_in(&self, ch: ChannelId) -> Vec<&ChatLine> {
        self.chat.iter().filter(|l| l.channel == ch).collect()
    }

    fn push_error(&mut self, e: String) {
        self.errors.push_back(e);
        while self.errors.len() > MAX_ERRORS {
            self.errors.pop_front();
        }
    }

    /// Reset per-connection state (keeps chat history and own mute state).
    pub fn reset_connection(&mut self) {
        self.my_id = None;
        self.ping_ms = None;
        self.channels.clear();
        self.users.clear();
        self.last_voice.clear();
        self.default_channel = None;
        self.screens.clear();
        self.sharing = false;
        self.watching.clear();
        self.watchers = 0;
        self.watchers_h264 = false;
    }

    pub fn apply(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Welcome {
                session_id,
                server_name,
                motd,
                default_channel,
                channels,
                users,
                max_image_bytes,
                max_screen_fps,
                max_screen_height,
                voice_tuning,
            } => {
                self.my_id = Some(session_id);
                self.max_image_bytes = max_image_bytes;
                self.max_screen_fps = max_screen_fps;
                self.max_screen_height = max_screen_height;
                self.voice_tuning = voice_tuning.clamped();
                self.server_name = server_name;
                self.motd = motd;
                self.default_channel = Some(default_channel);
                self.channels = channels.into_iter().map(|c| (c.id, c)).collect();
                self.users = users.into_iter().map(|u| (u.id, u)).collect();
                self.sharing = self.users.get(&session_id).is_some_and(|u| u.sharing);
                self.screens.clear();
                self.watching.clear();
                self.watchers = 0;
                self.watchers_h264 = false;
                self.last_voice.clear();
                self.status = ConnStatus::Connected;
            }
            ServerMsg::Error { code, msg } => {
                let text = if code == ErrorCode::BadVersion {
                    let base = crate::update::bad_version_text(error_text(code));
                    if msg.is_empty() {
                        base
                    } else {
                        format!("{base} ({msg})")
                    }
                } else if msg.is_empty() {
                    error_text(code).to_string()
                } else {
                    format!("{} ({msg})", error_text(code))
                };
                self.push_error(text);
            }
            ServerMsg::UserJoined(u) => {
                self.users.insert(u.id, u);
            }
            ServerMsg::UserLeft { user } => {
                self.users.remove(&user);
                self.last_voice.remove(&user);
                self.forget_watch(user);
            }
            ServerMsg::UserMoved { user, channel } => {
                if let Some(u) = self.users.get_mut(&user) {
                    u.channel = channel;
                }
                if Some(user) == self.my_id {
                    self.screens.clear();
                    self.watching.clear();
                } else {
                    self.forget_watch(user);
                }
            }
            ServerMsg::UserState {
                user,
                muted,
                deafened,
            } => {
                if let Some(u) = self.users.get_mut(&user) {
                    u.muted = muted;
                    u.deafened = deafened;
                }
            }
            ServerMsg::ChannelAdded(c) => {
                self.channels.insert(c.id, c);
            }
            ServerMsg::ChannelRemoved { channel } => {
                self.channels.remove(&channel);
            }
            ServerMsg::ChannelRenamed { channel, name } => {
                if let Some(c) = self.channels.get_mut(&channel) {
                    c.name = name;
                }
            }
            ServerMsg::Chat {
                from,
                channel,
                text,
                ts_ms,
            } => {
                let from_nick = self.nick_or_id(from);
                self.chat.push(ChatLine {
                    channel,
                    from_nick,
                    text,
                    ts_ms,
                    image: None,
                });
            }
            // The RTT is computed by `AppModel` (it owns the clock).
            ServerMsg::Pong { .. } => {}
            ServerMsg::ScreenShare { user, on } => {
                if let Some(u) = self.users.get_mut(&user) {
                    u.sharing = on;
                }
                if !on {
                    self.forget_watch(user);
                }
                if Some(user) == self.my_id {
                    self.sharing = on;
                    if !on {
                        self.watchers = 0;
                        self.watchers_h264 = false;
                    }
                }
            }
            ServerMsg::ScreenWatchers { count, h264 } => {
                if count > self.watchers {
                    self.watch_epoch += 1;
                }
                self.watchers = count;
                self.watchers_h264 = h264;
            }
        }
    }

    fn voice_state(&self) -> ClientMsg {
        ClientMsg::SetVoiceState {
            muted: self.muted,
            deafened: self.deafened,
        }
    }

    /// Hello-follow-up messages to restore voice state after (re)connect.
    pub fn restore_msgs(&self) -> Vec<ClientMsg> {
        if self.muted || self.deafened {
            vec![self.voice_state()]
        } else {
            vec![]
        }
    }

    /// Translate a UI action into wire messages (and local state changes).
    pub fn intent(&mut self, a: UiAction) -> Vec<ClientMsg> {
        match a {
            UiAction::Join(channel) => {
                if self.my_channel() == Some(channel) {
                    return vec![];
                }
                vec![ClientMsg::Join { channel }]
            }
            UiAction::Create(name) => {
                let name = name.trim().to_string();
                if name.is_empty() {
                    self.push_error(error_text(ErrorCode::BadChannelName).to_string());
                    return vec![];
                }
                vec![ClientMsg::CreateChannel { name }]
            }
            UiAction::Rename(channel, name) => {
                let name = name.trim().to_string();
                if name.is_empty() {
                    self.push_error(error_text(ErrorCode::BadChannelName).to_string());
                    return vec![];
                }
                vec![ClientMsg::RenameChannel { channel, name }]
            }
            UiAction::Delete(channel) => vec![ClientMsg::DeleteChannel { channel }],
            UiAction::Chat(text) => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    return vec![];
                }
                if text.chars().count() > MAX_CHAT {
                    self.push_error(error_text(ErrorCode::ChatTooLong).to_string());
                    return vec![];
                }
                vec![ClientMsg::Chat { text }]
            }
            UiAction::ToggleMute => {
                if self.deafened {
                    // Unmuting while deafened also undeafens.
                    self.deafened = false;
                    self.muted = false;
                } else {
                    self.muted = !self.muted;
                }
                vec![self.voice_state()]
            }
            UiAction::ToggleDeafen => {
                if self.deafened {
                    self.deafened = false;
                    self.muted = self.muted_before_deafen;
                } else {
                    self.muted_before_deafen = self.muted;
                    self.deafened = true;
                    self.muted = true;
                }
                vec![self.voice_state()]
            }
            UiAction::Ping(nonce) => vec![ClientMsg::Ping { nonce }],
            UiAction::SetScreenShare(on) => vec![ClientMsg::SetScreenShare { on }],
            UiAction::Watch(user, on) => {
                if on {
                    self.watching.insert(user);
                } else {
                    self.forget_watch(user);
                }
                vec![ClientMsg::WatchScreen { user, on }]
            }
        }
    }

    pub fn note_voice(&mut self, speaker: SessionId, now: Instant) {
        self.last_voice.insert(speaker, now);
    }

    pub fn is_speaking(&self, user: SessionId, now: Instant) -> bool {
        self.last_voice
            .get(&user)
            .is_some_and(|t| now.saturating_duration_since(*t) < SPEAKING_WINDOW)
    }

    /// Anyone speaking right now (drives repaint scheduling).
    pub fn anyone_speaking(&self, now: Instant) -> bool {
        self.last_voice
            .values()
            .any(|t| now.saturating_duration_since(*t) < SPEAKING_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vc_proto::all_server_samples;

    fn ch(id: u32, name: &str, permanent: bool) -> ChannelInfo {
        ChannelInfo {
            id: ChannelId(id),
            name: name.into(),
            permanent,
        }
    }

    fn user(id: u32, nick: &str, channel: u32) -> UserInfo {
        UserInfo {
            id: SessionId(id),
            nick: nick.into(),
            channel: ChannelId(channel),
            muted: false,
            deafened: false,
            sharing: false,
        }
    }

    fn welcomed() -> ClientSession {
        let mut s = ClientSession::new();
        s.apply(ServerMsg::Welcome {
            session_id: SessionId(1),
            server_name: "Srv".into(),
            motd: "motd!".into(),
            default_channel: ChannelId(1),
            channels: vec![ch(1, "Lobby", true), ch(2, "AFK", true)],
            users: vec![user(1, "me", 1), user(2, "bob", 1)],
            max_image_bytes: 1234,
            max_screen_fps: 30,
            max_screen_height: 720,
            voice_tuning: VoiceTuning::default(),
        });
        s
    }

    #[test]
    fn session_apply_welcome() {
        let s = welcomed();
        assert_eq!(s.status, ConnStatus::Connected);
        assert_eq!(s.my_id, Some(SessionId(1)));
        assert_eq!(s.server_name, "Srv");
        assert_eq!(s.motd, "motd!");
        assert_eq!(s.default_channel, Some(ChannelId(1)));
        assert_eq!(s.channels.len(), 2);
        assert_eq!(s.users.len(), 2);
        assert_eq!(s.my_channel(), Some(ChannelId(1)));
        assert_eq!(s.screen_caps(), (30, 720));
        assert_eq!(s.voice_tuning, VoiceTuning::default());
    }

    #[test]
    fn session_welcome_voice_tuning_clamped() {
        let welcome = |t: VoiceTuning| ServerMsg::Welcome {
            session_id: SessionId(1),
            server_name: "Srv".into(),
            motd: String::new(),
            default_channel: ChannelId(1),
            channels: vec![ch(1, "Lobby", true)],
            users: vec![user(1, "me", 1)],
            max_image_bytes: 0,
            max_screen_fps: 0,
            max_screen_height: 0,
            voice_tuning: t,
        };
        let mut s = ClientSession::new();
        assert_eq!(s.voice_tuning.preset, "balanced", "before Welcome");
        s.apply(welcome(VoiceTuning {
            preset: "weird".into(),
            jitter_frames: 0,
            playback_keep_frames: 200,
        }));
        assert_eq!(s.voice_tuning.jitter_frames, 1);
        assert_eq!(
            s.voice_tuning.playback_keep_frames,
            vc_proto::MAX_VOICE_FRAMES
        );
        let competitive = VoiceTuning {
            preset: "competitive".into(),
            jitter_frames: 2,
            playback_keep_frames: 2,
        };
        s.apply(welcome(competitive.clone()));
        assert_eq!(s.voice_tuning, competitive);
    }

    #[test]
    fn session_apply_user_joined_left() {
        let mut s = welcomed();
        s.apply(ServerMsg::UserJoined(user(3, "carol", 2)));
        assert_eq!(s.nick_of(SessionId(3)), Some("carol"));
        s.note_voice(SessionId(3), Instant::now());
        s.apply(ServerMsg::UserLeft { user: SessionId(3) });
        assert_eq!(s.nick_of(SessionId(3)), None);
        assert!(!s.last_voice.contains_key(&SessionId(3)));
    }

    #[test]
    fn session_apply_user_moved() {
        let mut s = welcomed();
        s.apply(ServerMsg::UserMoved {
            user: SessionId(1),
            channel: ChannelId(2),
        });
        assert_eq!(s.my_channel(), Some(ChannelId(2)));
        assert_eq!(s.users_in(ChannelId(1)).len(), 1);
    }

    #[test]
    fn session_apply_user_state() {
        let mut s = welcomed();
        s.apply(ServerMsg::UserState {
            user: SessionId(2),
            muted: true,
            deafened: true,
        });
        let b = &s.users[&SessionId(2)];
        assert!(b.muted && b.deafened);
    }

    #[test]
    fn session_apply_channel_added_removed_renamed() {
        let mut s = welcomed();
        s.apply(ServerMsg::ChannelAdded(ch(5, "Temp", false)));
        assert_eq!(s.channels[&ChannelId(5)].name, "Temp");
        s.apply(ServerMsg::ChannelRenamed {
            channel: ChannelId(5),
            name: "Temp2".into(),
        });
        assert_eq!(s.channels[&ChannelId(5)].name, "Temp2");
        s.apply(ServerMsg::ChannelRemoved {
            channel: ChannelId(5),
        });
        assert!(!s.channels.contains_key(&ChannelId(5)));
    }

    #[test]
    fn session_apply_chat() {
        let mut s = welcomed();
        s.apply(ServerMsg::Chat {
            from: SessionId(2),
            channel: ChannelId(1),
            text: "hi".into(),
            ts_ms: 5,
        });
        s.apply(ServerMsg::Chat {
            from: SessionId(99),
            channel: ChannelId(2),
            text: "ghost".into(),
            ts_ms: 6,
        });
        assert_eq!(
            s.chat_in(ChannelId(1)),
            vec![&ChatLine {
                channel: ChannelId(1),
                from_nick: "bob".into(),
                text: "hi".into(),
                ts_ms: 5,
                image: None,
            }]
        );
        assert_eq!(s.chat_in(ChannelId(2))[0].from_nick, "user 99");
    }

    #[test]
    fn session_apply_error() {
        let mut s = welcomed();
        s.apply(ServerMsg::Error {
            code: ErrorCode::NoSuchChannel,
            msg: String::new(),
        });
        assert_eq!(s.errors.back().unwrap(), "No such channel");
        for _ in 0..30 {
            s.apply(ServerMsg::Error {
                code: ErrorCode::NotAllowed,
                msg: "x".into(),
            });
        }
        assert_eq!(s.errors.len(), MAX_ERRORS);
        assert_eq!(s.errors.back().unwrap(), "Not allowed (x)");
    }

    #[test]
    fn session_apply_pong() {
        let mut s = welcomed();
        s.ping_ms = Some(5);
        s.apply(ServerMsg::Pong { nonce: 77 });
        assert_eq!(s.ping_ms, Some(5), "apply reads no clock");
    }

    #[test]
    fn reset_connection_clears_ping() {
        let mut s = welcomed();
        s.ping_ms = Some(12);
        s.reset_connection();
        assert_eq!(s.ping_ms, None);
    }

    #[test]
    fn session_apply_covers_all_variants() {
        let mut s = ClientSession::new();
        for m in all_server_samples() {
            s.apply(m);
        }
        let mut s = welcomed();
        for m in all_server_samples() {
            s.apply(m);
        }
    }

    #[test]
    fn intent_join() {
        let mut s = welcomed();
        assert_eq!(
            s.intent(UiAction::Join(ChannelId(2))),
            vec![ClientMsg::Join {
                channel: ChannelId(2)
            }]
        );
        assert!(
            s.intent(UiAction::Join(ChannelId(1))).is_empty(),
            "already there"
        );
    }

    #[test]
    fn intent_create_rename_delete() {
        let mut s = welcomed();
        assert_eq!(
            s.intent(UiAction::Create(" Raid ".into())),
            vec![ClientMsg::CreateChannel {
                name: "Raid".into()
            }]
        );
        assert!(s.intent(UiAction::Create("  ".into())).is_empty());
        assert_eq!(
            s.errors.pop_back().as_deref(),
            Some(error_text(ErrorCode::BadChannelName))
        );
        assert_eq!(
            s.intent(UiAction::Rename(ChannelId(3), "New".into())),
            vec![ClientMsg::RenameChannel {
                channel: ChannelId(3),
                name: "New".into()
            }]
        );
        assert!(
            s.intent(UiAction::Rename(ChannelId(3), "".into()))
                .is_empty()
        );
        assert_eq!(
            s.errors.pop_back().as_deref(),
            Some(error_text(ErrorCode::BadChannelName))
        );
        assert!(s.errors.is_empty());
        assert_eq!(
            s.intent(UiAction::Delete(ChannelId(3))),
            vec![ClientMsg::DeleteChannel {
                channel: ChannelId(3)
            }]
        );
        assert_eq!(
            s.intent(UiAction::Ping(4)),
            vec![ClientMsg::Ping { nonce: 4 }]
        );
        assert_eq!(
            s.intent(UiAction::SetScreenShare(true)),
            vec![ClientMsg::SetScreenShare { on: true }]
        );
        assert_eq!(
            s.intent(UiAction::SetScreenShare(false)),
            vec![ClientMsg::SetScreenShare { on: false }]
        );
    }

    #[test]
    fn intent_chat_rejects_empty_and_long() {
        let mut s = welcomed();
        assert!(s.intent(UiAction::Chat("   ".into())).is_empty());
        assert!(s.intent(UiAction::Chat("x".repeat(501))).is_empty());
        assert_eq!(s.errors.back().unwrap(), "Message too long");
        assert_eq!(
            s.intent(UiAction::Chat("x".repeat(500))),
            vec![ClientMsg::Chat {
                text: "x".repeat(500)
            }]
        );
    }

    #[test]
    fn intent_toggle_mute() {
        let mut s = welcomed();
        assert_eq!(
            s.intent(UiAction::ToggleMute),
            vec![ClientMsg::SetVoiceState {
                muted: true,
                deafened: false
            }]
        );
        assert!(s.muted);
        assert_eq!(
            s.intent(UiAction::ToggleMute),
            vec![ClientMsg::SetVoiceState {
                muted: false,
                deafened: false
            }]
        );
        assert!(!s.muted);
    }

    #[test]
    fn intent_deafen_implies_mute_and_restores() {
        let mut s = welcomed();
        // Not muted before: deafen → muted+deafened, undeafen → unmuted.
        assert_eq!(
            s.intent(UiAction::ToggleDeafen),
            vec![ClientMsg::SetVoiceState {
                muted: true,
                deafened: true
            }]
        );
        assert_eq!(
            s.intent(UiAction::ToggleDeafen),
            vec![ClientMsg::SetVoiceState {
                muted: false,
                deafened: false
            }]
        );
        // Muted before: undeafen keeps mute.
        s.intent(UiAction::ToggleMute);
        s.intent(UiAction::ToggleDeafen);
        assert!(s.muted && s.deafened);
        s.intent(UiAction::ToggleDeafen);
        assert!(s.muted && !s.deafened);
        // Unmute while deafened clears both.
        s.intent(UiAction::ToggleMute);
        s.intent(UiAction::ToggleDeafen);
        s.intent(UiAction::ToggleMute);
        assert!(!s.muted && !s.deafened);
    }

    #[test]
    fn speaking_indicator_expires() {
        let mut s = welcomed();
        let t = Instant::now();
        assert!(!s.is_speaking(SessionId(2), t));
        s.note_voice(SessionId(2), t);
        assert!(s.is_speaking(SessionId(2), t + Duration::from_millis(299)));
        assert!(s.anyone_speaking(t + Duration::from_millis(100)));
        assert!(!s.is_speaking(SessionId(2), t + Duration::from_millis(300)));
        assert!(!s.anyone_speaking(t + Duration::from_millis(301)));
    }

    #[test]
    fn format_ts_utc_known_value() {
        // 2023-11-14T22:13:20.123Z
        assert_eq!(format_ts_utc(1_700_000_000_123), "22:13:20 UTC");
        assert_eq!(format_ts_utc(0), "00:00:00 UTC");
        assert_eq!(format_ts_utc(86_399_999), "23:59:59 UTC");
    }

    #[test]
    fn error_text_for_every_code() {
        let mut seen = std::collections::HashSet::new();
        for c in ErrorCode::all() {
            let t = error_text(c);
            assert!(!t.is_empty());
            assert!(seen.insert(t), "duplicate text {t}");
        }
        assert_eq!(seen.len(), 14);
        assert_eq!(error_text(ErrorCode::ImageTooLarge), "Image too large");
    }

    fn img(w: usize, h: usize) -> Arc<ColorImage> {
        Arc::new(
            ColorImage::filled([w, h], egui::Color32::RED)
                .with_source_size(egui::vec2(w as f32 * 2.0, h as f32 * 2.0)),
        )
    }

    #[test]
    fn welcome_sets_image_cap() {
        assert_eq!(ClientSession::new().image_cap(), DEFAULT_IMAGE_CAP);
        assert_eq!(welcomed().image_cap(), 1234);
    }

    #[test]
    fn image_lines_and_eviction() {
        let mut s = welcomed();
        s.on_image(1, SessionId(2), ChannelId(1), 9, 8, 6, Some(img(4, 3)));
        let line = s.chat_in(ChannelId(1))[0].clone();
        assert_eq!(line.from_nick, "bob");
        assert_eq!(line.text, "");
        assert_eq!(line.image, Some(ChatImage { id: 1, w: 8, h: 6 }));
        assert!(s.images.contains_key(&1));
        for id in 2..=(MAX_IMAGES as u64 + 1) {
            s.on_image(id, SessionId(2), ChannelId(1), 9, 2, 2, Some(img(1, 1)));
        }
        assert_eq!(s.images.len(), MAX_IMAGES);
        assert!(!s.images.contains_key(&1), "oldest evicted");
        assert!(s.images.contains_key(&(MAX_IMAGES as u64 + 1)));
        // The line stays; the GUI shows it as expired.
        assert_eq!(s.chat_in(ChannelId(1)).len(), MAX_IMAGES + 1);
    }

    #[test]
    fn image_without_pixels_is_expired_line() {
        let mut s = welcomed();
        s.on_image(7, SessionId(2), ChannelId(1), 9, 640, 480, None);
        let line = s.chat_in(ChannelId(1))[0].clone();
        assert_eq!(
            line.image,
            Some(ChatImage {
                id: 7,
                w: 640,
                h: 480
            })
        );
        assert_eq!(line.from_nick, "bob");
        assert!(s.images.is_empty(), "no pixels kept");
    }

    fn share(s: &mut ClientSession, user: u32, on: bool) {
        s.apply(ServerMsg::ScreenShare {
            user: SessionId(user),
            on,
        });
    }

    #[test]
    fn screen_frames_only_from_sharers_in_my_channel() {
        let mut s = welcomed();
        s.apply(ServerMsg::UserJoined(user(3, "carol", 2)));
        // bob is not sharing yet.
        s.on_screen_frame(SessionId(2), img(2, 2));
        assert!(s.screens.is_empty());
        share(&mut s, 2, true);
        share(&mut s, 3, true);
        // Sharing alone is not enough: frames show only once watched.
        s.on_screen_frame(SessionId(2), img(2, 2));
        assert!(s.screens.is_empty(), "not watched yet");
        for u in [1, 2, 3] {
            s.intent(UiAction::Watch(SessionId(u), true));
        }
        s.on_screen_frame(SessionId(2), img(2, 2));
        s.on_screen_frame(SessionId(2), img(2, 2));
        // carol shares in another channel; my own frames are never shown.
        s.on_screen_frame(SessionId(3), img(2, 2));
        share(&mut s, 1, true);
        assert!(s.sharing);
        s.on_screen_frame(SessionId(1), img(2, 2));
        assert_eq!(
            s.screens.keys().copied().collect::<Vec<_>>(),
            [SessionId(2)]
        );
        assert_eq!(s.screens[&SessionId(2)].generation, 2);
        assert_eq!(s.sharers_here().len(), 1);
        // Off clears the view, and a late frame does not bring it back.
        share(&mut s, 2, false);
        assert!(s.screens.is_empty());
        s.on_screen_frame(SessionId(2), img(2, 2));
        assert!(s.screens.is_empty());
        assert!(!s.users[&SessionId(2)].sharing);
    }

    #[test]
    fn can_watch_only_other_sharers_in_my_channel() {
        let mut s = welcomed();
        s.apply(ServerMsg::UserJoined(user(3, "carol", 2)));
        s.apply(ServerMsg::UserJoined(user(4, "dave", 1)));
        share(&mut s, 1, true);
        share(&mut s, 2, true);
        share(&mut s, 3, true);
        let u = |id| s.users[&SessionId(id)].clone();
        assert!(s.can_watch(&u(2)), "sharer in my channel");
        assert!(!s.can_watch(&u(1)), "me");
        assert!(!s.can_watch(&u(3)), "another channel");
        assert!(!s.can_watch(&u(4)), "not sharing");
    }

    #[test]
    fn moving_clears_screens() {
        let mut s = welcomed();
        share(&mut s, 2, true);
        s.intent(UiAction::Watch(SessionId(2), true));
        s.on_screen_frame(SessionId(2), img(2, 2));
        assert_eq!(s.screens.len(), 1);
        s.apply(ServerMsg::UserMoved {
            user: SessionId(1),
            channel: ChannelId(2),
        });
        assert!(s.screens.is_empty());
        assert!(s.watching.is_empty(), "my move clears watching");
        // The sharer leaving also clears their view.
        s.apply(ServerMsg::UserMoved {
            user: SessionId(1),
            channel: ChannelId(1),
        });
        s.on_screen_frame(SessionId(2), img(2, 2));
        assert!(s.screens.is_empty(), "watching does not come back");
        s.intent(UiAction::Watch(SessionId(2), true));
        s.on_screen_frame(SessionId(2), img(2, 2));
        assert_eq!(s.screens.len(), 1);
        s.apply(ServerMsg::UserLeft { user: SessionId(2) });
        assert!(s.screens.is_empty());
        assert!(s.watching.is_empty());
        // Reset clears sharing state too.
        share(&mut s, 1, true);
        s.reset_connection();
        assert!(!s.sharing);
    }

    #[test]
    fn watch_intent_sends_and_tracks() {
        let mut s = welcomed();
        share(&mut s, 2, true);
        assert_eq!(
            s.intent(UiAction::Watch(SessionId(2), true)),
            vec![ClientMsg::WatchScreen {
                user: SessionId(2),
                on: true
            }]
        );
        assert_eq!(s.watched_here().len(), 1);
        s.on_screen_frame(SessionId(2), img(2, 2));
        assert_eq!(s.screens.len(), 1);
        assert_eq!(
            s.intent(UiAction::Watch(SessionId(2), false)),
            vec![ClientMsg::WatchScreen {
                user: SessionId(2),
                on: false
            }]
        );
        assert!(s.screens.is_empty() && s.watching.is_empty());
        s.on_screen_frame(SessionId(2), img(2, 2));
        assert!(s.screens.is_empty(), "unwatched frames are dropped");
    }

    #[test]
    fn watching_cleared_on_share_off_mover_and_reset() {
        let mut s = welcomed();
        s.apply(ServerMsg::UserJoined(user(3, "carol", 1)));
        share(&mut s, 2, true);
        share(&mut s, 3, true);
        s.intent(UiAction::Watch(SessionId(2), true));
        s.intent(UiAction::Watch(SessionId(3), true));
        share(&mut s, 2, false);
        assert_eq!(
            s.watching.iter().copied().collect::<Vec<_>>(),
            [SessionId(3)]
        );
        // A re-share is not watched automatically.
        share(&mut s, 2, true);
        assert_eq!(s.watched_here().len(), 1);
        s.apply(ServerMsg::UserMoved {
            user: SessionId(3),
            channel: ChannelId(2),
        });
        assert!(s.watching.is_empty(), "the mover is dropped");
        s.intent(UiAction::Watch(SessionId(2), true));
        s.reset_connection();
        assert!(s.watching.is_empty());
    }

    #[test]
    fn watcher_count_bumps_epoch_on_rise_and_resets() {
        let mut s = welcomed();
        share(&mut s, 1, true);
        let e0 = s.watch_epoch;
        for count in [1, 0, 1] {
            s.apply(ServerMsg::ScreenWatchers { count, h264: false });
        }
        assert_eq!(s.watchers, 1);
        assert_eq!(s.watch_epoch, e0 + 2, "1->0->1 bumps twice");
        s.apply(ServerMsg::ScreenWatchers {
            count: 1,
            h264: true,
        });
        assert_eq!(s.watch_epoch, e0 + 2, "no rise, no bump");
        assert!(s.watchers_h264);
        // My own share ending resets the count.
        share(&mut s, 1, false);
        assert_eq!(s.watchers, 0);
        share(&mut s, 1, true);
        s.apply(ServerMsg::ScreenWatchers {
            count: 3,
            h264: true,
        });
        s.reset_connection();
        assert_eq!(s.watchers, 0);
        assert!(!s.watchers_h264);
        s.apply(ServerMsg::ScreenWatchers {
            count: 2,
            h264: false,
        });
        let _ = welcomed_into(&mut s);
        assert_eq!(s.watchers, 0, "Welcome resets");
    }

    fn welcomed_into(s: &mut ClientSession) -> &mut ClientSession {
        s.apply(ServerMsg::Welcome {
            session_id: SessionId(1),
            server_name: "Srv".into(),
            motd: String::new(),
            default_channel: ChannelId(1),
            channels: vec![ch(1, "Lobby", true)],
            users: vec![user(1, "me", 1)],
            max_image_bytes: 0,
            max_screen_fps: 0,
            max_screen_height: 0,
            voice_tuning: VoiceTuning::default(),
        });
        s
    }
}
