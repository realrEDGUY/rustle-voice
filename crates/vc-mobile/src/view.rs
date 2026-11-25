//! What the Swift UI draws, as JSON. `Snapshot` is structural and changes rarely;
//! `Live` holds the fast-changing values (levels, ping, who is speaking). Swift
//! mirrors both with `Codable` structs, and the golden files in `tests/golden`
//! keep the two sides in step.

use std::time::Instant;

use serde::Serialize;
use vc_client::ServerKey;
use vc_client::app::{AppModel, DEFAULT_NICK};
use vc_client::profile::InputMode;
use vc_client::session::{ClientSession, ConnStatus};
use vc_client::tofu;

/// Chat lines sent for the selected server (the newest ones).
pub const CHAT_LINES: usize = 200;

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub selected: Option<ServerKey>,
    /// The nick the add-server form proposes.
    pub default_nick: String,
    pub settings: SettingsView,
    pub audio_warning: Option<String>,
    pub toasts: Vec<String>,
    pub tofu: Option<TofuView>,
    pub voice: VoiceView,
    pub servers: Vec<ServerView>,
    pub streams: Vec<StreamView>,
}

#[derive(Debug, Serialize)]
pub struct SettingsView {
    /// `"Vad"`, `"Ptt"` or `"Open"`.
    pub input_mode: &'static str,
    pub vad_threshold_db: f32,
    pub master_volume: f32,
    pub nickname: String,
}

#[derive(Debug, Serialize)]
pub struct TofuView {
    pub key: ServerKey,
    pub label: String,
    pub fingerprint: String,
}

#[derive(Debug, Serialize)]
pub struct VoiceView {
    pub talk_target: Option<ServerKey>,
    pub muted: bool,
    pub deafened: bool,
    pub server_label: Option<String>,
    pub channel_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusView {
    /// `disconnected`, `connecting`, `connected`, `reconnecting` or `failed`.
    pub kind: &'static str,
    pub label: String,
}

#[derive(Debug, Serialize)]
pub struct ServerView {
    pub key: ServerKey,
    pub label: String,
    pub addr: String,
    pub nick: String,
    pub password: String,
    pub status: StatusView,
    pub server_name: String,
    pub motd: String,
    pub voice_preset: String,
    pub my_id: Option<u32>,
    pub my_channel: Option<u32>,
    pub default_channel: Option<u32>,
    pub channels: Vec<ChannelView>,
    /// Only for the selected server; empty for the others.
    pub chat: Vec<ChatLineView>,
}

#[derive(Debug, Serialize)]
pub struct ChannelView {
    pub id: u32,
    pub name: String,
    pub permanent: bool,
    pub users: Vec<UserView>,
}

#[derive(Debug, Serialize)]
pub struct UserView {
    pub id: u32,
    pub nick: String,
    pub me: bool,
    pub muted: bool,
    pub deafened: bool,
    pub sharing: bool,
    /// My volume for this user, 0.0-2.0.
    pub volume: f32,
    pub muted_for_me: bool,
    pub watching: bool,
    pub stream_muted: bool,
}

#[derive(Debug, Serialize)]
pub struct ChatLineView {
    pub from: String,
    pub text: String,
    pub ts_ms: u64,
    pub image: Option<ChatImageView>,
}

#[derive(Debug, Serialize)]
pub struct ChatImageView {
    pub id: u64,
    pub w: u32,
    pub h: u32,
    /// The decoded pixels are still kept (else "[image expired]").
    pub available: bool,
}

#[derive(Debug, Serialize)]
pub struct StreamView {
    pub key: ServerKey,
    pub server: String,
    pub user: u32,
    pub nick: String,
    pub sound_muted: bool,
}

#[derive(Debug, Serialize)]
pub struct Live {
    pub transmitting: bool,
    /// Rounded to a whole dB.
    pub input_level_db: i32,
    pub servers: Vec<LiveServer>,
}

#[derive(Debug, Serialize)]
pub struct LiveServer {
    pub key: ServerKey,
    pub ping_ms: Option<u64>,
    pub speaking: Vec<u32>,
}

pub fn input_mode_name(m: InputMode) -> &'static str {
    match m {
        InputMode::Vad => "Vad",
        InputMode::Ptt => "Ptt",
        InputMode::Open => "Open",
    }
}

pub fn status_view(s: &ConnStatus) -> StatusView {
    let kind = match s {
        ConnStatus::Disconnected => "disconnected",
        ConnStatus::Connecting => "connecting",
        ConnStatus::Connected => "connected",
        ConnStatus::Reconnecting { .. } => "reconnecting",
        ConnStatus::Failed(_) => "failed",
    };
    StatusView {
        kind,
        label: s.label(),
    }
}

fn label_of(model: &AppModel, key: ServerKey, s: Option<&ClientSession>) -> String {
    model
        .saved_server(key)
        .map(|x| x.label.clone())
        .or_else(|| s.map(|s| s.server_name.clone()))
        .unwrap_or_default()
}

fn channels(model: &AppModel, key: ServerKey, s: &ClientSession) -> Vec<ChannelView> {
    s.channels
        .values()
        .map(|c| ChannelView {
            id: c.id.0,
            name: c.name.clone(),
            permanent: c.permanent,
            users: s
                .users_in(c.id)
                .into_iter()
                .map(|u| UserView {
                    id: u.id.0,
                    nick: u.nick.clone(),
                    me: Some(u.id) == s.my_id,
                    muted: u.muted,
                    deafened: u.deafened,
                    sharing: u.sharing,
                    volume: model.settings.user_volume(key, &u.nick),
                    muted_for_me: model.user_is_muted(key, &u.nick),
                    watching: s.watching.contains(&u.id),
                    stream_muted: model.stream_is_muted(key, u.id),
                })
                .collect(),
        })
        .collect()
}

fn chat(s: &ClientSession) -> Vec<ChatLineView> {
    let Some(ch) = s.my_channel() else {
        return vec![];
    };
    let lines = s.chat_in(ch);
    let skip = lines.len().saturating_sub(CHAT_LINES);
    lines
        .into_iter()
        .skip(skip)
        .map(|l| ChatLineView {
            from: l.from_nick.clone(),
            text: l.text.clone(),
            ts_ms: l.ts_ms,
            image: l.image.map(|i| ChatImageView {
                id: i.id,
                w: i.w,
                h: i.h,
                available: s.images.contains_key(&i.id),
            }),
        })
        .collect()
}

/// The structural view of the whole app (every server).
pub fn snapshot(model: &AppModel) -> Snapshot {
    let servers = model
        .saved
        .iter()
        .map(|saved| {
            let key = saved.key;
            let s = model.session(key);
            let connected = s.filter(|s| s.status == ConnStatus::Connected);
            ServerView {
                key,
                label: saved.label.clone(),
                addr: saved.addr.clone(),
                nick: saved.nick.clone(),
                password: saved.password.clone().unwrap_or_default(),
                status: status_view(&model.status(key)),
                server_name: s.map(|s| s.server_name.clone()).unwrap_or_default(),
                motd: s.map(|s| s.motd.clone()).unwrap_or_default(),
                voice_preset: s.map(|s| s.voice_tuning.preset.clone()).unwrap_or_default(),
                my_id: connected.and_then(|s| s.my_id).map(|i| i.0),
                my_channel: connected.and_then(|s| s.my_channel()).map(|c| c.0),
                default_channel: connected.and_then(|s| s.default_channel).map(|c| c.0),
                channels: connected
                    .map(|s| channels(model, key, s))
                    .unwrap_or_default(),
                chat: match s {
                    Some(s) if model.selected == Some(key) => chat(s),
                    _ => vec![],
                },
            }
        })
        .collect();
    let streams = model
        .sessions
        .iter()
        .filter(|(_, s)| s.status == ConnStatus::Connected)
        .flat_map(|(key, s)| {
            let server = label_of(model, *key, Some(s));
            s.watched_here().into_iter().map(move |u| StreamView {
                key: *key,
                server: server.clone(),
                user: u.id.0,
                nick: u.nick.clone(),
                sound_muted: model.stream_is_muted(*key, u.id),
            })
        })
        .collect();
    let voice = {
        let target = model
            .talk_target
            .filter(|k| model.status(*k) == ConnStatus::Connected);
        let s = target.and_then(|k| model.session(k));
        VoiceView {
            talk_target: target,
            muted: model.voice.muted,
            deafened: model.voice.deafened,
            server_label: target.map(|k| label_of(model, k, s)),
            channel_name: s.and_then(|s| {
                s.my_channel()
                    .and_then(|c| s.channels.get(&c))
                    .map(|c| c.name.clone())
            }),
        }
    };
    let tofu = model.ui.tofu_prompt.map(|(key, fp)| TofuView {
        key,
        label: label_of(model, key, model.session(key)),
        fingerprint: tofu::to_hex(&fp),
    });
    Snapshot {
        selected: model.selected,
        default_nick: if model.settings.nickname.trim().is_empty() {
            DEFAULT_NICK.to_string()
        } else {
            model.settings.nickname.clone()
        },
        settings: SettingsView {
            input_mode: input_mode_name(model.settings.input_mode),
            vad_threshold_db: model.settings.vad_threshold_db,
            master_volume: model.settings.master_volume,
            nickname: model.settings.nickname.clone(),
        },
        audio_warning: model.audio_warning.clone(),
        toasts: model.ui.toasts.clone(),
        tofu,
        voice,
        servers,
        streams,
    }
}

/// The fast-changing view at `now`.
pub fn live(model: &AppModel, now: Instant) -> Live {
    Live {
        transmitting: model.transmitting,
        input_level_db: model.input_level_db.round().clamp(-100.0, 0.0) as i32,
        servers: model
            .saved
            .iter()
            .map(|saved| {
                let s = model.session(saved.key);
                let mut speaking: Vec<u32> = s
                    .map(|s| {
                        s.users
                            .keys()
                            .filter(|id| s.is_speaking(**id, now))
                            .map(|id| id.0)
                            .collect()
                    })
                    .unwrap_or_default();
                speaking.sort_unstable();
                LiveServer {
                    key: saved.key,
                    ping_ms: s
                        .filter(|s| s.status == ConnStatus::Connected)
                        .and_then(|s| s.ping_ms),
                    speaking,
                }
            })
            .collect(),
    }
}
