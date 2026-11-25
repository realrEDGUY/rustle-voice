use serde::{Deserialize, Serialize};

use crate::ids::{ChannelId, SessionId};

/// Largest jitter depth / playback keep (in 20 ms frames) a client accepts from Welcome.
pub const MAX_VOICE_FRAMES: u8 = 8;
/// Longest preset display name a client keeps (chars).
pub const MAX_PRESET_NAME: usize = 32;

/// Voice tuning chosen by the server's `voice_preset`; numbers drive behaviour,
/// `preset` is display only. Unknown presets need no client release: the client
/// only reads the numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceTuning {
    /// Preset name, for display.
    pub preset: String,
    /// Frames a speaker's jitter buffer holds before playback starts.
    pub jitter_frames: u8,
    /// Playback backlog (frames) the client tolerates before trimming.
    pub playback_keep_frames: u8,
}

impl Default for VoiceTuning {
    fn default() -> Self {
        Self {
            preset: "balanced".into(),
            jitter_frames: 3,
            playback_keep_frames: 3,
        }
    }
}

impl VoiceTuning {
    /// Clamp both numbers to `1..=MAX_VOICE_FRAMES` and truncate the name to
    /// `MAX_PRESET_NAME` chars.
    pub fn clamped(mut self) -> Self {
        self.jitter_frames = self.jitter_frames.clamp(1, MAX_VOICE_FRAMES);
        self.playback_keep_frames = self.playback_keep_frames.clamp(1, MAX_VOICE_FRAMES);
        if self.preset.chars().count() > MAX_PRESET_NAME {
            self.preset = self.preset.chars().take(MAX_PRESET_NAME).collect();
        }
        self
    }
}

/// Messages a client sends on the control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ClientMsg {
    Hello {
        version: u32,
        nick: String,
        password: Option<String>,
        /// This client can decode H.264 screen frames.
        h264: bool,
    },
    Join {
        channel: ChannelId,
    },
    CreateChannel {
        name: String,
    },
    RenameChannel {
        channel: ChannelId,
        name: String,
    },
    DeleteChannel {
        channel: ChannelId,
    },
    Chat {
        text: String,
    },
    SetVoiceState {
        muted: bool,
        deafened: bool,
    },
    Ping {
        nonce: u64,
    },
    /// Start or stop sharing the primary screen with the sender's channel.
    SetScreenShare {
        on: bool,
    },
    /// Subscribe to (`on`) or unsubscribe from a sharer's frames in my channel.
    WatchScreen {
        user: SessionId,
        on: bool,
    },
}

/// Messages a server sends on the control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ServerMsg {
    Welcome {
        session_id: SessionId,
        server_name: String,
        motd: String,
        default_channel: ChannelId,
        channels: Vec<ChannelInfo>,
        users: Vec<UserInfo>,
        /// Largest chat image body this server accepts.
        max_image_bytes: u32,
        /// Highest screen-share frame rate this server allows (0 = no cap).
        max_screen_fps: u32,
        /// Highest screen-share frame height this server allows (0 = no cap).
        max_screen_height: u32,
        /// Voice latency tuning from the server's `voice_preset`. Absent from
        /// older servers, which means balanced.
        #[serde(default)]
        voice_tuning: VoiceTuning,
    },
    Error {
        code: ErrorCode,
        msg: String,
    },
    UserJoined(UserInfo),
    UserLeft {
        user: SessionId,
    },
    UserMoved {
        user: SessionId,
        channel: ChannelId,
    },
    UserState {
        user: SessionId,
        muted: bool,
        deafened: bool,
    },
    ChannelAdded(ChannelInfo),
    ChannelRemoved {
        channel: ChannelId,
    },
    ChannelRenamed {
        channel: ChannelId,
        name: String,
    },
    Chat {
        from: SessionId,
        channel: ChannelId,
        text: String,
        ts_ms: u64,
    },
    Pong {
        nonce: u64,
    },
    /// A user started or stopped sharing their screen. Sent to every user.
    ScreenShare {
        user: SessionId,
        on: bool,
    },
    /// Sent to a sharer when its watcher count, or whether every watcher can
    /// decode H.264, changes.
    ScreenWatchers {
        count: u32,
        /// True when there is at least one watcher and all of them decode H.264.
        h264: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: ChannelId,
    pub name: String,
    pub permanent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: SessionId,
    pub nick: String,
    pub channel: ChannelId,
    pub muted: bool,
    pub deafened: bool,
    pub sharing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorCode {
    BadVersion,
    BadPassword,
    NickTaken,
    BadNick,
    ServerFull,
    NotHandshaken,
    NoSuchChannel,
    ChannelNameTaken,
    BadChannelName,
    NotAllowed,
    ChatTooLong,
    SlowConsumer,
    HelloTimeout,
    ImageTooLarge,
}

impl ErrorCode {
    /// Every code, built through an exhaustive match so a new variant must be listed.
    pub fn all() -> Vec<ErrorCode> {
        fn check(c: ErrorCode) -> ErrorCode {
            match c {
                ErrorCode::BadVersion
                | ErrorCode::BadPassword
                | ErrorCode::NickTaken
                | ErrorCode::BadNick
                | ErrorCode::ServerFull
                | ErrorCode::NotHandshaken
                | ErrorCode::NoSuchChannel
                | ErrorCode::ChannelNameTaken
                | ErrorCode::BadChannelName
                | ErrorCode::NotAllowed
                | ErrorCode::ChatTooLong
                | ErrorCode::SlowConsumer
                | ErrorCode::HelloTimeout
                | ErrorCode::ImageTooLarge => c,
            }
        }
        [
            ErrorCode::BadVersion,
            ErrorCode::BadPassword,
            ErrorCode::NickTaken,
            ErrorCode::BadNick,
            ErrorCode::ServerFull,
            ErrorCode::NotHandshaken,
            ErrorCode::NoSuchChannel,
            ErrorCode::ChannelNameTaken,
            ErrorCode::BadChannelName,
            ErrorCode::NotAllowed,
            ErrorCode::ChatTooLong,
            ErrorCode::SlowConsumer,
            ErrorCode::HelloTimeout,
            ErrorCode::ImageTooLarge,
        ]
        .into_iter()
        .map(check)
        .collect()
    }

    /// Numeric code used as the QUIC application close code.
    pub fn as_u32(self) -> u32 {
        match self {
            ErrorCode::BadVersion => 1,
            ErrorCode::BadPassword => 2,
            ErrorCode::NickTaken => 3,
            ErrorCode::BadNick => 4,
            ErrorCode::ServerFull => 5,
            ErrorCode::NotHandshaken => 6,
            ErrorCode::NoSuchChannel => 7,
            ErrorCode::ChannelNameTaken => 8,
            ErrorCode::BadChannelName => 9,
            ErrorCode::NotAllowed => 10,
            ErrorCode::ChatTooLong => 11,
            ErrorCode::SlowConsumer => 12,
            ErrorCode::HelloTimeout => 13,
            ErrorCode::ImageTooLarge => 14,
        }
    }

    pub fn from_u32(v: u32) -> Option<ErrorCode> {
        ErrorCode::all().into_iter().find(|c| c.as_u32() == v)
    }
}

fn sample_channel() -> ChannelInfo {
    ChannelInfo {
        id: ChannelId(1),
        name: "Lobby".into(),
        permanent: true,
    }
}

fn sample_user() -> UserInfo {
    UserInfo {
        id: SessionId(7),
        nick: "alice".into(),
        channel: ChannelId(1),
        muted: true,
        deafened: false,
        sharing: false,
    }
}

/// One sample per `ClientMsg` variant. The inner `match` is exhaustive, so adding
/// a variant fails to compile until a sample is added here.
pub fn all_client_samples() -> Vec<ClientMsg> {
    let samples = vec![
        ClientMsg::Hello {
            version: crate::PROTOCOL_VERSION,
            nick: "alice".into(),
            password: Some("pw".into()),
            h264: true,
        },
        ClientMsg::Join {
            channel: ChannelId(2),
        },
        ClientMsg::CreateChannel {
            name: "Raid".into(),
        },
        ClientMsg::RenameChannel {
            channel: ChannelId(3),
            name: "Raid 2".into(),
        },
        ClientMsg::DeleteChannel {
            channel: ChannelId(3),
        },
        ClientMsg::Chat {
            text: "hi \"there\" ünïcode".into(),
        },
        ClientMsg::SetVoiceState {
            muted: true,
            deafened: false,
        },
        ClientMsg::Ping { nonce: u64::MAX },
        ClientMsg::SetScreenShare { on: true },
        ClientMsg::WatchScreen {
            user: SessionId(7),
            on: true,
        },
    ];
    let mut seen = [false; 10];
    for m in &samples {
        let idx = match m {
            ClientMsg::Hello { .. } => 0,
            ClientMsg::Join { .. } => 1,
            ClientMsg::CreateChannel { .. } => 2,
            ClientMsg::RenameChannel { .. } => 3,
            ClientMsg::DeleteChannel { .. } => 4,
            ClientMsg::Chat { .. } => 5,
            ClientMsg::SetVoiceState { .. } => 6,
            ClientMsg::Ping { .. } => 7,
            ClientMsg::SetScreenShare { .. } => 8,
            ClientMsg::WatchScreen { .. } => 9,
        };
        seen[idx] = true;
    }
    assert!(
        seen.iter().all(|s| *s),
        "every ClientMsg variant needs a sample"
    );
    samples
}

/// One sample per `ServerMsg` variant (plus one `Error` per `ErrorCode`).
pub fn all_server_samples() -> Vec<ServerMsg> {
    let mut samples = vec![
        ServerMsg::Welcome {
            session_id: SessionId(7),
            server_name: "Test".into(),
            motd: "Welcome!".into(),
            default_channel: ChannelId(1),
            channels: vec![sample_channel()],
            users: vec![sample_user()],
            max_image_bytes: 4 * 1024 * 1024,
            max_screen_fps: 30,
            max_screen_height: 720,
            voice_tuning: VoiceTuning {
                preset: "competitive".into(),
                jitter_frames: 2,
                playback_keep_frames: 2,
            },
        },
        ServerMsg::UserJoined(sample_user()),
        ServerMsg::UserLeft { user: SessionId(7) },
        ServerMsg::UserMoved {
            user: SessionId(7),
            channel: ChannelId(2),
        },
        ServerMsg::UserState {
            user: SessionId(7),
            muted: true,
            deafened: true,
        },
        ServerMsg::ChannelAdded(ChannelInfo {
            id: ChannelId(5),
            name: "Temp".into(),
            permanent: false,
        }),
        ServerMsg::ChannelRemoved {
            channel: ChannelId(5),
        },
        ServerMsg::ChannelRenamed {
            channel: ChannelId(1),
            name: "Hall".into(),
        },
        ServerMsg::Chat {
            from: SessionId(7),
            channel: ChannelId(1),
            text: "hello".into(),
            ts_ms: 1_700_000_000_123,
        },
        ServerMsg::Pong { nonce: 42 },
        ServerMsg::ScreenShare {
            user: SessionId(7),
            on: true,
        },
        ServerMsg::ScreenWatchers {
            count: 2,
            h264: true,
        },
    ];
    for code in ErrorCode::all() {
        samples.push(ServerMsg::Error {
            code,
            msg: format!("{code:?}"),
        });
    }
    let mut seen = [false; 13];
    for m in &samples {
        let idx = match m {
            ServerMsg::Welcome { .. } => 0,
            ServerMsg::Error { .. } => 1,
            ServerMsg::UserJoined(_) => 2,
            ServerMsg::UserLeft { .. } => 3,
            ServerMsg::UserMoved { .. } => 4,
            ServerMsg::UserState { .. } => 5,
            ServerMsg::ChannelAdded(_) => 6,
            ServerMsg::ChannelRemoved { .. } => 7,
            ServerMsg::ChannelRenamed { .. } => 8,
            ServerMsg::Chat { .. } => 9,
            ServerMsg::Pong { .. } => 10,
            ServerMsg::ScreenShare { .. } => 11,
            ServerMsg::ScreenWatchers { .. } => 12,
        };
        seen[idx] = true;
    }
    assert!(
        seen.iter().all(|s| *s),
        "every ServerMsg variant needs a sample"
    );
    samples
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_serde_roundtrip_all_variants() {
        let clients = all_client_samples();
        assert_eq!(clients.len(), 10);
        for m in clients {
            let bytes = crate::frame::encode(&m).unwrap();
            let mut d = crate::frame::FrameDecoder::new();
            d.push(&bytes);
            let back: ClientMsg = d.next().unwrap().unwrap();
            assert_eq!(back, m);
        }
        let servers = all_server_samples();
        assert_eq!(servers.len(), 12 + ErrorCode::all().len());
        for m in servers {
            let bytes = crate::frame::encode(&m).unwrap();
            let mut d = crate::frame::FrameDecoder::new();
            d.push(&bytes);
            let back: ServerMsg = d.next().unwrap().unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn error_code_u32_roundtrip() {
        for c in ErrorCode::all() {
            assert_eq!(ErrorCode::from_u32(c.as_u32()), Some(c));
        }
        assert_eq!(ErrorCode::from_u32(14), Some(ErrorCode::ImageTooLarge));
        assert_eq!(ErrorCode::all().len(), 14);
        assert_eq!(ErrorCode::from_u32(999), None);
    }

    #[test]
    fn welcome_without_voice_tuning_is_balanced() {
        // An older server sends no voice_tuning key.
        let json = r#"{"t":"Welcome","session_id":7,"server_name":"S","motd":"","default_channel":1,"channels":[],"users":[],"max_image_bytes":1024,"max_screen_fps":0,"max_screen_height":0}"#;
        let m: ServerMsg = serde_json::from_str(json).unwrap();
        match m {
            ServerMsg::Welcome { voice_tuning, .. } => {
                assert_eq!(voice_tuning, VoiceTuning::default())
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn voice_tuning_partial_fields_default() {
        let t: VoiceTuning =
            serde_json::from_str(r#"{"preset":"future","jitter_frames":2}"#).unwrap();
        assert_eq!(t.preset, "future");
        assert_eq!(t.jitter_frames, 2);
        assert_eq!(t.playback_keep_frames, 3);
    }

    #[test]
    fn voice_tuning_clamped() {
        let t = VoiceTuning {
            preset: "x".repeat(100),
            jitter_frames: 0,
            playback_keep_frames: 255,
        }
        .clamped();
        assert_eq!(t.jitter_frames, 1);
        assert_eq!(t.playback_keep_frames, MAX_VOICE_FRAMES);
        assert_eq!(t.preset.chars().count(), MAX_PRESET_NAME);
        // Multi-byte chars truncate on a char boundary.
        let t = VoiceTuning {
            preset: "é".repeat(40),
            ..VoiceTuning::default()
        }
        .clamped();
        assert_eq!(t.preset.chars().count(), MAX_PRESET_NAME);
        assert_eq!(VoiceTuning::default().clamped(), VoiceTuning::default());
    }
}
