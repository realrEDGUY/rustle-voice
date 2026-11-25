//! Sans-IO server core. The net layer feeds `Event`s and executes the returned `Action`s.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use vc_proto::{
    ChannelId, ChannelInfo, ClientBlob, ClientMsg, ClientVoice, ErrorCode, MAX_CHAT, MAX_NICK,
    MAX_SCREEN_FRAME_BYTES, PROTOCOL_VERSION, STREAM_BIT, ScreenKind, ServerBlob, ServerMsg,
    ServerVoice, SessionId, UserInfo, sniff, sniff_screen,
};

use crate::config::{ServerConfig, valid_channel_name};

#[derive(Debug, Clone)]
pub enum Event {
    Connected {
        sid: SessionId,
        now: Instant,
    },
    Msg {
        sid: SessionId,
        msg: ClientMsg,
    },
    Voice {
        sid: SessionId,
        bytes: Bytes,
    },
    /// A complete blob stream (header plus body) from a client.
    Blob {
        sid: SessionId,
        header: ClientBlob,
        body: Bytes,
        /// When the blob finished arriving (drives the screen fps rate-limit).
        now: Instant,
    },
    /// An image blob stream ran past `max_image_bytes`; the net layer already reset it.
    BlobTooLarge {
        sid: SessionId,
    },
    Disconnected {
        sid: SessionId,
    },
    Tick {
        now: Instant,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Send {
        to: SessionId,
        msg: ServerMsg,
    },
    /// `bytes` already carries the speaker header.
    ForwardVoice {
        to: Vec<SessionId>,
        bytes: Bytes,
    },
    Close {
        sid: SessionId,
        code: ErrorCode,
        reason: String,
    },
    /// Open one blob stream per recipient carrying `header` then `body`.
    SendBlob {
        to: Vec<SessionId>,
        header: ServerBlob,
        body: Bytes,
    },
    /// Allow this peer to open blob (uni) streams. Emitted right after Welcome.
    EnableBlobs {
        sid: SessionId,
    },
}

#[derive(Debug, Clone)]
struct Channel {
    name: String,
    permanent: bool,
}

#[derive(Debug, Clone)]
struct User {
    nick: String,
    channel: ChannelId,
    muted: bool,
    deafened: bool,
    sharing: bool,
    /// Sharers in this user's channel whose frames it receives.
    watching: BTreeSet<SessionId>,
    /// This client can decode H.264 screen frames (from Hello).
    h264: bool,
    /// Screen fps cap as a token bucket: up to `FRAME_BURST` frames of credit,
    /// refilled at `max_screen_fps` per second, so transfer jitter never drops an
    /// honest clamped sharer while the long-run rate stays capped.
    frame_tokens: f64,
    /// When `frame_tokens` was last refilled (`None` = full, nothing sent yet).
    tokens_at: Option<Instant>,
}

/// Screen frames a sharer may send back to back under the fps cap.
const FRAME_BURST: f64 = 2.0;

/// Read-only view of the server state (for logs, tests and diagnostics).
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub channels: Vec<ChannelInfo>,
    pub users: Vec<UserInfo>,
}

pub struct ServerState {
    cfg: ServerConfig,
    channels: BTreeMap<ChannelId, Channel>,
    default_channel: ChannelId,
    next_channel: u32,
    pending: HashMap<SessionId, Instant>,
    users: BTreeMap<SessionId, User>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn err(to: SessionId, code: ErrorCode, msg: impl Into<String>) -> Action {
    Action::Send {
        to,
        msg: ServerMsg::Error {
            code,
            msg: msg.into(),
        },
    }
}

impl ServerState {
    pub fn new(cfg: ServerConfig) -> Self {
        let mut channels = BTreeMap::new();
        let mut next_channel = 1;
        for name in &cfg.channels {
            channels.insert(
                ChannelId(next_channel),
                Channel {
                    name: name.trim().to_string(),
                    permanent: true,
                },
            );
            next_channel += 1;
        }
        Self {
            cfg,
            channels,
            default_channel: ChannelId(1),
            next_channel,
            pending: HashMap::new(),
            users: BTreeMap::new(),
        }
    }

    pub fn config(&self) -> &ServerConfig {
        &self.cfg
    }

    pub fn default_channel(&self) -> ChannelId {
        self.default_channel
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            channels: self.channel_infos(),
            users: self.user_infos(),
        }
    }

    fn channel_infos(&self) -> Vec<ChannelInfo> {
        self.channels
            .iter()
            .map(|(id, c)| ChannelInfo {
                id: *id,
                name: c.name.clone(),
                permanent: c.permanent,
            })
            .collect()
    }

    fn user_info(&self, id: SessionId, u: &User) -> UserInfo {
        let _ = self;
        UserInfo {
            id,
            nick: u.nick.clone(),
            channel: u.channel,
            muted: u.muted,
            deafened: u.deafened,
            sharing: u.sharing,
        }
    }

    fn user_infos(&self) -> Vec<UserInfo> {
        self.users
            .iter()
            .map(|(id, u)| self.user_info(*id, u))
            .collect()
    }

    fn broadcast(&self, msg: ServerMsg, out: &mut Vec<Action>) {
        for id in self.users.keys() {
            out.push(Action::Send {
                to: *id,
                msg: msg.clone(),
            });
        }
    }

    fn reject(sid: SessionId, code: ErrorCode, reason: &str, out: &mut Vec<Action>) {
        out.push(err(sid, code, reason));
        out.push(Action::Close {
            sid,
            code,
            reason: reason.to_string(),
        });
    }

    pub fn handle(&mut self, ev: Event) -> Vec<Action> {
        let mut out = Vec::new();
        match ev {
            Event::Connected { sid, now } => {
                self.pending
                    .insert(sid, now + Duration::from_millis(self.cfg.hello_timeout_ms));
            }
            Event::Tick { now } => {
                let expired: Vec<SessionId> = self
                    .pending
                    .iter()
                    .filter(|(_, deadline)| **deadline <= now)
                    .map(|(sid, _)| *sid)
                    .collect();
                for sid in expired {
                    self.pending.remove(&sid);
                    Self::reject(sid, ErrorCode::HelloTimeout, "no Hello in time", &mut out);
                }
            }
            Event::Disconnected { sid } => {
                self.pending.remove(&sid);
                if let Some(u) = self.users.remove(&sid) {
                    // The leaver is gone, so only its watchers' edges and its targets' counts remain.
                    self.clear_watchers_of(sid, &mut out);
                    self.resend_counts(&u.watching, &mut out);
                    self.broadcast(ServerMsg::UserLeft { user: sid }, &mut out);
                    self.cleanup_temp(u.channel, &mut out);
                }
            }
            Event::Voice { sid, bytes } => self.on_voice(sid, bytes, &mut out),
            Event::Blob {
                sid,
                header,
                body,
                now,
            } => self.on_blob(sid, header, body, now, &mut out),
            Event::BlobTooLarge { sid } => {
                if self.users.contains_key(&sid) {
                    out.push(err(sid, ErrorCode::ImageTooLarge, "image too large"));
                }
            }
            Event::Msg { sid, msg } => self.on_msg(sid, msg, &mut out),
        }
        out
    }

    fn on_voice(&self, sid: SessionId, bytes: Bytes, out: &mut Vec<Action>) {
        let Some(sender) = self.users.get(&sid) else {
            return; // voice before Hello is dropped
        };
        let Ok(voice) = ClientVoice::parse(&bytes) else {
            return;
        };
        if voice.is_stream() {
            // Stream sound is not mic audio: no mute/deafen gate, only watchers get it.
            if !sender.sharing {
                return;
            }
            let to: Vec<SessionId> = self
                .users
                .iter()
                .filter(|(id, u)| {
                    **id != sid
                        && u.channel == sender.channel
                        && u.watching.contains(&sid)
                        && !u.deafened
                })
                .map(|(id, _)| *id)
                .collect();
            if to.is_empty() {
                return;
            }
            out.push(Action::ForwardVoice {
                to,
                bytes: Bytes::from(ServerVoice::from_client(
                    SessionId(sid.0 | STREAM_BIT),
                    &bytes,
                )),
            });
            return;
        }
        if sender.muted || sender.deafened {
            return;
        }
        let to: Vec<SessionId> = self
            .users
            .iter()
            .filter(|(id, u)| **id != sid && u.channel == sender.channel && !u.deafened)
            .map(|(id, _)| *id)
            .collect();
        if to.is_empty() {
            return;
        }
        out.push(Action::ForwardVoice {
            to,
            bytes: Bytes::from(ServerVoice::from_client(sid, &bytes)),
        });
    }

    fn on_blob(
        &mut self,
        sid: SessionId,
        header: ClientBlob,
        body: Bytes,
        now: Instant,
        out: &mut Vec<Action>,
    ) {
        let max_fps = self.cfg.max_screen_fps;
        let Some(sender) = self.users.get_mut(&sid) else {
            tracing::debug!(%sid, "blob before Hello dropped");
            return;
        };
        let channel = sender.channel;
        match header {
            ClientBlob::Image => {
                if sniff(&body).is_none() {
                    tracing::debug!(%sid, len = body.len(), "non-image blob dropped");
                    return;
                }
                if body.len() > self.cfg.max_image_bytes as usize {
                    out.push(err(sid, ErrorCode::ImageTooLarge, "image too large"));
                    return;
                }
                let to: Vec<SessionId> = self
                    .users
                    .iter()
                    .filter(|(_, u)| u.channel == channel)
                    .map(|(id, _)| *id)
                    .collect();
                out.push(Action::SendBlob {
                    to,
                    header: ServerBlob::Image {
                        from: sid,
                        channel,
                        ts_ms: now_ms(),
                    },
                    body,
                });
            }
            ClientBlob::ScreenFrame => {
                let Some(kind) = sniff_screen(&body) else {
                    tracing::debug!(%sid, len = body.len(), "non-screen blob dropped");
                    return;
                };
                if !sender.sharing || body.len() > MAX_SCREEN_FRAME_BYTES as usize {
                    tracing::debug!(%sid, "screen frame dropped");
                    return;
                }
                if max_fps > 0 {
                    if let Some(at) = sender.tokens_at {
                        let refill =
                            now.saturating_duration_since(at).as_secs_f64() * max_fps as f64;
                        sender.frame_tokens = (sender.frame_tokens + refill).min(FRAME_BURST);
                    }
                    sender.tokens_at = Some(now);
                    if sender.frame_tokens < 1.0 {
                        tracing::debug!(%sid, max_fps, "screen frame over the fps cap dropped");
                        return;
                    }
                    sender.frame_tokens -= 1.0;
                }
                let h264 = kind == ScreenKind::H264;
                let to: Vec<SessionId> = self
                    .users
                    .iter()
                    .filter(|(id, u)| {
                        **id != sid
                            && u.channel == channel
                            && u.watching.contains(&sid)
                            && (!h264 || u.h264)
                    })
                    .map(|(id, _)| *id)
                    .collect();
                if to.is_empty() {
                    return;
                }
                out.push(Action::SendBlob {
                    to,
                    header: ServerBlob::ScreenFrame { from: sid },
                    body,
                });
            }
        }
    }

    fn on_hello(
        &mut self,
        sid: SessionId,
        version: u32,
        nick: String,
        password: Option<String>,
        h264: bool,
        out: &mut Vec<Action>,
    ) {
        if self.users.contains_key(&sid) {
            out.push(err(sid, ErrorCode::NotAllowed, "already handshaken"));
            return;
        }
        if version != PROTOCOL_VERSION {
            self.pending.remove(&sid);
            Self::reject(
                sid,
                ErrorCode::BadVersion,
                &format!("server speaks protocol {PROTOCOL_VERSION}, client sent {version}"),
                out,
            );
            return;
        }
        if let Some(expected) = &self.cfg.password
            && password.as_deref() != Some(expected.as_str())
        {
            self.pending.remove(&sid);
            Self::reject(sid, ErrorCode::BadPassword, "wrong password", out);
            return;
        }
        let nick = nick.trim().to_string();
        let len = nick.chars().count();
        if len == 0 || len > MAX_NICK || nick.chars().any(char::is_control) {
            self.pending.remove(&sid);
            Self::reject(sid, ErrorCode::BadNick, "nick must be 1-32 characters", out);
            return;
        }
        let lower = nick.to_lowercase();
        if self.users.values().any(|u| u.nick.to_lowercase() == lower) {
            self.pending.remove(&sid);
            Self::reject(sid, ErrorCode::NickTaken, "nick already in use", out);
            return;
        }
        if self.users.len() >= self.cfg.max_users {
            self.pending.remove(&sid);
            Self::reject(sid, ErrorCode::ServerFull, "server is full", out);
            return;
        }
        self.pending.remove(&sid);
        let user = User {
            nick,
            channel: self.default_channel,
            muted: false,
            deafened: false,
            sharing: false,
            watching: BTreeSet::new(),
            h264,
            frame_tokens: FRAME_BURST,
            tokens_at: None,
        };
        let info = self.user_info(sid, &user);
        self.users.insert(sid, user);
        out.push(Action::Send {
            to: sid,
            msg: ServerMsg::Welcome {
                session_id: sid,
                server_name: self.cfg.name.clone(),
                motd: self.cfg.motd.clone(),
                default_channel: self.default_channel,
                channels: self.channel_infos(),
                users: self.user_infos(),
                max_image_bytes: self.cfg.max_image_bytes,
                max_screen_fps: self.cfg.max_screen_fps,
                max_screen_height: self.cfg.max_screen_height,
                voice_tuning: self.cfg.voice_preset.tuning(),
            },
        });
        out.push(Action::EnableBlobs { sid });
        for id in self.users.keys().filter(|id| **id != sid) {
            out.push(Action::Send {
                to: *id,
                msg: ServerMsg::UserJoined(info.clone()),
            });
        }
    }

    fn on_msg(&mut self, sid: SessionId, msg: ClientMsg, out: &mut Vec<Action>) {
        if let ClientMsg::Hello {
            version,
            nick,
            password,
            h264,
        } = msg
        {
            self.on_hello(sid, version, nick, password, h264, out);
            return;
        }
        if !self.users.contains_key(&sid) {
            out.push(err(sid, ErrorCode::NotHandshaken, "send Hello first"));
            return;
        }
        match msg {
            ClientMsg::Hello { .. } => unreachable!("handled above"),
            ClientMsg::Join { channel } => {
                if !self.channels.contains_key(&channel) {
                    out.push(err(sid, ErrorCode::NoSuchChannel, "no such channel"));
                    return;
                }
                self.move_user(sid, channel, out);
            }
            ClientMsg::CreateChannel { name } => {
                if let Err((code, m)) = self.check_name(&name, None) {
                    out.push(err(sid, code, m));
                    return;
                }
                let id = ChannelId(self.next_channel);
                self.next_channel += 1;
                let ch = Channel {
                    name: name.trim().to_string(),
                    permanent: false,
                };
                let info = ChannelInfo {
                    id,
                    name: ch.name.clone(),
                    permanent: false,
                };
                self.channels.insert(id, ch);
                self.broadcast(ServerMsg::ChannelAdded(info), out);
                self.move_user(sid, id, out);
            }
            ClientMsg::RenameChannel { channel, name } => {
                let Some(ch) = self.channels.get(&channel) else {
                    out.push(err(sid, ErrorCode::NoSuchChannel, "no such channel"));
                    return;
                };
                if ch.permanent && !self.cfg.allow_channel_edit {
                    out.push(err(sid, ErrorCode::NotAllowed, "permanent channel"));
                    return;
                }
                if let Err((code, m)) = self.check_name(&name, Some(channel)) {
                    out.push(err(sid, code, m));
                    return;
                }
                let name = name.trim().to_string();
                if let Some(ch) = self.channels.get_mut(&channel) {
                    ch.name = name.clone();
                }
                self.broadcast(ServerMsg::ChannelRenamed { channel, name }, out);
            }
            ClientMsg::DeleteChannel { channel } => {
                let Some(ch) = self.channels.get(&channel) else {
                    out.push(err(sid, ErrorCode::NoSuchChannel, "no such channel"));
                    return;
                };
                if channel == self.default_channel {
                    out.push(err(sid, ErrorCode::NotAllowed, "default channel"));
                    return;
                }
                if ch.permanent && !self.cfg.allow_channel_edit {
                    out.push(err(sid, ErrorCode::NotAllowed, "permanent channel"));
                    return;
                }
                let members: Vec<SessionId> = self
                    .users
                    .iter()
                    .filter(|(_, u)| u.channel == channel)
                    .map(|(id, _)| *id)
                    .collect();
                let default = self.default_channel;
                for m in members {
                    self.stop_share(m, out);
                    self.clear_watching_by(m, out);
                    if let Some(u) = self.users.get_mut(&m) {
                        u.channel = default;
                    }
                    self.broadcast(
                        ServerMsg::UserMoved {
                            user: m,
                            channel: default,
                        },
                        out,
                    );
                }
                self.channels.remove(&channel);
                self.broadcast(ServerMsg::ChannelRemoved { channel }, out);
            }
            ClientMsg::Chat { text } => {
                if text.chars().count() > MAX_CHAT {
                    out.push(err(sid, ErrorCode::ChatTooLong, "chat message too long"));
                    return;
                }
                if text.trim().is_empty() {
                    return;
                }
                let channel = self.users[&sid].channel;
                let ts_ms = now_ms();
                for (id, u) in &self.users {
                    if u.channel == channel {
                        out.push(Action::Send {
                            to: *id,
                            msg: ServerMsg::Chat {
                                from: sid,
                                channel,
                                text: text.clone(),
                                ts_ms,
                            },
                        });
                    }
                }
            }
            ClientMsg::SetVoiceState { muted, deafened } => {
                let muted = muted || deafened;
                if let Some(u) = self.users.get_mut(&sid) {
                    u.muted = muted;
                    u.deafened = deafened;
                }
                self.broadcast(
                    ServerMsg::UserState {
                        user: sid,
                        muted,
                        deafened,
                    },
                    out,
                );
            }
            ClientMsg::Ping { nonce } => out.push(Action::Send {
                to: sid,
                msg: ServerMsg::Pong { nonce },
            }),
            ClientMsg::SetScreenShare { on: false } => self.stop_share(sid, out),
            ClientMsg::SetScreenShare { on: true } => {
                let Some(u) = self.users.get_mut(&sid) else {
                    return;
                };
                if u.sharing {
                    return;
                }
                u.sharing = true;
                self.broadcast(
                    ServerMsg::ScreenShare {
                        user: sid,
                        on: true,
                    },
                    out,
                );
            }
            ClientMsg::WatchScreen { user, on } => {
                let changed = if on {
                    let my_channel = self.users[&sid].channel;
                    let ok = user != sid
                        && self
                            .users
                            .get(&user)
                            .is_some_and(|t| t.sharing && t.channel == my_channel);
                    ok && self
                        .users
                        .get_mut(&sid)
                        .is_some_and(|u| u.watching.insert(user))
                } else {
                    self.users
                        .get_mut(&sid)
                        .is_some_and(|u| u.watching.remove(&user))
                };
                if changed {
                    self.send_watchers(user, out);
                }
            }
        }
    }

    fn check_name(
        &self,
        name: &str,
        except: Option<ChannelId>,
    ) -> Result<(), (ErrorCode, &'static str)> {
        if !valid_channel_name(name) {
            return Err((
                ErrorCode::BadChannelName,
                "channel name must be 1-32 characters",
            ));
        }
        let lower = name.trim().to_lowercase();
        if self
            .channels
            .iter()
            .any(|(id, c)| Some(*id) != except && c.name.to_lowercase() == lower)
        {
            return Err((ErrorCode::ChannelNameTaken, "channel name already in use"));
        }
        Ok(())
    }

    /// End a user's screen share (if any) and tell everyone, the sharer included.
    fn stop_share(&mut self, sid: SessionId, out: &mut Vec<Action>) {
        let Some(u) = self.users.get_mut(&sid) else {
            return;
        };
        if !u.sharing {
            return;
        }
        u.sharing = false;
        u.frame_tokens = FRAME_BURST;
        u.tokens_at = None;
        self.broadcast(
            ServerMsg::ScreenShare {
                user: sid,
                on: false,
            },
            out,
        );
        self.clear_watchers_of(sid, out);
    }

    /// Number of users currently receiving `sharer`'s frames.
    fn watcher_count(&self, sharer: SessionId) -> u32 {
        self.users
            .values()
            .filter(|u| u.watching.contains(&sharer))
            .count() as u32
    }

    /// True when `sharer` has at least one watcher and every watcher decodes H.264.
    fn watchers_h264(&self, sharer: SessionId) -> bool {
        let mut watchers = self
            .users
            .values()
            .filter(|u| u.watching.contains(&sharer))
            .peekable();
        watchers.peek().is_some() && watchers.all(|u| u.h264)
    }

    /// Tell `sharer` (if still connected) how many users watch it and whether all
    /// of them decode H.264. Sent on every watch-edge change, which covers every
    /// change of `(count, h264)`.
    fn send_watchers(&self, sharer: SessionId, out: &mut Vec<Action>) {
        if !self.users.contains_key(&sharer) {
            return;
        }
        out.push(Action::Send {
            to: sharer,
            msg: ServerMsg::ScreenWatchers {
                count: self.watcher_count(sharer),
                h264: self.watchers_h264(sharer),
            },
        });
    }

    /// Drop every watch edge pointing at `sharer`; tells the sharer it has 0 watchers.
    fn clear_watchers_of(&mut self, sharer: SessionId, out: &mut Vec<Action>) {
        for u in self.users.values_mut() {
            u.watching.remove(&sharer);
        }
        self.send_watchers(sharer, out);
    }

    /// Drop everything `viewer` watches and re-send the affected sharers' counts.
    fn clear_watching_by(&mut self, viewer: SessionId, out: &mut Vec<Action>) {
        let Some(u) = self.users.get_mut(&viewer) else {
            return;
        };
        let former = std::mem::take(&mut u.watching);
        self.resend_counts(&former, out);
    }

    fn resend_counts(&self, sharers: &BTreeSet<SessionId>, out: &mut Vec<Action>) {
        for s in sharers {
            self.send_watchers(*s, out);
        }
    }

    fn move_user(&mut self, sid: SessionId, to: ChannelId, out: &mut Vec<Action>) {
        let Some(from) = self.users.get(&sid).map(|u| u.channel) else {
            return;
        };
        if from == to {
            return;
        }
        self.stop_share(sid, out);
        self.clear_watching_by(sid, out);
        if let Some(u) = self.users.get_mut(&sid) {
            u.channel = to;
        }
        self.broadcast(
            ServerMsg::UserMoved {
                user: sid,
                channel: to,
            },
            out,
        );
        self.cleanup_temp(from, out);
    }

    /// Delete a temporary channel once its last member is gone.
    fn cleanup_temp(&mut self, channel: ChannelId, out: &mut Vec<Action>) {
        let Some(ch) = self.channels.get(&channel) else {
            return;
        };
        if ch.permanent || self.users.values().any(|u| u.channel == channel) {
            return;
        }
        self.channels.remove(&channel);
        self.broadcast(ServerMsg::ChannelRemoved { channel }, out);
    }
}
