//! Sans-IO tests of `ServerState::handle`.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use vc_proto::{
    ADPCM_FRAME_BYTES, ChannelId, ClientBlob, ClientMsg, ClientVoice, ErrorCode, PROTOCOL_VERSION,
    STREAM_BIT, ServerBlob, ServerMsg, ServerVoice, SessionId,
};
use vc_server::{Action, Event, ServerConfig, ServerState};

const LOBBY: ChannelId = ChannelId(1);
const AFK: ChannelId = ChannelId(2);

struct H {
    s: ServerState,
    t0: Instant,
}

fn sid(n: u32) -> SessionId {
    SessionId(n)
}

impl H {
    fn new() -> Self {
        Self::with(ServerConfig::default())
    }
    fn with(cfg: ServerConfig) -> Self {
        Self {
            s: ServerState::new(cfg),
            t0: Instant::now(),
        }
    }
    fn ev(&mut self, ev: Event) -> Vec<Action> {
        self.s.handle(ev)
    }
    fn msg(&mut self, n: u32, msg: ClientMsg) -> Vec<Action> {
        self.ev(Event::Msg { sid: sid(n), msg })
    }
    fn connect(&mut self, n: u32) -> Vec<Action> {
        self.ev(Event::Connected {
            sid: sid(n),
            now: self.t0,
        })
    }
    fn hello(&mut self, n: u32, nick: &str, pw: Option<&str>) -> Vec<Action> {
        self.hello_opts(n, nick, pw, false)
    }
    fn hello_opts(&mut self, n: u32, nick: &str, pw: Option<&str>, h264: bool) -> Vec<Action> {
        self.connect(n);
        self.msg(
            n,
            ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                nick: nick.into(),
                password: pw.map(Into::into),
                h264,
            },
        )
    }
    fn login(&mut self, n: u32, nick: &str) {
        self.login_opts(n, nick, false);
    }
    fn login_opts(&mut self, n: u32, nick: &str, h264: bool) {
        let a = self.hello_opts(n, nick, None, h264);
        assert!(matches!(
            a.first(),
            Some(Action::Send {
                msg: ServerMsg::Welcome { .. },
                ..
            })
        ));
    }
    fn voice(&mut self, n: u32, seq: u32) -> Vec<Action> {
        let payload = vec![7u8; ADPCM_FRAME_BYTES];
        let bytes = ClientVoice {
            seq,
            payload: &payload,
        }
        .encode();
        self.ev(Event::Voice {
            sid: sid(n),
            bytes: Bytes::from(bytes),
        })
    }
}

fn sends_to(a: &[Action], to: u32) -> Vec<ServerMsg> {
    a.iter()
        .filter_map(|x| match x {
            Action::Send { to: t, msg } if *t == sid(to) => Some(msg.clone()),
            _ => None,
        })
        .collect()
}

fn error_codes(a: &[Action], to: u32) -> Vec<ErrorCode> {
    sends_to(a, to)
        .into_iter()
        .filter_map(|m| match m {
            ServerMsg::Error { code, .. } => Some(code),
            _ => None,
        })
        .collect()
}

fn closes(a: &[Action]) -> Vec<(SessionId, ErrorCode)> {
    a.iter()
        .filter_map(|x| match x {
            Action::Close { sid, code, .. } => Some((*sid, *code)),
            _ => None,
        })
        .collect()
}

fn voice_targets(a: &[Action]) -> Vec<SessionId> {
    a.iter()
        .flat_map(|x| match x {
            Action::ForwardVoice { to, .. } => to.clone(),
            _ => vec![],
        })
        .collect()
}

fn channel_of(h: &H, n: u32) -> ChannelId {
    h.s.snapshot()
        .users
        .iter()
        .find(|u| u.id == sid(n))
        .unwrap()
        .channel
}

fn has_channel(h: &H, c: ChannelId) -> bool {
    h.s.snapshot().channels.iter().any(|x| x.id == c)
}

// ---------- handshake ----------

#[test]
fn hello_required_first() {
    let mut h = H::new();
    h.connect(1);
    let a = h.msg(1, ClientMsg::Join { channel: AFK });
    assert_eq!(error_codes(&a, 1), vec![ErrorCode::NotHandshaken]);
    let a = h.msg(1, ClientMsg::Chat { text: "x".into() });
    assert_eq!(error_codes(&a, 1), vec![ErrorCode::NotHandshaken]);
    // After Hello the same message works.
    h.msg(
        1,
        ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            nick: "a".into(),
            password: None,
            h264: false,
        },
    );
    let a = h.msg(1, ClientMsg::Join { channel: AFK });
    assert!(error_codes(&a, 1).is_empty());
    assert_eq!(channel_of(&h, 1), AFK);
}

#[test]
fn voice_before_hello_dropped() {
    let mut h = H::new();
    h.login(1, "listener");
    h.connect(2);
    assert!(h.voice(2, 0).is_empty());
    // Positive control: once handshaken, voice reaches the listener.
    h.msg(
        2,
        ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            nick: "talker".into(),
            password: None,
            h264: false,
        },
    );
    assert_eq!(voice_targets(&h.voice(2, 1)), vec![sid(1)]);
}

#[test]
fn version_mismatch_rejected() {
    let mut h = H::new();
    h.connect(1);
    let a = h.msg(
        1,
        ClientMsg::Hello {
            version: PROTOCOL_VERSION + 1,
            nick: "a".into(),
            password: None,
            h264: false,
        },
    );
    assert_eq!(error_codes(&a, 1), vec![ErrorCode::BadVersion]);
    assert_eq!(closes(&a), vec![(sid(1), ErrorCode::BadVersion)]);
    assert!(h.s.snapshot().users.is_empty());
}

#[test]
fn password_required_when_set() {
    let mut h = H::with(ServerConfig {
        password: Some("secret".into()),
        ..ServerConfig::default()
    });
    let a = h.hello(1, "a", None);
    assert_eq!(closes(&a), vec![(sid(1), ErrorCode::BadPassword)]);
    let a = h.hello(2, "b", Some("wrong"));
    assert_eq!(closes(&a), vec![(sid(2), ErrorCode::BadPassword)]);
    assert!(h.s.snapshot().users.is_empty());
}

#[test]
fn correct_password_accepted() {
    let mut h = H::with(ServerConfig {
        password: Some("secret".into()),
        ..ServerConfig::default()
    });
    let a = h.hello(1, "a", Some("secret"));
    assert!(closes(&a).is_empty());
    assert!(matches!(sends_to(&a, 1)[0], ServerMsg::Welcome { .. }));
}

#[test]
fn duplicate_nick_rejected_case_insensitive() {
    let mut h = H::new();
    h.login(1, "Alice");
    let a = h.hello(2, "aLiCe", None);
    assert_eq!(closes(&a), vec![(sid(2), ErrorCode::NickTaken)]);
    let a = h.hello(3, "  ALICE ", None);
    assert_eq!(closes(&a), vec![(sid(3), ErrorCode::NickTaken)]);
    // Different nick works.
    let a = h.hello(4, "bob", None);
    assert!(closes(&a).is_empty());
}

#[test]
fn nick_length_rules() {
    let mut h = H::new();
    for (i, bad) in ["", "   ", &"x".repeat(33)].iter().enumerate() {
        let a = h.hello(10 + i as u32, bad, None);
        assert_eq!(
            closes(&a),
            vec![(sid(10 + i as u32), ErrorCode::BadNick)],
            "{bad:?}"
        );
    }
    let a = h.hello(20, &"y".repeat(32), None);
    assert!(closes(&a).is_empty());
    let a = h.hello(21, "z", None);
    assert!(closes(&a).is_empty());
    // Trimmed before storing.
    h.login(22, "  spaced  ");
    assert!(h.s.snapshot().users.iter().any(|u| u.nick == "spaced"));
}

#[test]
fn max_users_enforced() {
    let mut h = H::with(ServerConfig {
        max_users: 2,
        ..ServerConfig::default()
    });
    h.login(1, "a");
    h.login(2, "b");
    let a = h.hello(3, "c", None);
    assert_eq!(closes(&a), vec![(sid(3), ErrorCode::ServerFull)]);
    // A slot frees up after a disconnect.
    h.ev(Event::Disconnected { sid: sid(1) });
    let a = h.hello(4, "d", None);
    assert!(closes(&a).is_empty());
}

#[test]
fn hello_deadline_closes() {
    let mut h = H::with(ServerConfig {
        hello_timeout_ms: 200,
        ..ServerConfig::default()
    });
    let t0 = h.t0;
    h.connect(1);
    h.connect(2);
    h.msg(
        2,
        ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            nick: "fast".into(),
            password: None,
            h264: false,
        },
    );
    let a = h.ev(Event::Tick {
        now: t0 + Duration::from_millis(100),
    });
    assert!(closes(&a).is_empty());
    let a = h.ev(Event::Tick {
        now: t0 + Duration::from_millis(250),
    });
    assert_eq!(closes(&a), vec![(sid(1), ErrorCode::HelloTimeout)]);
    // Closed once only.
    let a = h.ev(Event::Tick {
        now: t0 + Duration::from_millis(500),
    });
    assert!(closes(&a).is_empty());
}

#[test]
fn welcome_is_first_message_to_new_session() {
    let mut h = H::new();
    h.login(1, "a");
    let a = h.hello(2, "b", None);
    // The first Send of all actions goes to the new session and is Welcome.
    match &a[0] {
        Action::Send {
            to,
            msg: ServerMsg::Welcome { session_id, .. },
        } => {
            assert_eq!(*to, sid(2));
            assert_eq!(*session_id, sid(2));
        }
        other => panic!("first action not Welcome: {other:?}"),
    }
    let to_a = sends_to(&a, 1);
    assert!(matches!(&to_a[..], [ServerMsg::UserJoined(u)] if u.id == sid(2) && u.nick == "b"));
}

#[test]
fn welcome_lists_channels_users_motd() {
    let mut h = H::with(ServerConfig {
        name: "Srv".into(),
        motd: "hello motd".into(),
        channels: vec!["Lobby".into(), "AFK".into(), "Games".into()],
        ..ServerConfig::default()
    });
    h.login(1, "a");
    let a = h.hello(2, "b", None);
    match &sends_to(&a, 2)[0] {
        ServerMsg::Welcome {
            server_name,
            motd,
            default_channel,
            channels,
            users,
            ..
        } => {
            assert_eq!(server_name, "Srv");
            assert_eq!(motd, "hello motd");
            assert_eq!(*default_channel, LOBBY);
            let names: Vec<_> = channels.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(names, ["Lobby", "AFK", "Games"]);
            assert!(channels.iter().all(|c| c.permanent));
            let nicks: Vec<_> = users.iter().map(|u| u.nick.as_str()).collect();
            assert_eq!(nicks, ["a", "b"]);
        }
        other => panic!("{other:?}"),
    }
}

// ---------- channels ----------

#[test]
fn join_moves_and_broadcasts() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    let a = h.msg(1, ClientMsg::Join { channel: AFK });
    for to in [1, 2] {
        assert_eq!(
            sends_to(&a, to),
            vec![ServerMsg::UserMoved {
                user: sid(1),
                channel: AFK
            }]
        );
    }
    assert_eq!(channel_of(&h, 1), AFK);
    // Joining the channel you are in is a no-op.
    assert!(h.msg(1, ClientMsg::Join { channel: AFK }).is_empty());
}

#[test]
fn join_unknown_channel_errors() {
    let mut h = H::new();
    h.login(1, "a");
    let a = h.msg(
        1,
        ClientMsg::Join {
            channel: ChannelId(99),
        },
    );
    assert_eq!(error_codes(&a, 1), vec![ErrorCode::NoSuchChannel]);
    assert_eq!(channel_of(&h, 1), LOBBY);
}

fn create(h: &mut H, n: u32, name: &str) -> (Vec<Action>, ChannelId) {
    let a = h.msg(n, ClientMsg::CreateChannel { name: name.into() });
    let id = sends_to(&a, n)
        .into_iter()
        .find_map(|m| match m {
            ServerMsg::ChannelAdded(c) => Some(c.id),
            _ => None,
        })
        .expect("ChannelAdded");
    (a, id)
}

#[test]
fn create_temp_channel_creator_auto_joins() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    let (a, id) = create(&mut h, 1, "Raid");
    for to in [1, 2] {
        let m = sends_to(&a, to);
        assert!(matches!(&m[0], ServerMsg::ChannelAdded(c) if c.name == "Raid" && !c.permanent));
        assert_eq!(
            m[1],
            ServerMsg::UserMoved {
                user: sid(1),
                channel: id
            }
        );
    }
    assert_eq!(channel_of(&h, 1), id);
    assert!(has_channel(&h, id));
}

#[test]
fn create_channel_survives_until_last_leaves() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    let (_, id) = create(&mut h, 1, "Raid");
    h.msg(2, ClientMsg::Join { channel: id });
    let a = h.msg(1, ClientMsg::Join { channel: LOBBY });
    assert!(!sends_to(&a, 1).contains(&ServerMsg::ChannelRemoved { channel: id }));
    assert!(has_channel(&h, id));
    let a = h.msg(2, ClientMsg::Join { channel: AFK });
    assert!(sends_to(&a, 1).contains(&ServerMsg::ChannelRemoved { channel: id }));
    assert!(!has_channel(&h, id));
}

#[test]
fn temp_channel_deleted_when_empty_on_disconnect() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    let (_, id) = create(&mut h, 1, "Raid");
    let a = h.ev(Event::Disconnected { sid: sid(1) });
    let m = sends_to(&a, 2);
    assert_eq!(
        m,
        vec![
            ServerMsg::UserLeft { user: sid(1) },
            ServerMsg::ChannelRemoved { channel: id }
        ]
    );
    assert!(!has_channel(&h, id));
}

#[test]
fn delete_occupied_temp_channel_moves_members_to_default() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.login(3, "c");
    let (_, id) = create(&mut h, 1, "Raid");
    h.msg(2, ClientMsg::Join { channel: id });
    let a = h.msg(3, ClientMsg::DeleteChannel { channel: id });
    let m = sends_to(&a, 3);
    assert!(m.contains(&ServerMsg::UserMoved {
        user: sid(1),
        channel: LOBBY
    }));
    assert!(m.contains(&ServerMsg::UserMoved {
        user: sid(2),
        channel: LOBBY
    }));
    assert_eq!(m.last(), Some(&ServerMsg::ChannelRemoved { channel: id }));
    assert_eq!(channel_of(&h, 1), LOBBY);
    assert_eq!(channel_of(&h, 2), LOBBY);
    assert!(!has_channel(&h, id));
}

#[test]
fn permanent_channel_not_deletable() {
    let mut h = H::new();
    h.login(1, "a");
    let a = h.msg(1, ClientMsg::DeleteChannel { channel: AFK });
    assert_eq!(error_codes(&a, 1), vec![ErrorCode::NotAllowed]);
    assert!(has_channel(&h, AFK));
}

#[test]
fn default_channel_never_deletable_even_with_edit() {
    let mut h = H::with(ServerConfig {
        allow_channel_edit: true,
        ..ServerConfig::default()
    });
    h.login(1, "a");
    let a = h.msg(1, ClientMsg::DeleteChannel { channel: LOBBY });
    assert_eq!(error_codes(&a, 1), vec![ErrorCode::NotAllowed]);
    assert!(has_channel(&h, LOBBY));
}

#[test]
fn allow_channel_edit_permits_permanent_delete() {
    let mut h = H::with(ServerConfig {
        allow_channel_edit: true,
        ..ServerConfig::default()
    });
    h.login(1, "a");
    h.msg(1, ClientMsg::Join { channel: AFK });
    let a = h.msg(1, ClientMsg::DeleteChannel { channel: AFK });
    assert!(error_codes(&a, 1).is_empty());
    assert!(!has_channel(&h, AFK));
    assert_eq!(channel_of(&h, 1), LOBBY);
    // And rename of a permanent channel is allowed too.
    let a = h.msg(
        1,
        ClientMsg::RenameChannel {
            channel: LOBBY,
            name: "Hall".into(),
        },
    );
    assert_eq!(
        sends_to(&a, 1),
        vec![ServerMsg::ChannelRenamed {
            channel: LOBBY,
            name: "Hall".into()
        }]
    );
}

#[test]
fn rename_rules() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    // Permanent: not allowed without allow_channel_edit.
    let a = h.msg(
        1,
        ClientMsg::RenameChannel {
            channel: AFK,
            name: "Away".into(),
        },
    );
    assert_eq!(error_codes(&a, 1), vec![ErrorCode::NotAllowed]);
    // Temp: any user, even a non-member.
    let (_, id) = create(&mut h, 1, "Raid");
    let a = h.msg(
        2,
        ClientMsg::RenameChannel {
            channel: id,
            name: " Raid Night ".into(),
        },
    );
    assert_eq!(
        sends_to(&a, 1),
        vec![ServerMsg::ChannelRenamed {
            channel: id,
            name: "Raid Night".into()
        }]
    );
    // Case-only rename of itself is allowed (unique except itself).
    let a = h.msg(
        2,
        ClientMsg::RenameChannel {
            channel: id,
            name: "RAID NIGHT".into(),
        },
    );
    assert!(error_codes(&a, 2).is_empty());
    // Taken by another channel.
    let a = h.msg(
        2,
        ClientMsg::RenameChannel {
            channel: id,
            name: "lobby".into(),
        },
    );
    assert_eq!(error_codes(&a, 2), vec![ErrorCode::ChannelNameTaken]);
    // Unknown channel.
    let a = h.msg(
        2,
        ClientMsg::RenameChannel {
            channel: ChannelId(77),
            name: "x".into(),
        },
    );
    assert_eq!(error_codes(&a, 2), vec![ErrorCode::NoSuchChannel]);
}

#[test]
fn channel_name_unique_and_length() {
    let mut h = H::new();
    h.login(1, "a");
    for bad in ["", "   ", &"n".repeat(33)] {
        let a = h.msg(1, ClientMsg::CreateChannel { name: bad.into() });
        assert_eq!(
            error_codes(&a, 1),
            vec![ErrorCode::BadChannelName],
            "{bad:?}"
        );
    }
    let a = h.msg(
        1,
        ClientMsg::CreateChannel {
            name: "LOBBY".into(),
        },
    );
    assert_eq!(error_codes(&a, 1), vec![ErrorCode::ChannelNameTaken]);
    let (_, _) = create(&mut h, 1, &"n".repeat(32));
}

// ---------- chat ----------

#[test]
fn chat_only_to_channel_members() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b"); // same channel: positive control
    h.login(3, "c");
    h.msg(3, ClientMsg::Join { channel: AFK });
    let a = h.msg(1, ClientMsg::Chat { text: "hi".into() });
    let to_b = sends_to(&a, 2);
    assert!(
        matches!(&to_b[..], [ServerMsg::Chat { from, channel, text, .. }]
        if *from == sid(1) && *channel == LOBBY && text == "hi")
    );
    assert_eq!(sends_to(&a, 1).len(), 1, "sender gets its own line");
    assert!(sends_to(&a, 3).is_empty());
}

#[test]
fn chat_length_cap() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    let a = h.msg(
        1,
        ClientMsg::Chat {
            text: "é".repeat(501),
        },
    );
    assert_eq!(error_codes(&a, 1), vec![ErrorCode::ChatTooLong]);
    assert!(sends_to(&a, 2).is_empty());
    let a = h.msg(
        1,
        ClientMsg::Chat {
            text: "é".repeat(500),
        },
    );
    assert_eq!(sends_to(&a, 2).len(), 1);
}

#[test]
fn chat_has_timestamp() {
    let mut h = H::new();
    h.login(1, "a");
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let a = h.msg(1, ClientMsg::Chat { text: "t".into() });
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    match &sends_to(&a, 1)[0] {
        ServerMsg::Chat { ts_ms, .. } => assert!((before..=after).contains(ts_ms)),
        other => panic!("{other:?}"),
    }
}

// ---------- voice & state ----------

#[test]
fn voice_forwarded_only_same_channel() {
    let mut h = H::new();
    h.login(1, "talker");
    h.login(2, "same");
    h.login(3, "other");
    h.msg(3, ClientMsg::Join { channel: AFK });
    assert_eq!(voice_targets(&h.voice(1, 0)), vec![sid(2)]);
    // Move other into the channel: now it hears too.
    h.msg(3, ClientMsg::Join { channel: LOBBY });
    assert_eq!(voice_targets(&h.voice(1, 1)), vec![sid(2), sid(3)]);
}

#[test]
fn voice_not_to_deafened() {
    let mut h = H::new();
    h.login(1, "talker");
    h.login(2, "deaf");
    h.login(3, "hearing");
    h.msg(
        2,
        ClientMsg::SetVoiceState {
            muted: false,
            deafened: true,
        },
    );
    assert_eq!(voice_targets(&h.voice(1, 0)), vec![sid(3)]);
}

#[test]
fn voice_from_muted_dropped() {
    let mut h = H::new();
    h.login(1, "talker");
    h.login(2, "listener");
    assert_eq!(voice_targets(&h.voice(1, 0)), vec![sid(2)]);
    h.msg(
        1,
        ClientMsg::SetVoiceState {
            muted: true,
            deafened: false,
        },
    );
    assert!(h.voice(1, 1).is_empty());
    h.msg(
        1,
        ClientMsg::SetVoiceState {
            muted: false,
            deafened: false,
        },
    );
    assert_eq!(voice_targets(&h.voice(1, 2)), vec![sid(2)]);
}

#[test]
fn voice_not_echoed_to_sender() {
    let mut h = H::new();
    h.login(1, "talker");
    assert!(h.voice(1, 0).is_empty());
    h.login(2, "listener");
    let t = voice_targets(&h.voice(1, 1));
    assert_eq!(t, vec![sid(2)]);
}

#[test]
fn voice_header_has_speaker() {
    let mut h = H::new();
    h.login(1, "talker");
    h.login(2, "listener");
    let a = h.voice(1, 1234);
    let Action::ForwardVoice { bytes, .. } = &a[0] else {
        panic!("{a:?}")
    };
    let v = ServerVoice::parse(bytes).unwrap();
    assert_eq!(v.speaker, sid(1));
    assert_eq!(v.seq, 1234);
    assert_eq!(v.payload, &vec![7u8; ADPCM_FRAME_BYTES][..]);
    // Malformed voice is dropped.
    let a = h.ev(Event::Voice {
        sid: sid(1),
        bytes: Bytes::from_static(&[1, 2, 3]),
    });
    assert!(a.is_empty());
}

#[test]
fn voice_state_broadcast() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.msg(2, ClientMsg::Join { channel: AFK });
    let a = h.msg(
        1,
        ClientMsg::SetVoiceState {
            muted: true,
            deafened: false,
        },
    );
    let expect = ServerMsg::UserState {
        user: sid(1),
        muted: true,
        deafened: false,
    };
    assert_eq!(sends_to(&a, 1), vec![expect.clone()]);
    assert_eq!(sends_to(&a, 2), vec![expect], "broadcast to all channels");
}

#[test]
fn deafen_forces_mute() {
    let mut h = H::new();
    h.login(1, "a");
    let a = h.msg(
        1,
        ClientMsg::SetVoiceState {
            muted: false,
            deafened: true,
        },
    );
    assert_eq!(
        sends_to(&a, 1),
        vec![ServerMsg::UserState {
            user: sid(1),
            muted: true,
            deafened: true
        }]
    );
    let u = &h.s.snapshot().users[0];
    assert!(u.muted && u.deafened);
}

#[test]
fn ping_pong() {
    let mut h = H::new();
    h.login(1, "a");
    let a = h.msg(1, ClientMsg::Ping { nonce: 99 });
    assert_eq!(
        a,
        vec![Action::Send {
            to: sid(1),
            msg: ServerMsg::Pong { nonce: 99 }
        }]
    );
}

#[test]
fn disconnect_cleans_up_and_broadcasts() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    let a = h.ev(Event::Disconnected { sid: sid(1) });
    assert_eq!(sends_to(&a, 2), vec![ServerMsg::UserLeft { user: sid(1) }]);
    assert_eq!(h.s.snapshot().users.len(), 1);
    // Nick is free again.
    h.login(3, "a");
    // Unknown disconnect is a no-op.
    assert!(h.ev(Event::Disconnected { sid: sid(42) }).is_empty());
}

// ---------- images & screen share ----------

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 9, 9];

impl H {
    fn blob(&mut self, n: u32, header: ClientBlob, body: &[u8]) -> Vec<Action> {
        self.blob_at(n, header, body, self.t0)
    }
    fn blob_at(&mut self, n: u32, header: ClientBlob, body: &[u8], now: Instant) -> Vec<Action> {
        self.ev(Event::Blob {
            sid: sid(n),
            header,
            body: Bytes::copy_from_slice(body),
            now,
        })
    }
    fn share(&mut self, n: u32, on: bool) -> Vec<Action> {
        self.msg(n, ClientMsg::SetScreenShare { on })
    }
    fn watch(&mut self, n: u32, user: u32, on: bool) -> Vec<Action> {
        self.msg(
            n,
            ClientMsg::WatchScreen {
                user: sid(user),
                on,
            },
        )
    }
    fn frame_targets(&mut self, n: u32) -> Vec<SessionId> {
        blobs(&self.blob(n, ClientBlob::ScreenFrame, JPEG))
            .into_iter()
            .flat_map(|(to, _, _)| to)
            .collect()
    }
}

fn watcher_counts(a: &[Action], to: u32) -> Vec<u32> {
    sends_to(a, to)
        .into_iter()
        .filter_map(|m| match m {
            ServerMsg::ScreenWatchers { count, .. } => Some(count),
            _ => None,
        })
        .collect()
}

fn blobs(a: &[Action]) -> Vec<(Vec<SessionId>, ServerBlob, Bytes)> {
    a.iter()
        .filter_map(|x| match x {
            Action::SendBlob { to, header, body } => {
                Some((to.clone(), header.clone(), body.clone()))
            }
            _ => None,
        })
        .collect()
}

fn share_msgs(a: &[Action], to: u32) -> Vec<(SessionId, bool)> {
    sends_to(a, to)
        .into_iter()
        .filter_map(|m| match m {
            ServerMsg::ScreenShare { user, on } => Some((user, on)),
            _ => None,
        })
        .collect()
}

fn watcher_h264(a: &[Action], to: u32) -> Vec<(u32, bool)> {
    sends_to(a, to)
        .into_iter()
        .filter_map(|m| match m {
            ServerMsg::ScreenWatchers { count, h264 } => Some((count, h264)),
            _ => None,
        })
        .collect()
}

/// A minimal Annex-B access unit starting with an SPS NAL.
const H264: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE, 0, 0, 0, 1, 0x65, 0x88,
];

#[test]
fn welcome_carries_screen_caps() {
    let mut h = H::with(ServerConfig {
        max_screen_fps: 30,
        max_screen_height: 720,
        ..ServerConfig::default()
    });
    match &h.hello(1, "a", None)[0] {
        Action::Send {
            msg:
                ServerMsg::Welcome {
                    max_screen_fps,
                    max_screen_height,
                    ..
                },
            ..
        } => assert_eq!((*max_screen_fps, *max_screen_height), (30, 720)),
        other => panic!("{other:?}"),
    }
    // Defaults: no cap.
    let mut h = H::new();
    match &h.hello(1, "a", None)[0] {
        Action::Send {
            msg:
                ServerMsg::Welcome {
                    max_screen_fps,
                    max_screen_height,
                    ..
                },
            ..
        } => assert_eq!((*max_screen_fps, *max_screen_height), (0, 0)),
        other => panic!("{other:?}"),
    }
}

fn frame_at(h: &mut H, n: u32, now: Instant) -> usize {
    blobs(&h.blob_at(n, ClientBlob::ScreenFrame, JPEG, now)).len()
}

#[test]
fn screen_fps_cap_drops_too_fast_frames() {
    let mut h = H::with(ServerConfig {
        max_screen_fps: 10,
        ..ServerConfig::default()
    });
    h.login(1, "a");
    h.login(2, "b");
    h.share(1, true);
    h.watch(2, 1, true);
    let t = h.t0;
    let ms = |n| t + Duration::from_millis(n);
    // 10 fps cap, bucket of 2: two back-to-back frames pass, the third waits.
    assert_eq!(frame_at(&mut h, 1, t), 1);
    assert_eq!(frame_at(&mut h, 1, ms(20)), 1);
    assert_eq!(frame_at(&mut h, 1, ms(40)), 0);
    // 0.2 left at 20 ms, refilled at 10/s: a whole token again by about 100 ms.
    assert_eq!(frame_at(&mut h, 1, ms(105)), 1);
    assert_eq!(frame_at(&mut h, 1, ms(145)), 0, "0.45 tokens");
    // Steady at the cap afterwards: about one frame per 100 ms.
    assert_eq!(frame_at(&mut h, 1, ms(210)), 1);
    assert_eq!(frame_at(&mut h, 1, ms(320)), 1);
    assert_eq!(frame_at(&mut h, 1, ms(350)), 0);
}

fn capped(fps: u32) -> H {
    let mut h = H::with(ServerConfig {
        max_screen_fps: fps,
        ..ServerConfig::default()
    });
    h.login(1, "a");
    h.login(2, "b");
    h.share(1, true);
    h.watch(2, 1, true);
    h
}

#[test]
fn screen_fps_cap_absorbs_transfer_jitter() {
    let mut h = capped(30);
    let t = h.t0;
    // Sent every 33.3 ms, but arrival delay alternates +25 ms / +2 ms.
    let sent = (0..60)
        .map(|i| {
            let jitter = if i % 2 == 0 { 25_000 } else { 2_000 };
            let at = t + Duration::from_micros(i * 33_333 + jitter);
            frame_at(&mut h, 1, at)
        })
        .sum::<usize>();
    assert_eq!(sent, 60, "every honest frame is forwarded");
}

#[test]
fn screen_fps_cap_burst_of_three_forwards_two() {
    let mut h = capped(30);
    let t = h.t0;
    let n: usize = (0..3)
        .map(|i| frame_at(&mut h, 1, t + Duration::from_millis(i)))
        .sum();
    assert_eq!(n, 2);
}

#[test]
fn screen_fps_cap_holds_long_run_rate() {
    let mut h = capped(30);
    let t = h.t0;
    // 60 fps for 2 s under a 30 fps cap: at most 2 (burst) + 2 s x 30.
    let n: usize = (0..120)
        .map(|i| frame_at(&mut h, 1, t + Duration::from_micros(i * 16_667)))
        .sum();
    assert!(n <= 62, "{n} frames forwarded");
    assert!(n >= 58, "{n} frames forwarded");
}

#[test]
fn stop_share_refills_the_fps_bucket() {
    let mut h = capped(30);
    let t = h.t0;
    assert_eq!(frame_at(&mut h, 1, t), 1);
    assert_eq!(frame_at(&mut h, 1, t), 1);
    assert_eq!(frame_at(&mut h, 1, t), 0, "bucket empty");
    h.share(1, false);
    h.share(1, true);
    h.watch(2, 1, true);
    assert_eq!(frame_at(&mut h, 1, t), 1, "a new share starts full");
    assert_eq!(frame_at(&mut h, 1, t), 1);
}

#[test]
fn screen_fps_no_cap_when_zero() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.share(1, true);
    h.watch(2, 1, true);
    let t = h.t0;
    for _ in 0..5 {
        assert_eq!(frame_at(&mut h, 1, t), 1);
    }
}

#[test]
fn h264_as_image_dropped_and_as_frame_accepted() {
    let mut h = H::new();
    h.login_opts(1, "a", true);
    h.login_opts(2, "b", true);
    assert!(blobs(&h.blob(1, ClientBlob::Image, H264)).is_empty());
    h.share(1, true);
    h.watch(2, 1, true);
    assert_eq!(blobs(&h.blob(1, ClientBlob::ScreenFrame, H264)).len(), 1);
    // Garbage is still not a screen frame.
    assert!(blobs(&h.blob(1, ClientBlob::ScreenFrame, b"GIF89a....")).is_empty());
}

#[test]
fn h264_frames_skip_non_h264_watchers() {
    let mut h = H::new();
    h.login_opts(1, "a", true);
    h.login_opts(2, "b", true);
    h.login(3, "c");
    h.share(1, true);
    h.watch(2, 1, true);
    h.watch(3, 1, true);
    let b = blobs(&h.blob(1, ClientBlob::ScreenFrame, H264));
    assert_eq!(b[0].0, vec![sid(2)]);
    // JPEG goes to every watcher.
    assert_eq!(h.frame_targets(1), vec![sid(2), sid(3)]);
}

#[test]
fn screen_watchers_h264_flag() {
    let mut h = H::new();
    h.login_opts(1, "a", true);
    h.login_opts(2, "b", true);
    h.login(3, "c");
    h.share(1, true);
    assert_eq!(watcher_h264(&h.watch(2, 1, true), 1), vec![(1, true)]);
    // One watcher without H.264 flips it off.
    assert_eq!(watcher_h264(&h.watch(3, 1, true), 1), vec![(2, false)]);
    // And it flips back on when that watcher leaves.
    assert_eq!(watcher_h264(&h.watch(3, 1, false), 1), vec![(1, true)]);
    // No watchers: false.
    assert_eq!(watcher_h264(&h.watch(2, 1, false), 1), vec![(0, false)]);
}

fn stream_voice(h: &mut H, n: u32, seq: u32) -> Vec<Action> {
    h.voice(n, seq | STREAM_BIT)
}

fn forwarded(a: &[Action]) -> Vec<(Vec<SessionId>, SessionId, u32)> {
    a.iter()
        .filter_map(|x| match x {
            Action::ForwardVoice { to, bytes } => {
                let v = ServerVoice::parse(bytes).unwrap();
                Some((to.clone(), v.speaker, v.seq))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn stream_sound_reaches_only_undeafened_watchers() {
    let mut h = H::new();
    h.login(1, "sharer");
    h.login(2, "watcher");
    h.login(3, "deaf");
    h.login(4, "bystander");
    // Not sharing: dropped.
    assert!(stream_voice(&mut h, 1, 1).is_empty());
    h.share(1, true);
    h.watch(2, 1, true);
    h.watch(3, 1, true);
    h.msg(
        3,
        ClientMsg::SetVoiceState {
            muted: false,
            deafened: true,
        },
    );
    let f = forwarded(&stream_voice(&mut h, 1, 7));
    assert_eq!(
        f,
        vec![(vec![sid(2)], SessionId(1 | STREAM_BIT), 7 | STREAM_BIT)]
    );
    // Works while the sharer's mic is muted; mic voice does not.
    h.msg(
        1,
        ClientMsg::SetVoiceState {
            muted: true,
            deafened: false,
        },
    );
    assert_eq!(forwarded(&stream_voice(&mut h, 1, 8)).len(), 1);
    assert!(h.voice(1, 9).is_empty(), "muted mic is still gated");
}

#[test]
fn normal_voice_unchanged_by_stream_tag() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.login(3, "c");
    h.share(1, true);
    // Plain voice still goes to the whole channel with the untagged speaker.
    let f = forwarded(&h.voice(1, 3));
    assert_eq!(f, vec![(vec![sid(2), sid(3)], sid(1), 3)]);
}

#[test]
fn next_session_id_skips_stream_bit_and_zero() {
    use vc_server::net::next_session_id;
    assert_eq!(next_session_id(1), 2);
    assert_eq!(next_session_id(STREAM_BIT - 1), 1);
    assert_eq!(next_session_id(u32::MAX), 1);
    for s in [5, STREAM_BIT - 2, u32::MAX - 1] {
        assert_eq!(next_session_id(s) & STREAM_BIT, 0);
        assert_ne!(next_session_id(s), 0);
    }
}

#[test]
fn welcome_carries_image_limit_and_enables_blobs() {
    let mut h = H::with(ServerConfig {
        max_image_bytes: 12_345,
        ..ServerConfig::default()
    });
    let a = h.hello(1, "a", None);
    match &a[0] {
        Action::Send {
            msg: ServerMsg::Welcome {
                max_image_bytes, ..
            },
            ..
        } => assert_eq!(*max_image_bytes, 12_345),
        other => panic!("{other:?}"),
    }
    assert_eq!(a[1], Action::EnableBlobs { sid: sid(1) });
    // A rejected Hello never enables blob streams.
    let a = h.hello(2, "a", None);
    assert!(!a.iter().any(|x| matches!(x, Action::EnableBlobs { .. })));
}

#[test]
fn welcome_carries_voice_tuning() {
    let mut h = H::with(ServerConfig {
        voice_preset: vc_server::VoicePreset::Competitive,
        ..ServerConfig::default()
    });
    let a = h.hello(1, "a", None);
    match &a[0] {
        Action::Send {
            msg: ServerMsg::Welcome { voice_tuning, .. },
            ..
        } => assert_eq!(*voice_tuning, vc_server::VoicePreset::Competitive.tuning()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn image_scoped_to_channel_includes_sender_with_ts() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.login(3, "c");
    h.msg(3, ClientMsg::Join { channel: AFK });
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let a = h.blob(1, ClientBlob::Image, PNG);
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let b = blobs(&a);
    assert_eq!(b.len(), 1);
    let (to, header, body) = &b[0];
    assert_eq!(to, &vec![sid(1), sid(2)]);
    assert_eq!(&body[..], PNG);
    match header {
        ServerBlob::Image {
            from,
            channel,
            ts_ms,
        } => {
            assert_eq!(*from, sid(1));
            assert_eq!(*channel, LOBBY);
            assert!((before..=after).contains(ts_ms));
        }
        other => panic!("{other:?}"),
    }
    // JPEG is accepted too.
    assert_eq!(
        blobs(&h.blob(3, ClientBlob::Image, JPEG))[0].0,
        vec![sid(3)]
    );
}

#[test]
fn oversize_image_and_blob_too_large_give_image_too_large() {
    let mut h = H::with(ServerConfig {
        max_image_bytes: 16,
        ..ServerConfig::default()
    });
    h.login(1, "a");
    let mut big = PNG.to_vec();
    big.resize(17, 0);
    let a = h.blob(1, ClientBlob::Image, &big);
    assert!(blobs(&a).is_empty());
    assert_eq!(error_codes(&a, 1), vec![ErrorCode::ImageTooLarge]);
    big.truncate(16);
    assert_eq!(blobs(&h.blob(1, ClientBlob::Image, &big)).len(), 1);
    let a = h.ev(Event::BlobTooLarge { sid: sid(1) });
    assert_eq!(error_codes(&a, 1), vec![ErrorCode::ImageTooLarge]);
    assert!(closes(&a).is_empty(), "never kicks");
    // Unknown session: nothing.
    assert!(h.ev(Event::BlobTooLarge { sid: sid(9) }).is_empty());
}

#[test]
fn bad_blobs_dropped() {
    let mut h = H::new();
    h.connect(5);
    assert!(h.blob(5, ClientBlob::Image, PNG).is_empty(), "before Hello");
    h.login(1, "a");
    assert!(h.blob(1, ClientBlob::Image, b"").is_empty(), "empty");
    assert!(
        h.blob(1, ClientBlob::Image, b"GIF89a....").is_empty(),
        "not PNG/JPEG"
    );
    assert!(h.blob(9, ClientBlob::Image, PNG).is_empty(), "unknown sid");
}

#[test]
fn screen_share_broadcast_and_idempotent() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.msg(2, ClientMsg::Join { channel: AFK });
    let a = h.share(1, true);
    // Everyone hears it, the sharer included, whatever their channel.
    assert_eq!(share_msgs(&a, 1), vec![(sid(1), true)]);
    assert_eq!(share_msgs(&a, 2), vec![(sid(1), true)]);
    assert!(h.share(1, true).is_empty(), "no change, no broadcast");
    assert!(h.s.snapshot().users[0].sharing);
    let a = h.share(1, false);
    assert_eq!(share_msgs(&a, 2), vec![(sid(1), false)]);
    assert!(h.share(1, false).is_empty());
}

#[test]
fn late_joiner_sees_sharing() {
    let mut h = H::new();
    h.login(1, "a");
    h.share(1, true);
    let a = h.hello(2, "b", None);
    match &sends_to(&a, 2)[0] {
        ServerMsg::Welcome { users, .. } => {
            assert!(users.iter().find(|u| u.id == sid(1)).unwrap().sharing);
            assert!(!users.iter().find(|u| u.id == sid(2)).unwrap().sharing);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn screen_frames_only_from_sharer_and_not_echoed() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.login(3, "c");
    h.msg(3, ClientMsg::Join { channel: AFK });
    assert!(
        h.blob(1, ClientBlob::ScreenFrame, JPEG).is_empty(),
        "not sharing"
    );
    h.share(1, true);
    h.watch(2, 1, true);
    let b = blobs(&h.blob(1, ClientBlob::ScreenFrame, JPEG));
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].0, vec![sid(2)], "channel only, never the sender");
    assert_eq!(b[0].1, ServerBlob::ScreenFrame { from: sid(1) });
    // Over the fixed frame cap: dropped (no error).
    let mut big = JPEG.to_vec();
    big.resize(vc_proto::MAX_SCREEN_FRAME_BYTES as usize + 1, 0);
    assert!(h.blob(1, ClientBlob::ScreenFrame, &big).is_empty());
    // Alone in a channel: nobody to send to.
    h.share(3, true);
    assert!(h.blob(3, ClientBlob::ScreenFrame, JPEG).is_empty());
}

#[test]
fn move_stops_share_and_broadcasts_off() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.share(1, true);
    let a = h.msg(1, ClientMsg::Join { channel: AFK });
    assert_eq!(share_msgs(&a, 1), vec![(sid(1), false)]);
    assert_eq!(share_msgs(&a, 2), vec![(sid(1), false)]);
    // ScreenShare off goes out before UserMoved.
    let m = sends_to(&a, 2);
    let off = m
        .iter()
        .position(|x| matches!(x, ServerMsg::ScreenShare { .. }))
        .unwrap();
    let moved = m
        .iter()
        .position(|x| matches!(x, ServerMsg::UserMoved { .. }))
        .unwrap();
    assert!(off < moved);
    assert!(!h.s.snapshot().users[0].sharing);
    assert!(h.blob(1, ClientBlob::ScreenFrame, JPEG).is_empty());
    // Moving while not sharing sends no ScreenShare.
    let a = h.msg(1, ClientMsg::Join { channel: LOBBY });
    assert!(share_msgs(&a, 2).is_empty());
}

#[test]
fn delete_channel_stops_members_share() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    let (_, id) = create(&mut h, 1, "Raid");
    h.share(1, true);
    let a = h.msg(2, ClientMsg::DeleteChannel { channel: id });
    assert_eq!(share_msgs(&a, 2), vec![(sid(1), false)]);
    assert_eq!(share_msgs(&a, 1), vec![(sid(1), false)]);
    assert!(!h.s.snapshot().users[0].sharing);
}

#[test]
fn frames_reach_only_watchers() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.login(3, "c");
    h.share(1, true);
    assert!(h.frame_targets(1).is_empty(), "nobody watches yet");
    h.watch(2, 1, true);
    assert_eq!(h.frame_targets(1), vec![sid(2)]);
    h.watch(3, 1, true);
    assert_eq!(h.frame_targets(1), vec![sid(2), sid(3)]);
    h.watch(2, 1, false);
    assert_eq!(h.frame_targets(1), vec![sid(3)]);
}

#[test]
fn watch_ignored_for_non_sharer_other_channel_and_self() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.login(3, "c");
    // Not sharing.
    assert!(h.watch(2, 1, true).is_empty());
    h.share(1, true);
    assert!(h.frame_targets(1).is_empty());
    // Other channel.
    h.msg(3, ClientMsg::Join { channel: AFK });
    assert!(h.watch(3, 1, true).is_empty());
    // Self.
    assert!(h.watch(1, 1, true).is_empty());
    // Unknown user.
    assert!(h.watch(2, 99, true).is_empty());
    assert!(h.frame_targets(1).is_empty());
    // Unwatching something never watched sends nothing.
    assert!(h.watch(2, 1, false).is_empty());
}

#[test]
fn sharer_hears_watcher_count() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.login(3, "c");
    h.share(1, true);
    assert_eq!(watcher_counts(&h.watch(2, 1, true), 1), vec![1]);
    assert!(h.watch(2, 1, true).is_empty(), "no change, no count");
    assert_eq!(watcher_counts(&h.watch(3, 1, true), 1), vec![2]);
    assert_eq!(watcher_counts(&h.watch(2, 1, false), 1), vec![1]);
    assert_eq!(watcher_counts(&h.watch(3, 1, false), 1), vec![0]);
    // Only the sharer hears it.
    let a = h.watch(2, 1, true);
    assert!(watcher_counts(&a, 2).is_empty());
}

#[test]
fn stop_then_reshare_sends_frames_to_nobody() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.share(1, true);
    h.watch(2, 1, true);
    let a = h.share(1, false);
    assert_eq!(share_msgs(&a, 2), vec![(sid(1), false)]);
    assert_eq!(watcher_counts(&a, 1), vec![0]);
    h.share(1, true);
    assert!(h.frame_targets(1).is_empty(), "watching is per share");
}

#[test]
fn move_clears_watching_both_ways() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.login(3, "c");
    h.share(1, true);
    h.share(2, true);
    h.watch(2, 1, true);
    h.watch(3, 2, true);
    // Viewer 2 (also a sharer watched by 3) moves away.
    let a = h.msg(2, ClientMsg::Join { channel: AFK });
    assert_eq!(watcher_counts(&a, 1), vec![0], "1 lost its watcher");
    assert_eq!(watcher_counts(&a, 2), vec![0], "2's share ended");
    assert!(h.frame_targets(1).is_empty());
    // Coming back does not restore anything.
    h.msg(2, ClientMsg::Join { channel: LOBBY });
    assert!(h.frame_targets(1).is_empty());
}

#[test]
fn delete_channel_clears_watching() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.login(3, "c");
    let (_, id) = create(&mut h, 1, "Raid");
    h.msg(2, ClientMsg::Join { channel: id });
    h.share(1, true);
    h.watch(2, 1, true);
    let a = h.msg(3, ClientMsg::DeleteChannel { channel: id });
    assert_eq!(watcher_counts(&a, 1), vec![0]);
    // Both now in the lobby; 1 is no longer sharing, re-share has no watchers.
    h.share(1, true);
    assert!(h.frame_targets(1).is_empty());
}

#[test]
fn delete_channel_clears_sharer_and_viewer_in_it() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    let (_, id) = create(&mut h, 2, "Raid");
    h.login(3, "c");
    h.msg(3, ClientMsg::Join { channel: id });
    h.share(3, true);
    h.watch(2, 3, true);
    let a = h.msg(1, ClientMsg::DeleteChannel { channel: id });
    // 3's share ended and 2's edge is gone: every count 3 hears is 0.
    let counts = watcher_counts(&a, 3);
    assert!(!counts.is_empty() && counts.iter().all(|c| *c == 0));
    h.share(3, true);
    assert!(h.frame_targets(3).is_empty());
}

#[test]
fn viewer_disconnect_resends_count() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.share(1, true);
    h.watch(2, 1, true);
    let a = h.ev(Event::Disconnected { sid: sid(2) });
    assert_eq!(watcher_counts(&a, 1), vec![0]);
    assert!(h.frame_targets(1).is_empty());
}

#[test]
fn sharer_disconnect_sends_nothing_to_leaver() {
    let mut h = H::new();
    h.login(1, "a");
    h.login(2, "b");
    h.share(1, true);
    h.watch(2, 1, true);
    let a = h.ev(Event::Disconnected { sid: sid(1) });
    assert!(sends_to(&a, 1).is_empty());
    // 2's edge is gone: a new user 1 re-sharing gets no frames to 2.
    h.login(1, "a2");
    h.share(1, true);
    assert!(h.frame_targets(1).is_empty());
}
